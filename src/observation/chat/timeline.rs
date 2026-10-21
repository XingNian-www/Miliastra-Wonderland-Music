//! Stateful message continuity, revision handling, delivery and command deduplication.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use super::freshness::{BaselineKind, FreshFrame, Freshness, SequenceFreshness};
pub(crate) use super::scan::RecognizedChatMessage;
use super::{BubbleSequence, ChatIdentity, ObservedChatMessageId, VisualSessionId};
use crate::geometry::Rect;

const RETAINED_CAPACITY: usize = 4096;
/// 每个聊天最多记住多少条"已认领并派发成功"的命令文本。
const COMMAND_LOCK_CAPACITY: usize = 5;
/// 命令锁最多保留多少个聊天桶；Friend(OCR 原始名) 抖动会不断产生新桶，必须设上界。
const COMMAND_LOCK_CHAT_CAPACITY: usize = 32;
/// 一行高（marker 约 19px、行距约 35px），8px 挡不住重建后整体下移。
const COMMAND_LOCK_POSITION_TOLERANCE_PX: i32 = 28;
/// 同一"文本 + y 容差"连续多少个可信帧都判定缺失，才真正释放锁条目（抗单帧 OCR 抖动）。
const COMMAND_LOCK_MISSING_FRAMES_BEFORE_RELEASE: u8 = 2;

#[derive(Clone, Debug)]
pub(crate) struct ChatTimelineMessage {
    pub(crate) id: ObservedChatMessageId,
    pub(crate) text: String,
    pub(crate) sender: Option<String>,
    pub(crate) raw_sender: Option<String>,
    pub(crate) message_type: String,
    pub(crate) block: Rect,
    /// Still eligible for business delivery, not merely absent from the last frame.
    pub(crate) is_new: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ChatTimelineSnapshot {
    pub(crate) visual_session: VisualSessionId,
    pub(crate) captured_at: Instant,
    pub(crate) chat: ChatIdentity,
    pub(crate) messages: Vec<ChatTimelineMessage>,
}

#[derive(Clone)]
pub(crate) struct ChatTimeline {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

/// 已见消息的交付状态，用于细分“未判为新增”的排查原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BusinessDeliveryState {
    Pending,
    Claimed,
    IgnoredRevision,
    Fenced,
    NotTracked,
}

struct State {
    latest: Option<ChatTimelineSnapshot>,
    trackers: HashMap<ChatIdentity, Tracker>,
    business_pending: HashSet<ObservedChatMessageId>,
    unread_baseline: HashSet<ObservedChatMessageId>,
    business_claimed: HashSet<ObservedChatMessageId>,
    /// 已经打过“命令未执行原因”日志的消息 id：同一实例只打一次，避免每帧刷屏。
    not_executed_logged: HashSet<ObservedChatMessageId>,
    /// 聊天区曾不存在（或首屏为空）：说明“首屏没有命令”，冷基线应被消费。
    cold_baseline_consumed: bool,
    active_business_claims: HashSet<ObservedChatMessageId>,
    ignored_revisions: HashMap<ObservedChatMessageId, CapturedBusinessRevision>,
    latest_business_revisions: HashMap<ObservedChatMessageId, CapturedBusinessRevision>,
    revision_delivery_fences: HashMap<ObservedChatMessageId, Instant>,
    command_locks: HashMap<ChatIdentity, CommandLock>,
    /// 命令新老判定状态：按 ChatIdentity 存活，跨 Tracker 重建（视图消失）保留。
    chat_freshness: HashMap<ChatIdentity, SequenceFreshness>,
    /// 下一个可信帧要携带的基线事件（由 reset_chat_observation_baseline 写入）。
    pending_baseline: Option<BaselineKind>,
    next_command_lock_activity: u64,
    unread_delivery_fences: HashMap<ObservedChatMessageId, Instant>,
    friend_suppression_generation: u64,
    friend_suppressions: usize,
    friend_suppression_ended: Option<Instant>,
    subscribers: HashMap<u64, Subscription>,
    next_subscriber: u64,
    next_session: u64,
    requests: BTreeMap<u64, Refresh>,
    next_request: u64,
}

struct Tracker {
    session: VisualSessionId,
    next_sequence: u64,
    initialized: bool,
    visible: Vec<ChatTimelineMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BusinessMessageRevision {
    text: String,
    sender: Option<String>,
    raw_sender: Option<String>,
}

impl BusinessMessageRevision {
    fn new(text: &str, sender: Option<&str>, raw_sender: Option<&str>) -> Self {
        let identity = |value: Option<&str>| {
            value
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        Self {
            text: text.trim().to_owned(),
            sender: identity(sender),
            raw_sender: identity(raw_sender),
        }
    }

    fn from_message(message: &ChatTimelineMessage) -> Self {
        Self::new(
            &message.text,
            message.sender.as_deref(),
            message.raw_sender.as_deref(),
        )
    }

    fn corrects(&self, ignored: &Self) -> bool {
        !self.text.is_empty() && self != ignored
    }
}

#[derive(Clone)]
struct CapturedBusinessRevision {
    captured_at: Instant,
    revision: BusinessMessageRevision,
    y: Option<i32>,
}

/// 每个聊天一份的命令锁：记录最近"已认领并派发成功"的命令文本与屏幕位置。
/// 键是归一化文本（跨 visual_session 稳定，不含 bubble_sequence），因此视图重建后
/// 同一条命令能重新被认出；条目只按"当前屏幕是否还存在"释放，没有时间窗口。
#[derive(Clone, Debug, Default)]
struct CommandLock {
    entries: VecDeque<CommandLockEntry>,
    /// 最近一次“被当前视图触碰”的单调序号：写入或 publish 匹配/释放时刷新。
    /// 用于桶淘汰时区分“仍在用”的桶与 OCR 名字抖动留下的一次性桶。
    last_activity: u64,
}

#[derive(Clone, Debug)]
struct CommandLockEntry {
    text: String,
    y: i32,
    /// 连续多少个可信帧判定该条目已不在屏上；任一次命中即清零。
    missing_frames: u8,
}

impl CommandLockEntry {
    /// v3 锚点命中：归一化文本一致，且当前位置不晚于记录位置 + 一行高容差。
    fn matches(&self, normalized: &str, y: i32) -> bool {
        self.text == normalized && y <= self.y + COMMAND_LOCK_POSITION_TOLERANCE_PX
    }
}

impl CommandLock {
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// anchor_matched(msg)：锁中存在条目满足 normalize(entry.text) == normalize(msg.text)
    /// 且 msg.block.y <= entry.y + TOLERANCE（一行高）。
    fn matches(&self, message: &ChatTimelineMessage) -> bool {
        self.matches_text_at(&message.text, message.block.y)
    }

    fn matches_text_at(&self, text: &str, y: i32) -> bool {
        let text = normalize_command_lock_text(text);
        self.entries.iter().any(|entry| entry.matches(&text, y))
    }

    /// 同一条文本再次写入时只更新位置并复位缺失计数，不重复追加；超出容量丢弃最旧。
    fn record(&mut self, text: String, y: i32) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.text == text) {
            entry.y = y;
            entry.missing_frames = 0;
            return;
        }
        while self.entries.len() >= COMMAND_LOCK_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(CommandLockEntry {
            text,
            y,
            missing_frames: 0,
        });
    }

    /// 释放：按 (归一化文本 + y 容差) 判定"该条目还在屏上"；只有连续
    /// COMMAND_LOCK_MISSING_FRAMES_BEFORE_RELEASE 个可信帧都判定缺失才真正移除，
    /// 单帧 OCR 抖动（例如末字误读）不会误删条目。
    /// 空帧不构成"屏幕上没有"的证据，沿用 Tracker::observe 对空帧的既有语义。
    fn release_missing(&mut self, observed: &[ChatTimelineMessage]) {
        if observed.is_empty() {
            return;
        }
        let screen = observed
            .iter()
            .map(|message| (normalize_command_lock_text(&message.text), message.block.y))
            .collect::<Vec<_>>();
        for entry in &mut self.entries {
            if screen.iter().any(|(text, y)| entry.matches(text, *y)) {
                entry.missing_frames = 0;
            } else {
                entry.missing_frames = entry.missing_frames.saturating_add(1);
            }
        }
        self.entries
            .retain(|entry| entry.missing_frames < COMMAND_LOCK_MISSING_FRAMES_BEFORE_RELEASE);
    }
}

struct Subscription {
    chat: Option<ChatIdentity>,
    session: Option<VisualSessionId>,
    queue: VecDeque<ChatTimelineSnapshot>,
    failure: Option<String>,
    durable_business: bool,
}

/// 同一个大厅可能以两个身份出现：`PrimaryHall`（主聊天区 marker 规则判定）与
/// `SecondaryHall`（二级面板几何判定）。一级大厅下发消息必然经过一次二级面板，
/// 因此对绑定在大厅上的读者，另一块面板的快照不是“换了会话”，而是同一物理会话的
/// 另一视角：跳过即可——不入队、不清队列、不失败，让读者继续等它自己身份的消息。
///
/// 任务绑定期间出现的其他身份同样只被忽略：它们不能提供本任务输入，也不应仅因
/// 一个任务内状态停留较久就终止窗口；调用方的整体期限负责结束等待。
pub(crate) fn other_hall_panel(bound: &ChatIdentity, incoming: &ChatIdentity) -> bool {
    matches!(
        (bound, incoming),
        (ChatIdentity::PrimaryHall, ChatIdentity::SecondaryHall)
            | (ChatIdentity::SecondaryHall, ChatIdentity::PrimaryHall)
    )
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct NoChatRefreshError(String);

#[derive(Clone)]
enum RefreshFailure {
    NoChat(String),
    Failure(String),
}

impl RefreshFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::NoChat(reason) => NoChatRefreshError(reason).into(),
            Self::Failure(reason) => anyhow!(reason),
        }
    }
}

struct Refresh {
    requested_at: Instant,
    outcome: Option<std::result::Result<Option<ChatTimelineSnapshot>, RefreshFailure>>,
}

/// Each handle owns its queue; dropping it never changes another subscriber.
pub(crate) struct ChatTimelineSubscriber {
    shared: ChatTimeline,
    id: u64,
}

/// A business claim is committed only after dispatch succeeds. On failure it
/// becomes eligible again unless a discard guard superseded the claim.
pub(crate) struct SharedBusinessClaim {
    shared: ChatTimeline,
    id: ObservedChatMessageId,
    suppression_generation: u64,
    /// 被派发消息自身的文本与屏幕位置，认领时就捕获，绝不回头查当前 tracker
    /// （执行完成时气泡可能已经滚出 visible/latest）。
    text: String,
    y: i32,
    captured_at: Instant,
    committed: bool,
}

impl SharedBusinessClaim {
    /// 作为命令派发成功的提交点：确认已处理，并把该消息自身的文本与 y 写进命令锁。
    pub(crate) fn complete_as_command(self) -> Result<()> {
        self.commit(true)
    }

    /// 主动丢弃、被禁用或已处理的非命令永久消费，**绝不写命令锁**。
    /// 缺陷 1：二级/好友路径若在这里也写锁，普通闲聊会把真命令条目 FIFO 挤出，
    /// 重建后仍在屏上的已执行命令会重跑。
    pub(crate) fn complete_ignored(self) -> Result<()> {
        self.commit(false)
    }

    /// 未路由或空正文只忽略被认领的修订；有效正文/身份修订可再次交付。
    /// 身份参数必须来自被消费的快照，不能回查可能已经更新的 tracker。
    pub(crate) fn complete_unrecognized(
        mut self,
        sender: Option<&str>,
        raw_sender: Option<&str>,
    ) -> Result<()> {
        {
            let mut state = self.shared.lock()?;
            if matches!(self.id.chat, ChatIdentity::Friend(_))
                && state.friend_suppression_generation != self.suppression_generation
            {
                acknowledge_business_locked(&mut state, &self.id);
            } else {
                acknowledge_ignored_locked(
                    &mut state,
                    &self.id,
                    self.captured_at,
                    BusinessMessageRevision::new(&self.text, sender, raw_sender),
                    true,
                );
            }
        }
        self.committed = true;
        Ok(())
    }

