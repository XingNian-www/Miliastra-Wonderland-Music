use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::observation::chat::{
    CompletionAdvance, ObservationCompletionEvent, ObservationWatermark,
};
use crate::observation::shared::ObservationGap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusinessEvent {
    CompletionAdvance(CompletionAdvance),
    CompletionGap(ObservationGap),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusinessRuntimeSnapshot {
    latest_watermark: Option<ObservationWatermark>,
    terminal_failure_count: u64,
    completion_gap_count: u64,
}

impl BusinessRuntimeSnapshot {
    pub const fn latest_watermark(self) -> Option<ObservationWatermark> {
        self.latest_watermark
    }

    pub const fn terminal_failure_count(self) -> u64 {
        self.terminal_failure_count
    }

    pub const fn completion_gap_count(self) -> u64 {
        self.completion_gap_count
    }

    fn apply(&mut self, event: BusinessEvent) {
        match event {
            BusinessEvent::CompletionAdvance(advance) => {
                self.terminal_failure_count = self.terminal_failure_count.saturating_add(
                    advance
                        .events()
                        .iter()
                        .filter(|event| {
                            matches!(event, ObservationCompletionEvent::TerminalFailure { .. })
                        })
                        .count() as u64,
                );
                if let Some(watermark) = advance.watermark() {
                    self.latest_watermark = Some(watermark);
                }
            }
            BusinessEvent::CompletionGap(_) => {
                self.completion_gap_count = self.completion_gap_count.saturating_add(1);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusinessRuntimeError {
    ZeroQueueCapacity,
    RuntimeStopped,
    WorkerPanicked,
}

impl Display for BusinessRuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroQueueCapacity => {
                formatter.write_str("business runtime queue capacity must be greater than zero")
            }
            Self::RuntimeStopped => formatter.write_str("business runtime is stopped"),
            Self::WorkerPanicked => formatter.write_str("business runtime worker panicked"),
        }
    }
}

impl Error for BusinessRuntimeError {}

enum RuntimeMessage {
    Event(BusinessEvent),
    Snapshot(SyncSender<BusinessRuntimeSnapshot>),
    Shutdown(SyncSender<BusinessRuntimeSnapshot>),
}

struct RuntimeChannel {
    sender: SyncSender<RuntimeMessage>,
    accepting: Mutex<bool>,
}

#[derive(Clone)]
pub struct BusinessRuntimeHandle {
    channel: Arc<RuntimeChannel>,
}

impl BusinessRuntimeHandle {
    pub fn submit(&self, event: BusinessEvent) -> Result<(), BusinessRuntimeError> {
        self.send(RuntimeMessage::Event(event))
    }

    pub fn snapshot(&self) -> Result<BusinessRuntimeSnapshot, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(RuntimeMessage::Snapshot(response))?;
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }

    fn send(&self, message: RuntimeMessage) -> Result<(), BusinessRuntimeError> {
        let accepting = self
            .channel
            .accepting
            .lock()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
        if !*accepting {
            return Err(BusinessRuntimeError::RuntimeStopped);
        }
        self.channel
            .sender
            .send(message)
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }
}

pub struct BusinessRuntime {
    handle: BusinessRuntimeHandle,
    worker: Option<JoinHandle<()>>,
}

impl BusinessRuntime {
    pub fn start(queue_capacity: usize) -> Result<Self, BusinessRuntimeError> {
        if queue_capacity == 0 {
            return Err(BusinessRuntimeError::ZeroQueueCapacity);
        }
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let channel = Arc::new(RuntimeChannel {
            sender,
            accepting: Mutex::new(true),
        });
        let worker = thread::Builder::new()
            .name("business-runtime".to_string())
            .spawn(move || run_business_runtime(receiver))
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
        Ok(Self {
            handle: BusinessRuntimeHandle { channel },
            worker: Some(worker),
        })
    }

    pub fn handle(&self) -> BusinessRuntimeHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) -> Result<BusinessRuntimeSnapshot, BusinessRuntimeError> {
        self.stop_worker()
    }

    fn stop_worker(&mut self) -> Result<BusinessRuntimeSnapshot, BusinessRuntimeError> {
        let Some(worker) = self.worker.take() else {
            return Err(BusinessRuntimeError::RuntimeStopped);
        };
        let (response, receiver) = mpsc::sync_channel(1);
        let sent = {
            let mut accepting = self
                .handle
                .channel
                .accepting
                .lock()
                .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
            *accepting = false;
            self.handle
                .channel
                .sender
                .send(RuntimeMessage::Shutdown(response))
                .is_ok()
        };
        let snapshot = sent.then(|| receiver.recv().ok()).flatten();
        worker
            .join()
            .map_err(|_| BusinessRuntimeError::WorkerPanicked)?;
        snapshot.ok_or(BusinessRuntimeError::RuntimeStopped)
    }
}

