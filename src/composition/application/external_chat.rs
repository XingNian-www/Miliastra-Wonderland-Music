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
use crate::runtime::chat_source::fetch_health;
use crate::runtime::chat_source::ws::WebSocketChatSource;
use miliastra_kernel::chat_source::{ChatSource, ChatSourceEvent, ExternalChatMessage};

/// 断线超过这个时长就不再补拉：一次性重放大量命令比丢几条更糟。
const MAX_BACKLOG_IDLE_MS: u64 = 60_000;

/// 接管期间的健康检查间隔。
///
/// WS 连接存活只代表传输层还在；reattach 等待期里宿主可能还没有接入游戏
/// （gameConnected=false），此时如果继续接管、关掉 OCR，就会出现「连接正常
/// 但永远收不到消息」的静默失聪。active 期间每 5 秒带 token 核实一次。
const HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// 昵称补查"在途"标记的有效期。超过它还停在在途状态，说明补查线程卡住或
/// 已退出，允许这个 UID 重新排队，避免它永久不再补查。
const NAME_LOOKUP_IN_FLIGHT_MS: u64 = 10_000;

/// 昵称补查失败后的冷却。拿不到名字通常意味着服务端也没有，
/// 短期内反复查既没有收益，又会让后台线程空转。
const NAME_LOOKUP_RETRY_MS: u64 = 30_000;

/// 名称缓存有效期。
///
/// 备注是玩家在游戏里随时可以改的，一次查到就永久缓存会让「后设的备注」
/// 永远进不来。10 分钟是折中：改完备注最多等这么久就会同步，
/// 而正常聊天里同一发送者远达不到这个频次，不会造成额外请求。
const NAME_CACHE_TTL_MS: u64 = 600_000;

/// 主动补查的扫描间隔。
///
/// 只在"有人说话"时才查会漏掉从不说话的好友，所以另外按身份映射表定期扫一遍。
/// 真正的去重仍由 prime_name_lookup 的冷却窗口负责，这里只决定多久看一次表。
const NAME_PRIME_INTERVAL: Duration = Duration::from_secs(60);
/// 全量好友快照的轮询间隔；与 ChatBridge 的 60 秒全量重发对齐。
const FRIENDS_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(60);
/// 快照新鲜期略小于 60s，避免与 `/names` 轮询相位锁死后一直跳过。
const FRIENDS_SNAPSHOT_FRESH_MS: u64 = 45_000;

/// 主动补查里"还缺真实昵称"的账号重试间隔。
///
/// 比成功后的长 TTL 短得多（/names 的 names 可能当时为 null），
/// 又不能短到刷屏：本地 ChatBridge 一次请求很便宜，但仍按两分钟收敛。
const NAME_PRIME_INCOMPLETE_MS: u64 = 120_000;

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// 一次握手对本地水位的影响。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HandshakeAction {
    /// 本进程第一次连上：建立启动水位，丢弃宿主启动前的历史。
    FirstConnect,
    /// 同一宿主内的普通重连：水位保持，断线期间的消息走补拉。
    Reconnect,
    /// 宿主进程重启：旧水位作废，水位与本地游标一起归零。
    HostRestarted,
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
    /// 宿主进程的启动标识（hello.bootId）。同一宿主内恒定；变化即宿主重启。
    host_boot_id: std::sync::Mutex<Option<u64>>,
    /// 是否已经用掉"首次遇到一级聊天界面时自动切二级"的机会。
    initial_secondary_switch_done: AtomicBool,
    /// 最近一次**完整** `/friends` 快照写入身份映射的时间（毫秒）；0=从未。
    friends_snapshot_at_ms: AtomicU64,
    /// 旧桥 404 只 debug 一次。
    friends_unsupported: AtomicBool,
}

/// 一次补查的结果。
///
/// name 来自资料包（可能为 null、到达时序不确定），remark 是玩家在游戏里
/// 设的好友备注。两者任一有值就算查到过——否则 `names=null` 而 `remarks`
/// 有值的常见组合会被当成失败，每 30 秒重查一次。
#[derive(Clone, Default)]
struct ResolvedNames {
    name: Option<String>,
    remark: Option<String>,
    /// 查到时的毫秒时间戳，用于 TTL 过期重查。
    fetched_at_ms: u64,
}

/// 昵称补查进度。由后台补查线程写、接收线程读。
#[derive(Default)]
struct NameLookups {
    /// UID → 已补查到的名称与备注。
    ///
    /// 带时间戳是必需的：备注是玩家在游戏里随时能改的，一次查到就永久
    /// 缓存会让「后设的备注」永远进不来。超过 TTL 后允许重查。
    resolved: std::collections::HashMap<String, ResolvedNames>,
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

    /// 已补查到的昵称；没有或已过期就返回 None。
    pub(crate) fn cached_name(&self, uid: &str) -> Option<String> {
        let lookups = self.name_lookups();
        let resolved = lookups.resolved.get(uid.trim())?;
        if now_millis().saturating_sub(resolved.fetched_at_ms) >= NAME_CACHE_TTL_MS {
            return None;
        }
        resolved.name.clone()
    }