    /// Drop（派发失败）只撤销认领，绝不写记录，否则真失败的命令会被永久吞掉。
    fn commit(mut self, write_lock: bool) -> Result<()> {
        {
            let mut state = self.shared.lock()?;
            acknowledge_business_locked(&mut state, &self.id);
            if write_lock {
                record_command_lock_locked(&mut state, &self.id.chat, &self.text, self.y);
            }
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for SharedBusinessClaim {
    fn drop(&mut self) {
        // 命令成功或永久忽略后的墓碑必须保留；可修订忽略已在提交时释放认领。
        // Drop 只能回滚仍然活动的失败认领，不能撤销期间发生的永久 ACK。
        if self.committed {
            return;
        }
        if let Ok(mut state) = self.shared.inner.state.lock() {
            if !state.active_business_claims.remove(&self.id) {
                return;
            }
            state.business_claimed.remove(&self.id);
            if !matches!(self.id.chat, ChatIdentity::Friend(_))
                || state.friend_suppression_generation == self.suppression_generation
            {
                state.business_pending.insert(self.id.clone());
            }
        }
    }
}

pub(crate) struct FriendBusinessSuppression {
    shared: ChatTimeline,
}

impl Drop for FriendBusinessSuppression {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.inner.state.lock() {
            state.friend_suppressions -= 1;
            if state.friend_suppressions == 0 {
                // OCR captured before release can finish publishing afterwards.
                state.friend_suppression_ended = Some(Instant::now());
            }
        }
    }
}

impl ChatTimeline {
    /// 某条已见消息在当前账本里的交付状态；用于把“未判为新增”细分为可排查的原因。
    pub(crate) fn business_delivery_state(
        &self,
        id: &ObservedChatMessageId,
    ) -> Result<BusinessDeliveryState> {
        let state = self.lock()?;
        Ok(if state.business_pending.contains(id) {
            BusinessDeliveryState::Pending
        } else if state.business_claimed.contains(id) {
            BusinessDeliveryState::Claimed
        } else if state.ignored_revisions.contains_key(id) {
            BusinessDeliveryState::IgnoredRevision
        } else if state.revision_delivery_fences.contains_key(id) {
            BusinessDeliveryState::Fenced
        } else {
            BusinessDeliveryState::NotTracked
        })
    }

    /// 这条消息是否首次被记为“命令未执行”；true 表示应当打日志（同一实例只打一次）。
    pub(crate) fn note_not_executed_log(&self, id: &ObservedChatMessageId) -> Result<bool> {
        let mut state = self.lock()?;
        Ok(state.not_executed_logged.insert(id.clone()))
    }

    /// 标记下一次发布要携带的基线事件（由 reset_chat_observation_baseline 调用）。
    pub(crate) fn note_baseline(&self, reason: &str) -> Result<()> {
        let mut state = self.lock()?;
        state.pending_baseline = Some(BaselineKind::from_reason(reason));
        Ok(())
    }

    pub(crate) fn new() -> Self {
        let this = Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    latest: None,
                    trackers: HashMap::new(),
                    business_pending: HashSet::new(),
                    unread_baseline: HashSet::new(),
                    business_claimed: HashSet::new(),
                    not_executed_logged: HashSet::new(),
                    cold_baseline_consumed: false,
                    active_business_claims: HashSet::new(),
                    ignored_revisions: HashMap::new(),
                    latest_business_revisions: HashMap::new(),
                    revision_delivery_fences: HashMap::new(),
                    command_locks: HashMap::new(),
                    chat_freshness: HashMap::new(),
                    pending_baseline: None,
                    next_command_lock_activity: 0,
                    unread_delivery_fences: HashMap::new(),
                    friend_suppression_generation: 0,
                    friend_suppressions: 0,
                    friend_suppression_ended: None,
                    subscribers: HashMap::new(),
                    next_subscriber: 1,
                    next_session: 1,
                    requests: BTreeMap::new(),
                    next_request: 1,
                }),
                changed: Condvar::new(),
            }),
        };
        #[cfg(test)]
        this.prime_for_tests();
        this
    }

    /// 测试默认模拟“进程已启动并进入大厅”：预置 freshness 已初始化，并挂一次进入大厅的基线，
    /// 让首个有内容帧按 HallWorldSwitch（蓝 New/粉 Old）处理。AppStart 全旧语义由
    /// freshness 单元测试覆盖。
    #[cfg(test)]
    fn prime_for_tests(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            for chat in [ChatIdentity::PrimaryHall, ChatIdentity::SecondaryHall] {
                let mut freshness = SequenceFreshness::default();
                let _ = freshness.observe(&FreshFrame {
                    trusted: true,
                    baseline: Some(BaselineKind::AppStart),
                    bubbles: &[],
                });
                state.chat_freshness.insert(chat, freshness);
            }
            state.pending_baseline = Some(BaselineKind::HallWorldSwitch);
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.inner
            .state
            .lock()
            .map_err(|_| anyhow!("chat timeline state lock poisoned"))
    }

    pub(crate) fn latest(&self) -> Result<Option<ChatTimelineSnapshot>> {
        Ok(self.lock()?.latest.clone())
    }

    pub(crate) fn subscribe(&self) -> Result<ChatTimelineSubscriber> {
        let mut state = self.lock()?;
        Ok(self.subscribe_locked(&mut state, None, false))
    }

    /// A business reader survives transient capture/OCR failures and retains its queue.
    pub(crate) fn subscribe_business(&self) -> Result<ChatTimelineSubscriber> {
        let mut state = self.lock()?;
        Ok(self.subscribe_locked(&mut state, None, true))
    }

    /// Reading the passive baseline and starting the queue is one atomic operation.
    pub(crate) fn subscribe_with_snapshot(
        &self,
    ) -> Result<(Option<ChatTimelineSnapshot>, ChatTimelineSubscriber)> {
        let mut state = self.lock()?;
        let latest = state.latest.clone();
        let subscriber = self.subscribe_locked(&mut state, None, false);
        Ok((latest, subscriber))
    }

    pub(crate) fn subscribe_chat(&self, chat: ChatIdentity) -> Result<ChatTimelineSubscriber> {
        let mut state = self.lock()?;
        Ok(self.subscribe_locked(&mut state, Some(chat), false))
    }

    fn subscribe_locked(
        &self,
        state: &mut State,
        chat: Option<ChatIdentity>,
        durable_business: bool,
    ) -> ChatTimelineSubscriber {
        let id = state.next_subscriber;
        state.next_subscriber += 1;
        state.subscribers.insert(
            id,
            Subscription {
                chat,
                session: None,
                queue: VecDeque::new(),
                failure: None,
                durable_business,
            },
        );
        ChatTimelineSubscriber {
            shared: self.clone(),
            id,
        }
    }

    pub(crate) fn refresh_after_other_hall_panel(
        &self,
        expected: &ChatIdentity,
        snapshot: &ChatTimelineSnapshot,
    ) -> Result<Option<u64>> {
        if other_hall_panel(expected, &snapshot.chat) {
            self.request_refresh().map(Some)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn request_refresh(&self) -> Result<u64> {
        let mut state = self.lock()?;
        // Expired completed tokens fail explicitly rather than returning an unrelated baseline.
        while state.requests.len() >= RETAINED_CAPACITY {
            let completed = state
                .requests
                .iter()
                .find_map(|(id, request)| request.outcome.as_ref().map(|_| *id));
            let Some(id) = completed else {
                bail!("chat timeline refresh request capacity exceeded");
            };
            state.requests.remove(&id);
        }
        let id = state.next_request;
        state.next_request += 1;
        state.requests.insert(
            id,
            Refresh {
                requested_at: Instant::now(),
                outcome: None,
            },
        );
        self.inner.changed.notify_all();
        Ok(id)
    }

    /// The observer must bypass fingerprint caching while this fence is pending.
    pub(crate) fn pending_refresh(&self) -> Result<Option<Instant>> {
        Ok(self
            .lock()?
            .requests
            .values()
            .filter(|request| request.outcome.is_none())
            .map(|request| request.requested_at)
            .min())
    }

    pub(crate) fn wait_for_snapshot(
        &self,
        request: u64,
        timeout: Duration,
    ) -> Result<Option<ChatTimelineSnapshot>> {
        let started = Instant::now();
        let mut state = self.lock()?;
        loop {
            let refresh = state.requests.get(&request).ok_or_else(|| {
                anyhow!("unknown or expired chat timeline refresh token {request}")
            })?;
            if let Some(outcome) = &refresh.outcome {
                return outcome.clone().map_err(RefreshFailure::into_error);
            }
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return Ok(None);
            };
            if remaining.is_zero() {
                return Ok(None);
            }
            let (next, _) = self
                .inner
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| anyhow!("chat timeline state lock poisoned"))?;
            state = next;
        }
    }

    /// Publish only after OCR of this actual frame, including frames with zero messages.
    pub(crate) fn publish(
        &self,
        captured_at: Instant,
        chat: ChatIdentity,
        messages: Vec<RecognizedChatMessage>,
    ) -> Result<ChatTimelineSnapshot> {
        let mut state = self.lock()?;
        // A delayed frame must not roll the passive baseline or trackers backwards.
        if state
            .latest
            .as_ref()
            .is_some_and(|latest| latest.captured_at > captured_at)
        {
            bail!("chat timeline received an out-of-order frame");
        }
        if !state.trackers.contains_key(&chat) {
            let session = VisualSessionId::new(state.next_session);
            state.next_session += 1;
            state.trackers.insert(
                chat.clone(),
                Tracker {
                    session,
                    next_sequence: 1,
                    initialized: false,
                    visible: Vec::new(),
                },
            );
        }
        let suppress = matches!(chat, ChatIdentity::Friend(_))
            && (state.friend_suppressions > 0
                || state
                    .friend_suppression_ended
                    .is_some_and(|end| captured_at <= end));
        // 空 OCR 帧不消费基线：否则会在真正有内容的第一帧丢失基线语义。
        let frame_empty = messages.is_empty();
        // 冷基线消费：聊天区不存在（publish_no_chat）或首屏为空，都等价于“首屏没有命令”。
        // 首屏抑制的目的只是不让 BOT 刚启动就读到旧命令；无命令可抑制时，
        // 之后出现的命令按新命令处理（蓝色不再被 AppStart 吞、粉色不再走首屏历史提升）。
        let cold_consumed = state.cold_baseline_consumed || frame_empty;
        if state.cold_baseline_consumed && !frame_empty {
            // 之前出现过“首屏无聊天/空屏”，现在内容才出现：过期基线事件不得落到这一帧上，
            // 否则首屏无聊天时，内容再出现会被旧基线判成历史（粉色与蓝色同一规则）。
            state.pending_baseline = None;
        }
        let baseline = if frame_empty {
            None
        } else {
            state.pending_baseline.take()
        };
        let mut freshness = state.chat_freshness.remove(&chat).unwrap_or_default();
        if cold_consumed {
            freshness.mark_view_absent();
        }
        let tracker = state.trackers.get_mut(&chat).expect("tracker inserted");
        let initial = !tracker.initialized && !cold_consumed;
        let mut observed = tracker.observe(&chat, messages, &mut freshness, baseline);
        let visual_session = tracker.session;
        if suppress {
            for message in &mut tracker.visible {
                message.is_new = false;
            }
        }
        // 只有“可信的非空首帧”才真正消费掉冷基线；空帧或被抑制帧继续保留该标志。
        state.cold_baseline_consumed = frame_empty || suppress;
        state.chat_freshness.insert(chat.clone(), freshness);
        // 命令锁已降级为**冷启动兜底账本**：新老判定由 freshness 的序列对齐负责；
        // 这里只在“携带基线事件的帧”或“tracker 刚建立的首帧”用锁否决已经成功执行过的
        // 命令（防止进程重启/视图重建后把历史命令再执行一次）。常规流式帧不再被
        // “文本 + y<=记录+28”反压。release_missing 仍按屏幕是否存在做迟滞释放。
        let lock_empty = {
            // 这是当前视图在用的聊天：刷新活跃度，供桶淘汰时区分一次性抖动桶。
            state.next_command_lock_activity = state.next_command_lock_activity.wrapping_add(1);
            let activity = state.next_command_lock_activity;
            let lock = state.command_locks.entry(chat.clone()).or_default();
            lock.last_activity = activity;
            // 缺陷 4：suppress 帧（好友丢弃视图/导航中）不是当前聊天真实内容的可信证据，
            // 既不匹配也不释放，否则会把仍在屏上的条目误删。
            if !suppress {
                // 可信帧：先按 (文本 + y 容差) 释放滚走的记录（带迟滞）。
                lock.release_missing(&observed);
                // 命令锁降级为“仅冷启动兜底”：只在该帧携带基线事件或 tracker 刚建立时，
                // 用它否决已经成功执行过的命令；常规流式帧由 freshness 判决，锁不再反压。
                if baseline.is_some() || initial {
                    for message in &mut observed {
                        if message.is_new && lock.matches(message) {
                            message.is_new = false;
                        }
                    }
                }
            }
            lock.is_empty()
        };
        if lock_empty {
            // 聊天桶清空即移除，避免无界增长。
            state.command_locks.remove(&chat);
        }
        if !observed.is_empty() {
            state.ignored_revisions.retain(|id, _| {
                id.chat != chat || observed.iter().any(|message| message.id == *id)
            });
        }
        state.unread_baseline.retain(|id| {
            id.chat != chat
                || observed.is_empty()
                || observed.iter().any(|message| message.id == *id)
        });
        state.not_executed_logged.retain(|id| {
            id.chat != chat
                || observed.is_empty()
                || observed.iter().any(|message| message.id == *id)
        });
        for message in &mut observed {
            if suppress {
                message.is_new = false;
                state.business_pending.remove(&message.id);
                state.unread_baseline.remove(&message.id);
                state.ignored_revisions.remove(&message.id);
                state.latest_business_revisions.remove(&message.id);
                state.revision_delivery_fences.remove(&message.id);
                state.business_claimed.insert(message.id.clone());
            } else if initial && message.message_type == "pink" {
                // 首屏历史仅按粉色消息判定，不按会话权限或昵称角色判定。
                // 蓝色大厅命令照常交付；粉色首屏只允许未读流程提升所选气泡。
                message.is_new = false;
                state.business_pending.remove(&message.id);
                if state
                    .command_locks
                    .get(&chat)
                    .is_some_and(|lock| lock.matches(message))
                {
                    // 未读提升不能把视图重建前已成功的命令重新派发。
                    state.unread_baseline.remove(&message.id);
                    state.business_claimed.insert(message.id.clone());
                } else {
                    state.unread_baseline.insert(message.id.clone());
                }
            } else if state.business_claimed.contains(&message.id) {
                message.is_new = false;
            } else {
                if let Some(ignored) = state.ignored_revisions.get(&message.id) {
                    let corrected = captured_at >= ignored.captured_at
                        && BusinessMessageRevision::from_message(message)
                            .corrects(&ignored.revision);
                    if corrected {
                        if state
                            .command_locks
                            .get(&chat)
                            .is_some_and(|lock| lock.matches(message))
                        {
                            // 有效修订仍命中已执行命令锁，不能由 ignored 恢复资格绕过它。
                            acknowledge_business_locked(&mut state, &message.id);
                            message.is_new = false;
                        } else {
                            state.ignored_revisions.remove(&message.id);
                            state
                                .revision_delivery_fences
                                .insert(message.id.clone(), captured_at);
                            message.is_new = true;
                            set_business_new_locked(&mut state, &message.id, true);
                        }
                    } else {
                        message.is_new = false;
                    }
                }
                if message.is_new {
                    state.business_pending.insert(message.id.clone());
                }
            }
        }
        for message in &observed {
            if !suppress
                && (state.business_pending.contains(&message.id)
                    || state.active_business_claims.contains(&message.id)
                    || state.ignored_revisions.contains_key(&message.id))
            {
                let revision = BusinessMessageRevision::from_message(message);
                let retains_nonempty = revision.text.is_empty()
                    && state
                        .latest_business_revisions
                        .get(&message.id)
                        .is_some_and(|current| !current.revision.text.is_empty());
                if !retains_nonempty {
                    // 空 OCR 是退化观测，不能盖掉尚未消费的有效非空正文及其捕获边界。
                    state.latest_business_revisions.insert(
                        message.id.clone(),
                        CapturedBusinessRevision {
                            captured_at,
                            revision,
                            y: Some(message.block.y),
                        },
                    );
                }
            }
        }
        let State {
            latest_business_revisions,
            business_pending,
            active_business_claims,
            ignored_revisions,
            ..
        } = &mut *state;
        latest_business_revisions.retain(|id, _| {
            business_pending.contains(id)
                || active_business_claims.contains(id)
                || ignored_revisions.contains_key(id)
        });
        let snapshot = ChatTimelineSnapshot {
            captured_at,
            visual_session,
            chat,
            messages: observed,
        };
        for subscriber in state.subscribers.values_mut() {
            if subscriber.failure.is_some() {
                continue;
            }
            if subscriber
                .chat
                .as_ref()
                .is_some_and(|bound| other_hall_panel(bound, &snapshot.chat))
            {
                continue;
            }
            // Only snapshots from the bound chat can feed this task. A different
            // identity may be a long-lived UI transition, so wait for the target
            // instead of ending the task based on frame count or elapsed time here.
            if subscriber
                .chat
                .as_ref()
                .is_some_and(|bound| *bound != snapshot.chat)
            {
                continue;
            }
            if subscriber.queue.len() >= RETAINED_CAPACITY {
                subscriber.failure =
                    Some("chat timeline subscriber overflow (4096 snapshots)".into());
                subscriber.queue.clear();
            } else {
                let session_changed = subscriber
                    .session
                    .is_some_and(|session| session != snapshot.visual_session);
                if session_changed {
                    // 同一个聊天里的视图重建（机器人自己发言、面板滚动、重新申请帧）会让视觉会话号自增。
                    // 确认读者重置基线继续等待；业务读者保留尚未消费的旧会话快照。
                    log::info!(
                        "共享聊天读者改绑新视觉会话: session={:?} -> {:?}",
                        subscriber.session,
                        snapshot.visual_session
                    );
                    if !subscriber.durable_business {
                        subscriber.queue.clear();
                    }
                } else if subscriber.session.is_none() && subscriber.chat.is_some() {
                    // 聊天视图曾瞬态消失；同一聊天恢复后重新绑定，不丢后续消息。
                    log::info!("共享聊天读者在聊天恢复后重新绑定: chat={:?}", snapshot.chat);
                    if !subscriber.durable_business {
                        subscriber.queue.clear();
                    }
                }
                subscriber.session = Some(snapshot.visual_session);
                subscriber.queue.push_back(snapshot.clone());
            }
        }
        complete_refreshes(&mut state, captured_at, Ok(Some(snapshot.clone())));
        state.latest = Some(snapshot.clone());
        self.inner.changed.notify_all();
        Ok(snapshot)
    }

    pub(crate) fn publish_no_chat(
        &self,
        captured_at: Instant,
        reason: impl Into<String>,
    ) -> Result<()> {
        let reason = reason.into();
        let mut state = self.lock()?;
        if state
            .latest
            .as_ref()
            .is_some_and(|latest| latest.captured_at > captured_at)
        {
            bail!("chat timeline received an out-of-order no-chat frame");
        }
        state.latest = None;
        // A disappeared view starts new visual sessions on its next appearance.
        state.trackers.clear();
        // 聊天区不存在：首屏没有命令可抑制，冷基线就此消费（见 mark_view_absent 说明）。
        state.cold_baseline_consumed = true;
        // 聊天不可见：保留新鲜度状态，但**不再强制下一帧不连续**。
        // 真实案例：聊天框消失后回来的第一条新命令曾被“全旧”吞掉。交叠时交给对齐按
        // “底部追加”判新；无交叠（画面大改/命令被顶到顶部）时对齐自然失败并全旧，场景 3 仍成立。
        state.ignored_revisions.clear();
        let State {
            revision_delivery_fences,
            business_pending,
            business_claimed,
            active_business_claims,
            latest_business_revisions,
            ..
        } = &mut *state;
        revision_delivery_fences
            .retain(|id, _| business_pending.contains(id) || business_claimed.contains(id));
        latest_business_revisions
            .retain(|id, _| business_pending.contains(id) || active_business_claims.contains(id));
        // 缺陷 3：视图重建不是“条目已滚出屏幕”的证据。这里清零各条目的 missing_frames
        // （保留条目本身），否则重建前累计到 1 的计数会在重建后第 1 个可信帧凑满 2，
        // 把仍在屏上的条目误删，导致已执行命令重跑。
        for lock in state.command_locks.values_mut() {
            for entry in &mut lock.entries {
                entry.missing_frames = 0;
            }
        }
        state.unread_baseline.clear();
        for subscriber in state
            .subscribers
            .values_mut()
            .filter(|subscriber| subscriber.chat.is_some())
        {
            // 聊天视图瞬态消失（机器人自己发言、面板滚动、重新申请帧）不应立刻杀死确认读者。
            // 这里只清空队列并作废已绑定的视觉会话，等同一聊天重新出现后在 publish 里改绑；
            // 真正的失败仍由 report_failure 毒化，窗口自身的超时兜底结束。
            if !subscriber.durable_business {
                subscriber.queue.clear();
            }
            subscriber.session = None;
        }
        complete_refreshes(&mut state, captured_at, Err(RefreshFailure::NoChat(reason)));
        self.inner.changed.notify_all();
        Ok(())
    }

    /// Fail only subscribers and refresh requests that exist at the time of failure.
    pub(crate) fn report_failure(&self, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let mut state = self.lock()?;
        for subscriber in state.subscribers.values_mut() {
            if !subscriber.durable_business {
                fail_subscription(subscriber, &reason);
            }
        }
        for request in state
            .requests
            .values_mut()
            .filter(|request| request.outcome.is_none())
        {
            request.outcome = Some(Err(RefreshFailure::Failure(reason.clone())));
        }
        self.inner.changed.notify_all();
        Ok(())
    }

    /// Suppress friend business before navigating to a discard view. OCR and
    /// decision queues keep flowing; IDs observed here are consumed, not deferred.
    pub(crate) fn suppress_friend_business(&self) -> Result<FriendBusinessSuppression> {
        let mut state = self.lock()?;
        state.friend_suppressions += 1;
        state.friend_suppression_generation += 1;
        state.unread_delivery_fences.clear();
        state
            .business_pending
            .retain(|id| !matches!(id.chat, ChatIdentity::Friend(_)));
        state.unread_baseline.clear();
        state
            .ignored_revisions
            .retain(|id, _| !matches!(id.chat, ChatIdentity::Friend(_)));
        state
            .latest_business_revisions
            .retain(|id, _| !matches!(id.chat, ChatIdentity::Friend(_)));
        state
            .revision_delivery_fences
            .retain(|id, _| !matches!(id.chat, ChatIdentity::Friend(_)));
        for (chat, tracker) in &mut state.trackers {
            if matches!(chat, ChatIdentity::Friend(_)) {
                for message in &mut tracker.visible {
                    message.is_new = false;
                }
            }
        }
        Ok(FriendBusinessSuppression {
            shared: self.clone(),
        })
    }

    /// Deliberate unread navigation may deliver one selected first-view bubble.
    /// Reusing its shared ID means queued revisions and subsequent OCR cannot
    /// deliver it again. Historical siblings remain passive.
    pub(crate) fn deliver_unread_baseline(
        &self,
        snapshot: &ChatTimelineSnapshot,
        id: &ObservedChatMessageId,
    ) -> Result<bool> {
        let mut state = self.lock()?;
        if state.friend_suppressions > 0
            || !matches!(snapshot.chat, ChatIdentity::Friend(_))
            || !snapshot.messages.iter().any(|message| message.id == *id)
            || !state.unread_baseline.contains(id)
        {
            return Ok(false);
        }
        // Use the most recent actual OCR revision so eligibility cannot append
        // an older capture behind newer queued frames or dispatch stale OCR text.
        let mut revision = state
            .latest
            .as_ref()
            .filter(|latest| {
                latest.chat == snapshot.chat
                    && latest.messages.iter().any(|message| message.id == *id)
            })
            .cloned()
            .ok_or_else(|| anyhow!("unread chat changed before baseline delivery"))?;
        let selected = revision
            .messages
            .iter()
            .find(|message| message.id == *id)
            .expect("latest unread revision contains selected message");
        if state
            .command_locks
            .get(&snapshot.chat)
            .is_some_and(|lock| lock.matches(selected))
        {
            // 最新 OCR 可从误读修订为已成功文本；提升前再次校验实际修订。
            acknowledge_business_locked(&mut state, id);
            return Ok(false);
        }
        state.unread_baseline.remove(id);
        state
            .unread_delivery_fences
            .insert(id.clone(), revision.captured_at);
        state.business_pending.insert(id.clone());
        let selected_message = revision
            .messages
            .iter()
            .find(|message| message.id == *id)
            .expect("latest unread revision contains selected message");
        state.latest_business_revisions.insert(
            id.clone(),
            CapturedBusinessRevision {
                captured_at: revision.captured_at,
                revision: BusinessMessageRevision::from_message(selected_message),
                y: Some(selected_message.block.y),
            },
        );
        if let Some(tracker) = state.trackers.get_mut(&id.chat) {
            for message in &mut tracker.visible {
                if message.id == *id {
                    message.is_new = true;
                }
            }
        }
        revision.messages.retain(|message| message.id == *id);
        revision.messages[0].is_new = true;
        // An eligibility revision is needed even if the listener already drained
        // the passive baseline. Bound decision readers need no duplicate frame.
        for subscriber in state.subscribers.values_mut().filter(|s| s.chat.is_none()) {
            if subscriber.failure.is_some() {
                continue;
            }
            if subscriber.queue.len() >= RETAINED_CAPACITY {
                fail_subscription(
                    subscriber,
                    "chat timeline subscriber overflow (4096 snapshots)",
                );
            } else {
                subscriber.queue.push_back(revision.clone());
            }
        }
        self.inner.changed.notify_all();
        Ok(true)
    }

    pub(crate) fn is_business_pending(&self, id: &ObservedChatMessageId) -> Result<bool> {
        Ok(self.lock()?.business_pending.contains(id))
    }

    /// 快照消费同时核对修订时间，旧排队 OCR 不能抢先消费较新的有效修订。
    pub(crate) fn is_business_pending_at(
        &self,
        id: &ObservedChatMessageId,
        captured_at: Instant,
    ) -> Result<bool> {
        let state = self.lock()?;
        Ok(state.business_pending.contains(id)
            && !snapshot_precedes_delivery_fence(&state, id, captured_at))
    }

    /// `text`/`y` 必须是被派发消息自身的数据（由调用方在认领时连同快照一起捕获），
    /// 不能回头查当前 tracker：执行完成时气泡可能已经滚出 visible/latest，
    /// 那样会静默不写命令锁。
    pub(crate) fn claim_business(
        &self,
        id: &ObservedChatMessageId,
        captured_at: Instant,
        text: &str,
        y: i32,
    ) -> Result<Option<SharedBusinessClaim>> {
        let mut state = self.lock()?;
        if snapshot_precedes_delivery_fence(&state, id, captured_at)
            || state.business_claimed.contains(id)
            || !state.business_pending.remove(id)
        {
            return Ok(None);
        }
        state.business_claimed.insert(id.clone());
        state.active_business_claims.insert(id.clone());
        Ok(Some(SharedBusinessClaim {
            shared: self.clone(),
            id: id.clone(),
            suppression_generation: state.friend_suppression_generation,
            text: text.to_owned(),
            y,
            captured_at,
            committed: false,
        }))
    }

    pub(crate) fn acknowledge_business(&self, id: &ObservedChatMessageId) -> Result<bool> {
        let mut state = self.lock()?;
        Ok(acknowledge_business_locked(&mut state, id))
    }

    /// 只忽略本次空/未路由修订；正文或身份有效修订后，同一 ID 可以重新交付。
    /// 参数全部来自消费快照；启动基线、导航丢弃与成功命令使用永久 ACK。
    pub(crate) fn acknowledge_ignored(
        &self,
        id: &ObservedChatMessageId,
        captured_at: Instant,
        text: &str,
        sender: Option<&str>,
        raw_sender: Option<&str>,
    ) -> Result<bool> {
        let mut state = self.lock()?;
        Ok(acknowledge_ignored_locked(
            &mut state,
            id,
            captured_at,
            BusinessMessageRevision::new(text, sender, raw_sender),
            false,
        ))
    }

    /// 一级大厅命令路径的"真正派发成功"完成点：确认已处理，并用被派发消息自身的
    /// 文本与 y 写入命令锁。调用方必须只在成功派发点调用它；被禁用/被跳过的
    /// 路径只调 `acknowledge_business`，未路由修订使用 `acknowledge_ignored`。
    pub(crate) fn acknowledge_command(
        &self,
        id: &ObservedChatMessageId,
        text: &str,
        y: i32,
    ) -> Result<bool> {
        let mut state = self.lock()?;
        let found = acknowledge_business_locked(&mut state, id);
        record_command_lock_locked(&mut state, &id.chat, text, y);
        Ok(found)
    }
}

