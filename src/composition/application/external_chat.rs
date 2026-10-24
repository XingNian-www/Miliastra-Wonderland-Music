//! 外部聊天源线程：把 Miliastra-Wonderland-Chat 的推送注入时间线，并接管 OCR 的聊天内容识别。
//!
//! 与 `shared_observer` 平行：那边用模板匹配判界面、用 OCR 读文字；这边用 WS 读文字。
//! 界面识别仍由 `shared_observer` 负责，本模块只提供"当前是否接管"这一信号。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::config::ChatSourceConfig;
use crate::observation::chat::ChatTimeline;
use crate::observation::chat::ViewAbsence;
use crate::runtime::chat_source::ws::WebSocketChatSource;
use miliastra_kernel::chat_source::{ChatSource, ChatSourceEvent, ExternalChatMessage};

/// 断线超过这个时长就不再补拉：一次性重放大量命令比丢几条更糟。
const MAX_BACKLOG_IDLE_MS: u64 = 60_000;

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// 外部聊天源共享状态：供观察线程查询"该不该跳过 OCR"。
#[derive(Clone, Default)]
pub(crate) struct ExternalChatState {
    inner: Arc<ExternalChatInner>,
}

#[derive(Default)]
struct ExternalChatInner {
    /// 昵称 → 游戏 UID。外部源每条消息都带 UID，这里缓存下来供歌单等
    /// 需要"按人定位"的功能使用。程序重启后为空，此时歌单退化到 name: 键。
    nick_to_uid: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// 是否处于接管状态（已连接且租约未过期）。
    active: AtomicBool,
    /// 最近一次收到消息的序号，用于日志与断线补齐。
    last_sequence: AtomicU64,
    /// 启动水位：握手时的最大序号。
    ///
    /// 只接受大于它的消息，从而丢弃"程序启动前"的历史。只在**首次连接**
    /// 建立，重连不更新——否则断线期间到达的消息会被永久丢弃。
    watermark: AtomicU64,
    /// 是否已建立过水位。
    watermark_set: AtomicBool,
    /// 最后一次收到事件的时间（毫秒时间戳），用于判断断线时长。
    last_seen_ms: AtomicU64,
    /// 注入用的保留会话号；重连且需要丢弃旧墓碑时递增。
    epoch: AtomicU64,
}

impl ExternalChatState {
    /// 观察线程用它决定是否跳过 OCR 聊天识别。
    pub(crate) fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::Acquire)
    }

    pub(crate) fn last_sequence(&self) -> u64 {
        self.inner.last_sequence.load(Ordering::Acquire)
    }

    fn set_active(&self, active: bool) {
        if self.inner.active.swap(active, Ordering::AcqRel) != active {
            log::info!(
                "外部聊天源{}，{}",
                if active { "已接管" } else { "已退出" },
                if active {
                    "OCR 聊天内容识别停止，界面识别继续"
                } else {
                    "恢复 OCR 聊天识别"
                }
            );
        }
    }

    /// 记住一条消息的"昵称 → UID"对应关系（只缓存有昵称的）。
    pub(crate) fn remember_uid(&self, nickname: Option<&str>, uid: Option<&str>) {
        let (Some(name), Some(uid)) = (nickname, uid) else {
            return;
        };
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if let Ok(mut map) = self.inner.nick_to_uid.lock() {
            map.insert(name.to_string(), uid.to_string());
        }
    }

    /// 查某人最近一次使用的游戏 UID。
    pub(crate) fn uid_for(&self, nickname: &str) -> Option<String> {
        self.inner
            .nick_to_uid
            .lock()
            .ok()?
            .get(nickname.trim())
            .cloned()
    }

    /// 记录一次活跃（收到事件时调用）。
    pub(crate) fn touch(&self) {
        self.inner
            .last_seen_ms
            .store(now_millis(), Ordering::Release);
    }

    /// 距今多久没有活动，毫秒。从未活跃过返回 `None`。
    pub(crate) fn idle_millis(&self) -> Option<u64> {
        let seen = self.inner.last_seen_ms.load(Ordering::Acquire);
        (seen > 0).then(|| now_millis().saturating_sub(seen))
    }

    /// 首次连接时建立启动水位；已有水位则不动（重连场景）。
    pub(crate) fn establish_watermark(&self, sequence: u64) {
        if !self.inner.watermark_set.swap(true, Ordering::AcqRel) {
            self.inner.watermark.store(sequence, Ordering::Release);
            log::info!("外部聊天源建立启动水位: 序号 ≤ {sequence} 的历史消息将被忽略");
        }
    }

    /// 强制把水位提高到指定序号（跳过补拉时用）。
    pub(crate) fn force_watermark(&self, sequence: u64) {
        self.inner.watermark_set.store(true, Ordering::Release);
        self.inner.watermark.store(sequence, Ordering::Release);
    }

    /// 当前水位；未建立时为 0（表示不过滤）。
    pub(crate) fn watermark(&self) -> u64 {
        if self.inner.watermark_set.load(Ordering::Acquire) {
            self.inner.watermark.load(Ordering::Acquire)
        } else {
            0
        }
    }

    fn bump_epoch(&self) -> u64 {
        self.inner.epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::Acquire)
    }
}

