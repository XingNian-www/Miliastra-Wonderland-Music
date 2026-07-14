use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObservationSequence(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationGap {
    pub missing_from: ObservationSequence,
    pub missing_through: ObservationSequence,
    pub resume_at: ObservationSequence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservationRead<T> {
    Item {
        sequence: ObservationSequence,
        value: Arc<T>,
    },
    Gap(ObservationGap),
}

impl<T> ObservationRead<T> {
    pub fn item(sequence: ObservationSequence, value: T) -> Self {
        Self::Item {
            sequence,
            value: Arc::new(value),
        }
    }
}

pub struct SharedObservationStream<T> {
    capacity: NonZeroUsize,
    entries: VecDeque<(ObservationSequence, Arc<T>)>,
    next_sequence: u64,
}

impl<T> SharedObservationStream<T> {
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity.get()),
            next_sequence: 1,
        }
    }

    pub fn subscribe(&self) -> ObservationSubscriber {
        ObservationSubscriber {
            next_sequence: self.next_sequence,
        }
    }

    pub fn publish(&mut self, value: T) -> ObservationSequence {
        let sequence = ObservationSequence(self.next_sequence);
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("observation sequence exhausted");
        self.entries.push_back((sequence, Arc::new(value)));
        if self.entries.len() > self.capacity.get() {
            self.entries.pop_front();
        }
        sequence
    }
}

pub struct ObservationSubscriber {
    next_sequence: u64,
}

impl ObservationSubscriber {
    pub fn read_next<T>(
        &mut self,
        stream: &SharedObservationStream<T>,
    ) -> Option<ObservationRead<T>> {
        let (oldest, _) = stream.entries.front()?;
        if self.next_sequence < oldest.0 {
            let gap = ObservationGap {
                missing_from: ObservationSequence(self.next_sequence),
                missing_through: ObservationSequence(oldest.0 - 1),
                resume_at: *oldest,
            };
            self.next_sequence = oldest.0;
            return Some(ObservationRead::Gap(gap));
        }
        if self.next_sequence >= stream.next_sequence {
            return None;
        }

        let offset = (self.next_sequence - oldest.0) as usize;
        let (sequence, value) = stream.entries.get(offset)?;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .expect("observation subscriber sequence exhausted");
        Some(ObservationRead::Item {
            sequence: *sequence,
            value: Arc::clone(value),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_subscriber_receives_a_gap_without_blocking_the_shared_stream() {
        let mut stream = SharedObservationStream::new(NonZeroUsize::new(2).unwrap());
        let mut slow = stream.subscribe();
        let mut fast = stream.subscribe();

        let first = stream.publish("first");
        let second = stream.publish("second");
        assert_eq!(
            fast.read_next(&stream),
            Some(ObservationRead::item(first, "first"))
        );
        assert_eq!(
            fast.read_next(&stream),
            Some(ObservationRead::item(second, "second"))
        );

        let third = stream.publish("third");
        assert_eq!(
            slow.read_next(&stream),
            Some(ObservationRead::Gap(ObservationGap {
                missing_from: first,
                missing_through: first,
                resume_at: second,
            }))
        );
        assert_eq!(
            slow.read_next(&stream),
            Some(ObservationRead::item(second, "second"))
        );
        assert_eq!(
            slow.read_next(&stream),
            Some(ObservationRead::item(third, "third"))
        );
        assert_eq!(
            fast.read_next(&stream),
            Some(ObservationRead::item(third, "third"))
        );
    }
}
