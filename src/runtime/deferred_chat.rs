use std::collections::VecDeque;

use anyhow::{Result, anyhow};

use crate::features::turtle_soup::{
    TurtleSoupDelivery, TurtleSoupDeliveryIntent, TurtleSoupDeliveryOutcome,
    TurtleSoupDeliveryPort, TurtleSoupDeliveryPurpose, TurtleSoupReplyText,
};

pub(crate) const DEFAULT_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EnqueueOutcome {
    Added,
    DroppedMessage,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeferredChatTarget {
    Primary,
    SecondaryCurrentHall,
    CurrentHall,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeferredChatMessage {
    pub(crate) text: String,
    pub(crate) target: DeferredChatTarget,
    pub(crate) background_key: Option<String>,
    pub(crate) formal_epoch: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeferredChatBatch {
    messages: Vec<TurtleSoupReplyText>,
    rendered: bool,
    pub(crate) target: DeferredChatTarget,
    pub(crate) turtle_soup: TurtleSoupDelivery,
    max_attempts: u8,
    current_attempts: u8,
    protected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatchFailureOutcome {
    Retry,
    Exhausted,
}

impl DeferredChatBatch {
    fn new(
        messages: Vec<TurtleSoupReplyText>,
        target: DeferredChatTarget,
        turtle_soup: TurtleSoupDelivery,
        max_attempts: u8,
        protected: bool,
    ) -> Result<Self> {
        if messages.is_empty() {
            return Err(anyhow!("延迟聊天分段批次不能为空"));
        }
        Ok(Self {
            messages: messages.into(),
            rendered: false,
            target,
            turtle_soup,
            max_attempts: max_attempts.max(1),
            current_attempts: 0,
            protected,
        })
    }

    pub(crate) fn remaining_replies(&self) -> impl Iterator<Item = &TurtleSoupReplyText> {
        self.messages.iter()
    }

    pub(crate) fn cache_rendered_rows(&mut self, messages: Vec<String>) {
        self.messages = messages.into_iter().map(TurtleSoupReplyText::raw).collect();
        self.rendered = true;
    }

    pub(crate) fn replace_remaining(&mut self, messages: Vec<String>) {
        self.cache_rendered_rows(messages);
    }

    pub(crate) fn is_rendered(&self) -> bool {
        self.rendered
    }

    pub(crate) fn mark_sent(&mut self, count: usize) -> Result<bool> {
        if count > self.messages.len() {
            return Err(anyhow!(
                "延迟聊天批次成功数量越界: sent={} remaining={}",
                count,
                self.messages.len()
            ));
        }
        if count > 0 {
            self.messages.drain(..count);
            self.current_attempts = 0;
        }
        Ok(self.messages.is_empty())
    }

    pub(crate) fn mark_current_failed(&mut self) -> BatchFailureOutcome {
        self.current_attempts = self.current_attempts.saturating_add(1);
        if self.current_attempts >= self.max_attempts {
            BatchFailureOutcome::Exhausted
        } else {
            BatchFailureOutcome::Retry
        }
    }

    pub(crate) fn current_attempt(&self) -> u8 {
        self.current_attempts.saturating_add(1)
    }

    pub(crate) fn max_attempts(&self) -> u8 {
        self.max_attempts
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DeferredChatItem {
    Message(DeferredChatMessage),
    Batch(DeferredChatBatch),
}

impl DeferredChatItem {
    pub(crate) fn target(&self) -> DeferredChatTarget {
        match self {
            Self::Message(message) => message.target,
            Self::Batch(batch) => batch.target,
        }
    }

    fn protected(&self) -> bool {
        matches!(self, Self::Batch(batch) if batch.protected)
    }

    fn background_key(&self) -> Option<&str> {
        match self {
            Self::Message(message) => message.background_key.as_deref(),
            Self::Batch(_) => None,
        }
    }

    fn is_background_message(&self) -> bool {
        self.background_key().is_some()
    }

    fn critical(&self) -> bool {
        matches!(self, Self::Batch(batch) if matches!(
            batch.turtle_soup.purpose,
            TurtleSoupDeliveryPurpose::Opening | TurtleSoupDeliveryPurpose::Settlement
        ))
    }
}

impl From<DeferredChatMessage> for DeferredChatItem {
    fn from(message: DeferredChatMessage) -> Self {
        Self::Message(message)
    }
}

impl From<DeferredChatBatch> for DeferredChatItem {
    fn from(batch: DeferredChatBatch) -> Self {
        Self::Batch(batch)
    }
}

pub(crate) struct DeferredChatQueue {
    queue: VecDeque<DeferredChatItem>,
    capacity: usize,
}

impl DeferredChatQueue {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.queue.clear();
    }

    pub(crate) fn enqueue(&mut self, item: impl Into<DeferredChatItem>) -> Result<EnqueueOutcome> {
        let item = item.into();
        if let Some(background_key) = item.background_key() {
            self.queue
                .retain(|queued| queued.background_key() != Some(background_key));
            if self.queue.len() >= self.capacity {
                return Ok(EnqueueOutcome::Rejected);
            }
            self.queue.push_back(item);
            return Ok(EnqueueOutcome::Added);
        }
        let outcome = make_room_for_enqueue(&mut self.queue, self.capacity, item.critical())?;
        if outcome != EnqueueOutcome::Rejected {
            self.push_normal_with_background_priority(item);
        }
        Ok(outcome)
    }

    pub(crate) fn enqueue_front(
        &mut self,
        item: impl Into<DeferredChatItem>,
    ) -> Result<EnqueueOutcome> {
        let item = item.into();
        let outcome = make_room_for_enqueue(&mut self.queue, self.capacity, item.critical())?;
        if outcome != EnqueueOutcome::Rejected {
            self.queue.push_front(item);
        }
        Ok(outcome)
    }

    pub(crate) fn requeue_front(&mut self, item: DeferredChatItem) -> EnqueueOutcome {
        let outcome =
            make_room_for_requeue(&mut self.queue, self.capacity, &item, EvictionBias::Front);
        if outcome != EnqueueOutcome::Rejected {
            self.queue.push_front(item);
        }
        outcome
    }

    pub(crate) fn requeue_back(&mut self, item: DeferredChatItem) -> EnqueueOutcome {
        let outcome =
            make_room_for_requeue(&mut self.queue, self.capacity, &item, EvictionBias::Back);
        if outcome != EnqueueOutcome::Rejected {
            // 按「同目标保持原序」插回，而不是一律压到队尾。
            //
            // 调用方 `requeue_back` 的语义是「这个目标现在不可达，等我回来再发」，不是
            // 「这条回复比同目标的后来者更新」。一律 `push_back` 会让先入队的消息排到
            // 后来者之后，玩家看到的是一问一答被颠倒（`[A, B]` 取出 A 再压回尾 → `[B, A]`）。
            // 这里插到**第一条未受保护的同目标消息**之前：同目标之间相对顺序不变，
            // 而受保护批次（开场/结算）仍然留在它前面，不会被重试挤到后面。
            let index = self
                .queue
                .iter()
                .position(|queued| queued.target() == item.target() && !queued.protected())
                .or_else(|| {
                    // 同目标只剩受保护批次时，排到它们之后。
                    self.queue
                        .iter()
                        .rposition(|queued| queued.target() == item.target())
                        .map(|position| position + 1)
                })
                .unwrap_or(self.queue.len());
            self.queue.insert(index, item);
        }
        outcome
    }

    pub(crate) fn take_next(&mut self) -> Option<DeferredChatItem> {
        self.queue.pop_front()
    }

    fn push_normal_with_background_priority(&mut self, item: DeferredChatItem) {
        if !self
            .queue
            .iter()
            .any(DeferredChatItem::is_background_message)
        {
            self.queue.push_back(item);
            return;
        }

        let mut prioritized = VecDeque::with_capacity(self.queue.len() + 1);
        let mut background = VecDeque::new();
        while let Some(queued) = self.queue.pop_front() {
            if queued.is_background_message() {
                background.push_back(queued);
            } else {
                prioritized.push_back(queued);
            }
        }
        prioritized.push_back(item);
        prioritized.extend(background);
        self.queue = prioritized;
    }

    fn enqueue_turtle_soup(
        &mut self,
        intent: TurtleSoupDeliveryIntent,
    ) -> Result<TurtleSoupDeliveryOutcome> {
        let urgent = intent.is_urgent();
        let protected = intent.is_protected();
        let max_attempts = intent.max_attempts();
        let (messages, delivery) = intent.into_parts();
        let batch = DeferredChatBatch::new(
            messages,
            DeferredChatTarget::CurrentHall,
            delivery,
            max_attempts,
            protected,
        )?;
        let outcome = if urgent {
            self.enqueue_front(batch)?
        } else {
            self.enqueue(batch)?
        };
        Ok(match outcome {
            EnqueueOutcome::Added => TurtleSoupDeliveryOutcome::Added,
            EnqueueOutcome::DroppedMessage => TurtleSoupDeliveryOutcome::DroppedEarlierMessage,
            EnqueueOutcome::Rejected => TurtleSoupDeliveryOutcome::Rejected,
        })
    }
}

impl TurtleSoupDeliveryPort for DeferredChatQueue {
    fn deliver_turtle_soup(
        &mut self,
        intent: TurtleSoupDeliveryIntent,
    ) -> Result<TurtleSoupDeliveryOutcome> {
        self.enqueue_turtle_soup(intent)
    }
}

fn make_room_for_enqueue(
    queue: &mut VecDeque<DeferredChatItem>,
    capacity: usize,
    incoming_critical: bool,
) -> Result<EnqueueOutcome> {
    if queue.len() < capacity {
        return Ok(EnqueueOutcome::Added);
    }
    let index = eviction_candidate(queue, incoming_critical, EvictionBias::Front);
    let Some(index) = index else {
        return Ok(EnqueueOutcome::Rejected);
    };
    queue
        .remove(index)
        .ok_or_else(|| anyhow!("延迟聊天队列项目移除失败"))?;
    Ok(EnqueueOutcome::DroppedMessage)
}

/// 队列已满时该驱逐谁：入队与重排队共用同一套分层规则。
///
/// 1. 先驱逐后台消息（歌词）：它们每轮都被新内容覆盖，丢了不影响对话；
/// 2. 再驱逐任意未受保护的条目（受保护批次是玩家正在等的开场/结算，不能牺牲）；
/// 3. 仅当**即将入队的条目是关键的**（开场/结算批次）时，才允许驱逐一条非关键条目。
///
/// 方向（从队首还是队尾挑）只决定「同一层内部怎么选」，不改变层级本身 ——
/// 这是两件不同的事：方向是调用方的排版偏好，层级是「谁的损失最小」。
fn eviction_candidate(
    queue: &VecDeque<DeferredChatItem>,
    incoming_critical: bool,
    bias: EvictionBias,
) -> Option<usize> {
    let pick = |predicate: &dyn Fn(&DeferredChatItem) -> bool| match bias {
        EvictionBias::Front => queue.iter().position(predicate),
        EvictionBias::Back => queue.iter().rposition(predicate),
    };
    pick(&DeferredChatItem::is_background_message)
        .or_else(|| pick(&|item: &DeferredChatItem| !item.protected()))
        .or_else(|| {
            incoming_critical
                .then(|| pick(&|item: &DeferredChatItem| !item.critical()))
                .flatten()
        })
}

/// 同一驱逐层级内的取舍方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvictionBias {
    /// 从队首挑：让**最早**的一条先让位（正常入队与重排队首都用它，
    /// 与 `deferred_queue_preserves_fifo_and_drops_the_oldest_normal_message_at_capacity` 钉住的语义一致）。
    Front,
    /// 从队尾挑：让**最新**的一条先让位。
    Back,
}

/// 为**重排队**腾位置。
///
/// 早先这里自己写了一套「按方向取第一个/最后一个未受保护项」，与入队路径不一致：
/// 既不优先驱逐后台歌词，也没有把「受保护」当回事（`_protected` 参数根本没被读），
/// 于是重试会挤掉别的玩家正在等的开场/结算批次，或者放着最该牺牲的歌词不驱、
/// 把队列越堵越死。现在它和 [`make_room_for_enqueue`] 走同一个 [`eviction_candidate`]。
fn make_room_for_requeue(
    queue: &mut VecDeque<DeferredChatItem>,
    capacity: usize,
    incoming: &DeferredChatItem,
    bias: EvictionBias,
) -> EnqueueOutcome {
    if queue.len() < capacity {
        return EnqueueOutcome::Added;
    }
    let index = eviction_candidate(queue, incoming.critical(), bias);
    let Some(index) = index else {
        // 队列已满且没有可驱逐项:拒绝入队(含受保护消息),保持容量契约。
        // 受保护消息由调用方的批次 max_attempts 机制负责重试/放弃。
        log::warn!(
            "延迟聊天队列已满（上限 {capacity} 条）且没有可驱逐的旧消息，本条消息未入队；调用方会按批次重试次数决定是否放弃"
        );
        return EnqueueOutcome::Rejected;
    };
    queue.remove(index);
    EnqueueOutcome::DroppedMessage
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str) -> DeferredChatMessage {
        DeferredChatMessage {
            text: text.to_string(),
            target: DeferredChatTarget::Primary,
            background_key: None,
            formal_epoch: None,
        }
    }

    fn item(text: &str) -> DeferredChatItem {
        message(text).into()
    }

    fn background_message(text: &str, epoch: u64) -> DeferredChatMessage {
        DeferredChatMessage {
            text: text.to_string(),
            target: DeferredChatTarget::CurrentHall,
            background_key: Some("lyrics".to_string()),
            formal_epoch: Some(epoch),
        }
    }

    fn batch(purpose: TurtleSoupDeliveryPurpose, text: &str) -> DeferredChatBatch {
        DeferredChatBatch::new(
            vec![TurtleSoupReplyText::raw(text)],
            DeferredChatTarget::CurrentHall,
            TurtleSoupDelivery {
                generation: 1,
                purpose,
            },
            3,
            true,
        )
        .unwrap()
    }

    #[test]
    fn deferred_queue_preserves_fifo_and_drops_the_oldest_normal_message_at_capacity() {
        let mut queue = DeferredChatQueue::new(2);
        queue.enqueue(message("first")).unwrap();
        queue.enqueue(message("second")).unwrap();

        assert_eq!(
            queue.enqueue(message("third")).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert_eq!(queue.take_next(), Some(item("second")));
        assert_eq!(queue.take_next(), Some(item("third")));
    }

    #[test]
    fn protected_opening_survives_normal_pressure_and_settlement_replaces_repeat() {
        let mut queue = DeferredChatQueue::new(2);
        let opening = batch(TurtleSoupDeliveryPurpose::Opening, "opening");
        queue.enqueue(opening.clone()).unwrap();
        queue.enqueue(message("normal-1")).unwrap();
        assert_eq!(
            queue.enqueue(message("normal-2")).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert_eq!(queue.take_next(), Some(DeferredChatItem::Batch(opening)));

        let mut queue = DeferredChatQueue::new(1);
        queue
            .enqueue(batch(TurtleSoupDeliveryPurpose::SurfaceRepeat, "surface"))
            .unwrap();
        let settlement = batch(TurtleSoupDeliveryPurpose::Settlement, "bottom");
        assert_eq!(
            queue.enqueue(settlement.clone()).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert_eq!(queue.take_next(), Some(DeferredChatItem::Batch(settlement)));
    }

    #[test]
    fn background_lyrics_are_coalesced_and_never_evict_normal_replies() {
        let mut queue = DeferredChatQueue::new(3);
        queue.enqueue(message("normal-1")).unwrap();
        queue.enqueue(background_message("lyric-1", 1)).unwrap();
        queue.enqueue(message("normal-2")).unwrap();

        assert_eq!(
            queue.enqueue(background_message("lyric-2", 1)).unwrap(),
            EnqueueOutcome::Added
        );
        assert_eq!(queue.take_next(), Some(item("normal-1")));
        assert_eq!(queue.take_next(), Some(item("normal-2")));
        assert_eq!(
            queue.take_next(),
            Some(background_message("lyric-2", 1).into())
        );

        let mut full = DeferredChatQueue::new(2);
        full.enqueue(message("normal-1")).unwrap();
        full.enqueue(message("normal-2")).unwrap();
        assert_eq!(
            full.enqueue(background_message("lyric", 1)).unwrap(),
            EnqueueOutcome::Rejected
        );
        assert_eq!(full.take_next(), Some(item("normal-1")));
        assert_eq!(full.take_next(), Some(item("normal-2")));

        let mut mixed = DeferredChatQueue::new(2);
        mixed.enqueue(background_message("old lyric", 1)).unwrap();
        mixed.enqueue(message("normal-1")).unwrap();
        assert_eq!(
            mixed.enqueue(message("normal-2")).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert_eq!(mixed.take_next(), Some(item("normal-1")));
        assert_eq!(mixed.take_next(), Some(item("normal-2")));

        let mut ordered = DeferredChatQueue::new(4);
        ordered.enqueue(background_message("lyric", 1)).unwrap();
        ordered.enqueue(message("normal-1")).unwrap();
        ordered.enqueue(message("normal-2")).unwrap();
        assert_eq!(ordered.take_next(), Some(item("normal-1")));
        assert_eq!(ordered.take_next(), Some(item("normal-2")));
        assert_eq!(
            ordered.take_next(),
            Some(background_message("lyric", 1).into())
        );
    }

    #[test]
    fn requeue_direction_preserves_retry_priority_or_yields_to_an_active_target() {
        let mut queue = DeferredChatQueue::new(3);
        queue.enqueue(message("later")).unwrap();
        assert_eq!(queue.requeue_front(item("retry")), EnqueueOutcome::Added);
        assert_eq!(queue.take_next(), Some(item("retry")));

        let mut queue = DeferredChatQueue::new(3);
        let secondary = DeferredChatMessage {
            text: "secondary".to_string(),
            target: DeferredChatTarget::SecondaryCurrentHall,
            background_key: None,
            formal_epoch: None,
        };
        queue.enqueue(secondary.clone()).unwrap();
        queue.enqueue(message("primary")).unwrap();
        let secondary = queue.take_next().unwrap();
        assert_eq!(queue.requeue_back(secondary), EnqueueOutcome::Added);
        assert_eq!(queue.take_next(), Some(item("primary")));
    }

    /// `requeue_back` 不能把「同目标的消息顺序」颠倒。
    ///
    /// 真实场景：发送线程取出 A 时目标一时不可达，把 A 重新排回队尾；此时 B（同目标、
    /// 后到）已经在队列里。`push_back` 会得到 `[B, A]`，玩家看到的是倒过来的回答。
    /// 正确结果是 `[A, B]`。
    #[test]
    fn requeue_back_keeps_the_relative_order_of_same_target_messages() {
        let mut queue = DeferredChatQueue::new(4);
        queue.enqueue(message("first")).unwrap();
        queue.enqueue(message("second")).unwrap();

        let first = queue.take_next().unwrap();
        assert_eq!(queue.requeue_back(first), EnqueueOutcome::Added);

        assert_eq!(queue.take_next(), Some(item("first")));
        assert_eq!(queue.take_next(), Some(item("second")));
    }

    /// 重排队必须优先驱逐后台歌词，与正常入队同策。
    ///
    /// 早先 `make_room_for_requeue` 只看「谁没被保护」并按方向取第一个，
    /// 于是队列里明明有歌词可丢，回队的回复却把别人正在等的普通回复挤掉了。
    #[test]
    fn requeue_evicts_background_lyrics_before_normal_replies() {
        let mut queue = DeferredChatQueue::new(2);
        queue.enqueue(background_message("old lyric", 1)).unwrap();
        queue.enqueue(message("kept")).unwrap();

        let retry = message("retry");
        assert_eq!(
            queue.requeue_back(retry.into()),
            EnqueueOutcome::DroppedMessage
        );
        // 被丢的是歌词，不是 "kept"；回队的 "retry" 比 "kept" 更早，仍排在前面。
        assert_eq!(queue.take_next(), Some(item("retry")));
        assert_eq!(queue.take_next(), Some(item("kept")));
    }

    /// 受保护的阶段批次不能被重排队挤掉。
    ///
    /// 队列满且只装着受保护批次时，重排队必须**拒绝**而不是悄悄丢掉一整段
    /// 开场/结算；拒绝后由调用方的批次重试上限决定是否放弃。
    #[test]
    fn requeue_never_evicts_protected_batches() {
        let mut queue = DeferredChatQueue::new(1);
        let opening = batch(TurtleSoupDeliveryPurpose::Opening, "opening");
        queue.enqueue(opening.clone()).unwrap();

        assert_eq!(queue.requeue_back(item("retry")), EnqueueOutcome::Rejected);
        assert_eq!(queue.requeue_front(item("retry")), EnqueueOutcome::Rejected);
        assert_eq!(queue.take_next(), Some(DeferredChatItem::Batch(opening)));
    }

    #[test]
    fn partial_batch_progress_resets_failure_count_only_after_confirmed_sends() {
        let mut batch = DeferredChatBatch::new(
            vec![
                TurtleSoupReplyText::raw("first"),
                TurtleSoupReplyText::raw("second"),
            ],
            DeferredChatTarget::CurrentHall,
            TurtleSoupDelivery {
                generation: 1,
                purpose: TurtleSoupDeliveryPurpose::Opening,
            },
            3,
            true,
        )
        .unwrap();

        assert_eq!(batch.mark_current_failed(), BatchFailureOutcome::Retry);
        assert_eq!(batch.current_attempt(), 2);
        batch.replace_remaining(vec![
            "first rendered row".into(),
            "second rendered row".into(),
            "third rendered row".into(),
        ]);
        assert!(batch.is_rendered());
        assert!(!batch.mark_sent(0).unwrap());
        assert_eq!(batch.current_attempt(), 2);
        assert!(!batch.mark_sent(1).unwrap());
        assert_eq!(batch.remaining_replies().count(), 2);
        assert!(
            matches!(batch.remaining_replies().next(), Some(TurtleSoupReplyText::Raw(text)) if text == "second rendered row")
        );
        assert_eq!(
            batch
                .remaining_replies()
                .map(TurtleSoupReplyText::render_default)
                .collect::<Vec<_>>(),
            vec!["second rendered row", "third rendered row"]
        );
        assert_eq!(batch.current_attempt(), 1);
        assert!(batch.mark_sent(3).is_err());
        assert!(batch.is_rendered());
        assert!(batch.mark_sent(2).unwrap());
    }

    #[test]
    fn cached_override_rows_survive_partial_send_and_retry_without_rerendering() {
        let mut batch = batch(TurtleSoupDeliveryPurpose::Opening, "source");
        batch.cache_rendered_rows(vec![
            "汤面1/3：长覆盖第一段".into(),
            "汤面2/3：长覆盖第二段".into(),
            "汤面3/3：长覆盖第三段".into(),
        ]);
        assert!(batch.is_rendered());
        assert!(!batch.mark_sent(1).unwrap());
        assert_eq!(
            batch
                .remaining_replies()
                .map(TurtleSoupReplyText::render_default)
                .collect::<Vec<_>>(),
            vec!["汤面2/3：长覆盖第二段", "汤面3/3：长覆盖第三段"]
        );
        assert_eq!(batch.mark_current_failed(), BatchFailureOutcome::Retry);
        assert_eq!(
            batch
                .remaining_replies()
                .map(TurtleSoupReplyText::render_default)
                .collect::<Vec<_>>(),
            vec!["汤面2/3：长覆盖第二段", "汤面3/3：长覆盖第三段"]
        );
    }

    #[test]
    fn turtle_soup_opening_enters_at_the_front_as_a_protected_batch() {
        let mut queue = DeferredChatQueue::new(2);
        queue.enqueue(message("normal")).unwrap();
        let outcome = TurtleSoupDeliveryPort::deliver_turtle_soup(
            &mut queue,
            TurtleSoupDeliveryIntent::new(
                vec!["opening".to_string()],
                7,
                TurtleSoupDeliveryPurpose::Opening,
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(outcome, TurtleSoupDeliveryOutcome::Added);
        assert!(matches!(
            queue.take_next(),
            Some(DeferredChatItem::Batch(_))
        ));
        assert_eq!(queue.take_next(), Some(item("normal")));
    }
}
