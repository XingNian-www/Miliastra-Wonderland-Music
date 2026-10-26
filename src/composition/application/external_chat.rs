//! 外部聊天源线程：把 Miliastra-Wonderland-ChatBridge 的推送注入时间线，并接管 OCR 的聊天内容识别。
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

/// 昵称补查"在途"标记的有效期。超过它还停在在途状态，说明补查线程卡住或
/// 已退出，允许这个 UID 重新排队，避免它永久不再补查。
const NAME_LOOKUP_IN_FLIGHT_MS: u64 = 10_000;

/// 昵称补查失败后的冷却。拿不到名字通常意味着服务端也没有，
/// 短期内反复查既没有收益，又会让后台线程空转。
const NAME_LOOKUP_RETRY_MS: u64 = 30_000;

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
    /// 异步昵称补查的进度：已解析结果 + 下次允许查询的时间。
    name_lookups: std::sync::Mutex<NameLookups>,
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
    /// 是否已经用掉"首次遇到一级聊天界面时自动切二级"的机会。
    initial_secondary_switch_done: AtomicBool,
}

/// 昵称补查进度。由后台补查线程写、接收线程读。
#[derive(Default)]
struct NameLookups {
    /// UID → 已补查到的昵称。命中后同一发送者的后续消息不再走网络。
    resolved: std::collections::HashMap<String, String>,
    /// UID → 允许再次补查的毫秒时间戳（在途与失败冷却共用）。
    next_attempt_ms: std::collections::HashMap<String, u64>,
}

impl ExternalChatState {
    /// 观察线程用它决定是否跳过 OCR 聊天识别。
    /// 外部源此刻是否在接管。**这是唯一的来源仲裁查询**。
    ///
    /// 用途分三类，调用点不要各自发明判断：
    ///
    /// 1. **跳过 OCR**（`shared_observer`）：接管期间内容由 socket 提供，
    ///    再 OCR 会重复派发同一条消息 → 用 [`Self::should_skip_ocr`]。
    /// 2. **消息范围**（消费门控）：由 `ChatTimelineSnapshot::scope` 决定，
    ///    不看这里——来源已随消息一起传递。
    /// 3. **UI 驻留决策**（`maybe_switch_to_secondary`）：接管时才有"默认切二级"
    ///    这件事 → 用 [`Self::supplies_hall_messages`]。
    pub(crate) fn is_active(&self) -> bool {
        self.inner.active.load(Ordering::Acquire)
    }

    /// 外部源是否正在为大厅提供消息。
    ///
    /// 与 [`Self::is_active`] 同义，但语义更窄、更贴近调用点意图：
    /// 它回答"大厅内容是否来自 socket"，而不是"外部线程是否活着"。
    /// UI 驻留决策应该问这个，而不是直接读 `is_active`。
    pub(crate) fn supplies_hall_messages(&self) -> bool {
        self.is_active()
    }

    pub(crate) fn last_sequence(&self) -> u64 {
        self.inner.last_sequence.load(Ordering::Acquire)
    }

    /// 一次性取「是否在线 + 当前水位」。
    ///
    /// 分开调 `supplies_hall_messages()` 和 `last_sequence()` 是两次独立原子读，
    /// 中间可能恰好发生接管/退出，于是得到「在线」配「旧水位」这种不存在的组合。
    /// 投票窗口用它建基线，需要一致的快照。
    ///
    /// 返回 None 表示当前不由外部源供数，调用方应走 OCR 基线。
    pub(crate) fn hall_watermark(&self) -> Option<u64> {
        // 先读水位再读 active：若期间刚好退出，我们会读到 active=false 而返回 None，
        // 宁可退回 OCR 基线，也不要拿一个可能已经过期的水位去挡票。
        let sequence = self.inner.last_sequence.load(Ordering::Acquire);
        self.is_active().then_some(sequence)
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

    /// 已补查到的昵称；没有就返回 None。
    pub(crate) fn cached_name(&self, uid: &str) -> Option<String> {
        self.name_lookups().resolved.get(uid.trim()).cloned()
    }

    /// 申请一次异步昵称补查。
    ///
    /// 返回 false 表示已有结果、正在补查，或处于失败冷却中——调用方不要重复排队。
    pub(crate) fn begin_name_lookup(&self, uid: &str) -> bool {
        let uid = uid.trim();
        if uid.is_empty() {
            return false;
        }
        let now = now_millis();
        let mut lookups = self.name_lookups();
        if lookups.resolved.contains_key(uid) {
            return false;
        }
        if lookups
            .next_attempt_ms
            .get(uid)
            .is_some_and(|next| *next > now)
        {
            return false;
        }
        lookups
            .next_attempt_ms
            .insert(uid.to_string(), now + NAME_LOOKUP_IN_FLIGHT_MS);
        true
    }

    /// 登记补查结果：成功则缓存昵称，失败或查不到则进入冷却。
    pub(crate) fn finish_name_lookup(&self, uid: &str, name: Option<&str>) {
        let uid = uid.trim();
        if uid.is_empty() {
            return;
        }
        let mut lookups = self.name_lookups();
        match name.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => {
                lookups.resolved.insert(uid.to_string(), name.to_string());
                lookups.next_attempt_ms.remove(uid);
            }
            None => {
                lookups
                    .next_attempt_ms
                    .insert(uid.to_string(), now_millis() + NAME_LOOKUP_RETRY_MS);
            }
        }
    }