fn acknowledge_business_locked(state: &mut State, id: &ObservedChatMessageId) -> bool {
    state.unread_baseline.remove(id);
    state.unread_delivery_fences.remove(id);
    state.ignored_revisions.remove(id);
    state.latest_business_revisions.remove(id);
    state.revision_delivery_fences.remove(id);
    state.business_claimed.insert(id.clone());
    state.active_business_claims.remove(id);
    let found = state.business_pending.remove(id);
    set_business_new_locked(state, id, false);
    found
}

fn snapshot_precedes_delivery_fence(
    state: &State,
    id: &ObservedChatMessageId,
    captured_at: Instant,
) -> bool {
    state
        .unread_delivery_fences
        .get(id)
        .into_iter()
        .chain(state.revision_delivery_fences.get(id))
        .any(|fence| captured_at < *fence)
}

fn acknowledge_ignored_locked(
    state: &mut State,
    id: &ObservedChatMessageId,
    captured_at: Instant,
    revision: BusinessMessageRevision,
    claimed: bool,
) -> bool {
    if !claimed
        && (state.business_claimed.contains(id)
            || snapshot_precedes_delivery_fence(state, id, captured_at)
            || (!state.business_pending.contains(id) && !state.ignored_revisions.contains_key(id)))
    {
        return false;
    }
    if matches!(id.chat, ChatIdentity::Friend(_))
        && (state.friend_suppressions > 0
            || state
                .friend_suppression_ended
                .is_some_and(|end| captured_at <= end))
    {
        return acknowledge_business_locked(state, id);
    }
    if claimed {
        if !state.active_business_claims.remove(id) {
            // 永久 ACK 已覆盖此认领，迟到的忽略提交不能撤销成功墓碑。
            return false;
        }
        state.business_claimed.remove(id);
    }
    let found = state.business_pending.remove(id) || claimed;
    // 业务快照可能已整批读取，且其 tracker 已在 no_chat 中清空。
    // 独立保留尚未消费 ID 的最新修订，旧忽略提交不能抹掉积压的正确文字。
    let current = state.latest_business_revisions.get(id).cloned();
    let ignored_y = current.as_ref().and_then(|current| current.y);
    if current.as_ref().is_some_and(|current| {
        current.captured_at >= captured_at
            && current.y.is_some_and(|y| {
                state
                    .command_locks
                    .get(&id.chat)
                    .is_some_and(|lock| lock.matches_text_at(&current.revision.text, y))
            })
    }) {
        // 迟到的忽略提交也必须尊重锁；位置来自实际最新修订，未知位置不猜测。
        acknowledge_business_locked(state, id);
        return found;
    }
    if let Some(current) = current
        && current.captured_at >= captured_at
        && current.revision.corrects(&revision)
    {
        // 忽略提交落后于当前有效修订：保留更新并拦住所有更早的排队快照。
        state.ignored_revisions.remove(id);
        state
            .revision_delivery_fences
            .insert(id.clone(), current.captured_at);
        state.business_pending.insert(id.clone());
        set_business_new_locked(state, id, true);
    } else {
        state.ignored_revisions.insert(
            id.clone(),
            CapturedBusinessRevision {
                captured_at,
                revision,
                y: ignored_y,
            },
        );
        set_business_new_locked(state, id, false);
    }
    found
}