/// 启动外部聊天线程。返回共享状态；调用方把它交给观察线程。
pub(crate) fn spawn(
    timeline: ChatTimeline,
    config: ChatSourceConfig,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    listener_control: Option<std::sync::Arc<dyn Fn(crate::runtime::chat_listener::ChatListenerMode) + Send + Sync>>,
) -> Result<ExternalChatState> {
    let state = ExternalChatState::default();
    if !config.enabled {
        return Ok(state);
    }
    let worker_state = state.clone();
    thread::Builder::new()
        .name("external-chat".into())
        .spawn(move || run(timeline, config, identity_access, listener_control, worker_state))
        .map_err(|error| anyhow::anyhow!("启动外部聊天线程失败: {error}"))?;
    Ok(state)
}

fn run(
    timeline: ChatTimeline,
    config: ChatSourceConfig,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    listener_control: Option<std::sync::Arc<dyn Fn(crate::runtime::chat_listener::ChatListenerMode) + Send + Sync>>,
    state: ExternalChatState,
) {
    let lease = Duration::from_millis(config.lease_timeout_ms);
    let reconnect = Duration::from_millis(config.reconnect_ms);
    loop {
        // 登录器角色：Miliastra-Wonderland-Chat 没在监听时替用户拉起来。
        // 探测很便宜（300ms 超时），每次重连都试一次，这样用户中途关掉也能被重新拉起。
        if config.auto_start
            && let Err(error) = crate::runtime::chat_source::launcher::ensure_running(&config)
        {
            log::warn!("无法自动启动 Miliastra-Wonderland-Chat，等待人工处理: {error:#}");
        }
        let mut source = match WebSocketChatSource::new(config.url.clone(), config.token.clone()) {
            Ok(source) => source,
            Err(error) => {
                log::error!("外部聊天源初始化失败: {error:#}");
                thread::sleep(reconnect);
                continue;
            }
        };
        let mut last_event_at = Instant::now();
        loop {
            // 租约超时：外部源失联太久，必须回落到 OCR，否则聊天完全失聪。
            if last_event_at.elapsed() > lease {
                state.set_active(false);
            }
            match source.next_event() {
                Ok(Some(event)) => {
                    last_event_at = Instant::now();
                    match event {
                        ChatSourceEvent::Connected { last_sequence } => {
                            state.set_active(true);
                            log::info!("外部聊天源握手完成: lastSequence={last_sequence}");
                            // 首次连接：把启动前的历史挡在水位之外。
                            // 重连时水位已存在，不动它——断线期间的消息要补拉。
                            state.establish_watermark(last_sequence);
                            // API 模式默认走二级值守：消息来自 socket 而非 OCR，
                            // 二级界面停留时延迟最低。切不进去也不影响消息接收。
                            if let Some(control) = listener_control.as_ref() {
                                control(crate::runtime::chat_listener::ChatListenerMode::Secondary);
                            }
                            // 断线补拉：本地最后处理到哪，就从那里往后要。
                            // 但断线太久就不补了——一次性涌入几百条命令比丢几条更糟：
                            // 玩家看到的是 BOT 突然把一分钟前的命令全部重放一遍。
                            let local = state.last_sequence();
                            let idle = state.idle_millis();
                            let skip_backlog = idle.is_some_and(|ms| ms > MAX_BACKLOG_IDLE_MS);
                            if skip_backlog {
                                log::info!(
                                    "断线 {:?} 超过 {} 秒，跳过补拉并从当前序号重新开始",
                                    idle.map(std::time::Duration::from_millis),
                                    MAX_BACKLOG_IDLE_MS / 1000
                                );
                                // 把水位推到服务端当前值：断线期间的消息一律不执行。
                                state.force_watermark(last_sequence);
                            }
                            if !skip_backlog && last_sequence > local {
                                match crate::runtime::chat_source::fetch_messages_since(
                                    &ws_url_to_http(&config.url),
                                    &config.token,
                                    local,
                                ) {
                                    Ok(batch) => {
                                        if batch.truncated {
                                            // 缓冲不够，中间可能有缺口：重建基线而不是继续叠加。
                                            log::warn!(
                                                "断线补拉出现缺口（truncated），重建聊天基线"
                                            );
                                            reset_after_source_change(&timeline);
                                        }
                                        if !batch.messages.is_empty() {
                                            log::info!(
                                                "断线补拉 {} 条消息（since={local}）",
                                                batch.messages.len()
                                            );
                                            deliver(
                                                &timeline,
                                                &state,
                                                &config,
                                                identity_access.as_ref(),
                                                &config.url,
                                                batch.messages,
                                            );
                                        }
                                    }
                                    Err(error) => {
                                        log::warn!("断线补拉失败，继续接收新消息: {error:#}")
                                    }
                                }
                            }
                        }
                        ChatSourceEvent::HallChanged { hall_id, hall_type } => {
                            log::info!(
                                "外部聊天源报告大厅切换: hallId={hall_id} hallType={hall_type:?}，重置大厅相关状态"
                            );
                            reset_after_source_change(&timeline);
                            state.set_active(true);
                        }
                        ChatSourceEvent::Messages(messages) => {
                            state.set_active(true);
                            deliver(
                                &timeline,
                                &state,
                                &config,
                                identity_access.as_ref(),
                                &config.url,
                                messages,
                            );
                        }
                    }
                }
                Ok(None) => {
                    // 连接正常关闭（或读超时）：继续下一轮，不退出线程。
                    if last_event_at.elapsed() > lease {
                        state.set_active(false);
                    }
                    continue;
                }
                Err(error) => {
                    log::warn!("外部聊天源读取失败，{}ms 后重连: {error:#}", config.reconnect_ms);
                    state.set_active(false);
                    // 重连前重置：旧墓碑与会话号都不再适用于新连接。
                    state.bump_epoch();
                    reset_after_source_change(&timeline);
                    thread::sleep(reconnect);
                    break;
                }
            }
        }
    }
}

