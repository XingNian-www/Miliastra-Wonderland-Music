use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObservationSequence(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationGapKind {
    HistoryEvicted,
    ExclusiveSession,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationGap {
    pub kind: ObservationGapKind,
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
    entries: VecDeque<StoredObservation<T>>,
    next_sequence: u64,
}

enum StoredObservation<T> {
    Item {
        sequence: ObservationSequence,
        value: Arc<T>,
    },
    Gap(ObservationGap),
}

impl<T> StoredObservation<T> {
    fn first_sequence(&self) -> ObservationSequence {
        match self {
            Self::Item { sequence, .. } => *sequence,
            Self::Gap(gap) => gap.missing_from,
        }
    }
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
        self.next_sequence = next_sequence(self.next_sequence);
        self.push(StoredObservation::Item {
            sequence,
            value: Arc::new(value),
        });
        sequence
    }

    pub fn mark_gap(&mut self, kind: ObservationGapKind) -> ObservationGap {
        let missing = ObservationSequence(self.next_sequence);
        self.next_sequence = next_sequence(self.next_sequence);
        let gap = ObservationGap {
            kind,
            missing_from: missing,
            missing_through: missing,
            resume_at: ObservationSequence(self.next_sequence),
        };
        self.push(StoredObservation::Gap(gap.clone()));
        gap
    }

    fn push(&mut self, entry: StoredObservation<T>) {
        self.entries.push_back(entry);
        if self.entries.len() > self.capacity.get() {
            self.entries.pop_front();
        }
    }
}

fn next_sequence(sequence: u64) -> u64 {
    sequence
        .checked_add(1)
        .expect("observation sequence exhausted")
}

pub struct ObservationSubscriber {
    next_sequence: u64,
}

impl ObservationSubscriber {
    pub fn read_next<T>(
        &mut self,
        stream: &SharedObservationStream<T>,
    ) -> Option<ObservationRead<T>> {
        let oldest = stream.entries.front()?.first_sequence();
        if self.next_sequence < oldest.0 {
            let gap = ObservationGap {
                kind: ObservationGapKind::HistoryEvicted,
                missing_from: ObservationSequence(self.next_sequence),
                missing_through: ObservationSequence(oldest.0 - 1),
                resume_at: oldest,
            };
            self.next_sequence = oldest.0;
            return Some(ObservationRead::Gap(gap));
        }
        if self.next_sequence >= stream.next_sequence {
            return None;
        }

        for entry in &stream.entries {
            match entry {
                StoredObservation::Item { sequence, value } if sequence.0 == self.next_sequence => {
                    self.next_sequence = next_sequence(self.next_sequence);
                    return Some(ObservationRead::Item {
                        sequence: *sequence,
                        value: Arc::clone(value),
                    });
                }
                StoredObservation::Gap(gap)
                    if self.next_sequence >= gap.missing_from.0
                        && self.next_sequence <= gap.missing_through.0 =>
                {
                    self.next_sequence = gap.resume_at.0;
                    return Some(ObservationRead::Gap(gap.clone()));
                }
                _ => {}
            }
        }
        None
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
                kind: ObservationGapKind::HistoryEvicted,
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