fn set_business_new_locked(state: &mut State, id: &ObservedChatMessageId, is_new: bool) {
    if let Some(tracker) = state.trackers.get_mut(&id.chat) {
        for message in &mut tracker.visible {
            if message.id == *id {
                message.is_new = is_new;
            }
        }
    }
    if let Some(latest) = &mut state.latest {
        for message in &mut latest.messages {
            if message.id == *id {
                message.is_new = is_new;
            }
        }
    }
}

/// 认领并派发成功后把命令文本与屏幕位置写入该聊天的命令锁。
/// 文本与 y 来自被派发消息自身，不查询当前 tracker。
fn record_command_lock_locked(state: &mut State, chat: &ChatIdentity, text: &str, y: i32) {
    let normalized = normalize_command_lock_text(text);
    if normalized.is_empty() {
        return;
    }
    if !state.command_locks.contains_key(chat)
        && state.command_locks.len() >= COMMAND_LOCK_CHAT_CAPACITY
    {
        // Friend(OCR 原始名) 抖动会产生大量一次性聊天桶；超过上界时淘汰一个
        // **非当前聊天**且最不活跃的桶：优先空桶，其次 last_activity 最小，
        // 最后用 Debug 串确定性打破平局——禁止再直接用 keys().find()（HashMap
        // 迭代序不定，可能把正在用的 PrimaryHall 淘汰掉）。
        let evicted = state
            .command_locks
            .iter()
            .filter(|(key, _)| *key != chat)
            .min_by_key(|(key, lock)| {
                (
                    !lock.entries.is_empty(),
                    lock.last_activity,
                    format!("{key:?}"),
                )
            })
            .map(|(key, _)| key.clone());
        if let Some(evicted) = evicted {
            state.command_locks.remove(&evicted);
        }
    }
    state.next_command_lock_activity = state.next_command_lock_activity.wrapping_add(1);
    let activity = state.next_command_lock_activity;
    let lock = state.command_locks.entry(chat.clone()).or_default();
    lock.last_activity = activity;
    lock.record(normalized, y);
}

/// 命令锁键：折全角、小写化并去掉空白，保留 @/# 等命令前缀，避免不同命令互相命中。
/// 复用 `crate::text::normalize_chat_text`（与聊天比较同一份实现）：OCR 的同/全角标点
/// 抖动（`:` vs `：`）必须互相命中，否则同一屏命令会在重建后被当成新命令执行两次。
fn normalize_command_lock_text(text: &str) -> String {
    let normalized: String = crate::text::normalize_chat_text(text)
        .chars()
        .flat_map(char::to_lowercase)
        .collect();
    let without_noise = normalized.trim_end_matches(crate::text::OCR_TRAILING_NOISE);
    let command = without_noise.split_once('@').map(|(_, command)| command);
    // These commands have no parameters. The router already accepts trailing OCR noise.
    // Preserve song titles and user arguments, where punctuation can be meaningful.
    if command.is_some_and(|command| {
        matches!(
            command,
            "下一首"
                | "下一曲"
                | "上一首"
                | "上一曲"
                | "暂停"
                | "继续"
                | "恢复"
                | "播放"
                | "状态"
                | "帮助"
        )
    }) {
        without_noise.to_string()
    } else {
        normalized
    }
}

fn complete_refreshes(
    state: &mut State,
    captured_at: Instant,
    outcome: std::result::Result<Option<ChatTimelineSnapshot>, RefreshFailure>,
) {
    for request in state.requests.values_mut() {
        if request.outcome.is_none() && captured_at >= request.requested_at {
            request.outcome = Some(outcome.clone());
        }
    }
}

fn fail_subscription(subscriber: &mut Subscription, reason: &str) {
    if subscriber.failure.is_none() {
        subscriber.failure = Some(reason.to_owned());
    }
    subscriber.queue.clear();
}

impl ChatTimelineSubscriber {
    /// Bind to the caller's fresh baseline without replacing the retained queue.
    /// The live view may already have transitioned; snapshots from other identities
    /// are filtered until this task's bound chat returns.
    pub(crate) fn bind_chat(&self, chat: ChatIdentity, session: VisualSessionId) -> Result<()> {
        let mut state = self.shared.lock()?;
        let subscriber = state
            .subscribers
            .get_mut(&self.id)
            .ok_or_else(|| anyhow!("chat timeline subscription closed"))?;
        if let Some(reason) = &subscriber.failure {
            bail!("{reason}");
        }
        // The supplied fresh baseline owns this binding. The live view may already
        // have moved to a temporary identity; publish/read will filter until it returns.
        subscriber.chat = Some(chat);
        subscriber.session = Some(session);
        Ok(())
    }

    pub(crate) fn read(&self) -> Result<Vec<ChatTimelineSnapshot>> {
        let mut state = self.shared.lock()?;
        let subscriber = state
            .subscribers
            .get_mut(&self.id)
            .ok_or_else(|| anyhow!("chat timeline subscription closed"))?;
        if let Some(reason) = &subscriber.failure {
            bail!("{reason}");
        }
        Ok(subscriber.queue.drain(..).collect())
    }

    /// 本订阅者是否还有未被消费的快照。
    ///
    /// `publish` 已经在 `changed` 条件变量上 `notify_all`；标准库通道没有 `select`，
    /// 所以主循环用「短切片等帧 + 这里检查」把等待变成可中断的：一有发布就提前
    /// 回循环顶部派发，而不是等满一整轮（等帧 + 处理 + sleep）。
    pub(crate) fn has_pending(&self) -> Result<bool> {
        let state = self.shared.lock()?;
        let subscriber = state
            .subscribers
            .get(&self.id)
            .ok_or_else(|| anyhow!("chat timeline subscription closed"))?;
        if let Some(reason) = &subscriber.failure {
            bail!("{reason}");
        }
        Ok(!subscriber.queue.is_empty())
    }

