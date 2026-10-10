use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use super::{BubbleSequence, ChatIdentity, ObservedChatMessageId, VisualSessionId};
use crate::ui::geometry::Rect;

const RETAINED_CAPACITY: usize = 4096;

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
    committed: bool,
}

impl SharedBusinessClaim {
    pub(crate) fn complete(mut self) -> Result<()> {
        self.shared.acknowledge_business(&self.id)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for SharedBusinessClaim {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.inner.state.lock() {
            state.business_claimed.remove(&self.id);
            if !self.committed
                && (!matches!(self.id.chat, ChatIdentity::Friend(_))
                    || state.friend_suppression_generation == self.suppression_generation)
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
            } else if message.is_new && !state.business_claimed.contains(&message.id) {
                state.business_pending.insert(message.id.clone());
            } else if initial && matches!(chat, ChatIdentity::Friend(_)) {
                state.unread_baseline.insert(message.id.clone());
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
            if subscriber
                .chat
                .as_ref()
                .is_some_and(|chat| *chat != snapshot.chat)
                || subscriber
                    .session
                    .is_some_and(|session| session != snapshot.visual_session)
            {
                subscriber.failure = Some("visible chat identity changed".into());
                subscriber.queue.clear();
            } else if subscriber.queue.len() >= RETAINED_CAPACITY {
                subscriber.failure =
                    Some("shared chat OCR subscriber overflow (4096 snapshots)".into());
                subscriber.queue.clear();
            } else {
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
        state.unread_baseline.clear();
        for subscriber in state
            .subscribers
            .values_mut()
            .filter(|subscriber| subscriber.chat.is_some())
        {
            fail_subscription(subscriber, &reason);
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

    pub(crate) fn claim_business(
        &self,
        id: &ObservedChatMessageId,
        captured_at: Instant,
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
            committed: false,
        }))
    }

    pub(crate) fn acknowledge_business(&self, id: &ObservedChatMessageId) -> Result<bool> {
        let mut state = self.lock()?;
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
        Ok(found)
    }
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
        let id = &snapshot.messages[1].id;
        let failed_dispatch = shared
            .claim_business(id, snapshot.captured_at)
            .unwrap()
            .unwrap();
        let repeated = friend(&shared, vec![input("history", 10), input("command", 40)]);
        assert!(
            shared
                .claim_business(id, repeated.captured_at)
                .unwrap()
                .is_none()
        );
        drop(failed_dispatch);
        assert!(shared.is_business_pending(id).unwrap());
        let failed_dispatch = shared
            .claim_business(id, repeated.captured_at)
            .unwrap()
            .unwrap();
        let discard = shared.suppress_friend_business().unwrap();
        drop(discard);
        drop(failed_dispatch);
        assert!(!shared.is_business_pending(id).unwrap());
        let repeated = friend(&shared, vec![input("history", 10), input("command", 40)]);
        assert!(
            shared
                .claim_business(id, repeated.captured_at)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn baseline_promotion_uses_latest_ocr_and_skips_stale_queued_text() {
        let shared = SharedChatOcr::new();
        let listener = shared.subscribe().unwrap();
        let baseline = friend(&shared, vec![input("misread", 10)]);
        let selected = friend(&shared, vec![input("correct command", 10)]);
        let latest = friend(&shared, vec![input("latest corrected command", 10)]);
        let id = &selected.messages[0].id;
        assert_eq!(id, &baseline.messages[0].id);
        assert!(shared.deliver_unread_baseline(&selected, id).unwrap());
        let queue = listener.read().unwrap();
        assert_eq!(queue.last().unwrap().captured_at, latest.captured_at);
        assert!(
            queue
                .windows(2)
                .all(|pair| pair[0].captured_at <= pair[1].captured_at)
        );
        assert!(
            shared
                .claim_business(id, baseline.captured_at)
                .unwrap()
                .is_none()
        );
        assert!(
            shared
                .claim_business(id, selected.captured_at)
                .unwrap()
                .is_none()
        );
        let claim = shared
            .claim_business(id, latest.captured_at)
            .unwrap()
            .unwrap();
        assert_eq!(
            queue.last().unwrap().messages[0].text,
            "latest corrected command"
        );
        claim.complete().unwrap();
        assert!(
            shared
                .claim_business(id, latest.captured_at)
                .unwrap()
                .is_none()
        );
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
        assert!(!first.messages[0].is_new);
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
    fn no_chat_completes_refresh_and_ends_only_bound_decisions() {
        let shared = SharedChatOcr::new();
        let baseline = publish(&shared, vec![input("first", 10)]);
        let business = shared.subscribe().unwrap();
        let decision = shared.subscribe().unwrap();
        decision
            .bind_chat(baseline.chat, baseline.visual_session)
            .unwrap();
        let token = shared.request_refresh().unwrap();
        shared
            .publish_no_chat(Instant::now(), "chat disappeared")
            .unwrap();
        assert!(shared.latest().unwrap().is_none());
        assert!(shared.wait_for_snapshot(token, Duration::ZERO).is_err());
        assert!(decision.read().is_err());
        assert!(business.read().unwrap().is_empty());
        let after = publish(&shared, vec![input("first", 10)]);
        assert_ne!(after.visual_session, baseline.visual_session);
        assert_eq!(business.read().unwrap().len(), 1);
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
