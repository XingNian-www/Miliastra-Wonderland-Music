use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::features::turtle_soup::{
    TurtleSoupDelivery, TurtleSoupDeliveryIntent, TurtleSoupDeliveryOutcome,
    TurtleSoupDeliveryPort, TurtleSoupDeliveryPurpose,
};

pub const DEFAULT_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Added,
    DroppedMessage,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredChatTarget {
    Primary,
    SecondaryCurrentHall,
    CurrentHall,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredChatMessage {
    pub text: String,
    pub target: DeferredChatTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredChatBatch {
    messages: VecDeque<String>,
    pub target: DeferredChatTarget,
    pub turtle_soup: TurtleSoupDelivery,
    max_attempts: u8,
    current_attempts: u8,
    protected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchFailureOutcome {
    Retry,
    Exhausted,
}

impl DeferredChatBatch {
    pub fn new(
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

    pub fn remaining_texts(&self) -> Vec<&str> {
        self.messages.iter().map(String::as_str).collect()
    }

    pub fn mark_sent(&mut self, count: usize) -> Result<bool> {
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

    pub fn mark_current_failed(&mut self) -> BatchFailureOutcome {
        self.current_attempts = self.current_attempts.saturating_add(1);
        if self.current_attempts >= self.max_attempts {
            BatchFailureOutcome::Exhausted
        } else {
            BatchFailureOutcome::Retry
        }
    }

    pub fn current_attempt(&self) -> u8 {
        self.current_attempts.saturating_add(1)
    }

    pub fn max_attempts(&self) -> u8 {
        self.max_attempts
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeferredChatItem {
    Message(DeferredChatMessage),
    Batch(DeferredChatBatch),
}

impl DeferredChatItem {
    pub fn target(&self) -> DeferredChatTarget {
        match self {
            Self::Message(message) => message.target,
            Self::Batch(batch) => batch.target,
        }
    }

    fn protected(&self) -> bool {
        matches!(self, Self::Batch(batch) if batch.protected)
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

#[derive(Clone)]
pub struct DeferredChatQueue {
    inner: Arc<(Mutex<VecDeque<DeferredChatItem>>, Condvar)>,
    capacity: usize,
}

impl DeferredChatQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
            capacity: capacity.max(1),
        }
    }

    pub fn enqueue(&self, item: impl Into<DeferredChatItem>) -> Result<EnqueueOutcome> {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("延迟聊天队列互斥锁已损坏"))?;
        let item = item.into();
        let outcome = make_room_for_enqueue(&mut queue, self.capacity, item.critical())?;
        if outcome == EnqueueOutcome::Rejected {
            return Ok(outcome);
        }
        queue.push_back(item);
        cvar.notify_one();
        Ok(outcome)
    }

    pub fn enqueue_front(&self, item: impl Into<DeferredChatItem>) -> Result<EnqueueOutcome> {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("延迟聊天队列互斥锁已损坏"))?;
        let item = item.into();
        let outcome = make_room_for_enqueue(&mut queue, self.capacity, item.critical())?;
        if outcome == EnqueueOutcome::Rejected {
            return Ok(outcome);
        }
        queue.push_front(item);
        cvar.notify_one();
        Ok(outcome)
    }

    pub fn requeue_front(&self, item: DeferredChatItem) -> Result<EnqueueOutcome> {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("延迟聊天队列互斥锁已损坏"))?;
        let outcome = make_room_for_requeue(&mut queue, self.capacity, item.protected(), true);
        if outcome == EnqueueOutcome::Rejected {
            return Ok(outcome);
        }
        queue.push_front(item);
        cvar.notify_one();
        Ok(outcome)
    }

    pub fn requeue_back(&self, item: DeferredChatItem) -> Result<EnqueueOutcome> {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("延迟聊天队列互斥锁已损坏"))?;
        let outcome = make_room_for_requeue(&mut queue, self.capacity, item.protected(), false);
        if outcome == EnqueueOutcome::Rejected {
            return Ok(outcome);
        }
        queue.push_back(item);
        cvar.notify_one();
        Ok(outcome)
    }

    pub fn wait_take(&self, timeout: Duration) -> Result<Option<DeferredChatItem>> {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("延迟聊天队列互斥锁已损坏"))?;
        if queue.is_empty() {
            let (waited, _) = cvar
                .wait_timeout(queue, timeout)
                .map_err(|_| anyhow!("延迟聊天队列条件变量已损坏"))?;
            queue = waited;
        }
        Ok(queue.pop_front())
    }

    pub fn notify_all(&self) {
        let (_, cvar) = &*self.inner;
        cvar.notify_all();
    }
}

impl TurtleSoupDeliveryPort for DeferredChatQueue {
    fn deliver_turtle_soup(
        &self,
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

fn make_room_for_enqueue(
    queue: &mut VecDeque<DeferredChatItem>,
    capacity: usize,
    incoming_critical: bool,
) -> Result<EnqueueOutcome> {
    if queue.len() < capacity {
        return Ok(EnqueueOutcome::Added);
    }
    let index = queue.iter().position(|item| !item.protected()).or_else(|| {
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
    protected: bool,
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
    } else if protected {
        // 受保护批次已经开始发送，必须能放回队列等待下一段或重试。
        EnqueueOutcome::Added
    } else {
        EnqueueOutcome::Rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::turtle_soup::{
        TurtleSoupDeliveryIntent, TurtleSoupDeliveryOutcome, TurtleSoupDeliveryPort,
    };

    fn message(text: &str) -> DeferredChatMessage {
        DeferredChatMessage {
            text: text.to_string(),
            target: DeferredChatTarget::Primary,
        }
    }

    fn item(text: &str) -> DeferredChatItem {
        message(text).into()
    }

    #[test]
    fn takes_messages_in_fifo_order() {
        let queue = DeferredChatQueue::new(3);
        queue.enqueue(message("first")).unwrap();
        queue.enqueue(message("second")).unwrap();

        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("first"))
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("second"))
        );
    }

    #[test]
    fn drops_the_oldest_message_when_full() {
        let queue = DeferredChatQueue::new(2);
        queue.enqueue(message("first")).unwrap();
        queue.enqueue(message("second")).unwrap();

        assert_eq!(
            queue.enqueue(message("third")).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("second"))
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("third"))
        );
    }

    #[test]
    fn requeue_front_preserves_the_deferred_message() {
        let queue = DeferredChatQueue::new(2);
        queue.enqueue(message("later")).unwrap();
        queue.enqueue(message("latest")).unwrap();

        assert_eq!(
            queue.requeue_front(item("retry")).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("retry"))
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("later"))
        );
    }

    #[test]
    fn requeue_back_yields_to_messages_for_the_active_target() {
        let queue = DeferredChatQueue::new(3);
        queue
            .enqueue(DeferredChatMessage {
                text: "secondary".to_string(),
                target: DeferredChatTarget::SecondaryCurrentHall,
            })
            .unwrap();
        queue.enqueue(message("primary")).unwrap();

        let secondary = queue.wait_take(Duration::ZERO).unwrap().unwrap();
        assert_eq!(
            queue.requeue_back(secondary).unwrap(),
            EnqueueOutcome::Added
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("primary"))
        );
    }

    #[test]
    fn protected_batch_is_not_evicted_by_normal_messages() {
        let queue = DeferredChatQueue::new(2);
        let batch = DeferredChatBatch::new(
            vec!["汤面1/1：测试".to_string()],
            DeferredChatTarget::CurrentHall,
            TurtleSoupDelivery {
                generation: 1,
                purpose: TurtleSoupDeliveryPurpose::Opening,
            },
            3,
            true,
        )
        .unwrap();
        queue.enqueue(batch.clone()).unwrap();
        queue.enqueue(message("normal-1")).unwrap();

        assert_eq!(
            queue.enqueue(message("normal-2")).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(DeferredChatItem::Batch(batch))
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("normal-2"))
        );
    }

    #[test]
    fn settlement_can_supersede_a_queued_surface_repeat() {
        let queue = DeferredChatQueue::new(1);
        let repeat = DeferredChatBatch::new(
            vec!["汤面1/1：测试".to_string()],
            DeferredChatTarget::CurrentHall,
            TurtleSoupDelivery {
                generation: 1,
                purpose: TurtleSoupDeliveryPurpose::SurfaceRepeat,
            },
            3,
            true,
        )
        .unwrap();
        let settlement = DeferredChatBatch::new(
            vec!["汤底1/1：测试".to_string()],
            DeferredChatTarget::CurrentHall,
            TurtleSoupDelivery {
                generation: 1,
                purpose: TurtleSoupDeliveryPurpose::Settlement,
            },
            3,
            true,
        )
        .unwrap();
        queue.enqueue(repeat).unwrap();

        assert_eq!(
            queue.enqueue(settlement.clone()).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(DeferredChatItem::Batch(settlement))
        );
    }

    #[test]
    fn critical_batch_can_enter_at_the_front() {
        let queue = DeferredChatQueue::new(2);
        queue.enqueue(message("normal")).unwrap();
        let opening = DeferredChatBatch::new(
            vec!["汤面1/1：测试".to_string()],
            DeferredChatTarget::CurrentHall,
            TurtleSoupDelivery {
                generation: 1,
                purpose: TurtleSoupDeliveryPurpose::Opening,
            },
            3,
            true,
        )
        .unwrap();

        queue.enqueue_front(opening.clone()).unwrap();

        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(DeferredChatItem::Batch(opening))
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("normal"))
        );
    }

    #[test]
    fn batch_consumes_only_successfully_sent_messages() {
        let mut batch = DeferredChatBatch::new(
            vec![
                "第一段".to_string(),
                "第二段".to_string(),
                "第三段".to_string(),
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

        assert!(!batch.mark_sent(2).unwrap());
        assert_eq!(batch.remaining_texts(), vec!["第三段"]);
        assert!(batch.mark_sent(1).unwrap());
    }

    #[test]
    fn interrupted_batch_keeps_the_current_failure_count() {
        let mut batch = DeferredChatBatch::new(
            vec!["第一段".to_string(), "第二段".to_string()],
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
    }

    #[test]
    fn partial_success_moves_failure_tracking_to_the_next_message() {
        let mut batch = DeferredChatBatch::new(
            vec!["第一段".to_string(), "第二段".to_string()],
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
        assert!(!batch.mark_sent(1).unwrap());
        assert_eq!(batch.remaining_texts(), vec!["第二段"]);
        assert_eq!(batch.current_attempt(), 1);
        assert_eq!(batch.mark_current_failed(), BatchFailureOutcome::Retry);
        assert_eq!(batch.current_attempt(), 2);
    }

    #[test]
    fn batch_rejects_an_invalid_success_count() {
        let mut batch = DeferredChatBatch::new(
            vec!["唯一一段".to_string()],
            DeferredChatTarget::CurrentHall,
            TurtleSoupDelivery {
                generation: 1,
                purpose: TurtleSoupDeliveryPurpose::Opening,
            },
            3,
            true,
        )
        .unwrap();

        assert!(batch.mark_sent(2).is_err());
        assert_eq!(batch.remaining_texts(), vec!["唯一一段"]);
    }

    #[test]
    fn turtle_soup_opening_delivery_is_urgent_and_protected() {
        let queue = DeferredChatQueue::new(2);
        queue.enqueue(message("older normal message")).unwrap();

        let outcome = queue
            .deliver_turtle_soup(
                TurtleSoupDeliveryIntent::new(
                    vec!["汤面1/1：测试".to_string()],
                    7,
                    TurtleSoupDeliveryPurpose::Opening,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(outcome, TurtleSoupDeliveryOutcome::Added);

        assert_eq!(
            queue.enqueue(message("newer normal message")).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert!(matches!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(DeferredChatItem::Batch(_))
        ));
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(item("newer normal message"))
        );
    }
}