    fn name_lookups(&self) -> std::sync::MutexGuard<'_, NameLookups> {
        self.inner
            .name_lookups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// 首次握手时调用：原子地标记水位并返回本次是否为首次。
    ///
    /// 与 先 watermark_established() 再 establish_watermark() 相比，
    /// 这个组合没有"先查后设"的竞态，调用点也不必自己拼两段逻辑。
    pub(crate) fn take_first_handshake(&self, sequence: u64) -> bool {
        if self.inner.watermark_set.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.inner.watermark.store(sequence, Ordering::Release);
        log::info!("外部聊天源建立启动水位: 序号 ≤ {sequence} 的历史消息将被忽略");
        true
    }

    /// 认领"首次遇到一级聊天界面时自动切二级"这一次机会。
    ///
    /// 为什么不在握手时切：首次握手发生在 BOT 刚启动、游戏还没进界面的时候，
    /// 那一刻几乎不可能停在一级聊天页，按界面门控必然被跳过，默认行为等于失效。
    /// 所以改成"等第一次真正停在一级聊天界面时再切一次"，只认领一次；
    /// 之后用户手动切一级不会被再顶回二级。
    pub(crate) fn claim_initial_secondary_switch(&self) -> bool {
        !self
            .inner
            .initial_secondary_switch_done
            .swap(true, Ordering::AcqRel)
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
    config_store: Option<crate::config::SharedConfigStore>,
) -> Result<ExternalChatState> {
    let state = ExternalChatState::default();
    if !config.enabled {
        return Ok(state);
    }
    let worker_state = state.clone();
    thread::Builder::new()
        .name("external-chat".into())
        .spawn(move || {
            run(
                timeline,
                config,
                identity_access,
                config_store,
                worker_state,
            )
        })
        .map_err(|error| anyhow::anyhow!("启动外部聊天线程失败: {error}"))?;
    Ok(state)
}

fn run(
    timeline: ChatTimeline,
    config: ChatSourceConfig,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    config_store: Option<crate::config::SharedConfigStore>,
    state: ExternalChatState,
) {
    let lease = Duration::from_millis(config.lease_timeout_ms);
    let reconnect = Duration::from_millis(config.reconnect_ms);
    // 昵称补查放到独立线程：接收线程必须持续读 WS，任何 HTTP 等待都会让
    // 后续消息（可能正是玩家的命令）排在它后面。结果回写缓存，之后同 UID 直接命中。
    let names = spawn_name_resolver(
        state.clone(),
        ws_url_to_http(&config.url),
        config.token.clone(),
        identity_access.clone(),
        config_store.clone(),
    );
    loop {
        // 登录器角色：Miliastra-Wonderland-ChatBridge 没在监听时替用户拉起来。
        // 探测很便宜（300ms 超时），每次重连都试一次，这样用户中途关掉也能被重新拉起。
        if config.auto_start
            && let Err(error) = crate::runtime::chat_source::launcher::ensure_running(&config)
        {
            log::warn!("无法自动启动 Miliastra-Wonderland-ChatBridge，等待人工处理: {error:#}");
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
                            state.take_first_handshake(last_sequence);
                            // 这里**不再**直接切二级：首次握手发生在 BOT 刚启动、
                            // 游戏还没进界面的时候，那一刻不可能停在一级聊天页。
                            // 默认切换改由主导航循环在「首次真正停在一级」时完成
                            // （见 ApplicationRuntime::maybe_switch_to_secondary）。
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
                                                identity_access.as_ref(),
                                                config_store.as_ref(),
                                                &config.url,
                                                &names,
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
                                identity_access.as_ref(),
                                config_store.as_ref(),
                                &config.url,
                                &names,
                                messages,
                            );
                        }
                    }
                }
                Ok(None) => {
                    // 这里既可能是「读超时」，也可能是收到了 Ping/Pong/Close。
                    // 必须区分：ChatBridge 每 30s 发一次 Ping，若把心跳当成
                    // 无活动，租约(默认 10s)永远先超时，外部源会周期性地
                    // 「已退出 → 已接管」抖动，聊天源等于一直在断。
                    // conn: Some(_) 表示连接还在读，心跳/超时都算存活；
                    // None 表示连接已关闭，交给上层重连。
                    if source.is_connected() {
                        last_event_at = Instant::now();
                    } else if last_event_at.elapsed() > lease {
                        state.set_active(false);
                    }
                    continue;
                }
                Err(error) => {
                    log::warn!(
                        "外部聊天源读取失败，{}ms 后重连: {error:#}",
                        config.reconnect_ms
                    );
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

/// 把内存里的身份映射整体写回配置库的 `identity` 段。
///
/// 自动为好友 UID 建身份时调用。不写库的话，这条映射只活在当前进程：
/// 面板看不到、用户无法调权限，而且任何一次配置重载都会用库里的旧值
/// 把它冲掉（`LiveConfigs::apply` 里 `identity.replace(config.identity)`）。
///
/// 只更新 `identity` 段，其余段保持原值——避免与正在编辑的其他配置打架。
fn persist_identity(
    store: &crate::config::SharedConfigStore,
    identity: &crate::features::identity::IdentityAccess,
) -> anyhow::Result<()> {
    let snapshot = identity.snapshot();
    let mut guard = store
        .lock()
        .map_err(|_| anyhow::anyhow!("配置库锁被毒化"))?;
    let mut sections = guard
        .load_full()
        .map_err(|error| anyhow::anyhow!("读取配置失败: {error}"))?
        .to_db_value()
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("配置根不是对象"))?;
    sections.insert("identity".to_string(), serde_json::to_value(snapshot)?);
    let revision = guard
        .current_revision()
        .map_err(|error| anyhow::anyhow!("读取配置版本失败: {error}"))?;
    guard
        .save(revision, sections)
        .map_err(|error| anyhow::anyhow!("保存身份映射失败: {error}"))?;
    Ok(())
}
fn deliver(
    timeline: &ChatTimeline,
    state: &ExternalChatState,
    identity_access: Option<&crate::features::identity::IdentityAccess>,
    config_store: Option<&crate::config::SharedConfigStore>,
    source: &str,
    names: &std::sync::mpsc::Sender<String>,
    messages: Vec<ExternalChatMessage>,
) {
    let mut messages = messages;
    // 这条路径在 WS 接收线程上：缺昵称只排队补查，**绝不能在这里等 HTTP**，
    // 否则一条慢查询就会把后面所有消息（可能正是玩家的命令）一起拖住。
    let mut lookups: Vec<String> = Vec::new();
    for message in &mut messages {
        // 之前补查到的昵称直接复用，同一发送者不必再走网络。
        if message.sender.nickname.is_none()
            && let Some(uid) = message.sender.uid.as_ref()
            && let Some(name) = state.cached_name(uid.as_str())
        {
            message.sender.nickname = Some(name);
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
                    let created = identity.ensure_identity_for_account(
                        miliastra_kernel::chat_source::GAME_GENSHIN,
                        uid.as_str(),
                        message.sender.nickname.as_deref(),
                    );
                    // 自动建出来的身份必须落库。
                    //
                    // 只改内存的话有两个后果：面板看不到这条映射（用户无法
                    // 调权限），而且每次配置重载 live_configs 都会用库里的值
                    // `identity.replace(...)` 把内存覆盖回去，身份直接消失。
                    if created.is_some()
                        && let Some(store) = config_store
                        && let Err(error) = persist_identity(store, identity)
                    {
                        log::warn!("自动创建的身份未能写入数据库: {error:#}");
                    }
                }
            }
        }
        // 仍然缺昵称：交给后台线程补查，本线程立刻回去读 WS。
        if message.sender.nickname.is_none()
            && let Some(uid) = message.sender.uid.as_ref()
            && state.begin_name_lookup(uid.as_str())
        {
            lookups.push(uid.as_str().to_string());
        }
    }
    for uid in lookups {
        if names.send(uid.clone()).is_err() {
            // 补查线程没起来或已退出：解除在途标记，避免该 UID 永久不再补查。
            state.finish_name_lookup(&uid, None);
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

/// 后台昵称补查线程。
///
/// 接收线程只负责把缺昵称的 UID 丢进来；这里串行发请求（单次上限 1s），
/// 结果写回缓存。同一个 UID 只查一次，之后同发送者的消息直接复用。
fn spawn_name_resolver(
    state: ExternalChatState,
    http_base: String,
    token: String,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    config_store: Option<crate::config::SharedConfigStore>,
) -> std::sync::mpsc::Sender<String> {
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    let spawned = thread::Builder::new()
        .name("external-chat-names".into())
        .spawn(move || {
            for uid in receiver {
                match crate::runtime::chat_source::resolve_missing_name(&http_base, &token, &uid) {
                    Ok(Some(name)) => {
                        state.remember_uid(Some(&name), Some(&uid));
                        state.finish_name_lookup(&uid, Some(&name));
                        log::debug!("昵称补查命中: uid={uid} name={name}");
                        // 自动建的身份当时可能没有备注，补上（只填空值）。
                        if let Some(identity) = identity_access.as_ref()
                            && identity.backfill_account_note(
                                miliastra_kernel::chat_source::GAME_GENSHIN,
                                &uid,
                                &name,
                            )
                            && let Some(store) = config_store.as_ref()
                            && let Err(error) = persist_identity(store, identity)
                        {
                            log::warn!("补查到的昵称未能写入身份映射: {error:#}");
                        }
                    }
                    Ok(None) => {
                        state.finish_name_lookup(&uid, None);
                        log::debug!("昵称补查无结果，按未知发送者处理: uid={uid}");
                    }
                    Err(error) => {
                        state.finish_name_lookup(&uid, None);
                        log::debug!("昵称补查失败: {error:#}");
                    }
                }
            }
        });
    if let Err(error) = spawned {
        log::warn!("启动昵称补查线程失败，缺昵称的消息将按未知发送者处理: {error}");
    }
    sender
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

    /// 异步补查的去重：同一 UID 在途时不再排队，拿到结果后直接命中缓存。
    #[test]
    fn name_lookup_is_deduplicated_and_cached() {
        let state = ExternalChatState::default();
        assert!(state.begin_name_lookup("900000001"));
        // 在途期间同 UID 的后续消息不能被重复排队。
        assert!(!state.begin_name_lookup("900000001"));
        state.finish_name_lookup("900000001", Some("玩家癸"));
        assert_eq!(state.cached_name("900000001").as_deref(), Some("玩家癸"));
        // 已有昵称就不再查网络。
        assert!(!state.begin_name_lookup("900000001"));
    }

    /// 补查失败要进入冷却，避免每条缺昵称消息都打一次网络。
    #[test]
    fn failed_name_lookup_enters_cooldown() {
        let state = ExternalChatState::default();
        assert!(state.begin_name_lookup("42"));
        state.finish_name_lookup("42", None);
        assert_eq!(state.cached_name("42"), None);
        assert!(!state.begin_name_lookup("42"), "失败后应处于冷却期");
    }

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

    /// 回归：首次握手只建水位，不改值守模式。
    ///
    /// 切换改由主导航循环在「首次真正停在一级聊天界面」时完成——握手发生在
    /// BOT 刚启动、游戏还没进界面的时候，那时按界面门控必然被跳过。
    #[test]
    fn first_handshake_only_establishes_watermark() {
        let state = ExternalChatState::default();
        assert!(state.take_first_handshake(4));
        // 重连不重复建立水位，也不重复触发任何一次性动作。
        assert!(!state.take_first_handshake(10));
        assert!(!state.take_first_handshake(10));
        // 水位仍停在首次握手那一刻：断线期间的消息要能补拉。
        assert_eq!(state.watermark(), 4);
    }

    /// 默认二级切换只认领一次。
    ///
    /// 认领后即使用户手动切回一级，也不会被再次顶回二级。
    #[test]
    fn initial_secondary_switch_is_claimed_exactly_once() {
        let state = ExternalChatState::default();
        assert!(state.claim_initial_secondary_switch());
        assert!(!state.claim_initial_secondary_switch());
        assert!(!state.claim_initial_secondary_switch());
    }
    /// 回归：心跳/读超时不能被当成「外部源失联」。
    ///
    /// ChatBridge 每 30s 才发一次 Ping，而租约默认 10s。旧实现里
    /// next_event() 对 Ping/Pong 返回 Ok(None)，上层既没有事件可刷新
    /// last_event_at，也没有任何存活信号，于是必然「已退出 → 已接管」
    /// 周期性抖动，外部聊天等于一直在断。
    #[test]
    fn heartbeat_keeps_source_connected_so_lease_is_not_lost() {
        // is_connected 是新增的存活判据：连接还在就必须算活跃。
        // 默认实现为 true，保证新增来源不会被误判为断线。
        struct AlwaysConnected;
        impl ChatSource for AlwaysConnected {
            fn name(&self) -> &str {
                "test"
            }
            fn next_event(&mut self) -> anyhow::Result<Option<ChatSourceEvent>> {
                Ok(None)
            }
        }
        let source = AlwaysConnected;
        assert!(
            source.is_connected(),
            "读超时/心跳期间连接仍在，上层据此刷新租约"
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