    /// 申请一次异步补查。
    ///
    /// 返回 false 表示 TTL 内已有结果、正在补查，或处于失败冷却中。
    /// 结果过期后必须放行——否则玩家后设的备注永远进不来。
    pub(crate) fn begin_name_lookup(&self, uid: &str) -> bool {
        let uid = uid.trim();
        if uid.is_empty() {
            return false;
        }
        let now = now_millis();
        let mut lookups = self.name_lookups();
        if lookups
            .resolved
            .get(uid)
            .is_some_and(|resolved| now.saturating_sub(resolved.fetched_at_ms) < NAME_CACHE_TTL_MS)
        {
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

    /// 主动补查的排队申请（按身份映射表扫描用）。
    ///
    /// 与消息路径的 begin_name_lookup 区别只在冷却窗口：
    /// incomplete（身份里还缺真实昵称或备注）时用重试冷却，
    /// 而不是"成功查过就等 10 分钟"，否则 /names 当时返回 null 的玩家
    /// 要等很久才会被再补一次。
    pub(crate) fn prime_name_lookup(&self, uid: &str, incomplete: bool) -> bool {
        let uid = uid.trim();
        if uid.is_empty() {
            return false;
        }
        let now = now_millis();
        let window = if incomplete {
            NAME_PRIME_INCOMPLETE_MS
        } else {
            NAME_CACHE_TTL_MS
        };
        let mut lookups = self.name_lookups();
        if lookups
            .resolved
            .get(uid)
            .is_some_and(|resolved| now.saturating_sub(resolved.fetched_at_ms) < window)
        {
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

    /// 登记补查结果。
    ///
    /// **name 或 remark 任一有值就算查到过**：实测 /names 常见组合是
    /// `names:{uid:null}` 配 `remarks:{uid:"某备注"}`，只认 name 会把这种情况
    /// 判成失败，然后每 30 秒重查一次。两者都没有才进冷却。
    pub(crate) fn finish_name_lookup(&self, uid: &str, name: Option<&str>, remark: Option<&str>) {
        let uid = uid.trim();
        if uid.is_empty() {
            return;
        }
        let clean = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let name = clean(name);
        let remark = clean(remark);
        let mut lookups = self.name_lookups();
        if name.is_none() && remark.is_none() {
            lookups
                .next_attempt_ms
                .insert(uid.to_string(), now_millis() + NAME_LOOKUP_RETRY_MS);
            return;
        }
        lookups.resolved.insert(
            uid.to_string(),
            ResolvedNames {
                name,
                remark,
                fetched_at_ms: now_millis(),
            },
        );
        lookups.next_attempt_ms.remove(uid);
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

    /// 完整好友快照成功应用后标记；60 秒内优先信快照，跳过逐 UID `/names` 轮询。
    fn mark_friends_snapshot(&self) {
        self.inner
            .friends_snapshot_at_ms
            .store(now_millis(), Ordering::Release);
    }

    /// 最近是否有一份完整好友快照覆盖当前 60 秒窗口。
    fn friends_snapshot_recent(&self) -> bool {
        let at = self.inner.friends_snapshot_at_ms.load(Ordering::Acquire);
        at > 0 && now_millis().saturating_sub(at) < FRIENDS_SNAPSHOT_FRESH_MS
    }

    /// 旧桥无 `/friends` 时只记一次 debug。
    fn mark_friends_unsupported(&self) -> bool {
        !self.inner.friends_unsupported.swap(true, Ordering::AcqRel)
    }

    /// 首次连接时建立启动水位；已有水位则不动（重连场景）。
    pub(crate) fn establish_watermark(&self, sequence: u64) {
        if !self.inner.watermark_set.swap(true, Ordering::AcqRel) {
            self.inner.watermark.store(sequence, Ordering::Release);
            log::info!("外部聊天源建立启动水位：序号 ≤ {sequence} 的历史消息将被忽略");
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
        log::info!("外部聊天源建立启动水位：序号 ≤ {sequence} 的历史消息将被忽略");
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

    /// 这条消息序号是否应该被处理（过滤宿主启动前的历史）。
    ///
    /// 水位 0 表示不过滤：宿主重启后序号从 1 重新计数，旧的启动水位
    /// （例如 500）必须先被 apply_handshake 清零，否则新消息会被
    /// 「序号 ≤ 500」整批丢弃，而接管状态又会关掉 OCR，造成永久静默失聪。
    pub(crate) fn accepts_sequence(&self, sequence: u64) -> bool {
        let watermark = self.watermark();
        watermark == 0 || sequence > watermark
    }

    /// 处理一次握手，返回本次连接是否需要重置水位。
    ///
    /// 宿主重启的判定（任一成立）：
    /// 1. hello.bootId 与已记录的 bootId 不同；
    /// 2. hello.lastMessageId 小于本地已处理序号与本地水位中的较大者
    ///    （旧宿主没有 bootId 时的兜底判据）。
    ///
    /// 重置会把水位与本地游标一起归零：新宿主从 1 开始计数的消息不会再被
    /// 旧水位过滤。普通重连（bootId 不变且序号没有回退）不重置，断线期间
    /// 的消息仍可补拉。
    pub(crate) fn apply_handshake(
        &self,
        last_sequence: u64,
        boot_id: Option<u64>,
    ) -> HandshakeAction {
        let previous_boot = *self
            .inner
            .host_boot_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if boot_id.is_some() {
            *self
                .inner
                .host_boot_id
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = boot_id;
        }
        if self.take_first_handshake(last_sequence) {
            return HandshakeAction::FirstConnect;
        }
        let boot_changed = matches!((previous_boot, boot_id), (Some(old), Some(new)) if old != new);
        let local_high = self.last_sequence().max(self.watermark());
        let sequence_regressed = last_sequence < local_high;
        if boot_changed || sequence_regressed {
            self.inner.watermark.store(0, Ordering::Release);
            self.inner.last_sequence.store(0, Ordering::Release);
            let previous_boot_text =
                previous_boot.map_or_else(|| "无".to_string(), |id| id.to_string());
            let current_boot_text = boot_id.map_or_else(|| "无".to_string(), |id| id.to_string());
            log::warn!(
                "检测到宿主进程重启（bootId 由 {previous_boot_text} 变为 {current_boot_text}，服务端序号为 {last_sequence}，本地水位为 {local_high}），已重置外部聊天水位；新消息序号从 1 重新计数，不会被旧水位丢弃"
            );
            return HandshakeAction::HostRestarted;
        }
        HandshakeAction::Reconnect
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
    live_configs: crate::config::LiveConfigs,
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
                live_configs,
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
    live_configs: crate::config::LiveConfigs,
    state: ExternalChatState,
) {
    // 租约/重连间隔不再在启动时缓存：下面每轮从 live 快照读取，
    // 热更新后下一次判定/重连即生效（不重启进程、不断开游戏）。
    // 昵称补查放到独立线程：接收线程必须持续读 WS，任何 HTTP 等待都会让
    // 后续消息（可能正是玩家的命令）排在它后面。结果回写缓存，之后同 UID 直接命中。
    let http_base = ws_url_to_http(&config.url);
    let names = spawn_name_resolver(
        state.clone(),
        http_base.clone(),
        config.token.clone(),
        identity_access.clone(),
        config_store.clone(),
        live_configs.clone(),
    );
    // 消息驱动的补查只覆盖"说过话的人"。再开一个扫描线程按身份映射表主动补，
    // 尽量把从不说话的好友的真实昵称也拿到。
    spawn_name_priming(state.clone(), identity_access.clone(), names.clone());
    spawn_friends_snapshot(
        state.clone(),
        http_base.clone(),
        config.token.clone(),
        identity_access.clone(),
        config_store.clone(),
        live_configs.clone(),
    );
    loop {
        // 登录器角色：Miliastra-Wonderland-ChatBridge 没在监听时替用户拉起来。
        // 探测很便宜（300ms 超时），每次重连都试一次，这样用户中途关掉也能被重新拉起。
        if config.auto_start {
            // target_process 用于退出码 3 的「等游戏退出」判定；配置可能热更新，
            // 每轮都读一次最新快照。
            let target_process = live_configs.snapshot().window.target_process.clone();
            if let Err(error) =
                crate::runtime::chat_source::launcher::ensure_running(&config, &target_process)
            {
                log::warn!("无法自动启动 Miliastra-Wonderland-ChatBridge，等待人工处理：{error:#}");
            }
        }
        let mut source = match WebSocketChatSource::new(config.url.clone(), config.token.clone()) {
            Ok(source) => source,
            Err(error) => {
                let reconnect =
                    Duration::from_millis(super::live_chat_source_reconnect_ms(&live_configs));
                log::error!(
                    "外部聊天源初始化失败（步骤：建立 WebSocket 连接），{} 毫秒后重试；重试期间外部聊天源没有接管，聊天仍由 OCR 识别，外部源独有的消息（含备注名）拿不到：{error:#}",
                    reconnect.as_millis()
                );
                thread::sleep(reconnect);
                continue;
            }
        };
        let mut last_event_at = Instant::now();
        let mut last_health_at = Instant::now();
        loop {
            // 租约超时：最近一次真实消息/健康检查太久，必须回落到 OCR，
            // 否则聊天完全失聪。阈值每轮读 live 快照，热更新后下一次判定生效。
            let lease = Duration::from_millis(super::live_chat_source_lease_ms(&live_configs));
            if last_event_at.elapsed() > lease {
                state.set_active(false);
            }
            // H2：active 期间每 5 秒带 token 核实游戏是否真的接入。
            //
            // 租约只能由真实消息或健康检查刷新；WS 的 Ping/读超时不能续租。
            // 否则 reattach 等待期（WS 活着、游戏未接入）会一直保持接管，
            // OCR 被关掉又收不到消息，形成静默失聪。
            if state.is_active() && last_health_at.elapsed() >= HEALTH_POLL_INTERVAL {
                last_health_at = Instant::now();
                match fetch_health(&http_base, &config.token) {
                    Ok(health) if health.game_connected => {
                        log::debug!("外部聊天源健康检查通过，游戏仍处于接入状态");
                        last_event_at = Instant::now();
                    }
                    Ok(_) => {
                        state.set_active(false);
                        log::warn!(
                            "外部聊天源健康检查显示游戏已断开（gameConnected=false），已恢复 OCR 聊天识别；WS 保持连接，等待游戏重新接入"
                        );
                    }
                    Err(error) => {
                        // 健康检查失败不刷新 last_event_at：租约到期后自动回落 OCR。
                        log::warn!(
                            "外部聊天源健康检查失败，本次不刷新接管租约；租约到期后会自动恢复 OCR：{error:#}"
                        );
                    }
                }
            }
            match source.next_event() {
                Ok(Some(event)) => {
                    last_event_at = Instant::now();
                    match event {
                        ChatSourceEvent::Connected {
                            last_sequence,
                            boot_id,
                        } => {
                            // H1：宿主重启后消息序号从 1 重新开始。先判断并重置
                            // 旧水位，否则新消息会被「序号 ≤ 旧水位」当成启动前历史
                            // 整批丢弃，而 active=true 又关掉 OCR —— 永久静默失聪。
                            let handshake = state.apply_handshake(last_sequence, boot_id);
                            if handshake == HandshakeAction::HostRestarted {
                                state.bump_epoch();
                                reset_after_source_change(&timeline);
                                log::warn!(
                                    "外部聊天源检测到宿主重启，已重置水位与聊天基线；后续消息序号从 1 重新计数"
                                );
                            }
                            // H2：WS 握手只说明连接建立，不代表游戏已接入。
                            // reattach 等待期里 WS 能正常连上却没有任何游戏消息，
                            // 此时必须保持 OCR，而不是凭 is_connected 就接管。
                            let attached = match fetch_health(&http_base, &config.token) {
                                Ok(health) if health.game_connected => {
                                    last_event_at = Instant::now();
                                    last_health_at = Instant::now();
                                    state.set_active(true);
                                    log::info!(
                                        "外部聊天源握手完成且 /health 确认游戏已接入，从服务端序号 {last_sequence} 之后开始接收消息"
                                    );
                                    true
                                }
                                Ok(_) => {
                                    state.set_active(false);
                                    log::warn!(
                                        "外部聊天源握手完成但游戏尚未接入（gameConnected=false），保持 OCR 聊天识别，等待游戏接入"
                                    );
                                    false
                                }
                                Err(error) => {
                                    state.set_active(false);
                                    log::warn!(
                                        "外部聊天源握手完成但读取 /health 失败，暂不接管 OCR（避免游戏未接入时静默失聪）：{error:#}"
                                    );
                                    false
                                }
                            };
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
                                    "外部聊天源断线时间超过 {} 秒，跳过补拉历史消息，直接从当前序号继续接收（避免重放断线期间的旧命令）",
                                    MAX_BACKLOG_IDLE_MS / 1000
                                );
                                // 把水位推到服务端当前值：断线期间的消息一律不执行。
                                state.force_watermark(last_sequence);
                            }
                            if attached && !skip_backlog && last_sequence > local {
                                match crate::runtime::chat_source::fetch_messages_since(
                                    &http_base,
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
                                                "外部聊天源断线后补拉到 {} 条历史消息，将逐条投递给业务处理",
                                                batch.messages.len()
                                            );
                                            deliver(
                                                &timeline,
                                                &state,
                                                identity_access.as_ref(),
                                                config_store.as_ref(),
                                                &live_configs,
                                                &config.url,
                                                config.self_uid,
                                                &names,
                                                batch.messages,
                                            );
                                        }
                                    }
                                    Err(error) => {
                                        log::warn!("断线补拉失败，继续接收新消息：{error:#}")
                                    }
                                }
                            }
                        }
                        ChatSourceEvent::HallChanged { hall_id, hall_type } => {
                            log::info!(
                                "外部聊天源报告大厅已切换（大厅标识 {}，服务端大厅类型 {}），重置大厅相关状态与聊天基线",
                                hall_id,
                                hall_type
                                    .map(|value| value.to_string())
                                    .unwrap_or_else(|| "未提供".to_string())
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
                                &live_configs,
                                &config.url,
                                config.self_uid,
                                &names,
                                messages,
                            );
                        }
                    }
                }
                Ok(None) => {
                    // 读超时 / Ping / Pong / Close 都不刷新接管租约：
                    // WS 活着不等于游戏已接入，续租只能靠真实消息或 /health。
                    // 连接确实关闭时立即回落到 OCR；否则由健康检查续租、
                    // 租约到期兜底。这样既不会因为 30 秒一次的 Ping 造成
                    // 「已退出 → 已接管」抖动，也不会在游戏未接入时空转接管。
                    if !source.is_connected() {
                        state.set_active(false);
                    }
                    continue;
                }
                Err(error) => {
                    let reconnect =
                        Duration::from_millis(super::live_chat_source_reconnect_ms(&live_configs));
                    log::warn!(
                        "外部聊天源读取失败，{}ms 后重连：{error:#}",
                        reconnect.as_millis()
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

/// 身份持久化的进程级串行锁。
///
/// `persist_identity` 被消息投递自动建身份、`/names` 补查、`/friends` 三个线程调用。
/// 旧实现先在锁外 snapshot，再取 store 锁保存；两个线程交错时旧快照可能覆盖新值。
/// 现在锁内 snapshot + sync + save，保证写入的一定是当前最新快照。
static IDENTITY_PERSIST_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

fn identity_persist_guard() -> std::sync::MutexGuard<'static, ()> {
    IDENTITY_PERSIST_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    live_configs: &crate::config::LiveConfigs,
    identity: &crate::features::identity::IdentityAccess,
) -> anyhow::Result<()> {
    let _persist_guard = identity_persist_guard();
    let snapshot = identity.snapshot();
    // 写库之后必须同步 effective 快照。
    //
    // 只改 `IdentityAccess` 与配置库是不够的：`LiveConfigs::apply` 会用
    // 快照里的旧 identity 整体覆盖内存，自动注册的身份随即消失。
    // 线上表现为「新好友第一次说话时没有好友权限，去设置里手动保存一次才好」。
    live_configs.sync_identity_snapshot(snapshot.clone());
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
#[allow(clippy::too_many_arguments)]
fn deliver(
    timeline: &ChatTimeline,
    state: &ExternalChatState,
    identity_access: Option<&crate::features::identity::IdentityAccess>,
    config_store: Option<&crate::config::SharedConfigStore>,
    // 自动注册身份后要把新映射同步进 effective 快照，否则下次 apply 会抹掉它。
    live_configs: &crate::config::LiveConfigs,
    source: &str,
    // 本机账号 UID：私聊 peer 必须排除它，否则玩家私聊 BOT 时
    // peer 会变成 BOT 自己，按身份归属的功能（个人歌单等）全部失效。
    self_uid: u32,
    names: &std::sync::mpsc::Sender<String>,
    messages: Vec<ExternalChatMessage>,
) {
    let mut messages = messages;
    // **先丢启动前的历史，再决定补查**。
    //
    // 顺序反过来会放大请求量：`/messages` 补拉最多两千条启动前消息，
    // 逐条排 `/names` 会让那次冷启动把所有历史发送者都查一遍，
    // 而补查线程是串行的（单次 1 秒上限），后来的 UID 要排很久。
    let watermark = state.watermark();
    if watermark > 0 {
        let before = messages.len();
        messages.retain(|message| state.accepts_sequence(message.sequence));
        if messages.len() != before {
            log::debug!(
                "外部聊天源丢弃启动前历史 {} 条（水位 {watermark}）",
                before - messages.len()
            );
        }
    }
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
        if message.channel.is_private()
            && let Some(uid) = message.sender.uid.as_ref()
            && let Some(identity) = identity_access
        {
            let created = identity.ensure_identity_for_account(
                miliastra_kernel::chat_source::GAME_GENSHIN,
                uid.as_str(),
                message.sender.nickname.as_deref(),
            );
            // 自动建出来的身份必须落库。
            //
            // 只改内存的话有两个后果：面板看不到这条映射（用户无法
            // 调权限），而且每次配置重载 live_configs 都会用库里的值
            // identity.replace(...) 把内存覆盖回去，身份直接消失。
            if created.is_some()
                && let Some(store) = config_store
                && let Err(error) = persist_identity(store, live_configs, identity)
            {
                log::warn!(
                    "自动创建的身份未能写入数据库（步骤：好友首次私聊时自动登记身份），本次运行内该身份仍有效（可按 UID 判权限），但面板看不到它；配置重载或重启后会被库里的旧值覆盖，重新创建一次：{error:#}"
                );
            }
        }
        // 交给后台线程补查名称信息，本线程立刻回去读 WS。
        //
        // 原来只在**缺昵称**时才查。但备注（remarks）与昵称是同一份响应里的
        // 两个字段，只查昵称就永远拿不到备注——而身份映射要以备注为准。
        // 所以只要带 UID 就查；begin_name_lookup 本身按 UID 去重，
        // 同一发送者不会重复排队。
        if let Some(uid) = message.sender.uid.as_ref() {
            // 缺任一名字就走短冷却：备注串和真实昵称一样要贪婪补，避免遗漏。
            let incomplete = identity_access
                .as_ref()
                .and_then(|identity| {
                    identity.resolve_account(
                        miliastra_kernel::chat_source::GAME_GENSHIN,
                        uid.as_str(),
                    )
                })
                .is_none_or(|resolved| {
                    resolved.real_nickname.trim().is_empty()
                        || resolved.remark.trim().is_empty()
                });
            let covered_by_recent_snapshot = state.friends_snapshot_recent()
                && identity_access
                    .as_ref()
                    .and_then(|identity| {
                        identity.resolve_account(
                            miliastra_kernel::chat_source::GAME_GENSHIN,
                            uid.as_str(),
                        )
                    })
                    .is_some_and(|resolved| !resolved.real_nickname.trim().is_empty());
            if !covered_by_recent_snapshot && state.prime_name_lookup(uid.as_str(), incomplete) {
                lookups.push(uid.as_str().to_string());
            }
        }
    }
    for uid in lookups {
        if names.send(uid.clone()).is_err() {
            // 补查线程没起来或已退出：解除在途标记，避免该 UID 永久不再补查。
            state.finish_name_lookup(&uid, None, None);
        }
    }
    state.touch();
    if messages.is_empty() {
        return;
    }
    let last = messages.last().map(|message| message.sequence).unwrap_or(0);
    let epoch = crate::observation::chat::VisualSessionId::new(state.epoch());
    match timeline.inject_external(
        Instant::now(),
        epoch,
        source,
        // self_uid 为 0 表示未配置：传 None 退回原行为。
        (self_uid != 0).then(|| self_uid.to_string()).as_deref(),
        messages,
    ) {
        Ok(accepted) => {
            if !accepted.is_empty() {
                state.inner.last_sequence.store(last, Ordering::Release);
                log::debug!("外部聊天源注入 {} 条消息", accepted.len());
            }
        }
        Err(error) => log::warn!(
            "外部聊天源注入失败（步骤：把本批消息写入聊天时间线），本批消息没有进入处理流程，玩家的命令不会执行；序号水位不推进，重连后会重新补拉这一段：{error:#}"
        ),
    }
}

/// 后台昵称补查线程。
///
/// 这个身份还需要贪婪补查吗？
///
/// 两个名字都算：备注串（游戏内好友备注）与真实昵称。任一为空，
/// 或备注串还没被聊天桥确认过（玩家可能刚在游戏里设上），都按短冷却反复查。
/// 只补真实昵称会让"好友后设了备注"要等十分钟才生效。
fn needs_greedy_name_refresh(mapping: &crate::features::identity::IdentityMapping) -> bool {
    mapping.real_nickname.trim().is_empty()
        || mapping.remark.trim().is_empty()
        || !mapping.remark_from_chatbridge
}

/// 好友快照应用结果；不打印 UID/昵称/备注，只用于计数。
#[derive(Default)]
struct FriendApplyStats {
    changed: bool,
    /// 快照里命中已登记身份的数量。
    matched: usize,
    /// 快照里未登记、按约定不自动创建的数量。
    skipped_unregistered: usize,
    /// complete=true 时被清理旧备注的已有映射数量。
    cleared_absent: usize,
}

/// 把 `/friends` 全量快照应用到身份映射；不自动创建未登记 UID。
///
/// - 已登记 UID：name 非空写 real_nickname；remark 有值覆盖 remark。
/// - 清除只按 **identity UUID 聚合**，且仅在 complete=true：
///   * 只看该身份的 Genshin 账号；没有 Genshin 账号的映射完全跳过；
///   * 任一账号 remark 有值 → 保留；
///   * 所有 Genshin 账号都缺席，或全部出现且都 remark=null → 清缓存 remark；
///   * 只有 `remark_from_chatbridge=true` 的缓存值会被清，手填/随机键保留。
/// - complete=false：只更新列表内条目，绝不清理。
fn apply_friend_snapshot(
    identity: &crate::features::identity::IdentityAccess,
    snapshot: &crate::runtime::chat_source::FriendSnapshot,
) -> FriendApplyStats {
    let game = miliastra_kernel::chat_source::GAME_GENSHIN;
    let mut stats = FriendApplyStats::default();
    let mut by_uid: std::collections::HashMap<String, &crate::runtime::chat_source::FriendEntry> =
        std::collections::HashMap::new();
    for friend in &snapshot.friends {
        by_uid.insert(friend.uid.to_string(), friend);
        let uid = friend.uid.to_string();
        if identity.resolve_account(game, &uid).is_none() {
            stats.skipped_unregistered += 1;
            continue;
        }
        stats.matched += 1;
        if let Some(name) = friend.name.as_deref()
            && identity.record_real_nickname(game, &uid, name)
        {
            stats.changed = true;
        }
        if let Some(remark) = friend.remark.as_deref()
            && identity.set_account_remark(game, &uid, remark)
        {
            stats.changed = true;
        }
    }
    // 坏快照/跨账号护栏：本次没有任何已登记好友命中时（例如桥连了另一个号、
    // 空列表或全是不认识的人），只增补，绝不按“缺席”清任何身份。
    if !snapshot.complete || stats.matched == 0 {
        return stats;
    }
    for mapping in identity.snapshot().mappings {
        if mapping.role != crate::features::identity::IdentityRole::Friend {
            // Owner/Admin 永不因好友快照被自动清缓存。
            continue;
        }
        let genshin_accounts: Vec<String> = mapping
            .accounts
            .iter()
            .filter(|account| account.game.eq_ignore_ascii_case(game))
            .map(|account| account.uid.trim().to_string())
            .filter(|uid| !uid.is_empty())
            .collect();
        if genshin_accounts.is_empty() {
            // 没有 Genshin 账号的身份不归这份快照管。
            continue;
        }
        let mut present = 0usize;
        let mut absent = 0usize;
        let mut all_present_explicit_null = true;
        for uid in &genshin_accounts {
            match by_uid.get(uid) {
                Some(friend) => {
                    present += 1;
                    // 只有字段存在且显式为 null 才算“明确无备注”；
                    // remark 有值或字段缺失都视为不可清。
                    if !friend.remark_present || friend.remark.is_some() {
                        all_present_explicit_null = false;
                    }
                }
                None => absent += 1,
            }
        }
        let all_absent = present == 0;
        let all_present_without_remark =
            absent == 0 && present > 0 && all_present_explicit_null;
        if (all_absent || all_present_without_remark)
            && identity.clear_remark_for_identity(mapping.id)
        {
            stats.cleared_absent += 1;
            stats.changed = true;
        }
    }
    stats
}

/// 每 60 秒拉一次全量好友快照。
///
/// 成功且 complete=true 时，60 秒内标记为“快照新鲜”，主动 `/names` 轮询会跳过，
/// 避免全量快照与逐 UID 补查同时高频请求；404 只 debug 一次并继续保留 `/names` 兜底。
fn spawn_friends_snapshot(
    state: ExternalChatState,
    http_base: String,
    token: String,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    config_store: Option<crate::config::SharedConfigStore>,
    live_configs: crate::config::LiveConfigs,
) {
    let Some(identity) = identity_access else {
        return;
    };
    let spawned = thread::Builder::new()
        .name("external-chat-friends".into())
        .spawn(move || loop {
            match crate::runtime::chat_source::fetch_friends(&http_base, &token) {
                Ok(Some(snapshot)) => {
                    let stats = apply_friend_snapshot(&identity, &snapshot);
                    if snapshot.complete {
                        state.mark_friends_snapshot();
                    }
                    if stats.changed
                        && let Some(store) = config_store.as_ref()
                        && let Err(error) = persist_identity(store, &live_configs, &identity)
                    {
                        log::warn!(
                            "好友快照已更新身份映射，但未能写入配置库（步骤：全量好友快照同步）；本次运行内仍生效，配置重载后会被旧值覆盖：{error:#}"
                        );
                    }
                    if snapshot.complete {
                        log::debug!(
                            "全量好友快照已应用：命中 {} 条，未登记跳过 {} 条，清理缺席映射 {} 条",
                            stats.matched,
                            stats.skipped_unregistered,
                            stats.cleared_absent
                        );
                    } else {
                        log::debug!(
                            "增量好友快照已增补：命中 {} 条，未登记跳过 {} 条",
                            stats.matched,
                            stats.skipped_unregistered
                        );
                    }
                }
                Ok(None) => {
                    if state.mark_friends_unsupported() {
                        log::debug!(
                            "ChatBridge 没有 /friends 端点（旧版），继续使用 /names 逐 UID 补查兜底"
                        );
                    }
                }
                Err(error) => {
                    log::debug!("好友快照拉取失败，本次不写回身份映射：{error}");
                }
            }
            thread::sleep(FRIENDS_SNAPSHOT_INTERVAL);
        });
    if let Err(error) = spawned {
        log::warn!("启动好友快照线程失败，本次运行继续用 /names 兜底：{error}");
    }
}

/// 按身份映射表定期扫描，把还没拿到名字的账号排进补查队列。
///
/// 消息驱动的补查只在有人说话时发生，从不说话的好友永远拿不到真实昵称；
/// 这里主动扫表：缺真实昵称或备注串的用短冷却反复重试，齐全的按长 TTL 刷新。
fn spawn_name_priming(
    state: ExternalChatState,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    names: std::sync::mpsc::Sender<String>,
) {
    let Some(identity_access) = identity_access else {
        return;
    };
    let spawned = thread::Builder::new()
        .name("external-chat-names-prime".into())
        .spawn(move || {
            loop {
                // 完整好友快照 60 秒内已覆盖名字信息：跳过逐 UID 轮询，避免双频请求。
                if state.friends_snapshot_recent() {
                    thread::sleep(NAME_PRIME_INTERVAL);
                    continue;
                }
                // 先扫一遍再睡：启动后立刻补，不用等一个间隔。
                let config = identity_access.snapshot();
                let mut queued = 0usize;
                for mapping in &config.mappings {
                    let incomplete = needs_greedy_name_refresh(mapping);
                    for account in &mapping.accounts {
                        if !state.prime_name_lookup(&account.uid, incomplete) {
                            continue;
                        }
                        if names.send(account.uid.clone()).is_err() {
                            // 补查线程已退出：解除在途标记，本线程也没必要再跑。
                            state.finish_name_lookup(&account.uid, None, None);
                            return;
                        }
                        queued += 1;
                    }
                }
                if queued > 0 {
                    log::debug!("主动补查：本轮排入 {queued} 个账号的名称查询");
                }
                thread::sleep(NAME_PRIME_INTERVAL);
            }
        });
    if let Err(error) = spawned {
        log::warn!(
            "启动昵称主动补查线程失败，从不说话的好友将拿不到真实昵称：{error}"
        );
    }
}

/// 接收线程只负责把缺昵称的 UID 丢进来；这里串行发请求（单次上限 1s），
/// 结果写回缓存。同一个 UID 只查一次，之后同发送者的消息直接复用。
/// 把一次成功的 `/names` 结果写进身份映射；失败路径不会调用，因此不会误清备注。
fn apply_sender_names(
    identity: &crate::features::identity::IdentityAccess,
    uid: &str,
    names: &crate::runtime::chat_source::SenderNames,
) -> bool {
    let mut changed = false;
    if let Some(name) = names.name.as_deref() {
        changed |= identity.record_real_nickname(
            miliastra_kernel::chat_source::GAME_GENSHIN,
            uid,
            name,
        );
    }
    // `/names` 只写不删：remark 为空/缺失都不清缓存，删除只由 `/friends complete` 做。
    if let Some(remark) = names.remark.as_deref() {
        changed |= identity.set_account_remark(
            miliastra_kernel::chat_source::GAME_GENSHIN,
            uid,
            remark,
        );
    }
    changed
}

fn spawn_name_resolver(
    state: ExternalChatState,
    http_base: String,
    token: String,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    config_store: Option<crate::config::SharedConfigStore>,
    live_configs: crate::config::LiveConfigs,
) -> std::sync::mpsc::Sender<String> {
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    let spawned = thread::Builder::new()
        .name("external-chat-names".into())
        .spawn(move || {
            for uid in receiver {
                match crate::runtime::chat_source::resolve_sender_names(&http_base, &token, &uid) {
                    Ok(names) => {
                        state.finish_name_lookup(
                            &uid,
                            names.name.as_deref(),
                            names.remark.as_deref(),
                        );
                        // 备注也要进 UID 缓存：names 为 null、remark 有值是常见组合，
                        // 这时只缓存昵称的话，按备注名反查 UID（歌单归属）会落空。
                        state.remember_uid(names.name.as_deref(), Some(&uid));
                        state.remember_uid(names.remark.as_deref(), Some(&uid));
                        // 三个名字各写各的字段，**互不覆盖**：
                        //   names   → real_nickname（玩家自设昵称，只做展示/搜索兜底）
                        //   remarks → remark（游戏内好友备注 = 屏幕显示名 = OCR 匹配键）
                        //
                        // 备注是 OCR 侧 `SecondarySenderIdentity::effective_name()`
                        // 取的值（聊天区显示「昵称(备注)」），所以它才是匹配键的正主。
                        // 注意消息体 `from.name` 永远是资料包昵称、不含备注
                        // （ChatBridge 有测试 `message_sender_name_ignores_remarks`
                        // 专门钉住这点），所以备注只能走 /names 拿。
                        //
                        // 不打印 UID / 备注 / 昵称：三者都是真实玩家数据（MUST_READ §9）。
                        if let Some(identity) = identity_access.as_ref() {
                            let changed = apply_sender_names(identity, &uid, &names);
                            if changed
                                && let Some(store) = config_store.as_ref()
                                && let Err(error) =
                                    persist_identity(store, &live_configs, identity)
                            {
                                log::warn!(
                                    "补查到的名字未能写入身份映射（步骤：同步真实昵称与好友备注），本次运行内仍生效；配置重载或重启后会被库里的旧值覆盖，重新补查一次即可：{error:#}"
                                );
                            }
                        }
                        if names.remark.is_some() {
                            log::debug!("备注补查命中，已用好友备注覆盖该账号的屏幕匹配名");
                        }
                    }
                    Err(error) => {
                        state.finish_name_lookup(&uid, None, None);
                        log::debug!("昵称补查失败：{error:#}");
                    }
                }
            }
        });
    if let Err(error) = spawned {
        log::warn!("启动昵称补查线程失败，缺昵称的消息将按未知发送者处理：{error}");
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
        log::warn!(
            "重置聊天基线失败（步骤：切换数据源后重建聊天基线），命令锁也因此没有清空；新数据源的首屏历史可能被当作新消息重放到业务层，同一命令有被执行两次的风险：{error:#}"
        );
        return;
    }
    if let Err(error) = timeline.clear_command_locks() {
        log::warn!(
            "重置命令锁失败（步骤：切换数据源后清空旧的命令锁），旧锁的屏幕坐标对新数据源无意义；同文本命令可能被误判为已处理而静默跳过，也可能重复执行：{error:#}"
        );
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
        state.finish_name_lookup("900000001", Some("玩家癸"), None);
        assert_eq!(state.cached_name("900000001").as_deref(), Some("玩家癸"));
        // 已有昵称就不再查网络。
        assert!(!state.begin_name_lookup("900000001"));
    }

    /// 回归：只有备注、没有资料包昵称时也必须算「查到了」。
    ///
    /// 实测 /names 的常见组合就是 `names:{uid:null}` 配 `remarks:{uid:"某备注"}`。
    /// 如果只认 name，这种情况会被判失败进 30 秒冷却，然后永久重查——
    /// 既打网络，又让备注迟迟写不进身份映射。
    #[test]
    fn remark_only_lookup_counts_as_resolved() {
        let state = ExternalChatState::default();
        assert!(state.begin_name_lookup("900000002"));
        state.finish_name_lookup("900000002", None, Some("显示名甲"));
        // 没有昵称，但结果已缓存，不该再排队。
        assert_eq!(state.cached_name("900000002"), None);
        assert!(
            !state.begin_name_lookup("900000002"),
            "只有备注也算查到过，不能进失败冷却重查"
        );
    }

    /// 回归：TIL 过期后必须允许重查。
    ///
    /// 备注是玩家在游戏里随时能改的。如果命中一次就永久缓存，
    /// 「后设的备注」永远进不来——这正是本轮要修的问题。
    #[test]
    fn expired_name_cache_allows_refetch() {
        let state = ExternalChatState::default();
        assert!(state.begin_name_lookup("900000003"));
        state.finish_name_lookup("900000003", Some("玩家乙"), None);
        assert!(!state.begin_name_lookup("900000003"), "TTL 内不重查");
        // 把时间戳改成过期：模拟 TTL 之后。
        {
            let mut lookups = state.name_lookups();
            if let Some(resolved) = lookups.resolved.get_mut("900000003") {
                resolved.fetched_at_ms = 0;
            }
        }
        assert!(
            state.begin_name_lookup("900000003"),
            "过期后必须放行，否则后设的备注永远同步不进来"
        );
    }

    /// 补查失败要进入冷却，避免每条缺昵称消息都打一次网络。
    #[test]
    fn failed_name_lookup_enters_cooldown() {
        let state = ExternalChatState::default();
        assert!(state.begin_name_lookup("42"));
        state.finish_name_lookup("42", None, None);
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

    /// H1 回归（必须）：宿主重启后消息序号从 1 重新开始，旧水位必须清零。
    ///
    /// 旧行为只在首次握手建立水位且重连不更新：本地水位 500 时，宿主重启后的
    /// 新消息（id=1）会被「sequence > watermark」全部过滤；同时 hello 把状态
    /// 置为 active、OCR 被关掉，于是 BOT 永久静默失聪直到 Music 重启。
    #[test]
    fn host_restart_resets_watermark_and_keeps_new_message_id_1() {
        let state = ExternalChatState::default();
        // 旧宿主：启动水位 500（本进程已按旧规则建立过）。
        state.establish_watermark(500);
        assert_eq!(state.watermark(), 500);

        // 新宿主：bootId 变化且 lastMessageId 回退到 0。
        let action = state.apply_handshake(0, Some(1_802_240_000_000_000));
        assert_eq!(action, HandshakeAction::HostRestarted);
        assert_eq!(state.watermark(), 0, "宿主重启后旧水位必须清零");
        assert_eq!(state.last_sequence(), 0, "本地游标也要一起清零");

        // 关键断言：新宿主的第一条消息 id=1 不能被旧水位 500 丢弃。
        assert!(
            state.accepts_sequence(1),
            "宿主重启后 id=1 的新消息必须被处理，否则 BOT 静默失聪"
        );
    }

    /// 没有 bootId 的旧宿主：hello.lastMessageId 回退即视为宿主重启。
    #[test]
    fn missing_boot_id_falls_back_to_sequence_regression() {
        let state = ExternalChatState::default();
        state.establish_watermark(500);
        // 旧宿主不带 bootId，只能靠序号回退兜底。
        assert_eq!(
            state.apply_handshake(0, None),
            HandshakeAction::HostRestarted
        );
        assert_eq!(state.watermark(), 0);
        assert!(state.accepts_sequence(1));
    }

    /// bootId 变化优先于序号：即使 lastMessageId 看起来更大也要重置。
    #[test]
    fn boot_id_change_resets_even_with_larger_last_message_id() {
        let state = ExternalChatState::default();
        assert_eq!(
            state.apply_handshake(100, Some(7)),
            HandshakeAction::FirstConnect
        );
        assert_eq!(state.watermark(), 100);
        // 同一宿主内的普通重连：水位保持，断线期间的消息仍可补拉。
        assert_eq!(
            state.apply_handshake(150, Some(7)),
            HandshakeAction::Reconnect
        );
        assert_eq!(state.watermark(), 100);
        // 宿主重启：即使 lastMessageId=200 > 本地水位、序号没有回退，旧水位也已失效。
        // 这里 sequence_regressed 为 false，只有 bootId 判定能捕获——不要
        // 因为有序号回退兜底就删掉 bootId 比较。
        assert_eq!(
            state.apply_handshake(200, Some(8)),
            HandshakeAction::HostRestarted
        );
        assert_eq!(state.watermark(), 0);
        assert!(state.accepts_sequence(1));
    }

    /// 复审建议：bootId 判定的独立价值——宿主重启但新进程序号恰好没有回退。
    ///
    /// 真实事故形态通常是「旧水位 500 → 新宿主 hello 0」，这种会被
    /// sequence_regressed 兜住；但如果新宿主在握手前已经收到 600 条消息，
    /// hello=600 > 本地水位 500，序号完全没有回退，只有 bootId 变化能识别重启。
    /// 不要因为有序号回退兜底就删掉 bootId 比较。
    #[test]
    fn boot_id_change_catches_restart_even_without_sequence_regression() {
        let state = ExternalChatState::default();
        assert_eq!(
            state.apply_handshake(500, Some(1)),
            HandshakeAction::FirstConnect
        );
        // 同一 bootId 的普通重连：hello 600 不小于本地水位，序号回退兜底不会触发。
        assert_eq!(
            state.apply_handshake(600, Some(1)),
            HandshakeAction::Reconnect
        );
        assert_eq!(state.watermark(), 500, "普通重连保持水位");
        // 宿主重启：bootId 变化；hello=600 比本地水位 500 还大，没有序号回退。
        assert_eq!(
            state.apply_handshake(600, Some(2)),
            HandshakeAction::HostRestarted
        );
        assert_eq!(state.watermark(), 0);
        assert!(state.accepts_sequence(1));
    }

    /// 启动水位只挡宿主启动前的历史；首次握手建立后，同一宿主内不因重连改变。
    #[test]
    fn reconnect_without_restart_keeps_watermark_for_backlog() {
        let state = ExternalChatState::default();
        assert_eq!(
            state.apply_handshake(42, Some(7)),
            HandshakeAction::FirstConnect
        );
        assert_eq!(
            state.apply_handshake(100, Some(7)),
            HandshakeAction::Reconnect
        );
        assert_eq!(state.watermark(), 42, "普通重连不得抬高或清除水位");
        // 断线期间 43..100 的消息仍然应该被补拉后处理。
        assert!(state.accepts_sequence(43));
        assert!(state.accepts_sequence(100));
        assert!(!state.accepts_sequence(42));
    }

    /// 无 bootId 且序号没有回退：保持水位（不能把所有重连都误判成宿主重启）。
    #[test]
    fn reconnect_without_boot_id_and_without_regression_keeps_watermark() {
        let state = ExternalChatState::default();
        assert_eq!(
            state.apply_handshake(42, None),
            HandshakeAction::FirstConnect
        );
        assert_eq!(state.apply_handshake(43, None), HandshakeAction::Reconnect);
        assert_eq!(state.watermark(), 42);
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
    /// 回归：心跳/读超时期间连接仍被报告为存活，但接管租约不再由它刷新。
    ///
    /// H2 修复后，WS 活着不等于游戏已接入：租约只由真实消息或
    /// /health 的 gameConnected=true 刷新；is_connected 只用于区分
    /// 「读超时/Ping」与「连接已关」，不再续租，因此不会因为 30 秒一次的
    /// Ping 造成接管抖动，也不会在游戏未接入时空转接管。
    #[test]
    fn heartbeat_reports_connection_state_without_renewing_lease() {
        // is_connected 只回答「连接是否还开着」，用于区分读超时/心跳与
        // 连接已关；它不再刷新接管租约（租约由真实消息或 /health 维护）。
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
            "读超时/心跳期间 is_connected 仍报告连接存活；接管租约不由它刷新"
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

    #[test]
    fn greedy_refresh_covers_both_names() {
        use crate::features::identity::{IdentityMapping, IdentityRole};
        let mapping = |remark: &str, real: &str, from_chatbridge: bool| IdentityMapping {
            remark: remark.to_owned(),
            id: uuid::Uuid::new_v4(),
            role: IdentityRole::Friend,
            display_name: String::new(),
            real_nickname: real.to_owned(),
            remark_from_chatbridge: from_chatbridge,
            accounts: Vec::new(),
        };
        // 两个名字都齐、且备注串已被聊天桥确认：不必再贪婪补。
        assert!(!needs_greedy_name_refresh(&mapping("abcd1234", "真名甲", true)));
        // 真实昵称缺失。
        assert!(needs_greedy_name_refresh(&mapping("abcd1234", "", true)));
        // 备注串缺失。
        assert!(needs_greedy_name_refresh(&mapping("", "真名甲", true)));
        // 备注串还是随机占位（没被聊天桥确认过）：玩家可能刚在游戏里设上备注。
        assert!(needs_greedy_name_refresh(&mapping("abcd1234", "真名甲", false)));
    }

    fn identity_with_remark(uid: &str) -> crate::features::identity::IdentityAccess {
        let mut config = crate::features::identity::IdentityConfig::default();
        config
            .mappings
            .push(crate::features::identity::IdentityMapping {
                remark: "旧备注".to_string(),
                id: uuid::Uuid::new_v4(),
                role: crate::features::identity::IdentityRole::Friend,
                display_name: "显示甲".to_string(),
                real_nickname: "真实甲".to_string(),
                remark_from_chatbridge: true,
                accounts: vec![crate::features::identity::GameAccount::new(uid)],
            });
        crate::features::identity::IdentityAccess::new(config)
    }

    #[test]
    fn friends_snapshot_apply_only_updates_existing_and_clears_absent_on_complete() {
        let mut config = crate::features::identity::IdentityConfig::default();
        for (uid, remark, real) in [
            ("10001", "旧备注甲", "旧真名甲"),
            ("10002", "旧备注乙", "旧真名乙"),
        ] {
            config
                .mappings
                .push(crate::features::identity::IdentityMapping {
                    remark: remark.to_string(),
                    id: uuid::Uuid::new_v4(),
                    role: crate::features::identity::IdentityRole::Friend,
                    display_name: String::new(),
                    real_nickname: real.to_string(),
                    remark_from_chatbridge: true,
                    accounts: vec![crate::features::identity::GameAccount::new(uid)],
                });
        }
        let identity = crate::features::identity::IdentityAccess::new(config);
        let snapshot = crate::runtime::chat_source::FriendSnapshot {
            complete: true,
            updated_at: Some(1),
            friends: vec![
                crate::runtime::chat_source::FriendEntry {
                    uid: 10001,
                    name: Some("新真名甲".to_string()),
                    remark: Some("新备注甲".to_string()),
                    remark_present: true,
                },
                crate::runtime::chat_source::FriendEntry {
                    uid: 99999,
                    name: Some("未登记".to_string()),
                    remark: Some("不会创建".to_string()),
                    remark_present: true,
                },
            ],
        };
        let stats = apply_friend_snapshot(&identity, &snapshot);
        assert!(stats.changed);
        assert_eq!(stats.matched, 1);
        assert_eq!(stats.skipped_unregistered, 1);
        assert_eq!(stats.cleared_absent, 1);
        let mappings = identity.snapshot().mappings;
        assert_eq!(mappings.len(), 2, "不能自动创建未登记 UID");
        let first = mappings
            .iter()
            .find(|mapping| mapping.accounts.iter().any(|account| account.uid == "10001"))
            .unwrap();
        assert_eq!(first.real_nickname, "新真名甲");
        assert_eq!(first.remark, "新备注甲");
        let absent = mappings
            .iter()
            .find(|mapping| mapping.accounts.iter().any(|account| account.uid == "10002"))
            .unwrap();
        assert!(absent.remark.is_empty(), "complete 快照缺席者清备注");
        assert_eq!(absent.real_nickname, "旧真名乙", "保留真实昵称/身份");
        assert!(!identity
            .ai_name_pairs()
            .iter()
            .any(|pair| pair.left() == "旧备注乙"));

        // complete=false：列表内 remark:null 也不清，缺席者更不动。
        let identity = identity_with_remark("10003");
        let incomplete = crate::runtime::chat_source::FriendSnapshot {
            complete: false,
            updated_at: None,
            friends: vec![crate::runtime::chat_source::FriendEntry {
                uid: 10003,
                name: None,
                remark: None,
                remark_present: true,
            }],
        };
        assert!(!apply_friend_snapshot(&identity, &incomplete).changed);
        assert_eq!(identity.snapshot().mappings[0].remark, "旧备注");
    }

    #[test]
    fn friends_snapshot_clears_by_identity_uuid_and_preserves_handfilled() {
        use crate::features::identity::{
            GameAccount, IdentityAccess, IdentityConfig, IdentityMapping, IdentityRole,
        };
        use crate::runtime::chat_source::{FriendEntry, FriendSnapshot};
        let mut config = IdentityConfig::default();
        let mut index = 0usize;
        let mut push = |accounts: Vec<GameAccount>, remark: &str, flag: bool| {
            let current = index;
            index += 1;
            config.mappings.push(IdentityMapping {
                remark: remark.to_string(),
                id: uuid::Uuid::new_v4(),
                role: IdentityRole::Friend,
                display_name: String::new(),
                real_nickname: String::new(),
                remark_from_chatbridge: flag,
                accounts,
            });
            current
        };
        let multi = push(
            vec![GameAccount::new("30001"), GameAccount::new("30002")],
            "旧缓存甲",
            true,
        );
        let null_only = push(vec![GameAccount::new("30003")], "旧缓存乙", true);
        let other_game = push(
            vec![GameAccount {
                game: "starrail".to_string(),
                uid: "30004".to_string(),
            }],
            "旧缓存丙",
            true,
        );
        let no_accounts = push(Vec::new(), "旧缓存丁", true);
        let handfilled = push(vec![GameAccount::new("30005")], "手填备注", false);
        let missing_remark_field = push(vec![GameAccount::new("30007")], "旧缓存戊", true);
        let owner = index;
        config.mappings.push(IdentityMapping {
            remark: "管理员缓存".to_string(),
            id: uuid::Uuid::new_v4(),
            role: IdentityRole::Owner,
            display_name: String::new(),
            real_nickname: String::new(),
            remark_from_chatbridge: true,
            accounts: vec![GameAccount::new("30006")],
        });
        let identity = IdentityAccess::new(config);
        let ids: Vec<uuid::Uuid> = identity
            .snapshot()
            .mappings
            .iter()
            .map(|mapping| mapping.id)
            .collect();
        let snapshot = FriendSnapshot {
            complete: true,
            updated_at: Some(1),
            friends: vec![
                FriendEntry {
                    uid: 30001,
                    name: None,
                    remark: Some("新缓存甲".to_string()),
                    remark_present: true,
                },
                FriendEntry {
                    uid: 30002,
                    name: None,
                    remark: None,
                    remark_present: true,
                },
                FriendEntry {
                    uid: 30003,
                    name: None,
                    remark: None,
                    remark_present: true,
                },
                FriendEntry {
                    uid: 30007,
                    name: None,
                    remark: None,
                    remark_present: false,
                },
            ],
        };
        let stats = apply_friend_snapshot(&identity, &snapshot);
        assert_eq!(stats.cleared_absent, 1, "只有全 null 的身份清缓存");
        let mappings = identity.snapshot().mappings;
        let multi = mappings.iter().find(|m| m.id == ids[multi]).unwrap();
        assert_eq!(multi.remark, "新缓存甲", "一个账号有备注就保留");
        let null_only = mappings.iter().find(|m| m.id == ids[null_only]).unwrap();
        assert!(null_only.remark.is_empty(), "全部出现且都 null 才清");
        assert_eq!(
            mappings.iter().find(|m| m.id == ids[other_game]).unwrap().remark,
            "旧缓存丙",
            "其他游戏账号跳过"
        );
        assert_eq!(
            mappings.iter().find(|m| m.id == ids[no_accounts]).unwrap().remark,
            "旧缓存丁",
            "无账号映射跳过"
        );
        assert_eq!(
            mappings.iter().find(|m| m.id == ids[handfilled]).unwrap().remark,
            "手填备注",
            "手填备注保留"
        );
        assert_eq!(
            mappings.iter().find(|m| m.id == ids[owner]).unwrap().remark,
            "管理员缓存",
            "Owner/Admin 永不自动清"
        );
        assert_eq!(
            mappings.iter().find(|m| m.id == ids[missing_remark_field]).unwrap().remark,
            "旧缓存戊",
            "remark 字段缺失不得当 null 清"
        );

        // complete=false 即使全 null 也不清。
        let identity = identity_with_remark("30003");
        let incomplete = FriendSnapshot {
            complete: false,
            updated_at: None,
            friends: vec![FriendEntry {
                uid: 30003,
                name: None,
                remark: None,
                remark_present: true,
            }],
        };
        assert!(!apply_friend_snapshot(&identity, &incomplete).changed);
        assert_eq!(identity.snapshot().mappings[0].remark, "旧备注");

        // complete 空列表但 matched=0：按坏快照/跨账号护栏只增补、不清缺席。
        let empty = FriendSnapshot {
            complete: true,
            updated_at: Some(1),
            friends: Vec::new(),
        };
        let stats = apply_friend_snapshot(&identity, &empty);
        assert!(!stats.changed);
        assert_eq!(identity.snapshot().mappings[0].remark, "旧备注");
    }

    #[test]
    fn identity_persist_lock_serializes_writers() {
        let guard = identity_persist_guard();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let _guard = identity_persist_guard();
            let _ = sender.send(());
        });
        thread::sleep(Duration::from_millis(20));
        assert!(
            receiver.try_recv().is_err(),
            "持锁期间另一个写者不能进入持久化临界区"
        );
        drop(guard);
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("释放锁后写者应完成");
        worker.join().expect("写线程");
    }

    #[test]
    fn friends_snapshot_freshness_and_unsupported_are_one_shot() {
        let state = ExternalChatState::default();
        assert!(!state.friends_snapshot_recent());
        state.mark_friends_snapshot();
        assert!(state.friends_snapshot_recent());
        assert!(state.mark_friends_unsupported());
        assert!(!state.mark_friends_unsupported());
    }

    #[test]
    fn names_never_clear_remark_but_can_still_update_name() {
        let identity = identity_with_remark("10001");
        // 200 + 明确 remarks 字段但为空：/names 也不清，只更新 name。
        let empty = crate::runtime::chat_source::SenderNames {
            name: Some("新真名".to_string()),
            remark: None,
        };
        assert!(apply_sender_names(&identity, "10001", &empty));
        let mapping = &identity.snapshot().mappings[0];
        assert_eq!(mapping.remark, "旧备注", "/names 绝不能清 remark");
        assert!(mapping.remark_from_chatbridge);
        assert_eq!(mapping.real_nickname, "新真名");
        assert_eq!(mapping.display_name, "显示甲");

        // 无 remarks 字段：旧 ChatBridge，保留旧值。
        let legacy = identity_with_remark("10002");
        let no_field = crate::runtime::chat_source::SenderNames {
            name: None,
            remark: None,
        };
        assert!(!apply_sender_names(&legacy, "10002", &no_field));
        assert_eq!(legacy.snapshot().mappings[0].remark, "旧备注");

        // 有备注：覆盖。
        let replaced = identity_with_remark("10003");
        let with_remark = crate::runtime::chat_source::SenderNames {
            name: None,
            remark: Some("新备注".to_string()),
        };
        assert!(apply_sender_names(&replaced, "10003", &with_remark));
        assert_eq!(replaced.snapshot().mappings[0].remark, "新备注");
        assert!(replaced.snapshot().mappings[0].remark_from_chatbridge);
    }

    #[test]
    fn prime_lookup_queues_once_then_respects_the_cooldown() {
        let state = ExternalChatState::default();
        // 第一次必须放行。
        assert!(state.prime_name_lookup("100000001", true));
        // 在途期间不能重复排队。
        assert!(!state.prime_name_lookup("100000001", true));
        // 查到之后：缺名字的走短冷却，齐全的走长 TTL，两条路都不能立刻再排。
        state.finish_name_lookup("100000001", Some("玩家甲"), None);
        assert!(!state.prime_name_lookup("100000001", true));
        assert!(!state.prime_name_lookup("100000001", false));
        // 空 UID 永远不排队。
        assert!(!state.prime_name_lookup("  ", true));
    }
}