    pub(crate) fn wait(&self, timeout: Duration) -> Result<bool> {
        let started = Instant::now();
        let mut state = self.shared.lock()?;
        loop {
            let subscriber = state
                .subscribers
                .get(&self.id)
                .ok_or_else(|| anyhow!("chat timeline subscription closed"))?;
            if let Some(reason) = &subscriber.failure {
                bail!("{reason}");
            }
            if !subscriber.queue.is_empty() {
                return Ok(true);
            }
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return Ok(false);
            };
            if remaining.is_zero() {
                return Ok(false);
            }
            let (next, _) = self
                .shared
                .inner
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| anyhow!("chat timeline state lock poisoned"))?;
            state = next;
        }
    }
}

impl Drop for ChatTimelineSubscriber {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.inner.state.lock() {
            state.subscribers.remove(&self.id);
        }
    }
}

impl Tracker {
    fn observe(
        &mut self,
        chat: &ChatIdentity,
        input: Vec<RecognizedChatMessage>,
        freshness: &mut SequenceFreshness,
        baseline: Option<BaselineKind>,
    ) -> Vec<ChatTimelineMessage> {
        if input.is_empty() {
            // 空 OCR 帧可观测，但不构成底部追加证据，也不清除对齐基准。
            let _ = freshness.observe(&FreshFrame {
                trusted: true,
                baseline,
                bubbles: &[],
            });
            return Vec::new();
        }
        let decision = freshness.observe(&FreshFrame {
            trusted: true,
            baseline,
            bubbles: &input,
        });
        let mut current = Vec::with_capacity(input.len());
        for (index, input) in input.into_iter().enumerate() {
            let fresh = decision
                .freshness
                .get(index)
                .copied()
                .unwrap_or(Freshness::Unknown);
            let matched = decision
                .matched
                .get(index)
                .copied()
                .flatten()
                .and_then(|old| self.visible.get(old))
                .map(|old| old.id.clone());
            let id = matched.unwrap_or_else(|| {
                let id = ObservedChatMessageId::new(
                    self.session,
                    chat.clone(),
                    BubbleSequence::new(self.next_sequence),
                );
                self.next_sequence += 1;
                id
            });
            current.push(ChatTimelineMessage {
                id,
                is_new: fresh == Freshness::New,
                text: input.text,
                sender: input.sender,
                raw_sender: input.raw_sender,
                message_type: input.message_type,
                block: input.block,
            });
        }
        self.initialized = true;
        self.visible = current.clone();
        current
    }
}

