//! 外部聊天源线程：把 Miliastra-Wonderland-ChatBridge 的推送注入时间线，并接管 OCR 的聊天内容识别。
//!
//! 与 `shared_observer` 平行：那边用模板匹配判界面、用 OCR 读文字；这边用 WS 读文字。
//! 界面识别仍由 `shared_observer` 负责，本模块只提供"当前是否接管"这一信号。
//!
//! # 线程与停止合同
//!
//! 启用外部源时本模块一共起四条后台线程，全部由 [`ExternalChatWorkers`] 持有句柄：
//!
//! | 线程名 | 职责 | 等待方式 |
//! |---|---|---|
//! | `external-chat` | 主 WS 连接 / 重连 / 消息消费 | [`ExternalChatShutdown::wait`]，以及 WS 的 3 秒读超时 |
//! | `external-chat-names` | 逐个 UID 补查昵称与备注 | 带超时的队列接收 |
//! | `external-chat-names-prime` | 按身份映射表定期预扫 | [`ExternalChatShutdown::wait`] |
//! | `external-chat-friends` | 定期拉全量好友快照 | [`ExternalChatShutdown::wait`] |
//!
//! 停止只有一条路：调用 [`ExternalChatWorkers::request_shutdown`] 置停止标志并唤醒
//! 所有等待者，再调用 [`ExternalChatWorkers::shutdown`] 逐个 [`join`](std::thread::JoinHandle::join)。
//! 四条线程的每一次等待都是「可被停止信号打断的有界等待」，不存在裸
//! [`thread::sleep`]：进程退出时不会留下在后台空转的孤儿线程。

use std::collections::HashMap;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(test)]
#[cfg(test)]
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
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

/// 主动补查的扫描间隔。也是好友快照轮询间隔，两者共用同一个值。
///
/// 只在"有人说话"时才查会漏掉从不说话的好友，所以另外按身份映射表定期扫一遍。
/// 真正的去重仍由 prime_name_lookup 的冷却窗口负责，这里只决定多久看一次表。
const NAME_PRIME_INTERVAL: Duration = Duration::from_secs(60);
/// 全量好友快照的轮询间隔；与 ChatBridge 的 60 秒全量重发对齐。
const FRIENDS_SNAPSHOT_INTERVAL: Duration = NAME_PRIME_INTERVAL;
/// 快照新鲜期略小于 60s，避免与 `/names` 轮询相位锁死后一直跳过。
const FRIENDS_SNAPSHOT_FRESH_MS: u64 = 45_000;

/// 昵称补查线程轮询队列的间隔。
///
/// 只决定停止信号最多多久被它发现，与查询频率无关（查询本身由队列驱动，
/// 单次 HTTP 上限 1 秒）。停止时 [`ExternalChatWorkers::shutdown`] 也会在
/// 发出信号后释放全部发送端，因此最坏等待远小于这个值。
const NAME_RESOLVER_POLL_INTERVAL: Duration = Duration::from_millis(200);

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

/// 单调时钟读数的量化步长（毫秒）。
///
/// 冷却与 TTL 都以毫秒为单位；把单调读数按毫秒量化后比较，语义与毫秒时间戳一致，
/// 但**不依赖进程启动后的绝对时间点**。因此单测不必真的等：「从现在开始算的
/// 30 秒冷却」可以注入成「几毫秒的冷却」，等待多久完全由测试决定。
const MONOTONIC_STEP_MS: u64 = 1;

/// 单调时钟读数（毫秒，自量化步长起算）。
fn monotonic_millis() -> u64 {
    now_millis_since_marker(0)
}

/// 单调时钟读数（毫秒），保证结果至少是 `steps` 个量化步长。
fn now_millis_since_marker(steps: u64) -> u64 {
    let offset = Duration::from_millis(MONOTONIC_STEP_MS.saturating_mul(steps.saturating_add(1)));
    (offset + Instant::now().elapsed())
        .as_millis()
        .max(1)
        .try_into()
        .unwrap_or(u64::MAX)
}

/// 冷却窗口：从现在起 `window_ms` 毫秒内不再排队。
fn cooldown_from_now(window_ms: u64, now: u64) -> u64 {
    now.saturating_add(window_ms.max(MONOTONIC_STEP_MS))
}