fn deliver(
    timeline: &ChatTimeline,
    state: &ExternalChatState,
    config: &ChatSourceConfig,
    identity_access: Option<&crate::features::identity::IdentityAccess>,
    source: &str,
    messages: Vec<ExternalChatMessage>,
) {
    let mut messages = messages;
    // 昵称异步到达是常态（API 文档 §5），缺了就补查一次；
    // 仍查不到就留空，上层会按"未知发送者"处理、不授权任何权限。
    let http_base = ws_url_to_http(&config.url);
    for message in &mut messages {
        if message.sender.nickname.is_none() {
            if let Some(uid) = message.sender.uid.as_ref() {
                match crate::runtime::chat_source::resolve_missing_name(
                    &http_base,
                    &config.token,
                    uid.as_str(),
                ) {
                    Ok(Some(name)) => {
                        log::debug!("昵称补查命中: uid={} name={name}", uid.as_str());
                        message.sender.nickname = Some(name);
                    }
                    Ok(None) => log::debug!("昵称补查无结果，按未知发送者处理: uid={}", uid.as_str()),
                    Err(error) => log::debug!("昵称补查失败: {error:#}"),
                }
            }
        }
        state.remember_uid(
            message.sender.nickname.as_deref(),
            message.sender.uid.as_ref().map(|uid| uid.as_str()),
        );
        // 好友私聊的发送者若还没在身份映射里登记，自动建一条 friend 身份。
        // 依据是 UID（聊天 API 给的最可靠账号标识），昵称仅作可读备注。
        if message.channel.is_private() {
            if let Some(uid) = message.sender.uid.as_ref() {
                if let Some(identity) = identity_access {
                    identity.ensure_identity_for_account(
                        miliastra_kernel::chat_source::GAME_GENSHIN,
                        uid.as_str(),
                        message.sender.nickname.as_deref(),
                    );
                }
            }
        }
    }
    state.touch();
    // 丢掉启动前的历史：水位之前的一律不执行。
    let watermark = state.watermark();
    if watermark > 0 {
        let before = messages.len();
        messages.retain(|message| message.sequence > watermark);
        if messages.len() != before {
            log::debug!(
                "外部聊天源丢弃启动前历史 {} 条（水位 {watermark}）",
                before - messages.len()
            );
        }
        if messages.is_empty() {
            return;
        }
    }
    let last = messages.last().map(|message| message.sequence).unwrap_or(0);
    let epoch = crate::observation::chat::VisualSessionId::new(state.epoch());
    match timeline.inject_external(Instant::now(), epoch, source, messages) {
        Ok(accepted) => {
            if !accepted.is_empty() {
                state.inner.last_sequence.store(last, Ordering::Release);
                log::debug!("外部聊天源注入 {} 条消息", accepted.len());
            }
        }
        Err(error) => log::warn!("外部聊天源注入失败: {error:#}"),
    }
}