#[cfg(test)]
impl ChatTimeline {
    /// 测试观测口：指定聊天桶里已写入的命令锁文本（已归一化）。
    /// 仅供接线测试观察命令锁，不参与生产逻辑。
    pub(crate) fn command_lock_texts_for_test(&self, chat: &ChatIdentity) -> Vec<String> {
        self.inner
            .state
            .lock()
            .unwrap()
            .command_locks
            .get(chat)
            .map(|lock| {
                lock.entries
                    .iter()
                    .map(|entry| entry.text.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 测试观测口：指定聊天桶当前是否存在。
    pub(crate) fn command_lock_bucket_exists_for_test(&self, chat: &ChatIdentity) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .command_locks
            .contains_key(chat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(text: &str, y: i32) -> RecognizedChatMessage {
        RecognizedChatMessage {
            text: text.into(),
            sender: None,
            raw_sender: None,
            // 一级大厅消息的真实颜色是 blue；新基线只交付 blue/pink。
            message_type: "blue".into(),
            block: Rect::new(10, y, 120, 20),
        }
    }

    fn publish(
        shared: &ChatTimeline,
        messages: Vec<RecognizedChatMessage>,
    ) -> ChatTimelineSnapshot {
        // 测试夹具：把“进程已启动后的首个有内容帧”按 HallWorldSwitch 基线处理
        // （蓝色 New、粉色 Old），以便聚焦后续行为；AppStart 全旧语义由 freshness 单元测试覆盖。
        let first_ever = {
            let state = shared.inner.state.lock().unwrap();
            state.trackers.is_empty() && state.chat_freshness.is_empty()
        };
        if first_ever {
            // 1) 先消费 AppStart 基线（进程首帧全旧），让状态 initialized。
            shared.note_baseline("已启动").unwrap();
            let _ = shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![input("__seed__", 1)],
                )
                .unwrap();
            // 2) 真正的场景按“进入大厅”基线处理（蓝色 New、粉色 Old）。
            shared.note_baseline("已进入千星").unwrap();
        }
        shared
            .publish(Instant::now(), ChatIdentity::PrimaryHall, messages)
            .unwrap()
    }

    #[test]
    fn command_after_chat_reappears_is_still_new() {
        // 聊天框消失再回来：第一条新命令不能被“全旧”吞掉。
        let shared = ChatTimeline::new();
        publish(&shared, vec![input("A", 10)]);
        shared
            .publish_no_chat(Instant::now(), "chat hidden for a while")
            .unwrap();
        let again = publish(&shared, vec![input("A", 10), primary("甲", "@命令", 40)]);
        assert!(again.messages[1].is_new, "聊天恢复后的追加命令必须判新");
    }

    #[test]
    fn pink_command_after_chat_reappears_is_not_first_screen_suppressed() {
        // 聊天区不存在 => 首屏没有命令；恢复后首条粉色命令不应走“粉色首屏历史提升”而被吞。
        let shared = ChatTimeline::new();
        publish(&shared, vec![input("A", 10)]);
        shared
            .publish_no_chat(Instant::now(), "chat hidden for a while")
            .unwrap();
        let mut pink = primary("甲", "@点歌 晴", 40);
        pink.message_type = "pink".into();
        let again = publish(&shared, vec![input("A", 10), pink]);
        assert!(
            again.messages[1].is_new,
            "聊天恢复后首条粉色命令不应被首屏抑制"
        );
    }

    fn friend(
        shared: &ChatTimeline,
        mut messages: Vec<RecognizedChatMessage>,
    ) -> ChatTimelineSnapshot {
        // 此夹具模拟粉色好友气泡；非粉色同会话见独立颜色矩阵测试。
        for message in &mut messages {
            message.message_type = "pink".into();
        }
        shared
            .publish(
                Instant::now(),
                ChatIdentity::Friend(Arc::from("observed name")),
                messages,
            )
            .unwrap()
    }

    #[test]
    fn sequence_continuity_ignores_horizontal_position_and_block_size() {
        let mut tracker = Tracker {
            session: VisualSessionId::new(1),
            next_sequence: 1,
            initialized: false,
            visible: Vec::new(),
        };
        let mut freshness = SequenceFreshness::default();
        let first = tracker.observe(
            &ChatIdentity::PrimaryHall,
            vec![input("older", 40), input("latest", 70)],
            &mut freshness,
            None,
        );
        let mut corrected_older = input("older", 48);
        corrected_older.block = Rect::new(24, 48, 96, 26);
        let mut corrected_latest = input("latest corrected", 78);
        corrected_latest.block = Rect::new(24, 78, 96, 26);
        let corrected = tracker.observe(
            &ChatIdentity::PrimaryHall,
            vec![corrected_older, corrected_latest],
            &mut freshness,
            None,
        );

        assert_eq!(corrected[0].id, first[0].id);
        assert_eq!(corrected[1].id, first[1].id);

        let mut retained_latest = input("latest corrected", 40);
        retained_latest.block = Rect::new(30, 40, 88, 30);
        let scrolled = tracker.observe(
            &ChatIdentity::PrimaryHall,
            vec![retained_latest, input("new message", 70)],
            &mut freshness,
            None,
        );
        assert_eq!(scrolled[0].id, corrected[1].id);
        assert!(scrolled[1].is_new);
    }

    #[test]
    fn unread_promotion_cannot_repeat_success_after_view_rebuild() {
        let shared = ChatTimeline::new();
        let baseline = friend(&shared, vec![input("@点歌晴天", 40)]);
        let first = &baseline.messages[0];
        assert!(
            shared
                .deliver_unread_baseline(&baseline, &first.id)
                .unwrap()
        );
        claim(&shared, first, baseline.captured_at)
            .unwrap()
            .complete_as_command()
            .unwrap();
        shared
            .publish_no_chat(Instant::now(), "view rebuild after successful command")
            .unwrap();
        let rebuilt = friend(&shared, vec![input("@点歌晴天", 40)]);
        let repeated = &rebuilt.messages[0];
        assert_ne!(first.id, repeated.id);
        assert!(
            !shared
                .deliver_unread_baseline(&rebuilt, &repeated.id)
                .unwrap()
        );
        assert!(claim(&shared, repeated, rebuilt.captured_at).is_none());
        let fresh = friend(
            &shared,
            vec![input("@点歌晴天", 40), input("@点歌七里香", 70)],
        );
        assert!(
            shared.is_business_pending(&fresh.messages[1].id).unwrap(),
            "a distinct new pink command is still delivered"
        );
    }

    /// 用被派发消息自身的文本与 y 发起一次业务认领。
    fn claim(
        shared: &ChatTimeline,
        message: &ChatTimelineMessage,
        captured_at: Instant,
    ) -> Option<SharedBusinessClaim> {
        shared
            .claim_business(&message.id, captured_at, &message.text, message.block.y)
            .unwrap()
    }

    /// 真实一级大厅格式："[昵称]:@命令"，message_type 为 blue。
    fn primary(nickname: &str, command: &str, y: i32) -> RecognizedChatMessage {
        RecognizedChatMessage {
            message_type: "blue".into(),
            ..input(&format!("[{nickname}]:{command}"), y)
        }
    }

    /// 真实一级格式但用全角冒号："[昵称]：@命令"（OCR 会把半角冒号读成全角）。
    fn primary_fullwidth(nickname: &str, command: &str, y: i32) -> RecognizedChatMessage {
        RecognizedChatMessage {
            message_type: "blue".into(),
            ..input(&format!("[{nickname}]：{command}"), y)
        }
    }

    fn ignore_revision(
        shared: &ChatTimeline,
        snapshot: &ChatTimelineSnapshot,
        index: usize,
    ) -> bool {
        let message = &snapshot.messages[index];
        shared
            .acknowledge_ignored(
                &message.id,
                snapshot.captured_at,
                &message.text,
                message.sender.as_deref(),
                message.raw_sender.as_deref(),
            )
            .unwrap()
    }

    #[test]
    fn unrecognized_identity_revisions_retry_but_whitespace_does_not() {
        let shared = ChatTimeline::new();
        let first = publish(&shared, vec![input("#加入", 40)]);
        let id = &first.messages[0].id;
        ignore_revision(&shared, &first, 0);
        let mut with_sender = input("#加入", 40);
        with_sender.sender = Some("甲".into());
        let identified = publish(&shared, vec![with_sender.clone()]);
        assert_eq!(&identified.messages[0].id, id);
        assert!(shared.is_business_pending(id).unwrap());
        claim(&shared, &identified.messages[0], identified.captured_at)
            .unwrap()
            .complete_unrecognized(Some("甲"), None)
            .unwrap();
        assert!(!shared.is_business_pending(id).unwrap());
        let mut whitespace = with_sender.clone();
        whitespace.text = "  #加入  ".into();
        whitespace.sender = Some(" 甲 ".into());
        whitespace.raw_sender = Some("  ".into());
        let same = publish(&shared, vec![whitespace]);
        assert_eq!(&same.messages[0].id, id);
        assert!(!shared.is_business_pending(id).unwrap());
        with_sender.raw_sender = Some("甲（备注）".into());
        let remark = publish(&shared, vec![with_sender]);
        assert_eq!(&remark.messages[0].id, id);
        assert!(shared.is_business_pending(id).unwrap());
        assert!(
            !shared
                .is_business_pending_at(id, identified.captured_at)
                .unwrap()
        );
    }

    #[test]
    fn ignored_revision_records_retire_with_scrolled_or_rebuilt_visual_ids() {
        let shared = ChatTimeline::new();
        for index in 0..8 {
            let snapshot = publish(
                &shared,
                vec![input(&format!("unrecognized {index}"), index * 30)],
            );
            assert!(ignore_revision(&shared, &snapshot, 0));
            assert_eq!(
                shared.inner.state.lock().unwrap().ignored_revisions.len(),
                1
            );
        }
        shared
            .publish_no_chat(Instant::now(), "view rebuilt")
            .unwrap();
        assert!(
            shared
                .inner
                .state
                .lock()
                .unwrap()
                .ignored_revisions
                .is_empty()
        );
    }

    #[test]
    fn durable_business_queue_keeps_unread_commands_when_view_rebuilds() {
        let shared = ChatTimeline::new();
        let baseline = publish(&shared, vec![input("history", 10)]);
        let business = shared.subscribe_business().unwrap();
        let decisions = shared.subscribe_chat(ChatIdentity::PrimaryHall).unwrap();
        decisions
            .bind_chat(ChatIdentity::PrimaryHall, baseline.visual_session)
            .unwrap();
        let command = publish(&shared, vec![input("history", 10), input("@帮助", 40)]);
        let id = &command.messages[1].id;
        shared
            .publish_no_chat(Instant::now(), "view rebuilt")
            .unwrap();
        let rebuilt = publish(&shared, vec![input("other screen", 100)]);
        assert_ne!(command.visual_session, rebuilt.visual_session);
        assert!(!rebuilt.messages.iter().any(|message| message.id == *id));
        let retained = business.read().unwrap();
        assert_eq!(retained.len(), 2);
        assert!(retained[0].messages.iter().any(|message| message.id == *id));
        assert!(
            shared
                .is_business_pending_at(id, command.captured_at)
                .unwrap()
        );
        let new_baseline = decisions.read().unwrap();
        assert_eq!(new_baseline.len(), 1);
        assert_eq!(new_baseline[0].visual_session, rebuilt.visual_session);
    }

    #[test]
    fn friend_suppression_clears_pending_revision_metadata_before_late_ignore() {
        let shared = ChatTimeline::new();
        friend(&shared, vec![input("history", 10)]);
        let unread = friend(&shared, vec![input("history", 10), input("@点哥 晴天", 40)]);
        let corrected = friend(&shared, vec![input("history", 10), input("@点歌 晴天", 40)]);
        let id = &unread.messages[1].id;
        assert!(
            shared
                .inner
                .state
                .lock()
                .unwrap()
                .latest_business_revisions
                .contains_key(id)
        );
        let suppression = shared.suppress_friend_business().unwrap();
        drop(suppression);
        shared
            .publish_no_chat(Instant::now(), "view rebuilt")
            .unwrap();
        assert!(!ignore_revision(&shared, &unread, 1));
        assert!(
            !shared
                .is_business_pending_at(id, corrected.captured_at)
                .unwrap()
        );
        assert!(
            !shared
                .inner
                .state
                .lock()
                .unwrap()
                .latest_business_revisions
                .contains_key(id)
        );
    }

    #[test]
    fn parameterless_command_locks_survive_routable_punctuation_revisions() {
        let shared = ChatTimeline::new();
        let first = publish(&shared, vec![input("甲:@下一首", 40)]);
        let message = &first.messages[0];
        shared
            .acknowledge_command(&message.id, &message.text, message.block.y)
            .unwrap();
        shared
            .publish_no_chat(Instant::now(), "view hidden")
            .unwrap();
        let restored = publish(&shared, vec![input("甲:@下一首。!", 40)]);
        assert!(
            !shared
                .is_business_pending(&restored.messages[0].id)
                .unwrap()
        );
        for noise in crate::text::OCR_TRAILING_NOISE {
            for command in ["下一首", "下一曲", "上一首", "暂停", "继续", "播放", "帮助"]
            {
                let text = format!("甲:@{command}");
                assert_eq!(
                    normalize_command_lock_text(&format!("{text}{noise}")),
                    normalize_command_lock_text(&text),
                    "noise={noise:?}"
                );
            }
        }
        assert_ne!(
            normalize_command_lock_text("甲:@切歌!"),
            normalize_command_lock_text("甲:@切歌")
        );
        assert_ne!(
            normalize_command_lock_text("甲:@停止!"),
            normalize_command_lock_text("甲:@停止")
        );
        assert_ne!(
            normalize_command_lock_text("甲:@点歌 HELP!"),
            normalize_command_lock_text("甲:@点歌 HELP")
        );
        assert_ne!(
            normalize_command_lock_text("甲:@拉黑 昵称!"),
            normalize_command_lock_text("甲:@拉黑 昵称")
        );
    }

    #[test]
    fn corrected_ignored_bubble_cannot_override_an_existing_command_lock() {
        let shared = ChatTimeline::new();
        let first = publish(
            &shared,
            vec![
                input("anchor", 0),
                primary("甲", "@点哥 晴天", 40),
                primary("甲", "@点歌 晴天", 70),
            ],
        );
        let ignored_id = &first.messages[1].id;
        execute(&shared, &first, 2);
        assert!(ignore_revision(&shared, &first, 1));
        let corrected = publish(
            &shared,
            vec![
                input("anchor", 0),
                primary("甲", "@点歌 晴天", 40),
                primary("甲", "@点歌 晴天", 70),
            ],
        );
        assert_eq!(&corrected.messages[1].id, ignored_id);
        assert_ne!(corrected.messages[1].id, corrected.messages[2].id);
        assert!(!corrected.messages[1].is_new);
        assert!(!shared.is_business_pending(ignored_id).unwrap());
        assert!(claim(&shared, &corrected.messages[1], corrected.captured_at).is_none());
        assert!(!ignore_revision(&shared, &first, 1));
        assert_eq!(
            shared
                .command_lock_texts_for_test(&ChatIdentity::PrimaryHall)
                .len(),
            1
        );
        let state = shared.inner.state.lock().unwrap();
        assert!(!state.ignored_revisions.contains_key(ignored_id));
        assert!(!state.latest_business_revisions.contains_key(ignored_id));
    }

    #[test]
    fn late_unrecognized_completion_respects_current_lock_even_after_tracker_reset() {
        for hold_claim in [false, true] {
            for rebuild_before_completion in [false, true] {
                let shared = ChatTimeline::new();
                let first = publish(
                    &shared,
                    vec![
                        input("anchor", 0),
                        primary("甲", "@点哥 晴天", 40),
                        primary("甲", "@点歌 晴天", 70),
                    ],
                );
                let ignored_id = &first.messages[1].id;
                let active = if hold_claim {
                    Some(claim(&shared, &first.messages[1], first.captured_at).unwrap())
                } else {
                    None
                };
                execute(&shared, &first, 2);
                let corrected = publish(
                    &shared,
                    vec![
                        input("anchor", 0),
                        primary("甲", "@点歌 晴天", 40),
                        primary("甲", "@点歌 晴天", 70),
                    ],
                );
                assert_eq!(&corrected.messages[1].id, ignored_id);
                if rebuild_before_completion {
                    shared
                        .publish_no_chat(Instant::now(), "view rebuilt")
                        .unwrap();
                }
                if let Some(active) = active {
                    active.complete_unrecognized(None, None).unwrap();
                } else {
                    assert!(ignore_revision(&shared, &first, 1));
                }
                assert!(!shared.is_business_pending(ignored_id).unwrap());
                assert!(claim(&shared, &corrected.messages[1], corrected.captured_at).is_none());
                assert!(!ignore_revision(&shared, &first, 1));
                assert_eq!(
                    shared
                        .command_lock_texts_for_test(&ChatIdentity::PrimaryHall)
                        .len(),
                    1
                );
                let state = shared.inner.state.lock().unwrap();
                assert!(!state.ignored_revisions.contains_key(ignored_id));
                assert!(!state.latest_business_revisions.contains_key(ignored_id));
            }
        }
    }

    #[test]
    fn no_chat_refresh_errors_are_typed_without_relabeling_capture_failures() {
        let shared = ChatTimeline::new();
        let refresh = shared.request_refresh().unwrap();
        shared
            .publish_no_chat(Instant::now(), "chat temporarily hidden")
            .unwrap();
        let no_chat = shared
            .wait_for_snapshot(refresh, Duration::ZERO)
            .unwrap_err();
        assert_eq!(no_chat.to_string(), "chat temporarily hidden");
        assert!(no_chat.downcast_ref::<NoChatRefreshError>().is_some());
        let refresh = shared.request_refresh().unwrap();
        shared.report_failure("OCR inference failed").unwrap();
        let failure = shared
            .wait_for_snapshot(refresh, Duration::ZERO)
            .unwrap_err();
        assert_eq!(failure.to_string(), "OCR inference failed");
        assert!(failure.downcast_ref::<NoChatRefreshError>().is_none());
    }

    #[test]
    fn discard_publishes_to_decisions_before_routine_ack_without_business_race() {
        let shared = ChatTimeline::new();
        let baseline = friend(&shared, vec![input("history", 10)]);
        let business = shared.subscribe().unwrap();
        let decision = shared.subscribe_chat(baseline.chat.clone()).unwrap();
        let guard = shared.suppress_friend_business().unwrap();
        let token = shared.request_refresh().unwrap();
        let producer = shared.clone();
        let published = std::thread::spawn(move || {
            friend(
                &producer,
                vec![input("history", 10), input("discard command", 40)],
            )
        })
        .join()
        .unwrap();
        // Listener wins the race against the routine's post-publication ack.
        let queued = business.read().unwrap();
        assert_eq!(queued.len(), 1);
        let id = &published.messages[1].id;
        assert!(!shared.is_business_pending(id).unwrap());
        assert!(!shared.acknowledge_business(id).unwrap());
        assert_eq!(
            decision.read().unwrap()[0].messages[1].text,
            "discard command"
        );
        assert!(
            shared
                .wait_for_snapshot(token, Duration::ZERO)
                .unwrap()
                .is_some()
        );
        drop(guard);
        assert!(!shared.deliver_unread_baseline(&published, id).unwrap());
        let later = friend(
            &shared,
            vec![
                input("history", 10),
                input("discard command", 40),
                input("live command", 70),
            ],
        );
        assert_eq!(&later.messages[1].id, id);
        assert!(!shared.is_business_pending(id).unwrap());
        assert!(shared.is_business_pending(&later.messages[2].id).unwrap());
    }

    #[test]
    fn discard_cancels_queued_claims_and_nested_guards_preserve_suppression() {
        let shared = ChatTimeline::new();
        friend(&shared, vec![input("history", 10)]);
        let queued = friend(&shared, vec![input("history", 10), input("queued", 40)]);
        assert!(shared.is_business_pending(&queued.messages[1].id).unwrap());
        let outer = shared.suppress_friend_business().unwrap();
        let inner = shared.suppress_friend_business().unwrap();
        drop(outer);
        assert!(!shared.acknowledge_business(&queued.messages[1].id).unwrap());
        let pending_capture = Instant::now();
        drop(inner);
        // A frame captured while guarded can finish OCR after guard release.
        let delayed = shared
            .publish(
                pending_capture,
                queued.chat,
                vec![
                    RecognizedChatMessage {
                        message_type: "pink".into(),
                        ..input("history", 10)
                    },
                    RecognizedChatMessage {
                        message_type: "pink".into(),
                        ..input("queued", 40)
                    },
                    RecognizedChatMessage {
                        message_type: "pink".into(),
                        ..input("delayed", 70)
                    },
                ],
            )
            .unwrap();
        assert!(!shared.is_business_pending(&delayed.messages[2].id).unwrap());
        let later = friend(
            &shared,
            vec![
                input("history", 10),
                input("queued", 40),
                input("delayed", 70),
            ],
        );
        assert!(
            later
                .messages
                .iter()
                .all(|message| !shared.is_business_pending(&message.id).unwrap())
        );
    }

    #[test]
    fn deliberate_first_friend_unread_delivers_only_selected_shared_id_once() {
        let shared = ChatTimeline::new();
        let listener = shared.subscribe().unwrap();
        let baseline = friend(
            &shared,
            vec![input("old command", 10), input("unread command", 40)],
        );
        let id = &baseline.messages[1].id;
        assert!(baseline.messages.iter().all(|message| !message.is_new));
        // The listener may already have drained/skipped the first-view baseline.
        listener.read().unwrap();
        assert!(shared.deliver_unread_baseline(&baseline, id).unwrap());
        assert!(!shared.deliver_unread_baseline(&baseline, id).unwrap());
        let revision = listener.read().unwrap();
        assert_eq!(revision.len(), 1);
        assert_eq!(revision[0].messages.len(), 1);
        assert_eq!(&revision[0].messages[0].id, id);
        assert!(
            !shared
                .is_business_pending(&baseline.messages[0].id)
                .unwrap()
        );
        let repeated = friend(
            &shared,
            vec![input("old command", 10), input("unread command", 40)],
        );
        assert_eq!(&repeated.messages[1].id, id);
        assert!(shared.acknowledge_business(id).unwrap());
        assert!(!shared.acknowledge_business(id).unwrap());
        assert!(!shared.deliver_unread_baseline(&baseline, id).unwrap());
        assert!(
            !shared
                .is_business_pending(&listener.read().unwrap()[0].messages[1].id)
                .unwrap()
        );
    }

    #[test]
    fn failed_business_claim_restores_eligibility_but_discard_wins_rollback() {
        let shared = ChatTimeline::new();
        friend(&shared, vec![input("history", 10)]);
        let snapshot = friend(&shared, vec![input("history", 10), input("command", 40)]);
        let message = &snapshot.messages[1];
        let id = &message.id;
        let failed_dispatch = claim(&shared, message, snapshot.captured_at).unwrap();
        let repeated = friend(&shared, vec![input("history", 10), input("command", 40)]);
        assert!(claim(&shared, message, repeated.captured_at).is_none());
        drop(failed_dispatch);
        assert!(shared.is_business_pending(id).unwrap());
        let failed_dispatch = claim(&shared, message, repeated.captured_at).unwrap();
        let discard = shared.suppress_friend_business().unwrap();
        drop(discard);
        drop(failed_dispatch);
        assert!(!shared.is_business_pending(id).unwrap());
        let repeated = friend(&shared, vec![input("history", 10), input("command", 40)]);
        assert!(claim(&shared, message, repeated.captured_at).is_none());
    }

    #[test]
    fn discard_first_friend_baseline_cannot_be_promoted_after_release() {
        let shared = ChatTimeline::new();
        let guard = shared.suppress_friend_business().unwrap();
        let baseline = friend(&shared, vec![input("discard", 10)]);
        drop(guard);
        assert!(
            !shared
                .deliver_unread_baseline(&baseline, &baseline.messages[0].id)
                .unwrap()
        );
        let later = friend(&shared, vec![input("discard", 10)]);
        assert!(!shared.is_business_pending(&later.messages[0].id).unwrap());
    }

    #[test]
    fn atomic_subscription_excludes_old_passive_baseline() {
        let shared = ChatTimeline::new();
        let first = publish(&shared, vec![input("ordinary ignored text", 10)]);
        assert!(first.messages[0].sender.is_none());
        // v3：锁为空的 initial 帧把所有消息都标成新（启动首屏由 chat_baseline_primed 兜底）。
        assert!(first.messages[0].is_new);
        let (baseline, subscriber) = shared.subscribe_with_snapshot().unwrap();
        assert_eq!(baseline.unwrap().messages[0].id, first.messages[0].id);
        assert!(subscriber.read().unwrap().is_empty());
        publish(&shared, vec![input("ordinary ignored text", 10)]);
        assert_eq!(subscriber.read().unwrap().len(), 1);
    }

    #[test]
    fn independent_readers_retain_more_than_sixty_four_revisions() {
        let shared = ChatTimeline::new();
        let slow = shared.subscribe().unwrap();
        let fast = shared.subscribe().unwrap();
        let dropped = shared.subscribe().unwrap();
        drop(dropped);
        for revision in 0..100 {
            publish(&shared, vec![input(&format!("revision {revision}"), 10)]);
            assert_eq!(fast.read().unwrap().len(), 1);
        }
        let retained = slow.read().unwrap();
        assert_eq!(retained.len(), 100);
        assert_eq!(retained[0].messages[0].id, retained[99].messages[0].id);
        assert!(slow.read().unwrap().is_empty());
    }

    #[test]
    fn overflow_is_explicit_and_new_subscription_is_healthy() {
        let shared = ChatTimeline::new();
        let slow = shared.subscribe().unwrap();
        for _ in 0..=RETAINED_CAPACITY {
            publish(&shared, vec![]);
        }
        assert!(slow.read().unwrap_err().to_string().contains("overflow"));
        assert!(slow.wait(Duration::ZERO).is_err());
        let healthy = shared.subscribe().unwrap();
        publish(&shared, vec![]);
        assert_eq!(healthy.read().unwrap().len(), 1);
        assert!(slow.read().is_err());
    }

    #[test]
    fn refresh_requires_captured_frame_after_request_fence_including_empty_ocr() {
        let shared = ChatTimeline::new();
        let old_time = Instant::now();
        shared
            .publish(old_time, ChatIdentity::PrimaryHall, vec![input("old", 10)])
            .unwrap();
        let token = shared.request_refresh().unwrap();
        shared
            .publish(
                old_time,
                ChatIdentity::PrimaryHall,
                vec![input("stale", 10)],
            )
            .unwrap();
        assert!(shared.pending_refresh().unwrap().is_some());
        assert!(
            shared
                .wait_for_snapshot(token, Duration::ZERO)
                .unwrap()
                .is_none()
        );
        let fresh = publish(&shared, vec![]);
        let baseline = shared
            .wait_for_snapshot(token, Duration::ZERO)
            .unwrap()
            .unwrap();
        assert_eq!(baseline.captured_at, fresh.captured_at);
        assert!(baseline.messages.is_empty());
        assert!(shared.pending_refresh().unwrap().is_none());
    }

    #[test]
    fn request_tokens_keep_their_own_serviced_snapshots() {
        let shared = ChatTimeline::new();
        let one = shared.request_refresh().unwrap();
        let first = publish(&shared, vec![input("first", 10)]);
        let two = shared.request_refresh().unwrap();
        let second = publish(&shared, vec![input("second", 10)]);
        assert_eq!(
            shared
                .wait_for_snapshot(one, Duration::ZERO)
                .unwrap()
                .unwrap()
                .captured_at,
            first.captured_at
        );
        assert_eq!(
            shared
                .wait_for_snapshot(two, Duration::ZERO)
                .unwrap()
                .unwrap()
                .captured_at,
            second.captured_at
        );
    }

    #[test]
    fn publication_wakes_reader_and_refresh_waiters() {
        let shared = ChatTimeline::new();
        let subscriber = shared.subscribe().unwrap();
        let token = shared.request_refresh().unwrap();
        let producer = shared.clone();
        let thread = std::thread::spawn(move || publish(&producer, vec![input("observed", 10)]));
        assert!(subscriber.wait(Duration::from_secs(2)).unwrap());
        assert!(
            shared
                .wait_for_snapshot(token, Duration::from_secs(2))
                .unwrap()
                .is_some()
        );
        thread.join().unwrap();
    }

    #[test]
    fn capture_failure_is_sticky_only_for_existing_readers_and_requests() {
        let shared = ChatTimeline::new();
        let failed = shared.subscribe().unwrap();
        let old_request = shared.request_refresh().unwrap();
        shared.report_failure("capture unavailable").unwrap();
        assert!(failed.read().is_err());
        assert!(
            shared
                .wait_for_snapshot(old_request, Duration::ZERO)
                .is_err()
        );
        let healthy = shared.subscribe().unwrap();
        let new_request = shared.request_refresh().unwrap();
        publish(&shared, vec![input("healthy", 10)]);
        assert_eq!(healthy.read().unwrap().len(), 1);
        assert!(
            shared
                .wait_for_snapshot(new_request, Duration::ZERO)
                .unwrap()
                .is_some()
        );
        assert!(failed.read().is_err());
    }

    #[test]
    fn transient_capture_failure_preserves_business_but_poison_decision() {
        let shared = ChatTimeline::new();
        let business = shared.subscribe_business().unwrap();
        let decision = shared.subscribe().unwrap();
        shared
            .report_failure("initial capture unavailable")
            .unwrap();

        publish(&shared, vec![input("healthy", 10)]);

        assert_eq!(business.read().unwrap().len(), 1);
        assert!(decision.read().is_err());
    }

    #[test]
    fn no_chat_invalidates_session_without_poisoning_bound_decisions() {
        let shared = ChatTimeline::new();
        let baseline = publish(&shared, vec![input("first", 10)]);
        let business = shared.subscribe().unwrap();
        let decision = shared.subscribe().unwrap();
        decision
            .bind_chat(baseline.chat.clone(), baseline.visual_session)
            .unwrap();
        let token = shared.request_refresh().unwrap();
        shared
            .publish_no_chat(Instant::now(), "chat disappeared")
            .unwrap();
        assert!(shared.latest().unwrap().is_none());
        assert!(shared.wait_for_snapshot(token, Duration::ZERO).is_err());
        // 聊天视图瞬态消失只作废会话，不再毒化确认读者；机器人自己发言就会触发这种瞬态，
        // 毒化会让窗口被自己的回复杀死。
        assert!(decision.read().unwrap().is_empty());
        assert!(business.read().unwrap().is_empty());
        let after = publish(&shared, vec![input("first", 10)]);
        assert_ne!(after.visual_session, baseline.visual_session);
        assert_eq!(business.read().unwrap().len(), 1);
        // 同一聊天恢复后改绑，确认读者继续收到后续快照。
        assert_eq!(decision.read().unwrap().len(), 1);
        assert_eq!(decision.read().unwrap().len(), 0);
    }

    #[test]
    fn rebuilt_visual_session_keeps_bound_decision_reader_alive() {
        let shared = ChatTimeline::new();
        let baseline = publish(&shared, vec![input("候选", 10)]);
        let decision = shared.subscribe().unwrap();
        decision
            .bind_chat(baseline.chat.clone(), baseline.visual_session)
            .unwrap();
        // 机器人自己发出候选提示会让聊天区瞬态消失（输入框获焦、面板滚动），
        // 共享 OCR 因此清空 tracker；聊天重新出现时会话号自增，但聊天身份没变。
        shared
            .publish_no_chat(Instant::now(), "bot reply is scrolling the chat")
            .unwrap();
        assert!(decision.read().unwrap().is_empty());
        let rebuilt = publish(&shared, vec![input("候选", 10), input("确认", 40)]);
        assert_ne!(rebuilt.visual_session, baseline.visual_session);
        assert_eq!(rebuilt.chat, baseline.chat);
        let snapshots = decision.read().unwrap();
        assert_eq!(snapshots.len(), 1, "改绑后应继续收到新会话快照");
        assert_eq!(snapshots[0].messages.len(), 2);
    }

    #[test]
    fn refresh_after_other_hall_panel_requests_a_new_frame() {
        let shared = ChatTimeline::new();
        let first_refresh = shared.request_refresh().unwrap();
        let secondary = shared
            .publish(
                Instant::now(),
                ChatIdentity::SecondaryHall,
                vec![input("panel transition", 10)],
            )
            .unwrap();
        assert_eq!(
            shared
                .wait_for_snapshot(first_refresh, Duration::ZERO)
                .unwrap()
                .unwrap()
                .chat,
            ChatIdentity::SecondaryHall
        );
        let next_refresh = shared
            .refresh_after_other_hall_panel(&ChatIdentity::PrimaryHall, &secondary)
            .unwrap()
            .expect("opposite panel must rearm the baseline request");
        assert_ne!(first_refresh, next_refresh);

        let primary = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![input("target baseline", 10)],
            )
            .unwrap();
        assert_eq!(
            shared
                .wait_for_snapshot(next_refresh, Duration::ZERO)
                .unwrap()
                .unwrap()
                .chat,
            primary.chat
        );
    }

    #[test]
    fn other_hall_panel_does_not_end_bound_decision_reader() {
        let shared = ChatTimeline::new();
        let baseline = publish(&shared, vec![input("候选", 10)]);
        let decision = shared.subscribe().unwrap();
        decision
            .bind_chat(ChatIdentity::PrimaryHall, baseline.visual_session)
            .unwrap();
        // 队列里先放一条本身份消息，用来验证随后的“临时经过”不会把它清掉。
        publish(&shared, vec![input("候选", 10), input("确认", 40)]);
        // 一级大厅下发消息必然经过一次二级面板：那是同一大厅的另一块面板，不是换会话。
        shared
            .publish(
                Instant::now(),
                ChatIdentity::SecondaryHall,
                vec![input("面板里的闲聊", 10)],
            )
            .unwrap();
        let queued = decision.read().unwrap();
        assert_eq!(queued.len(), 1, "另一块大厅面板不得清空已排队的本身份消息");
        assert_eq!(queued[0].chat, ChatIdentity::PrimaryHall);
        assert_eq!(queued[0].messages[1].text, "确认");
        // 原绑定仍然活着：主大厅后续消息照常到达。
        publish(&shared, vec![input("后续", 60)]);
        let snapshots = decision.read().unwrap();
        assert_eq!(snapshots.len(), 1, "窗口必须继续接收它自己身份的消息");
        assert_eq!(snapshots[0].chat, ChatIdentity::PrimaryHall);
        assert_eq!(snapshots[0].messages[0].text, "后续");
    }

    #[test]
    fn other_hall_panel_never_returns_as_decision_input() {
        let shared = ChatTimeline::new();
        let baseline = publish(&shared, vec![input("候选", 10)]);
        let decision = shared.subscribe().unwrap();
        decision
            .bind_chat(ChatIdentity::PrimaryHall, baseline.visual_session)
            .unwrap();
        shared
            .publish(
                Instant::now(),
                ChatIdentity::SecondaryHall,
                vec![input("@1", 20)],
            )
            .unwrap();
        assert!(
            decision.read().unwrap().is_empty(),
            "另一块面板的气泡不得作为本窗口的确认输入返回"
        );
    }

    #[test]
    fn decision_reader_waits_through_long_foreign_state_and_no_chat() {
        let shared = ChatTimeline::new();
        let baseline = publish(&shared, vec![input("候选", 10)]);
        let decision = shared.subscribe().unwrap();
        decision
            .bind_chat(ChatIdentity::PrimaryHall, baseline.visual_session)
            .unwrap();

        for _ in 0..32 {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::Friend(Arc::from("friend")),
                    vec![input("not a decision", 10)],
                )
                .unwrap();
        }
        assert!(decision.read().unwrap().is_empty());
        shared
            .publish_no_chat(Instant::now(), "transient view gap")
            .unwrap();
        assert!(decision.read().unwrap().is_empty());
        for _ in 0..32 {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::Friend(Arc::from("friend")),
                    vec![input("still not a decision", 10)],
                )
                .unwrap();
        }
        assert!(decision.read().unwrap().is_empty());

        publish(&shared, vec![input("valid decision", 20)]);
        let snapshots = decision.read().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].messages[0].text, "valid decision");
    }

    #[test]
    fn long_foreign_state_is_filtered_until_bound_identity_returns() {
        let shared = ChatTimeline::new();
        let baseline = publish(&shared, vec![input("候选", 10)]);
        let decision = shared.subscribe().unwrap();
        decision
            .bind_chat(ChatIdentity::PrimaryHall, baseline.visual_session)
            .unwrap();
        for index in 0..64 {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::Friend(Arc::from("friend")),
                    vec![input(&format!("foreign input {index}"), 20)],
                )
                .unwrap();
        }
        assert!(decision.read().unwrap().is_empty());
        publish(&shared, vec![input("valid decision", 30)]);
        let snapshots = decision.read().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].messages[0].text, "valid decision");
    }

    #[test]
    fn bind_accepts_other_hall_panel_baseline() {
        let shared = ChatTimeline::new();
        // 基线落在二级面板上：这是同一大厅的同一物理会话，不应判为“基线后换会话”。
        let baseline = shared
            .publish(
                Instant::now(),
                ChatIdentity::SecondaryHall,
                vec![input("候选", 10)],
            )
            .unwrap();
        let decision = shared.subscribe().unwrap();
        decision
            .bind_chat(ChatIdentity::PrimaryHall, baseline.visual_session)
            .unwrap();
        publish(&shared, vec![input("确认", 40)]);
        let snapshots = decision.read().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].chat, ChatIdentity::PrimaryHall);
    }

    #[test]
    fn long_foreign_state_is_filtered_for_decision_not_business_reader() {
        let shared = ChatTimeline::new();
        let business = shared.subscribe().unwrap();
        let decision = shared.subscribe_chat(ChatIdentity::PrimaryHall).unwrap();
        for y in [10, 20, 30, 40, 50] {
            shared
                .publish(
                    Instant::now(),
                    ChatIdentity::Friend(Arc::from("friend")),
                    vec![input("hello", y)],
                )
                .unwrap();
        }
        assert!(decision.read().unwrap().is_empty());
        assert_eq!(business.read().unwrap().len(), 5);
    }

    #[test]
    fn pending_ids_survive_scroll_and_ack_invalidates_queued_revisions() {
        let shared = ChatTimeline::new();
        publish(&shared, vec![input("baseline", 10)]);
        let business = shared.subscribe().unwrap();
        let fresh = publish(&shared, vec![input("baseline", 10), input("command", 40)]);
        let id = fresh.messages[1].id.clone();
        publish(&shared, vec![input("command", 10), input("new", 40)]);
        publish(&shared, vec![input("new", 10)]);
        assert!(shared.is_business_pending(&id).unwrap());
        assert!(shared.acknowledge_business(&id).unwrap());
        let queued = business.read().unwrap();
        assert!(queued[0].messages[1].is_new);
        assert!(!shared.is_business_pending(&id).unwrap());
    }

    /// 模拟一次"认领并派发成功提交"：命令锁只在 complete() 里写入，
    /// 且写入的文本/y 来自被派发消息自身。
    fn execute(shared: &ChatTimeline, snapshot: &ChatTimelineSnapshot, index: usize) {
        let message = &snapshot.messages[index];
        assert!(shared.is_business_pending(&message.id).unwrap());
        shared
            .claim_business(
                &message.id,
                snapshot.captured_at,
                &message.text,
                message.block.y,
            )
            .unwrap()
            .expect("pending message should be claimable")
            .complete_as_command()
            .unwrap();
    }

    /// 窗口切换/视图重建：聊天区瞬态消失，tracker 清空，下一帧 visual_session 自增。
    fn rebuild(shared: &ChatTimeline) {
        shared
            .publish_no_chat(Instant::now(), "view rebuilt")
            .unwrap();
    }

    fn command_lock_bucket_count(shared: &ChatTimeline) -> usize {
        shared.inner.state.lock().unwrap().command_locks.len()
    }

    #[test]
    fn v3_empty_lock_initial_frame_marks_every_message_new() {
        let shared = ChatTimeline::new();
        let first = publish(
            &shared,
            vec![primary("甲", "@命令1", 10), primary("乙", "@命令2", 40)],
        );
        assert!(
            first.messages.iter().all(|message| message.is_new),
            "锁为空 + initial：全部 is_new=true（新命令要执行）"
        );
        assert!(
            first
                .messages
                .iter()
                .all(|message| shared.is_business_pending(&message.id).unwrap())
        );
    }

    #[test]
    fn v3_example3_rebuilt_screen_of_only_executed_commands_executes_nothing() {
        let shared = ChatTimeline::new();
        let first = publish(
            &shared,
            vec![primary("甲", "@命令1", 40), primary("甲", "@命令2", 70)],
        );
        execute(&shared, &first, 0);
        execute(&shared, &first, 1);
        rebuild(&shared);
        let rebuilt = publish(
            &shared,
            vec![primary("甲", "@命令1", 10), primary("甲", "@命令2", 40)],
        );
        assert!(rebuilt.messages.iter().all(|message| !message.is_new));
        assert!(
            rebuilt
                .messages
                .iter()
                .all(|message| !shared.is_business_pending(&message.id).unwrap())
        );
    }

    /// 桶淘汰不能只断言 len<=32：必须证明“在用桶没被淘汰、新桶写进去了、
    /// 被淘汰的是一次性抖动桶”。真实运行时 PrimaryHall 每帧都被 publish 触碰，
    /// 因此它的活跃度持续刷新；OCR 好友名抖动产生的桶写一次后再也没人碰。
    #[test]
    fn v3_command_lock_chat_bucket_cap_evicts_idle_jitter_buckets_keeps_active_chat() {
        let shared = ChatTimeline::new();
        let first = publish(&shared, vec![primary("甲", "@命令", 40)]);
        execute(&shared, &first, 0);
        let primary_hall = ChatIdentity::PrimaryHall;
        let command_text = normalize_command_lock_text("[甲]:@命令");

        for index in 0..COMMAND_LOCK_CHAT_CAPACITY + 8 {
            let chat = ChatIdentity::Friend(Arc::from(format!("ocr-name-{index}").as_str()));
            {
                let mut state = shared.inner.state.lock().unwrap();
                record_command_lock_locked(&mut state, &chat, "@命令", 10);
            }
            // 等价于真实运行时 PrimaryHall 每帧被 publish 触碰。
            publish(&shared, vec![primary("甲", "@命令", 40)]);
        }

        let state = shared.inner.state.lock().unwrap();
        assert_eq!(
            state.command_locks.len(),
            COMMAND_LOCK_CHAT_CAPACITY,
            "命令锁聊天桶必须收敛到上界"
        );
        assert!(
            state.command_locks.contains_key(&primary_hall),
            "持续在用/刚被 publish 触碰的 PrimaryHall 桶不得被淘汰"
        );
        let newest = ChatIdentity::Friend(Arc::from(
            format!("ocr-name-{}", COMMAND_LOCK_CHAT_CAPACITY + 8).as_str(),
        ));
        drop(state);
        {
            let mut state = shared.inner.state.lock().unwrap();
            record_command_lock_locked(&mut state, &newest, "@新命令", 10);
            assert!(state.command_locks.contains_key(&newest), "新桶必须写入");
            assert!(
                state.command_locks.contains_key(&primary_hall),
                "淘汰后 PrimaryHall 仍必须在"
            );
            assert_eq!(state.command_locks.len(), COMMAND_LOCK_CHAT_CAPACITY);
            assert!(
                !state
                    .command_locks
                    .contains_key(&ChatIdentity::Friend(Arc::from("ocr-name-0"))),
                "最不活跃的抖动桶应被优先淘汰"
            );
        }
        assert!(
            shared
                .command_lock_texts_for_test(&primary_hall)
                .contains(&command_text),
            "PrimaryHall 的命令条目不能被淘汰波及"
        );
    }

    #[test]
    fn v3_committed_claim_tombstone_blocks_second_dispatch() {
        let shared = ChatTimeline::new();
        let built = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        let message = &built.messages[0];
        let id = message.id.clone();
        claim(&shared, message, built.captured_at)
            .expect("pending message should be claimable")
            .complete_as_command()
            .unwrap();
        {
            let state = shared.inner.state.lock().unwrap();
            assert!(
                state.business_claimed.contains(&id),
                "complete() 之后认领墓碑必须保留，否则同一 id 可以再次入队"
            );
            assert!(!state.business_pending.contains(&id));
        }
        assert!(claim(&shared, message, built.captured_at).is_none());
        let repeat = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        assert!(!repeat.messages[0].is_new);
        assert!(!shared.is_business_pending(&id).unwrap());
    }

    /// 缺陷 2：命令锁归一化必须折全角。OCR 会把半角冒号读成全角，不折就会让同一条
    /// 已执行命令在重建后以“全角版本”重新命中失败而执行两次。
    #[test]
    fn v3_fullwidth_and_halfwidth_punctuation_hit_the_same_command_lock_entry() {
        // 半角写入 -> 全角命中
        let shared = ChatTimeline::new();
        let first = publish(&shared, vec![primary("甲", "@点歌 起风了", 40)]);
        execute(&shared, &first, 0);
        rebuild(&shared);
        let fullwidth = publish(&shared, vec![primary_fullwidth("甲", "@点歌 起风了", 40)]);
        assert!(
            !fullwidth.messages[0].is_new,
            "全角冒号必须命中半角写入的命令锁条目"
        );
        assert!(
            !shared
                .is_business_pending(&fullwidth.messages[0].id)
                .unwrap()
        );

        // 全角写入 -> 半角命中
        let shared = ChatTimeline::new();
        let first = publish(&shared, vec![primary_fullwidth("甲", "@点歌 起风了", 40)]);
        execute(&shared, &first, 0);
        rebuild(&shared);
        let halfwidth = publish(&shared, vec![primary("甲", "@点歌 起风了", 40)]);
        assert!(
            !halfwidth.messages[0].is_new,
            "半角冒号必须命中全角写入的命令锁条目"
        );
    }
}