/// 主动补查里"还缺真实昵称"时的重试窗口；不完整返回 `u64::MAX` 表示改用
/// 成功后的长 TTL（见 [`ExternalChatState::prime_name_lookup_within`]）。
fn incomplete_retry_ms(incomplete: bool) -> u64 {
    if incomplete {
        NAME_PRIME_INCOMPLETE_MS
    } else {
        u64::MAX
    }
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

struct ExternalChatInner {
    /// 昵称 → 游戏 UID。外部源每条消息都带 UID，这里缓存下来供歌单等
    /// 需要"按人定位"的功能使用。程序重启后为空，此时歌单退化到 name: 键。
    nick_to_uid: Mutex<HashMap<String, String>>,
    /// 异步昵称补查的进度：已解析结果 + 下次允许查询的时间。
    name_lookups: Mutex<NameLookups>,
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
    host_boot_id: Mutex<Option<u64>>,
    /// 是否已经用掉"首次遇到一级聊天界面时自动切二级"的机会。
    initial_secondary_switch_done: AtomicBool,
    /// 是否**曾经**成功接管过（握手成功且 /health 确认游戏已接入）。
    ///
    /// 「连上过又失联」与「从来没连上」是两件事：前者说明游戏/DLL 链路中途断了，
    /// 重启游戏有意义；后者是桥本身没起来，重启游戏只会白打断玩家。
    ever_connected: AtomicBool,
    /// 失联起始时刻（毫秒时间戳）；0 表示当前没在失联。
    lost_since_ms: AtomicU64,
    /// 是否暂停「失联后重启游戏」的判定。
    ///
    /// 闲置退出会**主动**关闭游戏：那一刻聊天源必然跟着断开，如果不抑制，
    /// 回落阈值一到就会把刚被闲置退出关掉的游戏重新拉起来，闲置退出形同虚设。
    /// 重新连上（游戏被重新启动并接入）时自动解除，无需人工干预。
    restart_suppressed: AtomicBool,
    /// 最近一次**完整** `/friends` 快照写入身份映射的时间（毫秒）；0=从未。
    friends_snapshot_at_ms: AtomicU64,
    /// 旧桥 404 只 debug 一次。
    friends_unsupported: AtomicBool,
    /// 「补查线程回到等待」代际（测试用）：每处理完一条队列消息就推进一次。
    #[cfg(test)]
    name_lookup_idle_epoch: AtomicU64,
    /// 与上一个代际配对的条件等待（测试用）；生产路径不存在。
    ///
    /// 自己带锁：等待方在锁内读代际、再在同一个锁上等待，等待方与通知方
    /// 不会在「读代际」与「进入等待」之间丢通知。
    #[cfg(test)]
    name_lookup_idle: Condvar,
    #[cfg(test)]
    name_lookup_idle_gate: Mutex<()>,
}

impl Default for ExternalChatInner {
    fn default() -> Self {
        Self {
            nick_to_uid: Mutex::new(HashMap::new()),
            name_lookups: Mutex::new(NameLookups::default()),
            active: AtomicBool::new(false),
            last_sequence: AtomicU64::new(0),
            watermark: AtomicU64::new(0),
            watermark_set: AtomicBool::new(false),
            last_seen_ms: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            host_boot_id: Mutex::new(None),
            initial_secondary_switch_done: AtomicBool::new(false),
            ever_connected: AtomicBool::new(false),
            lost_since_ms: AtomicU64::new(0),
            restart_suppressed: AtomicBool::new(false),
            friends_snapshot_at_ms: AtomicU64::new(0),
            friends_unsupported: AtomicBool::new(false),
            #[cfg(test)]
            name_lookup_idle_epoch: AtomicU64::new(0),
            #[cfg(test)]
            name_lookup_idle: Condvar::new(),
            #[cfg(test)]
            name_lookup_idle_gate: Mutex::new(()),
        }
    }
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
    /// UID → 允许再次补查的时刻（单调毫秒；在途与失败冷却共用）。
    ///
    /// 单调而非墙钟：冷却窗口是「从现在开始算多久」，不该被系统时间跳变影响，
    /// 也让等待多久完全由调用方决定（见 [`ExternalChatState::prime_name_lookup_within`]）。
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

    /// 仅供测试：把状态置成"已接管"，用于验证依赖该信号的决策分支。
    #[cfg(test)]
    pub(crate) fn mark_active_for_test(&self) {
        self.set_active(true);
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

    /// 外部源已连续失联多久；未失联或从未连上过则返回 None。
    ///
    /// 只回答「连上过、现在断了、断了多久」，不做任何等待或副作用：
    /// 是否重启游戏由主循环按配置决定（线程所有权在 ApplicationRuntime 手上）。
    pub(crate) fn lost_for(&self) -> Option<std::time::Duration> {
        if self.inner.restart_suppressed.load(Ordering::Acquire)
            || !self.inner.ever_connected.load(Ordering::Acquire)
            || self.is_active()
        {
            return None;
        }
        let since = self.inner.lost_since_ms.load(Ordering::Acquire);
        (since != 0)
            .then(|| std::time::Duration::from_millis(now_millis().saturating_sub(since).max(1)))
    }

    /// 新一次成功握手就解除抑制。
    ///
    /// 不必等到"完全接管"：闲置退出关掉游戏后，玩家手动重开时桥会重新握手，
    /// 那一刻就说明游戏回来了。若只在 `set_active(true)` 解除，一旦桥没接上
    /// （DLL 未注入 / 桥没运行），抑制会**永久**生效，把失联重启能力整体关死。
    pub(crate) fn lift_restart_suppression(&self) {
        self.inner
            .restart_suppressed
            .store(false, Ordering::Release);
        // 解除时如果**仍然**失联（握手成功但游戏还没真正接回来），必须立刻
        // 重新起算：抑制期间把 lost_since_ms 清成了 0，而后续不再有新的
        // `set_active(false)` 来写这个值，失联判定会永久失效。
        if self.inner.ever_connected.load(Ordering::Acquire)
            && !self.is_active()
            && self.inner.lost_since_ms.load(Ordering::Acquire) == 0
        {
            self.inner
                .lost_since_ms
                .store(now_millis(), Ordering::Release);
        }
    }

    /// 主动关掉游戏之后抑制「失联重启」判定（闲置退出用）。
    ///
    /// 与 [`Self::clear_loss`] 的区别：`clear_loss` 只是重新计时（下一次失联还会
    /// 触发），这里是**彻底不判**，直到聊天源重新连上。
    pub(crate) fn suppress_restart_after_intentional_close(&self) {
        self.inner.restart_suppressed.store(true, Ordering::Release);
        self.inner.lost_since_ms.store(0, Ordering::Release);
    }

    /// 失联起点（毫秒时间戳）；当前没在失联为 None。
    ///
    /// 供测试断言「起点没有被重复调用往后推」——比拿两次失联时长相减更直接，
    /// 也让用例不必真的等一段时间。
    pub(crate) fn loss_started_ms(&self) -> Option<u64> {
        let since = self.inner.lost_since_ms.load(Ordering::Acquire);
        (since != 0).then_some(since)
    }

    pub(crate) fn restart_suppressed(&self) -> bool {
        self.inner.restart_suppressed.load(Ordering::Acquire)
    }

    /// 重启游戏后清掉失联起点：下一次判定重新计时，不会因为旧的起点立刻又触发。
    pub(crate) fn clear_loss(&self) {
        self.inner.lost_since_ms.store(0, Ordering::Release);
    }

    fn set_active(&self, active: bool) {
        if active {
            self.inner.ever_connected.store(true, Ordering::Release);
            self.inner.lost_since_ms.store(0, Ordering::Release);
            // 重新连上说明游戏已经回来了（手动或自动拉起），解除闲置退出的抑制。
            self.inner
                .restart_suppressed
                .store(false, Ordering::Release);
        } else if self.inner.ever_connected.load(Ordering::Acquire)
            && self.inner.lost_since_ms.load(Ordering::Acquire) == 0
        {
            // 只记第一次失联的时刻：反复 set_active(false) 不能把起点一直往后推。
            self.inner
                .lost_since_ms
                .store(now_millis(), Ordering::Release);
        }
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
        self.cached_name_within(uid, NAME_CACHE_TTL_MS)
    }

    /// 同 [`Self::cached_name`]，但 TTL 由调用方给出。
    ///
    /// 生产只有一个 TTL（[`NAME_CACHE_TTL_MS`]）；参数化是为了让单测能注入
    /// 毫秒级 TTL，验证「过期后必须重新放行」而不必真的等十分钟。
    pub(crate) fn cached_name_within(&self, uid: &str, ttl_ms: u64) -> Option<String> {
        let lookups = self.name_lookups();
        let resolved = lookups.resolved.get(uid.trim())?;
        if now_millis().saturating_sub(resolved.fetched_at_ms) >= ttl_ms {
            return None;
        }
        resolved.name.clone()
    }

    /// 申请一次异步补查。
    ///
    /// 返回 false 表示 TTL 内已有结果、正在补查，或处于失败冷却中。
    /// 结果过期后必须放行——否则玩家后设的备注永远进不来。
    pub(crate) fn begin_name_lookup(&self, uid: &str) -> bool {
        self.begin_name_lookup_within(uid, NAME_CACHE_TTL_MS, NAME_LOOKUP_IN_FLIGHT_MS)
    }

    /// 同 [`Self::begin_name_lookup`]，冷却窗口由调用方给出（单测注入毫秒级窗口）。
    pub(crate) fn begin_name_lookup_within(
        &self,
        uid: &str,
        result_ttl_ms: u64,
        in_flight_ms: u64,
    ) -> bool {
        self.reserve_name_lookup(uid, result_ttl_ms, in_flight_ms)
            .is_some()
    }

    /// 同上，但返回本次申请登记在途标记的时刻；被拒绝时返回 None。
    fn reserve_name_lookup(&self, uid: &str, result_ttl_ms: u64, in_flight_ms: u64) -> Option<u64> {
        let uid = uid.trim();
        if uid.is_empty() {
            return None;
        }
        let now = now_millis();
        let attempts_now = monotonic_millis();
        let mut lookups = self.name_lookups();
        if lookups
            .resolved
            .get(uid)
            .is_some_and(|resolved| now.saturating_sub(resolved.fetched_at_ms) < result_ttl_ms)
        {
            return None;
        }
        if lookups
            .next_attempt_ms
            .get(uid)
            .is_some_and(|next| *next > attempts_now)
        {
            return None;
        }
        lookups.next_attempt_ms.insert(
            uid.to_string(),
            cooldown_from_now(in_flight_ms, attempts_now),
        );
        Some(attempts_now)
    }

    /// 主动补查的排队申请（按身份映射表扫描用）。
    ///
    /// 与消息路径的 begin_name_lookup 区别只在冷却窗口：
    /// incomplete（身份里还缺真实昵称或备注）时用重试冷却，
    /// 而不是"成功查过就等 10 分钟"，否则 /names 当时返回 null 的玩家
    /// 要等很久才会被再补一次。
    pub(crate) fn prime_name_lookup(&self, uid: &str, incomplete: bool) -> bool {
        self.prime_name_lookup_within(uid, incomplete_retry_ms(incomplete), NAME_CACHE_TTL_MS)
    }

    /// 同 [`Self::prime_name_lookup`]，成功 TTL 由调用方给出。
    pub(crate) fn prime_name_lookup_within(
        &self,
        uid: &str,
        incomplete_retry_ms: u64,
        complete_ttl_ms: u64,
    ) -> bool {
        let window = if incomplete_retry_ms < u64::MAX {
            incomplete_retry_ms
        } else {
            complete_ttl_ms
        };
        self.reserve_name_lookup(uid, window, NAME_LOOKUP_IN_FLIGHT_MS)
            .is_some()
    }

    /// 补查线程处理完一条队列消息：推进「已回到等待」代际（测试用）。
    #[cfg_attr(not(test), allow(dead_code))]
    fn announce_name_lookup_idle(&self) {
        #[cfg(test)]
        {
            let _gate = self
                .inner
                .name_lookup_idle_gate
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.inner
                .name_lookup_idle_epoch
                .fetch_add(1, Ordering::AcqRel);
            self.inner.name_lookup_idle.notify_all();
        }
    }

    /// 该 UID 当前是否仍处于补查冷却（含失败冷却）。
    ///
    /// 给测试做**条件等待**用：等到冷却真的结束，而不是靠固定睡眠猜时长。
    pub(crate) fn name_lookup_cooling_down(&self, uid: &str) -> bool {
        let uid = uid.trim();
        let attempts_now = monotonic_millis();
        self.name_lookups()
            .next_attempt_ms
            .get(uid)
            .is_some_and(|next| *next > attempts_now)
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
            let attempts_now = monotonic_millis();
            lookups.next_attempt_ms.insert(
                uid.to_string(),
                cooldown_from_now(NAME_LOOKUP_RETRY_MS, attempts_now),
            );
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

    /// 「补查线程回到等待」的当前代际（只给测试用：见 [`Self::wait_for_name_lookup_idle`]）。
    #[cfg(test)]
    pub(crate) fn name_lookup_idle_epoch(&self) -> u64 {
        self.inner.name_lookup_idle_epoch.load(Ordering::Acquire)
    }

    /// 等到补查线程处理完当前这一段，返回处理后的新代际；超时返回 None。
    ///
    /// 补查线程是串行的：队列里排着好几个 UID 时，前一条的结果与下一条的申请之间
    /// 有窗口。测试必须等「这一段真的处理完」才能断言结果，否则会偶发读到中间态。
    /// 代际与条件下变量都只在 `#[cfg(test)]` 下存在，生产路径没有任何额外开销。
    #[cfg(test)]
    pub(crate) fn wait_for_name_lookup_idle(
        &self,
        observed_epoch: u64,
        timeout: Duration,
    ) -> Option<u64> {
        let gate = self
            .inner
            .name_lookup_idle_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (gate, _) = self
            .inner
            .name_lookup_idle
            .wait_timeout_while(gate, timeout, |()| {
                self.name_lookup_idle_epoch() == observed_epoch
            })
            .unwrap_or_else(PoisonError::into_inner);
        drop(gate);
        let epoch = self.name_lookup_idle_epoch();
        (epoch != observed_epoch).then_some(epoch)
    }

    fn name_lookups(&self) -> MutexGuard<'_, NameLookups> {
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

/// 外部聊天源四条后台线程共用的停止信号。
///
/// 为什么不是一个裸 `AtomicBool` 加轮询：这些线程的等待周期以**分钟**计
/// （昵称预扫与好友快照都是 60 秒一次）。只置标志的话，`join` 要等到下一次
/// 轮询才会返回，进程退出会卡住一分钟以上，放弃等待又会留下孤儿线程。
/// 因此这里用「条件变量」：置标志后立刻唤醒所有等待者，等待方毫秒级返回。
///
/// 标志位本身是原子量而不是锁里的字段：WS 读循环要在**每次短读之间**无成本地问
/// 一次「该退了吗」，走锁就会把一次原子读放大成一次上下文切换。
#[derive(Clone)]
pub(crate) struct ExternalChatShutdown {
    inner: Arc<ShutdownInner>,
}

struct ShutdownInner {
    /// 快速判据，WS 读循环与各线程的循环顶部都读它。
    ///
    /// 单独用 `Arc` 包一层是为了把只读句柄交给 WS 层：那边只需要「是否该退」，
    /// 不该拿到条件变量，也就不该持有整个 `ShutdownInner`。
    requested: Arc<AtomicBool>,
    /// 与 `requested` 配对的条件变量。锁只保护「等待」这件事本身，
    /// 不参与标志判定，所以 `request` 之后不存在「读到旧值」的窗口。
    ///
    /// 同一个条件变量也承担「停止请求已发出」的通知：需要等到某一时刻的观察者
    /// （测试就是这种观察者）注册后与等待者排队，避免固定睡眠去猜时序。
    woke: Mutex<()>,
    changed: Condvar,
    /// 正在 `changed` 上等待的线程数（测试用），用来区分「被唤醒」与「请求先到」。
    #[cfg(test)]
    waiters: AtomicUsize,
}

impl Default for ExternalChatShutdown {
    fn default() -> Self {
        Self {
            inner: Arc::new(ShutdownInner {
                requested: Arc::new(AtomicBool::new(false)),
                woke: Mutex::new(()),
                changed: Condvar::new(),
                #[cfg(test)]
                waiters: AtomicUsize::new(0),
            }),
        }
    }
}

impl ShutdownInner {
    /// 等一次「停止请求」或「等待者数量变化」，或超时。
    ///
    /// 参数化 `done` 让 [`ExternalChatShutdown::request`] 之外的两类观察
    /// （停止等待、测试里的就位/唤醒观察）共用同一段等待逻辑。
    fn wait_on_changed<'a>(
        &'a self,
        gate: MutexGuard<'a, ()>,
        timeout: Duration,
        done: impl Fn() -> bool,
    ) -> MutexGuard<'a, ()> {
        self.changed
            .wait_timeout_while(gate, timeout, |()| !done())
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    /// 让等待者数量增减一次，并在**持锁**时通知观察者。
    ///
    /// 持锁改计数、持锁通知是标准用法：观察者若在「读计数」与「进入等待」之间
    /// 错过了这次变化，它下一次进入等待时仍会重新读计数，不会永久睡死。
    #[cfg(test)]
    fn shift_waiters(&self, delta: isize) {
        if delta > 0 {
            self.waiters.fetch_add(1, Ordering::AcqRel);
        } else {
            self.waiters.fetch_sub(1, Ordering::AcqRel);
        }
        self.changed.notify_all();
    }
}

impl ExternalChatShutdown {
    fn gate(&self) -> MutexGuard<'_, ()> {
        // 锁毒化不影响停止语义：继续用，否则一次无关 panic 会让关停路径本身 panic。
        self.inner
            .woke
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 是否已经请求停止。
    pub(crate) fn is_requested(&self) -> bool {
        self.inner.requested.load(Ordering::Acquire)
    }

    /// 停止标志的只读句柄，供 WS 读等待在分片之间检查。
    ///
    /// 只共享这一个原子量，不动条件变量：WS 层只需要「是否该退」，不需要等待能力。
    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        self.inner.requested.clone()
    }

    /// 请求停止并唤醒**所有**正在等待的线程。可重复调用，幂等。
    pub(crate) fn request(&self) {
        // 先持锁再置标志：等待者必须在「检查标志 → 进入等待」之间不可能错过通知，
        // 否则会出现置标志后它才睡下去、一觉睡到超时的丢通知窗口。
        let _gate = self.gate();
        if self.inner.requested.swap(true, Ordering::AcqRel) {
            return;
        }
        self.inner.changed.notify_all();
    }

    /// 当前在条件变量上等待的线程数（测试用：用来观察「被真的唤醒」）。
    #[cfg(test)]
    pub(crate) fn waiters(&self) -> usize {
        self.inner.waiters.load(Ordering::Acquire)
    }

    /// 条件等待到至少有一个等待者就位；超时返回 false。
    #[cfg(test)]
    pub(crate) fn wait_for_waiter(&self, timeout: Duration) -> bool {
        self.wait_for_waiter_count(timeout, |count| count > 0)
    }

    /// 条件等待到没有等待者；超时返回 false。
    #[cfg(test)]
    pub(crate) fn wait_for_no_waiter(&self, timeout: Duration) -> bool {
        self.wait_for_waiter_count(timeout, |count| count == 0)
    }

    /// 在停止条件变量上等一个等待者计数的条件成立。
    ///
    /// `request` 与等待者进出都以同一个条件变量通知，所以这里不会错过任何一次变化；
    /// 计数本身只是乐观判据，最终结论仍由外层断言给出。
    #[cfg(test)]
    fn wait_for_waiter_count(&self, timeout: Duration, done: impl Fn(usize) -> bool) -> bool {
        let started = Instant::now();
        let mut gate = self.gate();
        loop {
            if done(self.waiters()) {
                return true;
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return false;
            }
            gate = self
                .inner
                .wait_on_changed(gate, remaining, || done(self.waiters()));
        }
    }

    /// 等到停止请求已经发出；`request` 从未到来则等满 `timeout` 后返回 false。
    ///
    /// 与 [`Self::wait`] 的区别：`wait` 回答「此刻是否已停止」，这里回答
    /// 「从现在起是否等到了一次停止请求」。测试用它等「关停信号已经出膛」，
    /// 而不是睡一段时间猜它出膛了——两者在慢机器上的结论完全不同。
    ///
    /// 观察者同样持锁、同样在这个条件变量上排队，所以不会错过一次在等待开始前
    /// 刚发出的请求，也不会漏掉一次比别的等待者晚很多才发出的请求。
    pub(crate) fn wait_for_request(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        let mut gate = self.gate();
        while !self.is_requested() {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return false;
            }
            gate = self
                .inner
                .wait_on_changed(gate, remaining, || self.is_requested());
        }
        drop(gate);
        true
    }

    /// 最多等 `timeout`，返回「醒来时是否已请求停止」。
    ///
    /// 已经请求过停止时**立刻**返回 `true`，不进入等待；否则睡到超时或被 `request`
    /// 唤醒为止。调用方在返回 `true` 后应立即退出本次循环——这就是全部退出判据。
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let started = Instant::now();
        let gate = self.gate();
        if self.is_requested() {
            return true;
        }
        #[cfg(test)]
        self.inner.shift_waiters(1);
        let mut gate = self
            .inner
            .wait_on_changed(gate, timeout, || self.is_requested());
        // 条件变量允许虚假唤醒：醒来后仍要重新核对超时与停止标志，
        // 不能一醒就下结论（否则一次无关的唤醒会让线程误以为「该退了」）。
        while !self.is_requested() {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                break;
            }
            gate = self
                .inner
                .wait_on_changed(gate, remaining, || self.is_requested());
        }
        #[cfg(test)]
        self.inner.shift_waiters(-1);
        drop(gate);
        self.is_requested()
    }
}

