use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};

pub const DEFAULT_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Added,
    DroppedMessage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredChatTarget {
    Primary,
    SecondaryCurrentHall,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeferredChatMessage {
    pub text: String,
    pub target: DeferredChatTarget,
}

#[derive(Clone)]
pub struct DeferredChatQueue {
    inner: Arc<(Mutex<VecDeque<DeferredChatMessage>>, Condvar)>,
    capacity: usize,
}

impl DeferredChatQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
            capacity: capacity.max(1),
        }
    }

    pub fn enqueue(&self, message: DeferredChatMessage) -> Result<EnqueueOutcome> {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("deferred chat queue mutex poisoned"))?;
        let outcome = if queue.len() >= self.capacity {
            queue.pop_front();
            EnqueueOutcome::DroppedMessage
        } else {
            EnqueueOutcome::Added
        };
        queue.push_back(message);
        cvar.notify_one();
        Ok(outcome)
    }

    pub fn requeue_front(&self, message: DeferredChatMessage) -> Result<EnqueueOutcome> {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("deferred chat queue mutex poisoned"))?;
        let outcome = if queue.len() >= self.capacity {
            queue.pop_back();
            EnqueueOutcome::DroppedMessage
        } else {
            EnqueueOutcome::Added
        };
        queue.push_front(message);
        cvar.notify_one();
        Ok(outcome)
    }

    pub fn requeue_back(&self, message: DeferredChatMessage) -> Result<EnqueueOutcome> {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("deferred chat queue mutex poisoned"))?;
        if queue.len() >= self.capacity {
            return Ok(EnqueueOutcome::DroppedMessage);
        }
        queue.push_back(message);
        cvar.notify_one();
        Ok(EnqueueOutcome::Added)
    }

    pub fn wait_take(&self, timeout: Duration) -> Result<Option<DeferredChatMessage>> {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock
            .lock()
            .map_err(|_| anyhow!("deferred chat queue mutex poisoned"))?;
        if queue.is_empty() {
            let (waited, _) = cvar
                .wait_timeout(queue, timeout)
                .map_err(|_| anyhow!("deferred chat queue condvar poisoned"))?;
            queue = waited;
        }
        Ok(queue.pop_front())
    }

    pub fn notify_all(&self) {
        let (_, cvar) = &*self.inner;
        cvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str) -> DeferredChatMessage {
        DeferredChatMessage {
            text: text.to_string(),
            target: DeferredChatTarget::Primary,
        }
    }

    #[test]
    fn takes_messages_in_fifo_order() {
        let queue = DeferredChatQueue::new(3);
        queue.enqueue(message("first")).unwrap();
        queue.enqueue(message("second")).unwrap();

        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(message("first"))
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(message("second"))
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
            Some(message("second"))
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(message("third"))
        );
    }

    #[test]
    fn requeue_front_preserves_the_deferred_message() {
        let queue = DeferredChatQueue::new(2);
        queue.enqueue(message("later")).unwrap();
        queue.enqueue(message("latest")).unwrap();

        assert_eq!(
            queue.requeue_front(message("retry")).unwrap(),
            EnqueueOutcome::DroppedMessage
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(message("retry"))
        );
        assert_eq!(
            queue.wait_take(Duration::ZERO).unwrap(),
            Some(message("later"))
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
            Some(message("primary"))
        );
    }
}