fn same_rect(left: Rect, right: Rect) -> bool {
    left.x == right.x
        && left.y == right.y
        && left.width == right.width
        && left.height == right.height
}

/// The existing stream uses suffix/prefix alignment; here relative block geometry
/// additionally keeps repeated text bubbles distinct during layout changes.
fn sequence_overlap(previous: &[ChatTimelineMessage], current: &[RecognizedChatMessage]) -> usize {
    // A retained first bubble anchors corrections of later bubbles. Match the
    // message sequence and shared vertical scroll delta, not layout-derived x/size.
    if previous.len() == current.len() && !current.is_empty() && previous[0].text == current[0].text
    {
        let delta = i64::from(current[0].block.y) - i64::from(previous[0].block.y);
        if previous.iter().zip(current).all(|(left, right)| {
            left.message_type == right.message_type
                && (left.sender == right.sender || left.sender.is_none() || right.sender.is_none())
                && i64::from(right.block.y) - i64::from(left.block.y) == delta
        }) {
            return current.len();
        }
    }
    (1..=previous.len().min(current.len()))
        .rev()
        .find(|overlap| {
            let old = &previous[previous.len() - overlap..];
            let delta = i64::from(current[0].block.y) - i64::from(old[0].block.y);
            old.iter().zip(&current[..*overlap]).all(|(left, right)| {
                left.message_type == right.message_type
                    && left.text == right.text
                    && (left.sender == right.sender
                        || left.sender.is_none()
                        || right.sender.is_none())
                    && i64::from(right.block.y) - i64::from(left.block.y) == delta
            })
        })
        .unwrap_or(0)
}