/// 一条已命名、可等待的后台线程。
///
/// 只存 [`JoinHandle`] 的话关停日志只能写线程编号；把名字一起存下来，
/// 日志与线程列表里的名字才能对得上。
struct NamedWorker {
    name: &'static str,
    handle: JoinHandle<()>,
}

/// 线程进入长等待前的「已就位」握手（生产为空操作）。
#[cfg(not(test))]
fn announce_worker_ready(_name: &'static str) {}

/// 线程进入长等待前的「已就位」握手：由测试启动的线程向监听方发一次信号。
#[cfg(test)]
fn announce_worker_ready(name: &'static str) {
    if let Some(sender) = test_worker_ready::take(name) {
        let _ = sender.send(());
    }
}

/// 测试侧的「线程已进入长等待」登记表。
///
/// 键是（线程名, 槽位）：同名线程可能被多个用例同时启动，只按线程名登记会让一个
/// 用例的监听被另一个用例的线程取走。每个用例登记时独占一个槽位，用完归还，
/// 因此并行执行也互不串味。
#[cfg(test)]
mod test_worker_ready {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock, mpsc};

    type Key = (&'static str, usize);

    fn table() -> &'static Mutex<HashMap<Key, mpsc::Sender<()>>> {
        static TABLE: OnceLock<Mutex<HashMap<Key, mpsc::Sender<()>>>> = OnceLock::new();
        TABLE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// 登记一次监听，返回接收端与下个空闲槽位。
    pub(super) fn watch(name: &'static str) -> (mpsc::Receiver<()>, usize) {
        let (sender, receiver) = mpsc::channel();
        let mut table = table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut slot = 0;
        while table.contains_key(&(name, slot)) {
            slot += 1;
        }
        table.insert((name, slot), sender);
        (receiver, slot)
    }

    /// 线程侧取走自己的信号端（一次性）。
    pub(super) fn take(name: &'static str) -> Option<mpsc::Sender<()>> {
        let mut table = table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = (0..=64).find(|slot| table.contains_key(&(name, *slot)))?;
        table.remove(&(name, slot))
    }

    /// 用例结束归还槽位；没被线程取走的监听一并清理。
    pub(super) fn release(name: &'static str, slot: usize) {
        table()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(name, slot));
    }
}

/// 一个用例对某条线程「已就位」的独占监听，Drop 时归还槽位。
#[cfg(test)]
struct TestWorkerReadySlot {
    name: &'static str,
    slot: usize,
    listener: mpsc::Receiver<()>,
}

#[cfg(test)]
impl TestWorkerReadySlot {
    fn watch(name: &'static str) -> Self {
        let (listener, slot) = test_worker_ready::watch(name);
        Self {
            name,
            slot,
            listener,
        }
    }

    /// 等到线程报告就位；超时直接失败并指明是哪条线程。
    fn wait(&self, timeout: Duration) {
        self.listener
            .recv_timeout(timeout)
            .unwrap_or_else(|_| panic!("线程「{}」未在 {timeout:?} 内进入等待", self.name));
    }
}

#[cfg(test)]
impl Drop for TestWorkerReadySlot {
    fn drop(&mut self) {
        test_worker_ready::release(self.name, self.slot);
    }
}

/// 外部聊天源全部后台线程的句柄与停止信号。
///
/// 所有权的约定：**谁拿到 `ExternalChatWorkers`，谁负责在合适的时机 `shutdown()`**。
/// 没有显式关停时 [`Drop`] 会兜底发一次停止信号，保证线程不会永久空转。
pub(crate) struct ExternalChatWorkers {
    shutdown: ExternalChatShutdown,
    /// 全部已启动线程，按启动顺序排列：补查类线程在前，主连接线程在最后。
    workers: Vec<NamedWorker>,
    /// 关停与 Drop 都可能走到这里，用「已取走」标记避免重复 join（重复 join 会 panic）。
    closed: bool,
}

impl Default for ExternalChatWorkers {
    fn default() -> Self {
        Self::empty()
    }
}

impl ExternalChatWorkers {
    /// 未启用外部聊天源时使用：没有线程、没有等待者，`shutdown()` 是空操作。
    pub(crate) fn empty() -> Self {
        Self {
            shutdown: ExternalChatShutdown::default(),
            workers: Vec::new(),
            closed: false,
        }
    }

    /// 测试用：拿到内部停止信号，以便直接等「停止请求已发出」。
    #[cfg(test)]
    pub(crate) fn shutdown_token(&self) -> ExternalChatShutdown {
        self.shutdown.clone()
    }

    /// 由四条线程的停止信号与句柄组装。
    fn new(shutdown: ExternalChatShutdown, workers: Vec<NamedWorker>) -> Self {
        Self {
            shutdown,
            workers,
            closed: false,
        }
    }

    /// 已启动的后台线程数（`empty()` 为 0）。
    pub(crate) fn started(&self) -> usize {
        self.workers.len()
    }

    /// 只发出停止信号：置标志并唤醒所有等待者，**不阻塞**调用方。
    ///
    /// 供「先让它们开始退，再去做别的收尾」的场景使用；真正等待退出用 [`Self::shutdown`]。
    pub(crate) fn request_shutdown(&self) {
        self.shutdown.request();
    }