/// 把 WS 地址转成同源的 HTTP 基地址，用于 `/names` 补查。
///
/// `ws://127.0.0.1:18889/stream` → `http://127.0.0.1:18889`
fn ws_url_to_http(ws_url: &str) -> String {
    let (scheme, rest) = if let Some(rest) = ws_url.strip_prefix("wss://") {
        ("https", rest)
    } else if let Some(rest) = ws_url.strip_prefix("ws://") {
        ("http", rest)
    } else {
        ("http", ws_url)
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, Some(path)),
        None => (rest, None),
    };
    // 保留子路径：自建服务常挂在 /api/ 下，丢掉它会让 /names 与 /messages
    // 打到根路径而 404。同时去掉最后的 /stream 之类端点后缀——
    // 同一服务的 HTTP 接口与 WS 端点共享前缀。
    let prefix = path
        .map(|path| {
            let trimmed = path.trim_end_matches('/');
            trimmed
                .rsplit_once('/')
                .map(|(head, _)| head)
                .unwrap_or("")
                .to_string()
        })
        .filter(|prefix| !prefix.is_empty());
    match prefix {
        Some(prefix) => format!("{scheme}://{authority}/{prefix}"),
        None => format!("{scheme}://{authority}"),
    }
}

/// 数据源切换后的状态重置。
///
/// 与"切换数据源"的既有约定一致：用 `PanelReturn` 语义的基线（reason 不含
/// "千星/大厅"，否则会得到"蓝色判新"从而重放历史），并清空命令锁。
fn reset_after_source_change(timeline: &ChatTimeline) {
    if let Err(error) = timeline.note_baseline("外部聊天源切换") {
        log::warn!("重置聊天基线失败: {error:#}");
        return;
    }
    if let Err(error) = timeline.clear_command_locks() {
        log::warn!("重置命令锁失败: {error:#}");
    }
}