impl Drop for BusinessRuntime {
    fn drop(&mut self) {
        let _ = self.stop_worker();
    }
}

fn run_business_runtime(receiver: Receiver<RuntimeMessage>) {
    let mut snapshot = BusinessRuntimeSnapshot::default();
    while let Ok(message) = receiver.recv() {
        match message {
            RuntimeMessage::Event(event) => snapshot.apply(event),
            RuntimeMessage::Snapshot(response) => {
                let _ = response.send(snapshot);
            }
            RuntimeMessage::Shutdown(response) => {
                let _ = response.send(snapshot);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Instant;

    use super::*;
    use crate::observation::chat::ChatObservationLedger;
    use crate::observation::shared::{ObservationGapKind, SharedObservationStream};

    #[test]
    fn real_channel_applies_completion_events_in_order() {
        let runtime = BusinessRuntime::start(4).unwrap();
        let handle = runtime.handle();
        let mut ledger = ChatObservationLedger::new();
        let first = ledger.begin_frame(Instant::now());
        let second = ledger.begin_frame(Instant::now());
        let blocked = ledger.complete_success(second.id()).unwrap();
        let advance = ledger.complete_failure(first.id(), "failed").unwrap();

        handle
            .submit(BusinessEvent::CompletionAdvance(blocked))
            .unwrap();
        handle
            .submit(BusinessEvent::CompletionAdvance(advance))
            .unwrap();
        let snapshot = handle.snapshot().unwrap();

        assert_eq!(
            snapshot.latest_watermark().unwrap().completed_through,
            second.id()
        );
        assert_eq!(snapshot.terminal_failure_count(), 1);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn completion_gap_is_counted_without_inventing_a_watermark() {
        let runtime = BusinessRuntime::start(2).unwrap();
        let handle = runtime.handle();
        let mut stream = SharedObservationStream::<()>::new(NonZeroUsize::new(1).unwrap());
        let gap = stream.mark_gap(ObservationGapKind::HistoryEvicted);

        handle.submit(BusinessEvent::CompletionGap(gap)).unwrap();
        let snapshot = handle.snapshot().unwrap();

        assert_eq!(snapshot.completion_gap_count(), 1);
        assert_eq!(snapshot.latest_watermark(), None);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_drains_prior_events_and_stops_cloned_handles() {
        let runtime = BusinessRuntime::start(1).unwrap();
        let handle = runtime.handle();
        let mut ledger = ChatObservationLedger::new();
        let frame = ledger.begin_frame(Instant::now());
        handle
            .submit(BusinessEvent::CompletionAdvance(
                ledger.complete_failure(frame.id(), "failed").unwrap(),
            ))
            .unwrap();

        let final_snapshot = runtime.shutdown().unwrap();

        assert_eq!(final_snapshot.terminal_failure_count(), 1);
        assert_eq!(handle.snapshot(), Err(BusinessRuntimeError::RuntimeStopped));
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert!(matches!(
            BusinessRuntime::start(0),
            Err(BusinessRuntimeError::ZeroQueueCapacity)
        ));
    }
}