    /// 停止并等待全部后台线程退出。
    ///
    /// 顺序固定为「先全部发信号，再逐个 join」：
    ///
    /// 1. `request()` 让四条线程同时开始退出，不会出现「等 A 的时候 B 还占着锁」；
    /// 2. 主连接线程排在最后——它可能正卡在一次 HTTP 健康检查或 WS 读分片上，
    ///    其余线程等待期间它已经在自行退出，整体关停耗时取最长的那一条。
    ///
    /// 主连接线程的两类阻塞等待都设了上界：WS 读等待按 100 毫秒分片检查停止标志
    /// （见 [`crate::runtime::chat_source::ws`]），单次健康检查/补拉的 HTTP 超时
    /// 分别是 2 秒与 10 秒；实测已连接空闲期与重连退避期的关停都在亚秒级。
    ///
    /// 线程 panic 不会让关停中断：载荷转成可读中文日志后继续 join 剩余线程。
    pub(crate) fn shutdown(mut self) {
        self.shutdown.request();
        for worker in std::mem::take(&mut self.workers) {
            match worker.handle.join() {
                Ok(()) => log::debug!("外部聊天源线程「{}」已停止", worker.name),
                Err(payload) => log::error!(
                    "外部聊天源线程「{}」意外终止（步骤：关停外部聊天源线程），其余线程仍会继续等待退出，该线程负责的推送在本次运行内不再恢复：{}",
                    worker.name,
                    crate::runtime::ui::panic_payload_text(payload.as_ref())
                ),
            }
        }
        self.closed = true;
    }
}

impl Drop for ExternalChatWorkers {
    /// 兜底：没人显式关停时至少把停止信号发出去。
    ///
    /// 这里**不** join：`Drop` 可能发生在不合适的时机（例如持锁路径上），
    /// 阻塞等待会把问题放大。发信号已经足够——每条线程的等待都是可打断的有界等待，
    /// 醒来自会退出，不会永久空转。
    fn drop(&mut self) {
        if !self.closed {
            self.shutdown.request();
            if !self.workers.is_empty() {
                log::warn!(
                    "外部聊天源线程句柄未显式关停就被释放（共 {} 条），已自动发出停止信号让它们退出；线程退出结果无法再回收，若某些线程长期不消失说明它们的等待不可打断",
                    self.workers.len()
                );
            }
        }
    }
}

/// 启动外部聊天源线程，返回共享状态与控制句柄。调用方把状态交给观察线程，把句柄留在关停路径上。
///
/// 主连接线程是**必需**的：它起不来就直接返回错误。三条补查线程是尽力而为的
/// 辅助线程，起不来只降级（日志里说明缺什么），不影响外部聊天源本身工作。
pub(crate) fn spawn_managed(
    timeline: ChatTimeline,
    config: ChatSourceConfig,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    config_store: Option<crate::config::SharedConfigStore>,
    live_configs: crate::config::LiveConfigs,
) -> Result<(ExternalChatState, ExternalChatWorkers)> {
    let state = ExternalChatState::default();
    if !config.enabled {
        return Ok((state, ExternalChatWorkers::empty()));
    }
    let shutdown = ExternalChatShutdown::default();
    let http_base = ws_url_to_http(&config.url);
    let mut workers = Vec::new();
    // 昵称补查线程：它的发送端要交给另外两条线程排队，句柄要进 workers。
    let resolver = spawn_name_resolver(
        state.clone(),
        &shutdown,
        &http_base,
        config.token.clone(),
        identity_access.clone(),
        config_store.clone(),
        live_configs.clone(),
    );
    // 消息驱动的补查只覆盖"说过话的人"。再开一个扫描线程按身份映射表主动补，
    // 尽量把从不说话的好友的真实昵称也拿到。
    //
    // 补查线程没起来时拿不到队列，spawn_name_priming 直接返回 None：
    // 没有消费方还去扫表，只会把账号标成"在途"再也没人补查。
    let primed_names = resolver.as_ref().map(|resolver| resolver.names.clone());
    if let Some(worker) = spawn_name_priming(
        state.clone(),
        &shutdown,
        identity_access.clone(),
        primed_names.clone(),
    ) {
        workers.push(worker);
    }
    if let Some(resolver) = resolver {
        workers.push(resolver.worker);
    }
    if let Some(worker) = spawn_friends_snapshot(
        state.clone(),
        &shutdown,
        http_base.clone(),
        config.token.clone(),
        identity_access.clone(),
        config_store.clone(),
        live_configs.clone(),
    ) {
        workers.push(worker);
    }
    // 主连接线程也要一个发送端：收到缺昵称的消息由它排队。
    // 三个发送端共享同一条队列，任一存在队列就不会断开——关停靠的是停止标志
    // 与 200 毫秒的轮询上限，不依赖发送端被释放。
    let main_names = primed_names;
    let main_shutdown = shutdown.clone();
    let main_state = state.clone();
    let handle = thread::Builder::new()
        .name("external-chat".into())
        .spawn(move || {
            run(
                &main_shutdown,
                &config,
                &live_configs,
                &http_base,
                MainWorkerContext {
                    timeline,
                    state: main_state,
                    config_store,
                    identity_access,
                    names: main_names,
                },
            );
            announce_worker_ready("external-chat");
        })
        .map_err(|error| anyhow::anyhow!("启动外部聊天线程失败: {error}"))?;
    workers.push(NamedWorker {
        name: "external-chat",
        handle,
    });
    Ok((state, ExternalChatWorkers::new(shutdown, workers)))
}

/// 消息消费线程一次投递所需的全部依赖。
///
/// 原来这些参数逐个穿过 `run` → `deliver` 两层，参数表长到需要 `too_many_arguments`；
/// 收进一个结构体后，新增依赖只改这里。
struct DeliveryContext<'a> {
    timeline: &'a ChatTimeline,
    state: &'a ExternalChatState,
    live_configs: &'a crate::config::LiveConfigs,
    source: &'a str,
    self_uid: u32,
    identity_access: Option<&'a crate::features::identity::IdentityAccess>,
    config_store: Option<&'a crate::config::SharedConfigStore>,
    /// 昵称补查队列；补查线程没起来时为 None，此时缺昵称的消息按未知发送者处理。
    names: Option<&'a std::sync::mpsc::Sender<String>>,
}

/// 主连接线程的依赖整体移交：线程闭包只捕获这一份，参数表不再随功能增长。
struct MainWorkerContext {
    timeline: ChatTimeline,
    state: ExternalChatState,
    config_store: Option<crate::config::SharedConfigStore>,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    names: Option<std::sync::mpsc::Sender<String>>,
}

/// 主连接线程：自动拉起 ChatBridge → 建立 WS → 消费事件 → 断线重连。
///
/// 每个等待点都必须能被停止信号打断，因此这里**没有**裸 `thread::sleep`：
/// 重连退避用 [`ExternalChatShutdown::wait`]；已连上时由 WS 读等待的分片
/// （见 [`crate::runtime::chat_source::ws`]）把控制权交回循环顶部，停止信号一到
/// 下一次分片检查就返回。剩下两类阻塞「整段」调用是 HTTP 健康检查与断线补拉，
/// 各有 2 秒 / 10 秒超时，无法中途打断，因此排在最长的关停路径上。
fn run(
    shutdown: &ExternalChatShutdown,
    config: &ChatSourceConfig,
    live_configs: &crate::config::LiveConfigs,
    http_base: &str,
    context: MainWorkerContext,
) {
    let MainWorkerContext {
        timeline,
        state,
        config_store,
        identity_access,
        names,
    } = context;
    let delivery = DeliveryContext {
        timeline: &timeline,
        state: &state,
        live_configs,
        source: &config.url,
        self_uid: config.self_uid,
        identity_access: identity_access.as_ref(),
        config_store: config_store.as_ref(),
        names: names.as_ref(),
    };
    while !shutdown.is_requested() {
        // 登录器角色：Miliastra-Wonderland-ChatBridge 没在监听时替用户拉起来。
        // 探测很便宜（300ms 超时），每次重连都试一次，这样用户中途关掉也能被重新拉起。
        if config.auto_start {
            // target_process 用于退出码 3 的「等游戏退出」判定；配置可能热更新，
            // 每轮都读一次最新快照。
            let target_process = live_configs.snapshot().window.target_process.clone();
            if let Err(error) =
                crate::runtime::chat_source::launcher::ensure_running(config, &target_process)
            {
                log::warn!("无法自动启动 Miliastra-Wonderland-ChatBridge，等待人工处理：{error:#}");
            }
        }
        let mut source = match WebSocketChatSource::new(config.url.clone(), config.token.clone()) {
            Ok(mut source) => {
                // 已连接但长时间无消息时，WS 的读等待必须能被停止信号打断，
                // 否则 `join` 要等满一次读超时（3 秒）甚至更久。
                source.set_interrupt(shutdown.flag());
                source
            }
            Err(error) => {
                let reconnect = reconnect_interval(live_configs);
                log::error!(
                    "外部聊天源初始化失败（步骤：建立 WebSocket 连接），{} 毫秒后重试；重试期间外部聊天源没有接管，聊天仍由 OCR 识别，外部源独有的消息（含备注名）拿不到：{error:#}",
                    reconnect.as_millis()
                );
                announce_worker_ready("external-chat");
                if shutdown.wait(reconnect) {
                    break;
                }
                continue;
            }
        };
        let mut last_event_at = Instant::now();
        let mut last_health_at = Instant::now();
        loop {
            // 每个等待点之后都先问一句「该退出了吗」：停止信号要在下一个阻塞调用之前生效。
            if shutdown.is_requested() {
                break;
            }
            // 租约超时：最近一次真实消息/健康检查太久，必须回落到 OCR，
            // 否则聊天完全失聪。阈值每轮读 live 快照，热更新后下一次判定生效。
            let lease = Duration::from_millis(super::live_chat_source_lease_ms(live_configs));
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
                match fetch_health(http_base, &config.token) {
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
                            handle_connected(
                                &delivery,
                                http_base,
                                config,
                                &state,
                                shutdown,
                                last_sequence,
                                boot_id,
                            );
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
                            deliver(&delivery, messages);
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
                    let reconnect = reconnect_interval(live_configs);
                    log::warn!(
                        "外部聊天源读取失败，{}ms 后重连：{error:#}",
                        reconnect.as_millis()
                    );
                    state.set_active(false);
                    // 重连前重置：旧墓碑与会话号都不再适用于新连接。
                    state.bump_epoch();
                    reset_after_source_change(&timeline);
                    // 退避等待同样可被停止信号打断：等待本身就是「还能不能再连」的判据，
                    // 返回 true 时外层循环顶部会立刻收尾，不会再多建一条连接。
                    announce_worker_ready("external-chat");
                    let _ = shutdown.wait(reconnect);
                    break;
                }
            }
        }
        // 内层循环退出有两种原因：断线要重连，或者收到了停止信号。
        // 后者必须跳出外层循环，否则会立刻又建一条新连接。
        if shutdown.is_requested() {
            break;
        }
    }
    // 停止接管并恢复 OCR：线程退出后没人再刷新租约，若仍停留在「已接管」，
    // 观察线程会一直跳过 OCR 聊天识别，聊天静默失聪。
    state.set_active(false);
    log::debug!("外部聊天源线程已停止，接管状态已清除，聊天识别交回 OCR");
}

