use std::collections::VecDeque;

use anyhow::{Result, anyhow};

use crate::features::turtle_soup::{
    TurtleSoupDelivery, TurtleSoupDeliveryIntent, TurtleSoupDeliveryOutcome,
    TurtleSoupDeliveryPort, TurtleSoupDeliveryPurpose,
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
    messages: VecDeque<String>,
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
        messages: Vec<String>,
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
            target,
            turtle_soup,
            max_attempts: max_attempts.max(1),
            current_attempts: 0,
            protected,
        })
    }

    pub(crate) fn remaining_texts(&self) -> Vec<&str> {
        self.messages.iter().map(String::as_str).collect()
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
        let outcome = make_room_for_requeue(&mut self.queue, self.capacity, item.protected(), true);
        if outcome != EnqueueOutcome::Rejected {
            self.queue.push_front(item);
        }
        outcome
    }

    pub(crate) fn requeue_back(&mut self, item: DeferredChatItem) -> EnqueueOutcome {
        let outcome =
            make_room_for_requeue(&mut self.queue, self.capacity, item.protected(), false);
        if outcome != EnqueueOutcome::Rejected {
            self.queue.push_back(item);
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
    let index = queue
        .iter()
        .position(DeferredChatItem::is_background_message)
        .or_else(|| queue.iter().position(|item| !item.protected()))
        .or_else(|| {
            incoming_critical
                .then(|| queue.iter().position(|item| !item.critical()))
                .flatten()
        });
    let Some(index) = index else {
        return Ok(EnqueueOutcome::Rejected);
    };
    queue
        .remove(index)
        .ok_or_else(|| anyhow!("延迟聊天队列项目移除失败"))?;
    Ok(EnqueueOutcome::DroppedMessage)
}

fn make_room_for_requeue(
    queue: &mut VecDeque<DeferredChatItem>,
    capacity: usize,
    _protected: bool,
    remove_from_back: bool,
) -> EnqueueOutcome {
    if queue.len() < capacity {
        return EnqueueOutcome::Added;
    }
    let index = if remove_from_back {
        queue.iter().rposition(|item| !item.protected())
    } else {
        queue.iter().position(|item| !item.protected())
    };
    if let Some(index) = index {
        queue.remove(index);
        EnqueueOutcome::DroppedMessage
    } else {
        // 队列已满且没有可驱逐项:拒绝入队(含受保护消息),保持容量契约。
        // 受保护消息由调用方的批次 max_attempts 机制负责重试/放弃。
        log::warn!("延迟聊天队列已满({capacity})且无可驱逐项,拒绝消息入队");
        EnqueueOutcome::Rejected
    }
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
            vec![text.to_string()],
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

    #[test]
    fn partial_batch_progress_resets_failure_count_only_after_confirmed_sends() {
        let mut batch = DeferredChatBatch::new(
            vec!["first".to_string(), "second".to_string()],
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
        assert!(!batch.mark_sent(0).unwrap());
        assert_eq!(batch.current_attempt(), 2);
        assert!(!batch.mark_sent(1).unwrap());
        assert_eq!(batch.remaining_texts(), vec!["second"]);
        assert_eq!(batch.current_attempt(), 1);
        assert!(batch.mark_sent(2).is_err());
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