/// 供观察线程使用的"是否跳过 OCR"查询。
pub(crate) fn should_skip_ocr(state: &ExternalChatState) -> bool {
    state.is_active()
}

/// 让观察线程在接管期间发布空帧而不是 `publish_no_chat`。
///
/// `publish_no_chat` 会清 tracker、置 `cold_baseline_consumed`，
/// 那是对"聊天区不在画面上"的错误断言——接管期间聊天区明明还在。
pub(crate) const EMPTY_FRAME_REASON: &str = "external chat source active";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_to_http_maps_scheme_and_strips_endpoint() {
        // /names 与 /messages 与 WS 同源，只换 scheme、去掉端点后缀。
        assert_eq!(
            ws_url_to_http("ws://127.0.0.1:18889/stream"),
            "http://127.0.0.1:18889"
        );
        assert_eq!(
            ws_url_to_http("wss://example.com:9443/stream"),
            "https://example.com:9443"
        );
        // 无路径时保持原样。
        assert_eq!(
            ws_url_to_http("ws://127.0.0.1:18889"),
            "http://127.0.0.1:18889"
        );
    }

    #[test]
    fn ws_url_to_http_preserves_subpath_for_self_hosted_apis() {
        // 自建服务常挂在子路径下：丢掉前缀会让 /names 与 /messages 打到根路径而 404。
        assert_eq!(
            ws_url_to_http("ws://host:8080/api/stream"),
            "http://host:8080/api"
        );
        assert_eq!(
            ws_url_to_http("ws://host:8080/chat/v1/stream"),
            "http://host:8080/chat/v1"
        );
    }

    #[test]
    fn inactive_state_does_not_skip_ocr() {
        // 默认未接管：观察线程必须继续走 OCR，否则聊天完全没输入。
        let state = ExternalChatState::default();
        assert!(!state.is_active());
        assert!(!should_skip_ocr(&state));
    }

    #[test]
    fn watermark_is_set_once_and_ignores_startup_history() {
        let state = ExternalChatState::default();
        // 握手前没有水位：不过滤。
        assert_eq!(state.watermark(), 0);
        // 首次握手给出 lastMessageId=42：之后 ≤42 的历史都不执行。
        state.establish_watermark(42);
        assert_eq!(state.watermark(), 42);
        // 重连时 lastMessageId 更大，但水位不能跟着涨——
        // 否则断线期间（42..100）的消息会被永久丢弃。
        state.establish_watermark(100);
        assert_eq!(state.watermark(), 42, "重连不得抬高水位");
    }

    #[test]
    fn force_watermark_skips_backlog_after_long_outage() {
        let state = ExternalChatState::default();
        state.establish_watermark(42);
        // 断线超过阈值：不补拉，水位直接推到服务端当前值。
        state.force_watermark(300);
        assert_eq!(state.watermark(), 300);
        // 之后的 301 才执行，200..300 全部丢弃——避免一次性重放大量命令。
        assert!(301 > state.watermark());
        assert!(300 <= state.watermark());
    }

    #[test]
    fn idle_millis_reports_none_before_first_event() {
        let state = ExternalChatState::default();
        assert!(state.idle_millis().is_none());
        state.touch();
        assert!(state.idle_millis().is_some_and(|ms| ms < 5_000));
    }

    #[test]
    fn uid_cache_remembers_only_complete_pairs() {
        let state = ExternalChatState::default();
        state.remember_uid(Some("玩家癸"), Some("900000001"));
        assert_eq!(state.uid_for("玩家癸").as_deref(), Some("900000001"));
        // 昵称或 UID 缺失时不记，避免产生半截对应。
        state.remember_uid(None, Some("1"));
        state.remember_uid(Some("甲"), None);
        assert_eq!(state.uid_for("甲"), None);
    }
}