/// 重连间隔：每轮从 live 快照取，热更新后下一次重连即生效。
fn reconnect_interval(live_configs: &crate::config::LiveConfigs) -> Duration {
    Duration::from_millis(super::live_chat_source_reconnect_ms(live_configs))
}

/// 处理一次握手：判水位、核 gameConnected、必要时补拉断线期间的消息。
///
/// 抽成独立函数只是为了让 [`run`] 的读循环一眼能看清结构；语义与原来内联时完全一致。
fn handle_connected(
    delivery: &DeliveryContext<'_>,
    http_base: &str,
    config: &ChatSourceConfig,
    state: &ExternalChatState,
    shutdown: &ExternalChatShutdown,
    last_sequence: u64,
    boot_id: Option<u64>,
) {
    let timeline = delivery.timeline;
    // H1：宿主重启后消息序号从 1 重新开始。先判断并重置
    // 旧水位，否则新消息会被「序号 ≤ 旧水位」当成启动前历史
    // 整批丢弃，而 active=true 又关掉 OCR —— 永久静默失聪。
    // 握手成功本身就是"游戏回来了"的信号：解除闲置退出的
    // 抑制，不必等到完全接管（桥可能还没确认 gameConnected）。
    if state.restart_suppressed() {
        state.lift_restart_suppression();
        log::info!("外部聊天源重新握手成功，解除闲置退出留下的「不自动重启游戏」抑制");
    }
    let handshake = state.apply_handshake(last_sequence, boot_id);
    if handshake == HandshakeAction::HostRestarted {
        state.bump_epoch();
        reset_after_source_change(timeline);
        log::warn!("外部聊天源检测到宿主重启，已重置水位与聊天基线；后续消息序号从 1 重新计数");
    }
    // 停止信号已发出就没必要再握手接管：这次握手一旦失败，调用方会以为断线要重连。
    if shutdown.is_requested() {
        return;
    }
    // H2：WS 握手只说明连接建立，不代表游戏已接入。
    // reattach 等待期里 WS 能正常连上却没有任何游戏消息，
    // 此时必须保持 OCR，而不是凭 is_connected 就接管。
    let attached = match fetch_health(http_base, &config.token) {
        Ok(health) if health.game_connected => {
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
    if !attached || skip_backlog || last_sequence <= local || shutdown.is_requested() {
        return;
    }
    match crate::runtime::chat_source::fetch_messages_since(http_base, &config.token, local) {
        Ok(batch) => {
            if batch.truncated {
                // 缓冲不够，中间可能有缺口：重建基线而不是继续叠加。
                log::warn!("断线补拉出现缺口（truncated），重建聊天基线");
                reset_after_source_change(timeline);
            }
            if !batch.messages.is_empty() {
                log::info!(
                    "外部聊天源断线后补拉到 {} 条历史消息，将逐条投递给业务处理",
                    batch.messages.len()
                );
                deliver(delivery, batch.messages);
            }
        }
        Err(error) => log::warn!("断线补拉失败，继续接收新消息：{error:#}"),
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
fn deliver(delivery: &DeliveryContext<'_>, messages: Vec<ExternalChatMessage>) {
    let DeliveryContext {
        timeline,
        state,
        live_configs,
        source,
        self_uid,
        identity_access,
        config_store,
        names,
    } = *delivery;
    // 本机账号 UID：私聊 peer 必须排除它，否则玩家私聊 BOT 时
    // peer 会变成 BOT 自己，按身份归属的功能（个人歌单等）全部失效。
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
        // 备注由 `/friends` 权威同步；这里只补真实昵称。
        if let Some(uid) = message.sender.uid.as_ref() {
            let incomplete = identity_access
                .as_ref()
                .and_then(|identity| {
                    identity
                        .resolve_account(miliastra_kernel::chat_source::GAME_GENSHIN, uid.as_str())
                })
                .is_none_or(|resolved| resolved.real_nickname.trim().is_empty());
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
        // 补查线程没起来或已退出：解除在途标记，避免该 UID 永久不再补查。
        // 队列不存在时两种情况一样——把在途标记退回去即可，消息照常投递。
        let queued = names
            .as_ref()
            .is_some_and(|names| names.send(uid.clone()).is_ok());
        if !queued {
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
/// remark 是游戏内备注，可手填/新建占位，空值也正常，不再拿它判断“不完整”；
/// 游戏备注由 `/friends` 每 60 秒权威同步。这里只补真实昵称。
fn needs_greedy_name_refresh(mapping: &crate::features::identity::IdentityMapping) -> bool {
    mapping.real_nickname.trim().is_empty()
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
///   * 所有 Genshin 账号都缺席，或全部出现且都 remark=null → 清 remark；
///   * remark 现在只表示真实游戏备注，清理不区分来源标记。
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
        let all_present_without_remark = absent == 0 && present > 0 && all_present_explicit_null;
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
///
/// 没有身份句柄时这份快照无处可写，因此不起线程。等待间隔走
/// [`ExternalChatShutdown::wait`]：停止信号一到，60 秒的等待立刻返回。
fn spawn_friends_snapshot(
    state: ExternalChatState,
    shutdown: &ExternalChatShutdown,
    http_base: String,
    token: String,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    config_store: Option<crate::config::SharedConfigStore>,
    live_configs: crate::config::LiveConfigs,
) -> Option<NamedWorker> {
    let identity = identity_access?;
    let worker_shutdown = shutdown.clone();
    let handle = thread::Builder::new()
        .name("external-chat-friends".into())
        .spawn(move || loop {
            if worker_shutdown.is_requested() {
                break;
            }
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
            announce_worker_ready("external-chat-friends");
            if worker_shutdown.wait(FRIENDS_SNAPSHOT_INTERVAL) {
                break;
            }
        });
    match handle {
        Ok(handle) => Some(NamedWorker {
            name: "external-chat-friends",
            handle,
        }),
        Err(error) => {
            log::warn!("启动好友快照线程失败，本次运行继续用 /names 兜底：{error}");
            None
        }
    }
}

/// 按身份映射表定期扫描，把还没拿到名字的账号排进补查队列。
///
/// 消息驱动的补查只在有人说话时发生，从不说话的好友永远拿不到真实昵称；
/// 这里主动扫表：缺真实昵称或备注串的用短冷却反复重试，齐全的按长 TTL 刷新。
///
/// 队列本身兼作存活判据：补查线程退出后发送会失败，本线程随即结束——
/// 否则它扫出来的账号只会停在"在途"，此后永远不再补查。
fn spawn_name_priming(
    state: ExternalChatState,
    shutdown: &ExternalChatShutdown,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    names: Option<std::sync::mpsc::Sender<String>>,
) -> Option<NamedWorker> {
    let identity_access = identity_access?;
    let names = names?;
    let worker_shutdown = shutdown.clone();
    let handle = thread::Builder::new()
        .name("external-chat-names-prime".into())
        .spawn(move || {
            loop {
                if worker_shutdown.is_requested() {
                    break;
                }
                // 完整好友快照 60 秒内已覆盖名字信息：跳过逐 UID 轮询，避免双频请求。
                if state.friends_snapshot_recent() {
                    announce_worker_ready("external-chat-names-prime");
                    if worker_shutdown.wait(NAME_PRIME_INTERVAL) {
                        break;
                    }
                    continue;
                }
                // 先扫一遍再睡：启动后立刻补，不用等一个间隔。
                let config = identity_access.snapshot();
                let mut queued = 0usize;
                for mapping in &config.mappings {
                    if worker_shutdown.is_requested() {
                        return;
                    }
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
                announce_worker_ready("external-chat-names-prime");
                if worker_shutdown.wait(NAME_PRIME_INTERVAL) {
                    break;
                }
            }
        });
    match handle {
        Ok(handle) => Some(NamedWorker {
            name: "external-chat-names-prime",
            handle,
        }),
        Err(error) => {
            log::warn!("启动昵称主动补查线程失败，从不说话的好友将拿不到真实昵称：{error}");
            None
        }
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
        changed |=
            identity.record_real_nickname(miliastra_kernel::chat_source::GAME_GENSHIN, uid, name);
    }
    // `/names` 只写不删：remark 为空/缺失都不清缓存，删除只由 `/friends complete` 做。
    if let Some(remark) = names.remark.as_deref() {
        changed |=
            identity.set_account_remark(miliastra_kernel::chat_source::GAME_GENSHIN, uid, remark);
    }
    changed
}

/// 后台昵称补查线程的句柄与投递队列发送端。
///
/// 两者必须一起交出去：队列用来排队，句柄用来在关停时等待线程真正退出。
struct NameResolver {
    names: std::sync::mpsc::Sender<String>,
    worker: NamedWorker,
}

/// 等下一个待补查 UID；停止请求到达时立即返回 `None`。
///
/// 不能用 `for uid in receiver`：那会在队列空时无限阻塞，停止信号来了也醒不过来，
/// `join` 只能等到有下一条消息——而没有新消息正是关停时的常态。
/// 这里把「排队」和「停止」合成一个等待：队列来消息就处理，停止信号一到就返回，
/// 不再靠 200 毫秒轮询上限去猜。空闲时被唤醒的代价只是一次重新检查。
fn recv_name_until_stopped(
    receiver: &std::sync::mpsc::Receiver<String>,
    shutdown: &ExternalChatShutdown,
) -> Option<String> {
    loop {
        if shutdown.is_requested() {
            return None;
        }
        if let Ok(uid) = receiver.try_recv() {
            return Some(uid);
        }
        if shutdown.wait(NAME_RESOLVER_POLL_INTERVAL) {
            return None;
        }
    }
}
fn spawn_name_resolver(
    state: ExternalChatState,
    shutdown: &ExternalChatShutdown,
    http_base: &str,
    token: String,
    identity_access: Option<crate::features::identity::IdentityAccess>,
    config_store: Option<crate::config::SharedConfigStore>,
    live_configs: crate::config::LiveConfigs,
) -> Option<NameResolver> {
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    let worker_shutdown = shutdown.clone();
    let http_base = http_base.to_string();
    let spawned = thread::Builder::new()
        .name("external-chat-names".into())
        .spawn(move || {
            loop {
                announce_worker_ready("external-chat-names");
                match recv_name_until_stopped(&receiver, &worker_shutdown) {
                    Some(uid) => {
                        resolve_one_name(
                            &state,
                            identity_access.as_ref(),
                            config_store.as_ref(),
                            &live_configs,
                            &http_base,
                            &token,
                            &uid,
                        );
                        state.announce_name_lookup_idle();
                    }
                    // 停止请求或全部发送端已释放：不会再有人排队，本线程任务完成。
                    None => break,
                }
            }
        });
    match spawned {
        Ok(handle) => Some(NameResolver {
            names: sender,
            worker: NamedWorker {
                name: "external-chat-names",
                handle,
            },
        }),
        Err(error) => {
            log::warn!("启动昵称补查线程失败，缺昵称的消息将按未知发送者处理：{error}");
            None
        }
    }
}

/// 查一个 UID 的昵称与备注，并写回缓存与身份映射。
fn resolve_one_name(
    state: &ExternalChatState,
    identity_access: Option<&crate::features::identity::IdentityAccess>,
    config_store: Option<&crate::config::SharedConfigStore>,
    live_configs: &crate::config::LiveConfigs,
    http_base: &str,
    token: &str,
    uid: &str,
) {
    match crate::runtime::chat_source::resolve_sender_names(http_base, token, uid) {
        Ok(names) => {
            state.finish_name_lookup(uid, names.name.as_deref(), names.remark.as_deref());
            // 备注也要进 UID 缓存：names 为 null、remark 有值是常见组合，
            // 这时只缓存昵称的话，按备注名反查 UID（歌单归属）会落空。
            state.remember_uid(names.name.as_deref(), Some(uid));
            state.remember_uid(names.remark.as_deref(), Some(uid));
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
            // 不打印 UID / 备注 / 昵称：三者都是真实玩家数据。
            if let Some(identity) = identity_access {
                let changed = apply_sender_names(identity, uid, &names);
                if changed
                    && let Some(store) = config_store
                    && let Err(error) = persist_identity(store, live_configs, identity)
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
            state.finish_name_lookup(uid, None, None);
            log::debug!("昵称补查失败：{error:#}");
        }
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
    use futures_util::{SinkExt as _, StreamExt as _};

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

    /// 失联判定的三条前提必须是「连上过 + 现在断了 + 没被闲置退出抑制」。
    #[test]
    fn lost_for_requires_a_previous_connection_and_respects_suppression() {
        let state = ExternalChatState::default();
        // 从没连上过：不算失联（桥没起来，重启游戏没有意义）。
        assert!(state.lost_for().is_none());

        state.set_active(true);
        assert!(state.lost_for().is_none(), "在线时不算失联");

        state.set_active(false);
        assert!(
            state.lost_for().is_some(),
            "连上过又断开，必须能报出失联时长"
        );

        // 闲置退出主动关游戏：抑制后不再报告失联，避免把游戏重新拉起来。
        state.suppress_restart_after_intentional_close();
        assert!(state.lost_for().is_none(), "主动关游戏后不得再判失联");
        assert!(state.restart_suppressed());

        // 重新连上要自动解除抑制。
        state.set_active(true);
        assert!(!state.restart_suppressed());
    }

    /// 失联判定只在「连上过 + 断开满阈值」时成立；从没连上过一律不成立。
    ///
    /// 这条守的是「桥没起来」与「注入后链路断了」的区分：前者重启游戏无效。
    #[test]
    fn lost_threshold_only_applies_after_a_real_connection() {
        use std::time::Duration;

        let threshold = Duration::from_millis(30_000);
        let state = ExternalChatState::default();
        // 从未连上：即使长时间没有任何消息，也不该被判定为"需要重启游戏"。
        assert!(state.lost_for().is_none_or(|lost| lost < threshold));

        // 真的接管过一次（/health 确认游戏接入）：立刻置上"连过"。
        state.set_active(true);
        assert!(state.lost_for().is_none(), "在线时不算失联");

        // 断线（健康检查显示游戏断开）：立刻进入失联计时，但要**满阈值**才升级。
        state.set_active(false);
        let lost = state.lost_for().expect("连过又断开必须能报出失联");
        assert!(lost < threshold, "刚断开时还不该超过阈值：{lost:?}");
    }

    /// 抑制必须在"新一次握手"时就能解除，而不是等完全接管。
    ///
    /// 否则一次闲置退出可能把「失联就重启」永久关死：玩家手动重开游戏后，
    /// 若桥没接上（DLL 未注入 / 桥没运行），`set_active(true)` 永远不执行，
    /// 抑制就一直在。
    #[test]
    fn suppression_can_be_lifted_by_a_handshake_without_full_takeover() {
        let state = ExternalChatState::default();
        state.set_active(true); // 曾经连上过
        state.set_active(false); // 链路断了，记下失联起点
        state.suppress_restart_after_intentional_close(); // 闲置退出关游戏：抑制 + 清起点
        assert!(state.restart_suppressed());
        assert!(state.lost_for().is_none(), "抑制期间不判失联");

        // 只是握手成功，还没接管（set_active 未被调用）。
        state.lift_restart_suppression();
        assert!(!state.restart_suppressed(), "握手后必须解除抑制");
        // 解除后若确实仍处于失联，必须能重新报告——否则抑制会把
        // "失联就重启"永久关死（起点被清零且后续没人再写）。
        assert!(
            state.lost_for().is_some(),
            "解除抑制后应重新判得失联（起点要在解除时补回来）"
        );
    }

    /// 失联期间重新接管要清掉失联起点，否则下一次判定会拿旧起点立刻触发。
    #[test]
    fn reconnect_clears_the_loss_timer() {
        let state = ExternalChatState::default();
        state.set_active(true);
        state.set_active(false);
        assert!(state.lost_for().is_some());
        state.set_active(true);
        assert!(state.lost_for().is_none(), "重新接管后不该继续报失联");
        assert!(!state.restart_suppressed(), "重新接管要解除闲置退出的抑制");
    }

    /// 反复 set_active(false) 不能把失联起点一直往后推。
    ///
    /// 判据是「起点没被重写」，不需要真的走一段墙钟时间：直接读上报的起点，
    /// 再插一次 `set_active(false)`，起点必须一模一样。时间只作为兜底——若实测
    /// 确实过了 1 毫秒，失联时长自然也该跟着涨。
    #[test]
    fn repeated_inactive_updates_keep_the_first_loss_timestamp() {
        let state = ExternalChatState::default();
        state.set_active(true);
        state.set_active(false);
        let first_since = state.loss_started_ms().expect("失联起点");
        let first = state.lost_for().expect("失联");

        state.set_active(false);
        assert_eq!(
            state.loss_started_ms(),
            Some(first_since),
            "重复的 set_active(false) 不得把失联起点往后推"
        );
        let second = state.lost_for().expect("失联");
        assert!(
            second >= first,
            "失联时长不能倒退：first={first:?} second={second:?}"
        );
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
    fn greedy_refresh_only_needs_real_nickname() {
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
        // remark 可手填/占位，空值也正常；只有真实昵称缺失才需要贪婪补查。
        assert!(!needs_greedy_name_refresh(&mapping(
            "abcd1234",
            "真名甲",
            true
        )));
        assert!(!needs_greedy_name_refresh(&mapping("", "真名甲", true)));
        assert!(!needs_greedy_name_refresh(&mapping(
            "abcd1234",
            "真名甲",
            false
        )));
        assert!(needs_greedy_name_refresh(&mapping("abcd1234", "", true)));
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
            .find(|mapping| {
                mapping
                    .accounts
                    .iter()
                    .any(|account| account.uid == "10001")
            })
            .unwrap();
        assert_eq!(first.real_nickname, "新真名甲");
        assert_eq!(first.remark, "新备注甲");
        let absent = mappings
            .iter()
            .find(|mapping| {
                mapping
                    .accounts
                    .iter()
                    .any(|account| account.uid == "10002")
            })
            .unwrap();
        assert!(absent.remark.is_empty(), "complete 快照缺席者清备注");
        assert_eq!(absent.real_nickname, "旧真名乙", "保留真实昵称/身份");
        assert!(
            !identity
                .ai_name_pairs()
                .iter()
                .any(|pair| pair.left() == "旧备注乙")
        );

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
    fn friends_snapshot_clears_by_identity_uuid_and_scope() {
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
        assert_eq!(stats.cleared_absent, 3, "全 null、缺席的手填与 owner 都清");
        let mappings = identity.snapshot().mappings;
        let multi = mappings.iter().find(|m| m.id == ids[multi]).unwrap();
        assert_eq!(multi.remark, "新缓存甲", "一个账号有备注就保留");
        let null_only = mappings.iter().find(|m| m.id == ids[null_only]).unwrap();
        assert!(null_only.remark.is_empty(), "全部出现且都 null 才清");
        assert_eq!(
            mappings
                .iter()
                .find(|m| m.id == ids[other_game])
                .unwrap()
                .remark,
            "旧缓存丙",
            "其他游戏账号跳过"
        );
        assert_eq!(
            mappings
                .iter()
                .find(|m| m.id == ids[no_accounts])
                .unwrap()
                .remark,
            "旧缓存丁",
            "无账号映射跳过"
        );
        assert!(
            mappings
                .iter()
                .find(|m| m.id == ids[handfilled])
                .unwrap()
                .remark
                .is_empty(),
            "手填备注已不是合法 remark，缺席时清空"
        );
        assert!(
            mappings
                .iter()
                .find(|m| m.id == ids[owner])
                .unwrap()
                .remark
                .is_empty(),
            "Owner 缺席也由完整快照清空"
        );
        assert_eq!(
            mappings
                .iter()
                .find(|m| m.id == ids[missing_remark_field])
                .unwrap()
                .remark,
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
        // 「写者已尝试取锁」由屏障给出：各方都到齐后写者才开始抢锁，
        // 不再靠睡一小会儿猜它已经跑起来了。
        let attempted = Arc::new(std::sync::Barrier::new(2));
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_attempted = Arc::clone(&attempted);
        let worker = thread::spawn(move || {
            worker_attempted.wait();
            let _guard = identity_persist_guard();
            let _ = sender.send(());
        });
        attempted.wait();
        // 此刻写者已经发起取锁，而锁在测试手上：它只可能停在等锁上。
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
    fn complete_null_clears_handfilled_remark_too() {
        use crate::features::identity::{
            GameAccount, IdentityAccess, IdentityConfig, IdentityMapping, IdentityRole,
        };
        use crate::runtime::chat_source::{FriendEntry, FriendSnapshot};
        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            remark: "手填备注".to_string(),
            id: uuid::Uuid::new_v4(),
            role: IdentityRole::Friend,
            display_name: String::new(),
            real_nickname: "真名".to_string(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount::new("50001")],
        });
        let identity = IdentityAccess::new(config);
        let snapshot = FriendSnapshot {
            complete: true,
            updated_at: Some(1),
            friends: vec![FriendEntry {
                uid: 50001,
                name: None,
                remark: None,
                remark_present: true,
            }],
        };
        let stats = apply_friend_snapshot(&identity, &snapshot);
        assert!(stats.changed);
        assert_eq!(stats.cleared_absent, 1);
        let mapping = &identity.snapshot().mappings[0];
        assert!(mapping.remark.is_empty());
        assert!(!mapping.remark_from_chatbridge);
    }

    #[test]
    fn complete_snapshot_overwrites_and_clears_owner_admin_remarks() {
        use crate::features::identity::{
            GameAccount, IdentityAccess, IdentityConfig, IdentityMapping, IdentityRole,
        };
        use crate::runtime::chat_source::{FriendEntry, FriendSnapshot};
        let mut config = IdentityConfig::default();
        for (uid, role, remark) in [
            ("10000001", IdentityRole::Owner, "示例备注A"),
            ("10000002", IdentityRole::Admin, "示例备注B"),
            ("10000003", IdentityRole::Admin, "示例备注C"),
            ("10001", IdentityRole::Friend, "好友旧备注"),
        ] {
            config.mappings.push(IdentityMapping {
                remark: remark.to_string(),
                id: uuid::Uuid::new_v4(),
                role,
                display_name: String::new(),
                real_nickname: String::new(),
                remark_from_chatbridge: true,
                accounts: vec![GameAccount::new(uid)],
            });
        }
        let identity = IdentityAccess::new(config);
        let snapshot = FriendSnapshot {
            complete: true,
            updated_at: Some(1),
            friends: vec![
                FriendEntry {
                    uid: 10000001,
                    name: None,
                    remark: Some("实际备注".to_string()),
                    remark_present: true,
                },
                FriendEntry {
                    uid: 10000002,
                    name: None,
                    remark: None,
                    remark_present: true,
                },
                FriendEntry {
                    uid: 10001,
                    name: None,
                    remark: None,
                    remark_present: true,
                },
            ],
        };
        let stats = apply_friend_snapshot(&identity, &snapshot);
        assert_eq!(stats.matched, 3);
        assert_eq!(
            stats.cleared_absent, 3,
            "admin null、缺席 admin、friend null 清空"
        );
        let mappings = identity.snapshot().mappings;
        let by_uid = |uid: &str| {
            mappings
                .iter()
                .find(|mapping| mapping.accounts.iter().any(|account| account.uid == uid))
                .unwrap()
        };
        assert_eq!(by_uid("10000001").remark, "实际备注");
        assert!(by_uid("10000002").remark.is_empty());
        assert!(by_uid("10000003").remark.is_empty(), "缺席 admin 清空");
        assert!(by_uid("10001").remark.is_empty());
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
    // ---------------------------------------------------------------------
    // 停止合同：四条后台线程必须都能被毫秒级唤醒并干净 join。
    //
    // 这些用例共用一条判据：**墙钟时间上界**。调度器抖动在毫秒级，而上界都设在
    // 数百毫秒到 1 秒，既不会因机器慢而偶发失败，也足以区分「被信号唤醒」与
    // 「睡满 60 秒周期」这两种实现。
    //
    // 「线程是否已经进入等待」不再靠睡眠猜：工厂函数在长等待前自查一次
    // [`test_worker_ready`] 登记表，测试据此拿到确定信号（见下面的 `worker_ready`）。
    // ---------------------------------------------------------------------

    /// 等待上界：比 60 秒周期小两个数量级，慢机器也留足余量。
    const PROMPT_BOUND: Duration = Duration::from_secs(1);

    /// 单个停止等待的上界：验证「被唤醒」而不是「恰好到点」。
    const WAKE_BOUND: Duration = Duration::from_millis(100);

    /// 登记对某条后台线程「已进入长等待」的独占监听。
    ///
    /// 必须在启动线程**之前**登记；返回的槽位在 Drop 时归还，因此并行执行的
    /// 用例即使启动同名线程也各拿各的信号。
    fn worker_ready(name: &'static str) -> super::TestWorkerReadySlot {
        super::TestWorkerReadySlot::watch(name)
    }

    /// 等一条线程的就位信号（最多 `PROMPT_BOUND`）。
    fn wait_ready(listener: &super::TestWorkerReadySlot) {
        listener.wait(PROMPT_BOUND);
    }

    /// 等一个条件成立，最多 `PROMPT_BOUND`；超时直接失败。
    ///
    /// 替代「睡一小会儿再看一眼」的固定睡眠：条件一成立就返回，慢机器上也不会
    /// 因调度抖动判错。轮询间隔只影响响应速度，不决定结论。
    fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + PROMPT_BOUND;
        while !condition() {
            assert!(Instant::now() < deadline, "{what}（等待超时）");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn shutdown_token_wakes_wait_timeout_immediately() {
        let shutdown = ExternalChatShutdown::default();
        assert!(!shutdown.is_requested());

        // 后台线程按 60 秒等待；停止请求必须让它立刻返回，而不是睡满周期。
        //
        // 时序用**等待者计数**钉死，不靠固定睡眠：只有在确认等待者真的已经在
        // 条件变量上之后才请求。条件等待带硬超时（1 秒 ≪ 60 秒），因此
        // 「计数永不到达」或「唤醒没生效」都会在 1 秒内失败而不是挂死。
        let waiter = shutdown.clone();
        let handle = thread::spawn(move || {
            let started = Instant::now();
            let stopped = waiter.wait(Duration::from_secs(60));
            (stopped, started.elapsed())
        });
        let t0 = Instant::now();
        let arrived = shutdown.wait_for_waiter(WAKE_BOUND);
        eprintln!("DIAG wait_for_waiter={:?} arrived={arrived}", t0.elapsed());
        assert!(
            arrived,
            "等待线程必须先在条件变量上就位，否则测到的只是「请求先于等待」的快路径"
        );
        let requested_at = Instant::now();
        shutdown.request();
        assert!(
            shutdown.wait_for_no_waiter(WAKE_BOUND),
            "请求发出后等待者必须已被唤醒（而不是睡满 60 秒）"
        );
        let (stopped, elapsed) = handle.join().expect("等待线程不应崩溃");
        assert!(stopped, "request 之后 wait 必须报告已停止");
        assert!(
            elapsed < WAKE_BOUND,
            "60 秒等待必须被 request 立刻唤醒，实际等待了 {elapsed:?}（请求后 {:?}）",
            requested_at.elapsed()
        );
        assert!(shutdown.is_requested());
    }

    /// 已经请求停止后再次 wait 必须立即返回，不再睡一个周期。
    #[test]
    fn shutdown_token_wait_returns_immediately_when_already_requested() {
        let shutdown = ExternalChatShutdown::default();
        shutdown.request();
        let started = Instant::now();
        assert!(shutdown.wait(Duration::from_secs(60)));
        assert!(
            started.elapsed() < Duration::from_millis(20),
            "已停止后的等待不能真的睡：{:?}",
            started.elapsed()
        );
    }

    /// 没有停止请求时，wait 必须在超时后如实返回 false，不能谎报停止。
    ///
    /// 这条守的是「超时」与「已停止」不能混为一谈：把超时当停止会让线程
    /// 在第一轮就退出，外部聊天源直接不工作。
    #[test]
    fn shutdown_token_wait_times_out_without_request() {
        let shutdown = ExternalChatShutdown::default();
        let started = Instant::now();
        assert!(!shutdown.wait(Duration::from_millis(60)));
        assert!(!shutdown.is_requested());
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "未请求停止时必须真的等到超时：{:?}",
            started.elapsed()
        );
    }

    /// `empty()` 不启动任何线程，关停是空操作且可重复安全调用。
    #[test]
    fn empty_workers_shutdown_is_a_noop() {
        let workers = ExternalChatWorkers::empty();
        assert_eq!(workers.started(), 0);
        workers.request_shutdown();
        workers.shutdown();

        // Drop 兜底也要能在空集合上安全执行。
        let workers = ExternalChatWorkers::empty();
        drop(workers);
    }

    /// 三条定期线程：启动后由 shutdown 在亚秒内全部唤醒并 join 成功。
    ///
    /// 用真实的 60 秒周期配置，所以断言成立只可能来自停止信号本身。
    #[test]
    fn name_priming_and_friends_snapshot_threads_exit_and_join_promptly_on_shutdown() {
        let shutdown = ExternalChatShutdown::default();
        let state = ExternalChatState::default();
        let identity = crate::features::identity::IdentityAccess::new(
            identity_config_with_account("700000001"),
        );
        let mut workers = Vec::new();
        let readiness = [
            worker_ready("external-chat-names"),
            worker_ready("external-chat-names-prime"),
            worker_ready("external-chat-friends"),
        ];

        // 预扫线程：配置里有一个账号，正常情况下会先扫一轮再等 60 秒。
        let resolver = spawn_name_resolver(
            state.clone(),
            &shutdown,
            // 指向必然连不上的本地端口：补查请求立刻失败，不拖慢用例。
            "http://127.0.0.1:9",
            String::new(),
            Some(identity.clone()),
            None,
            crate::config::LiveConfigs::default(),
        );
        let names = resolver.as_ref().map(|resolver| resolver.names.clone());
        if let Some(worker) =
            spawn_name_priming(state.clone(), &shutdown, Some(identity.clone()), names)
        {
            workers.push(worker);
        }
        if let Some(resolver) = resolver {
            workers.push(resolver.worker);
        }
        // 好友快照线程：HTTP 立刻失败，随后等 60 秒。
        if let Some(worker) = spawn_friends_snapshot(
            state.clone(),
            &shutdown,
            "http://127.0.0.1:9".to_string(),
            String::new(),
            Some(identity),
            None,
            crate::config::LiveConfigs::default(),
        ) {
            workers.push(worker);
        }
        let workers = ExternalChatWorkers::new(shutdown.clone(), workers);
        assert_eq!(
            workers.started(),
            3,
            "三条定期线程都应启动（补查、预扫、好友快照）"
        );

        // 等三条线程各自报告「已进入 60 秒等待」：这是下一轮判断的起点，不再靠睡眠猜。
        for listener in &readiness {
            wait_ready(listener);
        }
        assert!(!shutdown.is_requested());

        let started = Instant::now();
        workers.shutdown();
        let elapsed = started.elapsed();
        assert!(
            elapsed < PROMPT_BOUND,
            "三条定期线程必须在停止信号后亚秒级退出并 join，实际耗时 {elapsed:?}（周期是 60 秒）"
        );
        assert!(shutdown.is_requested(), "shutdown 必须发出停止信号");
    }

    /// `request_shutdown` 只发信号不阻塞：调用方可以先让它退，再去做别的收尾。
    #[test]
    fn request_shutdown_does_not_block_on_thread_join() {
        let shutdown = ExternalChatShutdown::default();
        let state = ExternalChatState::default();
        let identity = crate::features::identity::IdentityAccess::new(
            identity_config_with_account("700000002"),
        );
        let mut workers = Vec::new();
        let ready = worker_ready("external-chat-friends");
        if let Some(worker) = spawn_friends_snapshot(
            state,
            &shutdown,
            "http://127.0.0.1:9".to_string(),
            String::new(),
            Some(identity),
            None,
            crate::config::LiveConfigs::default(),
        ) {
            workers.push(worker);
        }
        let workers = ExternalChatWorkers::new(shutdown.clone(), workers);
        // 线程已进入 60 秒等待：此时请求停止才谈得上「不阻塞」。
        wait_ready(&ready);

        let started = Instant::now();
        workers.request_shutdown();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "request_shutdown 不能等待线程退出：{:?}",
            elapsed
        );
        assert!(shutdown.is_requested());
        // 随后的 shutdown 完成真正的等待。
        let started = Instant::now();
        workers.shutdown();
        assert!(started.elapsed() < PROMPT_BOUND);
    }

    /// 未显式 shutdown 时，Drop 兜底必须发出停止信号，不留孤儿线程。
    #[test]
    fn dropping_workers_requests_shutdown_as_a_fallback() {
        let shutdown = ExternalChatShutdown::default();
        let state = ExternalChatState::default();
        let identity = crate::features::identity::IdentityAccess::new(
            identity_config_with_account("700000003"),
        );
        let probe = shutdown.clone();
        let workers = spawn_friends_snapshot(
            state,
            &shutdown,
            "http://127.0.0.1:9".to_string(),
            String::new(),
            Some(identity),
            None,
            crate::config::LiveConfigs::default(),
        )
        .map(|worker| ExternalChatWorkers::new(probe, vec![worker]))
        .expect("好友快照线程应启动");

        drop(workers);
        assert!(
            shutdown.is_requested(),
            "Drop 必须兜底发出停止信号，否则线程会继续空转"
        );
    }

    /// 主连接线程：端点连不上（重连退避期）也要能被 shutdown 立刻打断并 join。
    #[test]
    fn spawn_managed_with_unreachable_endpoint_exits_and_joins_promptly() {
        let started = Instant::now();
        let ready = worker_ready("external-chat");
        let (state, workers) = spawn_managed(
            ChatTimeline::new(),
            unreachable_chat_source_config(),
            None,
            None,
            crate::config::LiveConfigs::default(),
        )
        .expect("主连接线程应能启动");
        assert!(!state.is_active(), "连不上时不得处于接管状态");
        // 没有身份句柄时预扫与好友快照线程不起（它们要写身份映射），
        // 但昵称补查仍会跑：它只往缓存里写，不依赖身份映射。
        assert_eq!(
            workers.started(),
            2,
            "未提供身份句柄时只起补查线程与主连接线程"
        );

        // 等主线程真正进入「连接失败 → 退避等待」：退避间隔配成 30 秒。
        wait_ready(&ready);
        let shutdown_started = Instant::now();
        workers.shutdown();
        let elapsed = shutdown_started.elapsed();
        assert!(
            elapsed < PROMPT_BOUND,
            "重连退避中的主连接线程必须被立刻唤醒，实际耗时 {elapsed:?}（退避是 30 秒）"
        );
        assert!(
            !state.is_active(),
            "线程退出后必须清掉接管状态，否则 OCR 会被永久跳过"
        );
        assert!(started.elapsed() < PROMPT_BOUND * 3);
    }

    /// 已连接但长时间没有消息：WS 读等待必须被停止信号打断。
    ///
    /// 用本地 mock 端点完成一次真实的 WS 握手，然后**不再发任何消息**，
    /// 模拟「连接正常但没有聊天」。此时主线程停在读等待上：只有分片检查
    /// 停止标志，才能让 join 保持亚秒级而不是等满一次读超时。
    #[test]
    fn connected_idle_websocket_read_is_interrupted_by_shutdown() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("本地地址").port();
        // 握手完成的信号：由 mock 服务端在 accept_async 返回后发出，
        // 测试在接收端等它——「客户端确实连上了」是事件，不是靠睡眠猜出来的。
        let (handshake_tx, handshake_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("创建 mock 服务端 runtime");
            runtime.block_on(async move {
                listener.set_nonblocking(true).expect("设为非阻塞");
                let listener = tokio::net::TcpListener::from_std(listener).expect("接管监听");
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                // 用同一个 tokio-tungstenite 完成握手，不在测试里手搓协议细节。
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let _ = handshake_tx.send(());
                // 握手后保持静默，直到客户端关闭（测试收尾时）。
                while socket.next().await.is_some() {}
            });
        });

        let (state, workers) = spawn_managed(
            ChatTimeline::new(),
            idle_websocket_chat_source_config(port),
            None,
            None,
            crate::config::LiveConfigs::default(),
        )
        .expect("主连接线程应能启动");
        // 同前：没有身份句柄时是补查线程 + 主连接线程。
        assert_eq!(workers.started(), 2);

        handshake_rx
            .recv_timeout(PROMPT_BOUND)
            .unwrap_or_else(|_| panic!("mock 端点必须在 {PROMPT_BOUND:?} 内完成 WS 握手"));

        let shutdown_started = Instant::now();
        workers.shutdown();
        let elapsed = shutdown_started.elapsed();
        assert!(
            elapsed < PROMPT_BOUND,
            "已连接空闲期的读等待必须被停止信号打断，实际耗时 {elapsed:?}"
        );
        assert!(!state.is_active(), "线程退出后必须清掉接管状态");
        let _ = server.join();
    }

    /// 分片读等待不能吞消息：空闲若干分片后到达的消息必须照常投递。
    ///
    /// 这条守的是把 3 秒整段等待改成 100 毫秒分片之后最容易引入的回归——
    /// 若每次分片超时都重建读取状态，空闲后的第一条消息会被丢掉。
    /// 判据用「消息序号是否推进」，因为那正是业务侧判断消息有没有进来的依据。
    #[test]
    fn messages_arriving_after_idle_slices_are_still_delivered() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().expect("本地地址").port();
        let (handshake_tx, handshake_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("创建 mock 服务端 runtime");
            runtime.block_on(async move {
                listener.set_nonblocking(true).expect("设为非阻塞");
                let listener = tokio::net::TcpListener::from_std(listener).expect("接管监听");
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let _ = handshake_tx.send(());
                // 先静默一阵：客户端会在同一段读等待里跨过多个 100 毫秒分片。
                // 这里睡的是 mock **服务端**的定时器：它定义的是「消息何时到达」这个
                // 真实业务时序（客户端跨分片空闲），不是测试在等某个条件成立，
                // 因此保留——条件等待改的只是测试主线程侧的固定睡眠。
                tokio::time::sleep(Duration::from_millis(350)).await;
                // 用公开占位值构造一条大厅消息，不含任何真实玩家数据。
                let frame = r#"{"type":"message","message":{"id":42,"ts":1,"channel":"current_hall","hallId":1,"hallType":16,"from":{"uid":900000001,"name":"示例玩家"},"to":null,"text":"@状态"}}"#;
                if socket
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        frame.into(),
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
                while socket.next().await.is_some() {}
            });
        });

        let (state, workers) = spawn_managed(
            ChatTimeline::new(),
            idle_websocket_chat_source_config(port),
            None,
            None,
            crate::config::LiveConfigs::default(),
        )
        .expect("主连接线程应能启动");

        handshake_rx
            .recv_timeout(PROMPT_BOUND)
            .expect("mock 端点必须完成握手");

        // 消息在静默期之后才发：期间读等待跨了多个分片。条件一成立（序号推进到
        // 42）立刻返回，不再固定睡 10ms 反复试；超时判失败并给出实际序号。
        wait_until(
            || state.last_sequence() == 42,
            "空闲多个分片之后到达的消息必须照常投递",
        );

        workers.shutdown();
        let _ = server.join();
    }

    /// 主连接线程起不来时，`spawn_managed` 必须返回错误而不是静默成功。
    #[test]
    fn spawn_managed_propagates_thread_start_failure() {
        // 这条路径只在系统资源耗尽时才真正失败，无法在单测里稳定复现；
        // 这里只钉住「未启用时返回空句柄而不是错误」这一条契约。
        let config = ChatSourceConfig {
            enabled: false,
            ..ChatSourceConfig::default()
        };
        let (state, workers) = spawn_managed(
            ChatTimeline::new(),
            config,
            None,
            None,
            crate::config::LiveConfigs::default(),
        )
        .expect("未启用外部源不是错误");
        assert!(!state.is_active());
        assert_eq!(workers.started(), 0, "未启用时不得起任何线程");
        workers.shutdown();
    }

    /// 一份指向必然连不上的端点的外部聊天源配置。
    ///
    /// 端口 9（discard）在测试机上不会有监听者，连接会立刻被拒绝；
    /// 退避间隔配成 30 秒，用来证明 join 不等它。
    fn unreachable_chat_source_config() -> ChatSourceConfig {
        ChatSourceConfig {
            enabled: true,
            url: "ws://127.0.0.1:9/stream".to_string(),
            reconnect_ms: 30_000,
            lease_timeout_ms: 10_000,
            auto_start: false,
            ..ChatSourceConfig::default()
        }
    }

    /// 指向本地 mock 端点的配置；退避同样配成 30 秒，断开后不会自己重连太频繁。
    fn idle_websocket_chat_source_config(port: u16) -> ChatSourceConfig {
        ChatSourceConfig {
            enabled: true,
            url: format!("ws://127.0.0.1:{port}/stream"),
            token: "test-token".to_string(),
            reconnect_ms: 30_000,
            lease_timeout_ms: 10_000,
            auto_start: false,
            ..ChatSourceConfig::default()
        }
    }

    fn identity_config_with_account(uid: &str) -> crate::features::identity::IdentityConfig {
        let mut config = crate::features::identity::IdentityConfig::default();
        config
            .mappings
            .push(crate::features::identity::IdentityMapping {
                remark: "示例备注".to_string(),
                id: uuid::Uuid::new_v4(),
                role: crate::features::identity::IdentityRole::Friend,
                display_name: String::new(),
                real_nickname: String::new(),
                remark_from_chatbridge: true,
                accounts: vec![crate::features::identity::GameAccount::new(uid)],
            });
        config
    }
}
