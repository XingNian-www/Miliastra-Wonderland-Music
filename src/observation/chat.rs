use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VisualSessionId(u64);

impl VisualSessionId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ChatIdentity {
    PrimaryHall,
    SecondaryHall,
    Friend(Arc<str>),
    PublicChannel,
    StrangerMessages,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BubbleSequence(u64);

impl BubbleSequence {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObservedChatMessageId {
    pub visual_session: VisualSessionId,
    pub chat: ChatIdentity,
    pub bubble_sequence: BubbleSequence,
}

impl ObservedChatMessageId {
    pub fn new(
        visual_session: VisualSessionId,
        chat: ChatIdentity,
        bubble_sequence: BubbleSequence,
    ) -> Self {
        Self {
            visual_session,
            chat,
            bubble_sequence,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObservationFrameId(u64);

impl ObservationFrameId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservedFrame {
    id: ObservationFrameId,
    captured_at: Instant,
}

impl ObservedFrame {
    pub fn id(self) -> ObservationFrameId {
        self.id
    }

    pub fn captured_at(self) -> Instant {
        self.captured_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameCompletionOutcome {
    Success,
    TerminalFailure(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedObservationFrame {
    frame: ObservedFrame,
    outcome: FrameCompletionOutcome,
}

impl CompletedObservationFrame {
    pub fn frame(&self) -> ObservedFrame {
        self.frame
    }

    pub fn outcome(&self) -> &FrameCompletionOutcome {
        &self.outcome
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObservationWatermark {
    pub completed_through: ObservationFrameId,
    pub captured_through: Instant,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionAdvance {
    completed: Vec<CompletedObservationFrame>,
    watermark: Option<ObservationWatermark>,
}

impl CompletionAdvance {
    pub fn completed(&self) -> &[CompletedObservationFrame] {
        &self.completed
    }

    pub fn watermark(&self) -> Option<ObservationWatermark> {
        self.watermark
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationLedgerError {
    UnknownFrame(ObservationFrameId),
    AlreadyCompleted(ObservationFrameId),
}

impl fmt::Display for ObservationLedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFrame(frame) => {
                write!(formatter, "observation frame {} is unknown", frame.get())
            }
            Self::AlreadyCompleted(frame) => {
                write!(
                    formatter,
                    "observation frame {} is already complete",
                    frame.get()
                )
            }
        }
    }
}

impl Error for ObservationLedgerError {}

struct PendingFrame {
    frame: ObservedFrame,
    outcome: Option<FrameCompletionOutcome>,
}

pub struct ChatObservationLedger {
    pending: BTreeMap<ObservationFrameId, PendingFrame>,
    next_frame_id: u64,
    next_to_release: u64,
    watermark: Option<ObservationWatermark>,
}

impl ChatObservationLedger {
    pub fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            next_frame_id: 1,
            next_to_release: 1,
            watermark: None,
        }
    }

    pub fn begin_frame(&mut self, captured_at: Instant) -> ObservedFrame {
        let id = ObservationFrameId(self.next_frame_id);
        self.next_frame_id = self
            .next_frame_id
            .checked_add(1)
            .expect("observation frame sequence exhausted");
        let frame = ObservedFrame { id, captured_at };
        self.pending.insert(
            id,
            PendingFrame {
                frame,
                outcome: None,
            },
        );
        frame
    }

    pub fn complete_success(
        &mut self,
        frame: ObservationFrameId,
    ) -> Result<CompletionAdvance, ObservationLedgerError> {
        self.complete(frame, FrameCompletionOutcome::Success)
    }

    pub fn complete_failure(
        &mut self,
        frame: ObservationFrameId,
        reason: impl Into<Arc<str>>,
    ) -> Result<CompletionAdvance, ObservationLedgerError> {
        self.complete(
            frame,
            FrameCompletionOutcome::TerminalFailure(reason.into()),
        )
    }

    pub fn watermark(&self) -> Option<ObservationWatermark> {
        self.watermark
    }

    fn complete(
        &mut self,
        frame: ObservationFrameId,
        outcome: FrameCompletionOutcome,
    ) -> Result<CompletionAdvance, ObservationLedgerError> {
        let pending = self
            .pending
            .get_mut(&frame)
            .ok_or(ObservationLedgerError::UnknownFrame(frame))?;
        if pending.outcome.is_some() {
            return Err(ObservationLedgerError::AlreadyCompleted(frame));
        }
        pending.outcome = Some(outcome);

        let mut completed = Vec::new();
        loop {
            let id = ObservationFrameId(self.next_to_release);
            let is_complete = self
                .pending
                .get(&id)
                .is_some_and(|pending| pending.outcome.is_some());
            if !is_complete {
                break;
            }
            let pending = self
                .pending
                .remove(&id)
                .expect("complete pending frame checked above");
            let outcome = pending.outcome.expect("complete outcome checked above");
            self.next_to_release = self
                .next_to_release
                .checked_add(1)
                .expect("observation release sequence exhausted");
            self.watermark = Some(ObservationWatermark {
                completed_through: pending.frame.id,
                captured_through: pending.frame.captured_at,
            });
            completed.push(CompletedObservationFrame {
                frame: pending.frame,
                outcome,
            });
        }

        let watermark = if completed.is_empty() {
            None
        } else {
            Some(
                self.watermark
                    .expect("released frames always advance the watermark"),
            )
        };
        Ok(CompletionAdvance {
            completed,
            watermark,
        })
    }
}

impl Default for ChatObservationLedger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn terminal_failure_releases_the_completion_watermark_past_later_finished_frames() {
        let mut ledger = ChatObservationLedger::new();
        let started = Instant::now();
        let first = ledger.begin_frame(started);
        let second = ledger.begin_frame(started + Duration::from_millis(20));

        let blocked = ledger.complete_success(second.id()).unwrap();
        assert!(blocked.completed().is_empty());
        assert_eq!(blocked.watermark(), None);

        let released = ledger
            .complete_failure(first.id(), "OCR retry exhausted")
            .unwrap();
        assert_eq!(released.completed().len(), 2);
        assert!(matches!(
            released.completed()[0].outcome(),
            FrameCompletionOutcome::TerminalFailure(reason)
                if reason.as_ref() == "OCR retry exhausted"
        ));
        assert_eq!(
            released.completed()[1].outcome(),
            &FrameCompletionOutcome::Success
        );
        assert_eq!(
            released.watermark(),
            Some(ObservationWatermark {
                completed_through: second.id(),
                captured_through: second.captured_at(),
            })
        );
    }
}
