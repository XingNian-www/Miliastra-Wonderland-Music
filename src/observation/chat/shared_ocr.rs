use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use super::{BubbleSequence, ChatIdentity, ObservedChatMessageId, VisualSessionId};
use crate::ui::geometry::Rect;

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
pub(crate) struct SharedChatInput {
    pub(crate) text: String,
    pub(crate) sender: Option<String>,
    pub(crate) raw_sender: Option<String>,
    pub(crate) message_type: String,
    pub(crate) block: Rect,
}

#[derive(Clone, Debug)]
pub(crate) struct SharedChatMessage {
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
pub(crate) struct ChatOcrSnapshot {
    pub(crate) visual_session: VisualSessionId,
    pub(crate) captured_at: Instant,
    pub(crate) chat: ChatIdentity,
    pub(crate) messages: Vec<SharedChatMessage>,
}

#[derive(Clone)]
pub(crate) struct SharedChatOcr {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    latest: Option<ChatOcrSnapshot>,
    trackers: HashMap<ChatIdentity, Tracker>,
    business_pending: HashSet<ObservedChatMessageId>,
    unread_baseline: HashSet<ObservedChatMessageId>,
    business_claimed: HashSet<ObservedChatMessageId>,
    command_locks: HashMap<ChatIdentity, CommandLock>,
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
    visible: Vec<SharedChatMessage>,
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
    fn matches(&self, message: &SharedChatMessage) -> bool {
        let text = normalize_command_lock_text(&message.text);
        self.entries
            .iter()
            .any(|entry| entry.matches(&text, message.block.y))
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
    fn release_missing(&mut self, observed: &[SharedChatMessage]) {
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
    queue: VecDeque<ChatOcrSnapshot>,
    failure: Option<String>,
    durable_business: bool,
}

struct Refresh {
    requested_at: Instant,
    outcome: Option<std::result::Result<Option<ChatOcrSnapshot>, String>>,
}

/// Each handle owns its queue; dropping it never changes another subscriber.
pub(crate) struct SharedChatSubscriber {
    shared: SharedChatOcr,
    id: u64,
}

/// A business claim is committed only after dispatch succeeds. On failure it
/// becomes eligible again unless a discard guard superseded the claim.
pub(crate) struct SharedBusinessClaim {
    shared: SharedChatOcr,
    id: ObservedChatMessageId,
    suppression_generation: u64,
    /// 被派发消息自身的文本与屏幕位置，认领时就捕获，绝不回头查当前 tracker
    /// （执行完成时气泡可能已经滚出 visible/latest）。
    text: String,
    y: i32,
    committed: bool,
}

impl SharedBusinessClaim {
    /// 作为命令派发成功的提交点：确认已处理，并把该消息自身的文本与 y 写进命令锁。
    pub(crate) fn complete_as_command(mut self) -> Result<()> {
        self.commit(true)
    }

    /// 非命令（普通闲聊/未路由/被禁用）的提交点：只确认已处理，**绝不写命令锁**。
    /// 缺陷 1：二级/好友路径若在这里也写锁，普通闲聊会把真命令条目 FIFO 挤出，
    /// 重建后仍在屏上的已执行命令会重跑。
    pub(crate) fn complete_ignored(mut self) -> Result<()> {
        self.commit(false)
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
        // 提交成功后认领墓碑必须保留：一旦 Drop 无条件移除 business_claimed，
        // 同一 id 就还能再次进入 business_pending，而下游去重只覆盖"已入队未启动"，
        // 执行完成后不再拦截，会造成真实重复执行。
        if self.committed {
            return;
        }
        if let Ok(mut state) = self.shared.inner.state.lock() {
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
    shared: SharedChatOcr,
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

impl SharedChatOcr {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    latest: None,
                    trackers: HashMap::new(),
                    business_pending: HashSet::new(),
                    unread_baseline: HashSet::new(),
                    business_claimed: HashSet::new(),
                    command_locks: HashMap::new(),
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
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.inner
            .state
            .lock()
            .map_err(|_| anyhow!("shared chat OCR state lock poisoned"))
    }

    pub(crate) fn latest(&self) -> Result<Option<ChatOcrSnapshot>> {
        Ok(self.lock()?.latest.clone())
    }

    pub(crate) fn subscribe(&self) -> Result<SharedChatSubscriber> {
        let mut state = self.lock()?;
        Ok(self.subscribe_locked(&mut state, None, false))
    }

    /// A business reader survives transient capture/OCR failures and retains its queue.
    pub(crate) fn subscribe_business(&self) -> Result<SharedChatSubscriber> {
        let mut state = self.lock()?;
        Ok(self.subscribe_locked(&mut state, None, true))
    }

    /// Reading the passive baseline and starting the queue is one atomic operation.
    pub(crate) fn subscribe_with_snapshot(
        &self,
    ) -> Result<(Option<ChatOcrSnapshot>, SharedChatSubscriber)> {
        let mut state = self.lock()?;
        let latest = state.latest.clone();
        let subscriber = self.subscribe_locked(&mut state, None, false);
        Ok((latest, subscriber))
    }

    pub(crate) fn subscribe_chat(&self, chat: ChatIdentity) -> Result<SharedChatSubscriber> {
        let mut state = self.lock()?;
        Ok(self.subscribe_locked(&mut state, Some(chat), false))
    }

    fn subscribe_locked(
        &self,
        state: &mut State,
        chat: Option<ChatIdentity>,
        durable_business: bool,
    ) -> SharedChatSubscriber {
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
        SharedChatSubscriber {
            shared: self.clone(),
            id,
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
                bail!("shared chat OCR refresh request capacity exceeded");
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
    ) -> Result<Option<ChatOcrSnapshot>> {
        let started = Instant::now();
        let mut state = self.lock()?;
        loop {
            let refresh = state.requests.get(&request).ok_or_else(|| {
                anyhow!("unknown or expired shared chat OCR refresh token {request}")
            })?;
            if let Some(outcome) = &refresh.outcome {
                return outcome.clone().map_err(|reason| anyhow!(reason));
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
                .map_err(|_| anyhow!("shared chat OCR state lock poisoned"))?;
            state = next;
        }
    }

    /// Publish only after OCR of this actual frame, including frames with zero messages.
    pub(crate) fn publish(
        &self,
        captured_at: Instant,
        chat: ChatIdentity,
        messages: Vec<SharedChatInput>,
    ) -> Result<ChatOcrSnapshot> {
        let mut state = self.lock()?;
        // A delayed frame must not roll the passive baseline or trackers backwards.
        if state
            .latest
            .as_ref()
            .is_some_and(|latest| latest.captured_at > captured_at)
        {
            bail!("shared chat OCR received an out-of-order frame");
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
        let tracker = state.trackers.get_mut(&chat).expect("tracker inserted");
        let initial = !tracker.initialized;
        let mut observed = tracker.observe(&chat, messages);
        let visual_session = tracker.session;
        if suppress {
            for message in &mut tracker.visible {
                message.is_new = false;
            }
        }
        // v3 命令锁：不做起点、不做传递性、不按 y 排序（marker 本就全局有序），
        // 也不依赖"命令样/@# 前缀"谓词。
        //   anchor_matched(msg) ⟺ 锁中存在 (normalize 文本一致 且 msg.y <= entry.y + 一行高)。
        //   initial 帧（Tracker 新 tracker）：is_new = !anchor_matched(msg)；
        //   非 initial 帧：保留 Tracker::observe 的 is_new，只把命中的强制压回未新，
        //                  绝不把未命中的置新。
        // 锁为空 + initial ⇒ 所有消息 is_new=true：新命令要执行，启动首屏由
        // chat_baseline_primed 另行兜底。
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
                if initial {
                    for message in &mut observed {
                        message.is_new = !lock.matches(message);
                    }
                } else {
                    for message in &mut observed {
                        if lock.matches(message) {
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
        state.unread_baseline.retain(|id| {
            id.chat != chat
                || observed.is_empty()
                || observed.iter().any(|message| message.id == *id)
        });
        for message in &mut observed {
            if suppress {
                message.is_new = false;
                state.business_pending.remove(&message.id);
                state.unread_baseline.remove(&message.id);
            } else if initial && matches!(chat, ChatIdentity::Friend(_)) {
                // 好友首屏（含窗口重建后的首帧）保持被动基线：只有用户点未读才交付。
                // 命令锁的 initial 判定不能改变这条既有语义，所以这里把 is_new 压回未新。
                message.is_new = false;
                state.business_pending.remove(&message.id);
                state.unread_baseline.insert(message.id.clone());
            } else if message.is_new && !state.business_claimed.contains(&message.id) {
                state.business_pending.insert(message.id.clone());
            }
        }
        let snapshot = ChatOcrSnapshot {
            captured_at,
            visual_session,
            chat,
            messages: observed,
        };
        for subscriber in state.subscribers.values_mut() {
            if subscriber.failure.is_some() {
                continue;
            }
            let chat_changed = subscriber
                .chat
                .as_ref()
                .is_some_and(|chat| *chat != snapshot.chat);
            if chat_changed {
                // 真正切换了聊天上下文（例如大厅 -> 好友私聊）：确认窗口必须结束，
                // 否则会把另一个会话里的命令当成当前窗口的确认。
                subscriber.failure = Some("visible chat identity changed".into());
                subscriber.queue.clear();
            } else if subscriber.queue.len() >= RETAINED_CAPACITY {
                subscriber.failure =
                    Some("shared chat OCR subscriber overflow (4096 snapshots)".into());
                subscriber.queue.clear();
            } else {
                let session_changed = subscriber
                    .session
                    .is_some_and(|session| session != snapshot.visual_session);
                if session_changed {
                    // 同一个聊天里的视图重建（机器人自己发言、面板滚动、重新申请帧）会让视觉会话号自增。
                    // 这不是错误：改绑到新会话、重置基线，读者继续等待，窗口不再被自己的回复杀死。
                    log::info!(
                        "共享聊天读者改绑新视觉会话: session={:?} -> {:?}",
                        subscriber.session,
                        snapshot.visual_session
                    );
                    subscriber.queue.clear();
                } else if subscriber.session.is_none() && subscriber.chat.is_some() {
                    // 聊天视图曾瞬态消失；同一聊天恢复后重新绑定，不丢后续消息。
                    log::info!("共享聊天读者在聊天恢复后重新绑定: chat={:?}", snapshot.chat);
                    subscriber.queue.clear();
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
            bail!("shared chat OCR received an out-of-order no-chat frame");
        }
        state.latest = None;
        // A disappeared view starts new visual sessions on its next appearance.
        state.trackers.clear();
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
            subscriber.queue.clear();
            subscriber.session = None;
        }
        complete_refreshes(&mut state, captured_at, Err(reason));
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
            request.outcome = Some(Err(reason.clone()));
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
        snapshot: &ChatOcrSnapshot,
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
        state.unread_baseline.remove(id);
        state
            .unread_delivery_fences
            .insert(id.clone(), revision.captured_at);
        state.business_pending.insert(id.clone());
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
                    "shared chat OCR subscriber overflow (4096 snapshots)",
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
        if state
            .unread_delivery_fences
            .get(id)
            .is_some_and(|fence| captured_at < *fence)
            || state.business_claimed.contains(id)
            || !state.business_pending.remove(id)
        {
            return Ok(None);
        }
        state.business_claimed.insert(id.clone());
        Ok(Some(SharedBusinessClaim {
            shared: self.clone(),
            id: id.clone(),
            suppression_generation: state.friend_suppression_generation,
            text: text.to_owned(),
            y,
            committed: false,
        }))
    }

    pub(crate) fn acknowledge_business(&self, id: &ObservedChatMessageId) -> Result<bool> {
        let mut state = self.lock()?;
        Ok(acknowledge_business_locked(&mut state, id))
    }

    /// 一级大厅命令路径的"真正派发成功"完成点：确认已处理，并用被派发消息自身的
    /// 文本与 y 写入命令锁。调用方必须只在成功派发点调用它；被禁用/被跳过/未路由的
    /// 路径只调 \`acknowledge_business\`，绝不能写锁。
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
    let found = state.business_pending.remove(id);
    if let Some(tracker) = state.trackers.get_mut(&id.chat) {
        for message in &mut tracker.visible {
            if message.id == *id {
                message.is_new = false;
            }
        }
    }
    if let Some(latest) = &mut state.latest {
        for message in &mut latest.messages {
            if message.id == *id {
                message.is_new = false;
            }
        }
    }
    found
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
    crate::text::normalize_chat_text(text)
        .chars()
        .flat_map(char::to_lowercase)
        .collect()
}

fn complete_refreshes(
    state: &mut State,
    captured_at: Instant,
    outcome: std::result::Result<Option<ChatOcrSnapshot>, String>,
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

impl SharedChatSubscriber {
    /// Bind after the fresh baseline without replacing the already-retained queue.
    pub(crate) fn bind_chat(&self, chat: ChatIdentity, session: VisualSessionId) -> Result<()> {
        let mut state = self.shared.lock()?;
        let matches = state
            .latest
            .as_ref()
            .is_some_and(|latest| latest.chat == chat && latest.visual_session == session);
        let subscriber = state
            .subscribers
            .get_mut(&self.id)
            .ok_or_else(|| anyhow!("shared chat OCR subscription closed"))?;
        if let Some(reason) = &subscriber.failure {
            bail!("{reason}");
        }
        if !matches {
            fail_subscription(subscriber, "visible chat changed after baseline");
            self.shared.inner.changed.notify_all();
            bail!("visible chat changed after baseline");
        }
        subscriber.chat = Some(chat);
        subscriber.session = Some(session);
        Ok(())
    }

    pub(crate) fn read(&self) -> Result<Vec<ChatOcrSnapshot>> {
        let mut state = self.shared.lock()?;
        let subscriber = state
            .subscribers
            .get_mut(&self.id)
            .ok_or_else(|| anyhow!("shared chat OCR subscription closed"))?;
        if let Some(reason) = &subscriber.failure {
            bail!("{reason}");
        }
        Ok(subscriber.queue.drain(..).collect())
    }

    pub(crate) fn wait(&self, timeout: Duration) -> Result<bool> {
        let started = Instant::now();
        let mut state = self.shared.lock()?;
        loop {
            let subscriber = state
                .subscribers
                .get(&self.id)
                .ok_or_else(|| anyhow!("shared chat OCR subscription closed"))?;
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
                .map_err(|_| anyhow!("shared chat OCR state lock poisoned"))?;
            state = next;
        }
    }
}

impl Drop for SharedChatSubscriber {
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
        input: Vec<SharedChatInput>,
    ) -> Vec<SharedChatMessage> {
        // Empty OCR frames are observable but do not erase the last reliable visual anchor.
        if input.is_empty() {
            return Vec::new();
        }
        let initial = !self.initialized;
        let overlap = sequence_overlap(&self.visible, &input);
        let start = self.visible.len().saturating_sub(overlap);
        let mut used = vec![false; self.visible.len()];
        let mut current = Vec::with_capacity(input.len());
        for (index, input) in input.into_iter().enumerate() {
            let matched = if index < overlap {
                Some(start + index)
            } else if overlap == 0 {
                // Same visual block can acquire corrected text or previously unknown sender.
                self.visible
                    .iter()
                    .enumerate()
                    .find(|(index, old)| {
                        !used[*index]
                            && old.message_type == input.message_type
                            && same_rect(old.block, input.block)
                    })
                    .map(|(index, _)| index)
            } else {
                None
            };
            let (id, is_new) = if let Some(index) = matched {
                used[index] = true;
                let old = &self.visible[index];
                (old.id.clone(), old.is_new)
            } else {
                let id = ObservedChatMessageId::new(
                    self.session,
                    chat.clone(),
                    BubbleSequence::new(self.next_sequence),
                );
                self.next_sequence += 1;
                (id, !initial)
            };
            current.push(SharedChatMessage {
                id,
                is_new,
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
impl SharedChatOcr {
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

    fn input(text: &str, y: i32) -> SharedChatInput {
        SharedChatInput {
            text: text.into(),
            sender: None,
            raw_sender: None,
            message_type: "ordinary".into(),
            block: Rect::new(10, y, 120, 20),
        }
    }

    fn publish(shared: &SharedChatOcr, messages: Vec<SharedChatInput>) -> ChatOcrSnapshot {
        shared
            .publish(Instant::now(), ChatIdentity::PrimaryHall, messages)
            .unwrap()
    }

    fn friend(shared: &SharedChatOcr, messages: Vec<SharedChatInput>) -> ChatOcrSnapshot {
        shared
            .publish(
                Instant::now(),
                ChatIdentity::Friend(Arc::from("observed name")),
                messages,
            )
            .unwrap()
    }

    /// 用被派发消息自身的文本与 y 发起一次业务认领。
    fn claim(
        shared: &SharedChatOcr,
        message: &SharedChatMessage,
        captured_at: Instant,
    ) -> Option<SharedBusinessClaim> {
        shared
            .claim_business(&message.id, captured_at, &message.text, message.block.y)
            .unwrap()
    }

    /// 真实一级大厅格式："[昵称]:@命令"，message_type 为 blue。
    fn primary(nickname: &str, command: &str, y: i32) -> SharedChatInput {
        SharedChatInput {
            message_type: "blue".into(),
            ..input(&format!("[{nickname}]:{command}"), y)
        }
    }

    /// 真实一级格式但用全角冒号："[昵称]：@命令"（OCR 会把半角冒号读成全角）。
    fn primary_fullwidth(nickname: &str, command: &str, y: i32) -> SharedChatInput {
        SharedChatInput {
            message_type: "blue".into(),
            ..input(&format!("[{nickname}]：{command}"), y)
        }
    }

    #[test]
    fn discard_publishes_to_decisions_before_routine_ack_without_business_race() {
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
                    input("history", 10),
                    input("queued", 40),
                    input("delayed", 70),
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
    fn baseline_promotion_uses_latest_ocr_and_skips_stale_queued_text() {
        let shared = SharedChatOcr::new();
        let listener = shared.subscribe().unwrap();
        let baseline = friend(&shared, vec![input("misread", 10)]);
        let selected = friend(&shared, vec![input("correct command", 10)]);
        let latest = friend(&shared, vec![input("latest corrected command", 10)]);
        let message = &selected.messages[0];
        let id = &message.id;
        assert_eq!(id, &baseline.messages[0].id);
        assert!(shared.deliver_unread_baseline(&selected, id).unwrap());
        let queue = listener.read().unwrap();
        assert_eq!(queue.last().unwrap().captured_at, latest.captured_at);
        assert!(
            queue
                .windows(2)
                .all(|pair| pair[0].captured_at <= pair[1].captured_at)
        );
        assert!(claim(&shared, message, baseline.captured_at).is_none());
        assert!(claim(&shared, message, selected.captured_at).is_none());
        let claim_handle = claim(&shared, message, latest.captured_at).unwrap();
        assert_eq!(
            queue.last().unwrap().messages[0].text,
            "latest corrected command"
        );
        claim_handle.complete_as_command().unwrap();
        assert!(claim(&shared, message, latest.captured_at).is_none());
    }

    #[test]
    fn discard_first_friend_baseline_cannot_be_promoted_after_release() {
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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
    fn identity_change_ends_filtered_reader_not_business_reader() {
        let shared = SharedChatOcr::new();
        let business = shared.subscribe().unwrap();
        let decision = shared.subscribe_chat(ChatIdentity::PrimaryHall).unwrap();
        shared
            .publish(
                Instant::now(),
                ChatIdentity::Friend(Arc::from("friend")),
                vec![input("hello", 10)],
            )
            .unwrap();
        assert!(decision.read().is_err());
        assert_eq!(business.read().unwrap().len(), 1);
    }

    #[test]
    fn repeated_text_and_ocr_revisions_keep_distinct_visual_ids() {
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![input("anchor", 10), input("same", 40)]);
        let next = publish(&shared, vec![input("same", 10), input("same", 40)]);
        assert_eq!(next.messages[0].id, first.messages[1].id);
        assert_ne!(next.messages[0].id, next.messages[1].id);
        assert!(next.messages[1].is_new);
        let mut correction = input("corrected OCR", 40);
        correction.sender = Some("actor".into());
        correction.raw_sender = Some("actor (remark)".into());
        let revised = publish(&shared, vec![input("same", 10), correction]);
        assert_eq!(revised.messages[1].id, next.messages[1].id);
        assert!(revised.messages[1].is_new);
        shared
            .acknowledge_business(&revised.messages[1].id)
            .unwrap();
        assert!(!shared.is_business_pending(&revised.messages[1].id).unwrap());
        let repeat = publish(&shared, vec![input("same", 10), input("corrected OCR", 40)]);
        assert!(!repeat.messages[1].is_new);
    }

    #[test]
    fn pending_ids_survive_scroll_and_ack_invalidates_queued_revisions() {
        let shared = SharedChatOcr::new();
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
    fn execute(shared: &SharedChatOcr, snapshot: &ChatOcrSnapshot, index: usize) {
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
    fn rebuild(shared: &SharedChatOcr) {
        shared
            .publish_no_chat(Instant::now(), "view rebuilt")
            .unwrap();
    }

    fn command_lock_bucket_count(shared: &SharedChatOcr) -> usize {
        shared.inner.state.lock().unwrap().command_locks.len()
    }

    #[test]
    fn v3_empty_lock_initial_frame_marks_every_message_new() {
        let shared = SharedChatOcr::new();
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
    fn v3_example1_rebuilt_screen_executes_only_command2() {
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        execute(&shared, &first, 0);
        rebuild(&shared);
        let rebuilt = publish(
            &shared,
            vec![primary("甲", "@命令1", 10), primary("甲", "@命令2", 40)],
        );
        assert!(
            !rebuilt.messages[0].is_new,
            "锚点命中：已执行命令不重复派发"
        );
        assert!(rebuilt.messages[1].is_new, "未锚定的命令2 是新命令");
        assert!(!shared.is_business_pending(&rebuilt.messages[0].id).unwrap());
        assert!(shared.is_business_pending(&rebuilt.messages[1].id).unwrap());
    }

    #[test]
    fn v3_example2_resent_same_text_same_position_is_suppressed() {
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        execute(&shared, &first, 0);
        rebuild(&shared);
        // 已知取舍：重发同文本且仍落在同一位置（28px 内）会被墓碑抑制。
        let rebuilt = publish(
            &shared,
            vec![primary("甲", "@命令1", 40), primary("甲", "@命令2", 70)],
        );
        assert!(!rebuilt.messages[0].is_new, "同位置同文本的重发被抑制");
        assert!(rebuilt.messages[1].is_new, "只执行命令2");
        assert!(!shared.is_business_pending(&rebuilt.messages[0].id).unwrap());
        assert!(shared.is_business_pending(&rebuilt.messages[1].id).unwrap());
    }

    #[test]
    fn v3_example3_rebuilt_screen_of_only_executed_commands_executes_nothing() {
        let shared = SharedChatOcr::new();
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

    /// 非 initial 分支的真实场景：同一 visual session 内旧气泡滚出，新气泡在原锚点
    /// 同一 y 处重发——Tracker 会给出 **fresh id + is_new=true**，只有非 initial 分支的
    /// “命中锁则压回未新”才能抑制它。（旧用例因为 Tracker 复用同一 id/is_new=false 而空过。）
    #[test]
    fn v3_non_initial_fresh_bubble_at_anchor_position_is_suppressed() {
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        execute(&shared, &first, 0);
        // 命令滚出：同会话内换一条位置完全不同的消息，Tracker 不会复用命令 id。
        publish(&shared, vec![primary("乙", "中间消息", 200)]);
        // 新气泡在原锚点同一 y 处重发。
        let repeat = publish(
            &shared,
            vec![primary("乙", "中间消息", 200), primary("甲", "@命令1", 40)],
        );
        let command = repeat
            .messages
            .iter()
            .find(|message| message.text.contains("@命令1"))
            .expect("resurfaced command");
        assert_ne!(
            command.id, first.messages[0].id,
            "旧气泡已滚出，重发必须是 fresh id，否则用例又是空过"
        );
        assert!(
            !command.is_new,
            "非 initial 分支必须把命中锁的 fresh 气泡压回未新"
        );
        assert!(!shared.is_business_pending(&command.id).unwrap());
    }

    #[test]
    fn v3_ce1_chatter_above_executed_command_does_not_drag_it_back() {
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        execute(&shared, &first, 0);
        rebuild(&shared);
        let rebuilt = publish(
            &shared,
            vec![input("[丙]:今天天气不错", 10), primary("甲", "@命令1", 40)],
        );
        assert!(rebuilt.messages[0].is_new, "闲聊本身是新消息，交由路由过滤");
        assert!(
            !rebuilt.messages[1].is_new,
            "普通闲聊在已执行命令上方不再连坐"
        );
        assert!(!shared.is_business_pending(&rebuilt.messages[1].id).unwrap());
    }

    #[test]
    fn v3_ce2_unrouted_mention_does_not_drag_executed_command_back() {
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        execute(&shared, &first, 0);
        rebuild(&shared);
        let rebuilt = publish(
            &shared,
            vec![input("[惜月]:@惜月!", 10), primary("甲", "@命令1", 40)],
        );
        assert!(rebuilt.messages[0].is_new);
        assert!(
            !rebuilt.messages[1].is_new,
            "@惜月! 这类未路由 @ 文本不造成连坐"
        );
        assert!(!shared.is_business_pending(&rebuilt.messages[1].id).unwrap());
    }

    #[test]
    fn v3_ce4_rebuild_shift_down_within_one_line_still_matches() {
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        execute(&shared, &first, 0);
        rebuild(&shared);
        let shifted = publish(&shared, vec![primary("甲", "@命令1", 68)]);
        assert!(!shifted.messages[0].is_new, "整体下移 28px 内仍命中");
        rebuild(&shared);
        let beyond = publish(&shared, vec![primary("甲", "@命令1", 69)]);
        assert!(beyond.messages[0].is_new, "下移超过一行高就不命中");
    }

    #[test]
    fn v3_ce5_single_frame_ocr_jitter_keeps_entry_until_second_missing_frame() {
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        execute(&shared, &first, 0);
        // 第一帧抖动：末字误读，条目匹配不上，只累计 1 个缺失帧。
        publish(&shared, vec![primary("甲", "@命令l", 40)]);
        rebuild(&shared);
        let recovered = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        assert!(!recovered.messages[0].is_new, "单帧 OCR 抖动不得误删锁条目");
        assert!(
            !shared
                .is_business_pending(&recovered.messages[0].id)
                .unwrap()
        );
        // 边界：连续两个可信帧都判定缺失才真正释放。
        rebuild(&shared);
        publish(&shared, vec![primary("甲", "@命令l", 40)]);
        publish(&shared, vec![primary("甲", "@命令l", 40)]);
        assert_eq!(
            command_lock_bucket_count(&shared),
            0,
            "聊天桶清空后必须移除，避免无界增长"
        );
        rebuild(&shared);
        let replayed = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        assert!(
            replayed.messages[0].is_new,
            "连续两帧缺失后条目被释放，可重新派发"
        );
        assert!(
            shared
                .is_business_pending(&replayed.messages[0].id)
                .unwrap()
        );
    }

    #[test]
    fn v3_command_lock_keeps_only_five_entries_so_evicted_text_is_new_again() {
        let shared = SharedChatOcr::new();
        let executed = publish(
            &shared,
            vec![
                primary("甲", "@命令1", 40),
                primary("甲", "@命令2", 70),
                primary("甲", "@命令3", 100),
                primary("甲", "@命令4", 130),
                primary("甲", "@命令5", 160),
                primary("甲", "@命令6", 190),
            ],
        );
        for index in 0..6 {
            execute(&shared, &executed, index);
        }
        rebuild(&shared);
        // 命令1 已被挤出 5 条上限；命令6 仍在锁内。
        let rebuilt = publish(
            &shared,
            vec![primary("甲", "@命令1", 10), primary("甲", "@命令6", 40)],
        );
        assert!(rebuilt.messages[0].is_new, "被挤出上限的更早命令应重新派发");
        assert!(!rebuilt.messages[1].is_new, "仍在锁内的命令6 被抑制");
        assert!(shared.is_business_pending(&rebuilt.messages[0].id).unwrap());
        assert!(!shared.is_business_pending(&rebuilt.messages[1].id).unwrap());
    }

    /// 桶淘汰不能只断言 len<=32：必须证明“在用桶没被淘汰、新桶写进去了、
    /// 被淘汰的是一次性抖动桶”。真实运行时 PrimaryHall 每帧都被 publish 触碰，
    /// 因此它的活跃度持续刷新；OCR 好友名抖动产生的桶写一次后再也没人碰。
    #[test]
    fn v3_command_lock_chat_bucket_cap_evicts_idle_jitter_buckets_keeps_active_chat() {
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
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

    #[test]
    fn v3_failed_dispatch_does_not_write_command_lock() {
        let shared = SharedChatOcr::new();
        let built = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        let message = &built.messages[0];
        let failed = claim(&shared, message, built.captured_at)
            .expect("pending message should be claimable");
        // 派发失败：认领被 Drop 撤销，命令锁绝不能写入，否则真失败的命令会被永久吞掉。
        drop(failed);
        assert!(shared.is_business_pending(&message.id).unwrap());
        assert_eq!(command_lock_bucket_count(&shared), 0);
        rebuild(&shared);
        let rebuilt = publish(
            &shared,
            vec![primary("甲", "@命令1", 10), primary("甲", "@命令2", 40)],
        );
        assert!(rebuilt.messages[0].is_new, "锁为空：命令1 仍是新命令");
        assert!(rebuilt.messages[1].is_new);
        assert!(shared.is_business_pending(&rebuilt.messages[0].id).unwrap());
    }

    /// 缺陷 2：命令锁归一化必须折全角。OCR 会把半角冒号读成全角，不折就会让同一条
    /// 已执行命令在重建后以“全角版本”重新命中失败而执行两次。
    #[test]
    fn v3_fullwidth_and_halfwidth_punctuation_hit_the_same_command_lock_entry() {
        // 半角写入 -> 全角命中
        let shared = SharedChatOcr::new();
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
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![primary_fullwidth("甲", "@点歌 起风了", 40)]);
        execute(&shared, &first, 0);
        rebuild(&shared);
        let halfwidth = publish(&shared, vec![primary("甲", "@点歌 起风了", 40)]);
        assert!(
            !halfwidth.messages[0].is_new,
            "半角冒号必须命中全角写入的命令锁条目"
        );
    }

    /// 缺陷 3：missing 计数不得跨视图重建残留。重建前已累计 1 个缺失帧时，
    /// 重建后第 1 帧若再累加就会凑满 2，把仍在屏上的条目误删。
    #[test]
    fn v3_missing_frames_do_not_survive_a_view_rebuild() {
        let shared = SharedChatOcr::new();
        let first = publish(&shared, vec![primary("甲", "@命令1", 40)]);
        execute(&shared, &first, 0);
        // 命令滚出 1 帧：missing_frames=1（未释放）。
        publish(&shared, vec![primary("乙", "闲聊", 200)]);
        rebuild(&shared);
        // 重建后第 1 帧仍不见命令：计数若残留会在这里凑满 2。
        publish(&shared, vec![primary("乙", "闲聊", 200)]);
        assert!(
            shared
                .command_lock_texts_for_test(&ChatIdentity::PrimaryHall)
                .iter()
                .any(|text| text.contains("@命令1")),
            "重建后第 1 个可信帧不得释放条目（跨重建计数必须清零）"
        );
        let recovered = publish(
            &shared,
            vec![primary("乙", "闲聊", 200), primary("甲", "@命令1", 40)],
        );
        let command = recovered
            .messages
            .iter()
            .find(|message| message.text.contains("@命令1"))
            .expect("recovered command");
        assert!(!command.is_new, "跨重建后条目仍应抑制重跑");
        assert!(!shared.is_business_pending(&command.id).unwrap());
    }

    /// 缺陷 4：suppress 帧（好友丢弃视图/导航中）不是可信内容证据，既不匹配也不释放。
    /// 若仍释放，两帧“看起来消失”就会误删仍在屏上的条目。
    #[test]
    fn v3_suppressed_friend_frames_do_not_release_command_lock_entries() {
        let shared = SharedChatOcr::new();
        let chat = ChatIdentity::Friend(Arc::from("observed name"));
        let publish_friend = |messages: Vec<SharedChatInput>| {
            shared
                .publish(Instant::now(), chat.clone(), messages)
                .unwrap()
        };
        // 好友首帧是被动基线，不派发。
        publish_friend(vec![input("history", 10)]);
        let seeded = publish_friend(vec![input("history", 10), input("[甲]:@命令", 40)]);
        let command = seeded
            .messages
            .iter()
            .find(|message| message.text.contains("@命令"))
            .unwrap()
            .clone();
        assert!(shared.is_business_pending(&command.id).unwrap());
        claim(&shared, &command, seeded.captured_at)
            .expect("pending friend command should be claimable")
            .complete_as_command()
            .unwrap();

        let guard = shared.suppress_friend_business().unwrap();
        for _ in 0..2 {
            publish_friend(vec![input("history", 10)]);
        }
        drop(guard);

        let recovered = publish_friend(vec![input("history", 10), input("[甲]:@命令", 40)]);
        let command_again = recovered
            .messages
            .iter()
            .find(|message| message.text.contains("@命令"))
            .unwrap();
        assert!(
            !command_again.is_new,
            "suppress 帧不得释放命令锁条目，否则恢复后命令会重跑"
        );
        assert!(!shared.is_business_pending(&command_again.id).unwrap());
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
fn sequence_overlap(previous: &[SharedChatMessage], current: &[SharedChatInput]) -> usize {
    // A retained first block anchors corrections of later blocks. Prefer that
    // whole layout to a shorter repeated-text suffix that would invent a bubble.
    if previous.len() == current.len()
        && !current.is_empty()
        && previous[0].text == current[0].text
        && previous.iter().zip(current).all(|(left, right)| {
            left.message_type == right.message_type && same_rect(left.block, right.block)
        })
    {
        return current.len();
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
                    && left.block.x == right.block.x
                    && left.block.width == right.block.width
                    && left.block.height == right.block.height
                    && i64::from(right.block.y) - i64::from(left.block.y) == delta
            })
        })
        .unwrap_or(0)
}
