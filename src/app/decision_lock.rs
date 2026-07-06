use std::collections::HashMap;

use super::chat_scan::ChatMessage;

const BOTTOM_TOLERANCE_PX: i32 = 8;

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
struct DecisionMessageKey {
    message_type: String,
    text: String,
}

#[derive(Clone, Debug, Default)]
pub(super) struct DecisionScreenLock {
    existing_bottoms: HashMap<DecisionMessageKey, i32>,
    consumed_bottoms: HashMap<DecisionMessageKey, i32>,
}

impl DecisionScreenLock {
    pub(super) fn from_messages<A, P>(
        messages: &[ChatMessage],
        accepts_message_type: &A,
        is_decision: &P,
    ) -> Self
    where
        A: Fn(&str) -> bool,
        P: Fn(&str) -> bool,
    {
        let mut lock = Self::default();
        for message in messages {
            if accepts_message_type(&message.message_type) && is_decision(&message.text) {
                lock.record_existing(message);
            }
        }
        lock
    }

    pub(super) fn is_existing(&self, message: &ChatMessage) -> bool {
        self.has_record(&self.existing_bottoms, message)
    }

    pub(super) fn accept_once(&mut self, message: &ChatMessage) -> bool {
        if self.is_existing(message) || self.has_record(&self.consumed_bottoms, message) {
            return false;
        }
        self.record_consumed(message);
        true
    }

    fn record_existing(&mut self, message: &ChatMessage) {
        record_bottom(&mut self.existing_bottoms, message);
    }

    fn record_consumed(&mut self, message: &ChatMessage) {
        record_bottom(&mut self.consumed_bottoms, message);
    }

    fn has_record(
        &self,
        records: &HashMap<DecisionMessageKey, i32>,
        message: &ChatMessage,
    ) -> bool {
        let bottom = message.block.bottom();
        records
            .get(&message_key(message))
            .is_some_and(|existing_bottom| bottom <= existing_bottom + BOTTOM_TOLERANCE_PX)
    }
}

fn record_bottom(records: &mut HashMap<DecisionMessageKey, i32>, message: &ChatMessage) {
    let bottom = message.block.bottom();
    records
        .entry(message_key(message))
        .and_modify(|value| *value = (*value).max(bottom))
        .or_insert(bottom);
}

fn message_key(message: &ChatMessage) -> DecisionMessageKey {
    DecisionMessageKey {
        message_type: message.message_type.clone(),
        text: message.text.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::geometry::Rect;
    use super::*;

    fn message(message_type: &str, y: i32, text: &str) -> ChatMessage {
        ChatMessage {
            message_type: message_type.to_string(),
            block: Rect::new(0, y, 100, 20),
            text: text.to_string(),
        }
    }

    #[test]
    fn locks_existing_decision_with_small_ocr_position_jitter() {
        let existing = [message("blue", 100, "用户：@确认")];
        let lock = DecisionScreenLock::from_messages(
            &existing,
            &|message_type| message_type == "blue",
            &|text| text.contains("@确认"),
        );

        assert!(lock.is_existing(&message("blue", 107, "用户：@确认")));
        assert!(!lock.is_existing(&message("blue", 140, "用户：@确认")));
    }

    #[test]
    fn consumes_same_visible_decision_once() {
        let mut lock = DecisionScreenLock::default();

        assert!(lock.accept_once(&message("blue", 100, "用户：@确认")));
        assert!(!lock.accept_once(&message("blue", 104, "用户：@确认")));
        assert!(lock.accept_once(&message("blue", 140, "用户：@确认")));
    }
}
