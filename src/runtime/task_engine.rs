use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::features::turtle_soup::{
    TurtleSoupDeliveryIntent, TurtleSoupDeliveryOutcome, TurtleSoupDeliveryPort,
};
use crate::runtime::deferred_chat::{
    DEFAULT_CAPACITY as DEFERRED_CHAT_CAPACITY, DeferredChatItem, DeferredChatQueue, EnqueueOutcome,
};
use crate::runtime::scheduler::{
    DiagnosticTaskCompletion, DiagnosticTaskLease, DiagnosticTaskSnapshot,
    DiagnosticTaskSubmission, FormalScheduler, FormalSchedulerError, FormalSchedulerSnapshot,
    FormalTaskCancelAction, FormalTaskCancelOutcome, FormalTaskCompletion, FormalTaskDedupKey,
    FormalTaskEnqueueOutcome, FormalTaskLease, FormalTaskSubmission, SchedulerLane,
    SchedulerLaneLease,
};

pub(crate) trait TaskEngineStateSink: Send + Sync {
    fn publish_scheduler(&self, _snapshot: FormalSchedulerSnapshot) {}
    fn publish_diagnostics(&self, _snapshot: Vec<DiagnosticTaskSnapshot>) {}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskEngineError(String);

impl TaskEngineError {
    fn state_unavailable() -> Self {
        Self("任务引擎状态不可用".to_string())
    }
}

impl Display for TaskEngineError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for TaskEngineError {}

impl From<FormalSchedulerError> for TaskEngineError {
    fn from(error: FormalSchedulerError) -> Self {
        Self(error.to_string())
    }
}

struct TaskEngineState {
    scheduler: FormalScheduler,
    deferred_chat: DeferredChatQueue,
    generation: u64,
    accepting: bool,
}

struct TaskEngineInner {
    state: Mutex<TaskEngineState>,
    changed: Condvar,
    state_sink: Option<Arc<dyn TaskEngineStateSink>>,
}

#[derive(Clone)]
pub(crate) struct TaskEngineHandle {
    inner: Arc<TaskEngineInner>,
}

pub(crate) struct TaskEnginePermit {
    engine: TaskEngineHandle,
    lease: Option<SchedulerLaneLease>,
}

impl Drop for TaskEnginePermit {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        if let Err(error) = self.engine.release_lane(lease) {
            log::debug!("释放任务引擎通道失败: {error}");
        }
    }
}

impl TaskEngineHandle {
    pub(crate) fn new(state_sink: Option<Arc<dyn TaskEngineStateSink>>) -> Self {
        let engine = Self {
            inner: Arc::new(TaskEngineInner {
                state: Mutex::new(TaskEngineState {
                    scheduler: FormalScheduler::new(),
                    deferred_chat: DeferredChatQueue::new(DEFERRED_CHAT_CAPACITY),
                    generation: 0,
                    accepting: true,
                }),
                changed: Condvar::new(),
                state_sink,
            }),
        };
        engine.publish_current_state();
        engine
    }

    pub(crate) fn enqueue_formal(
        &self,
        submission: FormalTaskSubmission,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        let (outcome, scheduler) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let outcome = state.scheduler.enqueue(submission)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::scheduler_snapshot(&state))
        };
        self.publish_scheduler(scheduler);
        Ok(outcome)
    }

    pub(crate) fn enqueue_deferred(
        &self,
        item: impl Into<DeferredChatItem>,
    ) -> Result<EnqueueOutcome, TaskEngineError> {
        let (outcome, scheduler) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let outcome = state
                .deferred_chat
                .enqueue(item)
                .map_err(|error| TaskEngineError(error.to_string()))?;
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::scheduler_snapshot(&state))
        };
        self.publish_scheduler(scheduler);
        Ok(outcome)
    }

    pub(crate) fn requeue_deferred_front(
        &self,
        item: DeferredChatItem,
    ) -> Result<EnqueueOutcome, TaskEngineError> {
        let (outcome, scheduler) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let outcome = state.deferred_chat.requeue_front(item);
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::scheduler_snapshot(&state))
        };
        self.publish_scheduler(scheduler);
        Ok(outcome)
    }

    pub(crate) fn requeue_deferred_back(
        &self,
        item: DeferredChatItem,
    ) -> Result<EnqueueOutcome, TaskEngineError> {
        let (outcome, scheduler) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let outcome = state.deferred_chat.requeue_back(item);
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::scheduler_snapshot(&state))
        };
        self.publish_scheduler(scheduler);
        Ok(outcome)
    }

    pub(crate) fn enqueue_diagnostic(
        &self,
        submission: DiagnosticTaskSubmission,
    ) -> Result<DiagnosticTaskSnapshot, TaskEngineError> {
        let (task, scheduler, diagnostics) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let task = state.scheduler.enqueue_diagnostic(submission)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            (
                task,
                Self::scheduler_snapshot(&state),
                state.scheduler.diagnostic_snapshot(),
            )
        };
        self.publish_scheduler(scheduler);
        self.publish_diagnostics(diagnostics);
        Ok(task)
    }

    pub(crate) fn take_next_formal(&self) -> Result<Option<FormalTaskLease>, TaskEngineError> {
        let (task, scheduler) = {
            let mut state = self.lock_state()?;
            let task = state.scheduler.take_next();
            let scheduler = task.as_ref().map(|_| {
                Self::mark_changed(&mut state, &self.inner.changed);
                Self::scheduler_snapshot(&state)
            });
            (task, scheduler)
        };
        if let Some(scheduler) = scheduler {
            self.publish_scheduler(scheduler);
        }
        Ok(task)
    }

    pub(crate) fn restore_formal(&self, lease: FormalTaskLease) -> Result<(), TaskEngineError> {
        let (shutdown_plan, scheduler, diagnostics) = {
            let mut state = self.lock_state()?;
            state.scheduler.restore(lease)?;
            let shutdown_plan = if state.accepting {
                None
            } else {
                Some(state.scheduler.begin_shutdown())
            };
            Self::mark_changed(&mut state, &self.inner.changed);
            (
                shutdown_plan,
                Self::scheduler_snapshot(&state),
                state.scheduler.diagnostic_snapshot(),
            )
        };
        if let Some(plan) = shutdown_plan {
            plan.cancel_queued();
        }
        self.publish_scheduler(scheduler);
        self.publish_diagnostics(diagnostics);
        Ok(())
    }

    pub(crate) fn take_next_deferred(
        &self,
    ) -> Result<Option<(DeferredChatItem, TaskEnginePermit)>, TaskEngineError> {
        let (item, lease, scheduler) = {
            let mut state = self.lock_state()?;
            if state.deferred_chat.is_empty() {
                return Ok(None);
            }
            let Some(lease) = state.scheduler.try_acquire_lane(SchedulerLane::Deferred)? else {
                return Ok(None);
            };
            let item = state
                .deferred_chat
                .take_next()
                .expect("deferred queue was checked as non-empty");
            Self::mark_changed(&mut state, &self.inner.changed);
            (item, lease, Self::scheduler_snapshot(&state))
        };
        self.publish_scheduler(scheduler);
        Ok(Some((
            item,
            TaskEnginePermit {
                engine: self.clone(),
                lease: Some(lease),
            },
        )))
    }

    pub(crate) fn take_next_diagnostic(
        &self,
    ) -> Result<Option<DiagnosticTaskLease>, TaskEngineError> {
        let (task, scheduler, diagnostics) = {
            let mut state = self.lock_state()?;
            if !state.deferred_chat.is_empty() {
                return Ok(None);
            }
            let task = state.scheduler.take_next_diagnostic();
            let Some(_) = task.as_ref() else {
                return Ok(None);
            };
            Self::mark_changed(&mut state, &self.inner.changed);
            (
                task,
                Self::scheduler_snapshot(&state),
                state.scheduler.diagnostic_snapshot(),
            )
        };
        self.publish_scheduler(scheduler);
        self.publish_diagnostics(diagnostics);
        Ok(task)
    }

    pub(crate) fn complete_formal(
        &self,
        task_id: u64,
        completion: FormalTaskCompletion,
    ) -> Result<(), TaskEngineError> {
        let scheduler = {
            let mut state = self.lock_state()?;
            state.scheduler.complete(task_id, completion)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            Self::scheduler_snapshot(&state)
        };
        self.publish_scheduler(scheduler);
        Ok(())
    }

    pub(crate) fn complete_diagnostic(
        &self,
        task_id: u64,
        completion: DiagnosticTaskCompletion,
    ) -> Result<(), TaskEngineError> {
        let (scheduler, diagnostics) = {
            let mut state = self.lock_state()?;
            state.scheduler.complete_diagnostic(task_id, completion)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            (
                Self::scheduler_snapshot(&state),
                state.scheduler.diagnostic_snapshot(),
            )
        };
        self.publish_scheduler(scheduler);
        self.publish_diagnostics(diagnostics);
        Ok(())
    }

    pub(crate) fn cancel_formal(
        &self,
        task_id: u64,
    ) -> Result<FormalTaskCancelOutcome, TaskEngineError> {
        let (action, scheduler) = {
            let mut state = self.lock_state()?;
            let action = state.scheduler.cancel(task_id);
            let scheduler = if matches!(action, FormalTaskCancelAction::NotFound) {
                None
            } else {
                Self::mark_changed(&mut state, &self.inner.changed);
                Some(Self::scheduler_snapshot(&state))
            };
            (action, scheduler)
        };
        if let Some(scheduler) = scheduler {
            self.publish_scheduler(scheduler);
        }
        Ok(match action {
            FormalTaskCancelAction::CanceledBeforeStart(work) => {
                work.cancel();
                FormalTaskCancelOutcome::CanceledBeforeStart
            }
            FormalTaskCancelAction::CancellationRequested => {
                FormalTaskCancelOutcome::CancellationRequested
            }
            FormalTaskCancelAction::AlreadyFinished => FormalTaskCancelOutcome::AlreadyFinished,
            FormalTaskCancelAction::NotFound => FormalTaskCancelOutcome::NotFound,
        })
    }

    pub(crate) fn snapshot(&self) -> Result<FormalSchedulerSnapshot, TaskEngineError> {
        let state = self.lock_state()?;
        Ok(Self::scheduler_snapshot(&state))
    }

    pub(crate) fn diagnostic_task_snapshot(
        &self,
        id: u64,
    ) -> Result<Option<DiagnosticTaskSnapshot>, TaskEngineError> {
        Ok(self.lock_state()?.scheduler.diagnostic_task_snapshot(id))
    }

    pub(crate) fn contains_formal_dedup_key(
        &self,
        key: &FormalTaskDedupKey,
    ) -> Result<bool, TaskEngineError> {
        Ok(self.lock_state()?.scheduler.contains_dedup_key(key))
    }

    pub(crate) fn is_idle(&self) -> Result<bool, TaskEngineError> {
        let state = self.lock_state()?;
        Ok(Self::scheduler_snapshot(&state).is_idle())
    }

    pub(crate) fn generation(&self) -> Result<u64, TaskEngineError> {
        Ok(self.lock_state()?.generation)
    }

    pub(crate) fn wait_for_change(&self, observed_generation: u64) -> Result<u64, TaskEngineError> {
        let state = self.lock_state()?;
        if state.generation != observed_generation {
            return Ok(state.generation);
        }
        let state = self
            .inner
            .changed
            .wait_while(state, |state| state.generation == observed_generation)
            .map_err(|_| TaskEngineError::state_unavailable())?;
        Ok(state.generation)
    }

    pub(crate) fn wait_for_change_timeout(
        &self,
        observed_generation: u64,
        timeout: Duration,
    ) -> Result<u64, TaskEngineError> {
        let state = self.lock_state()?;
        if state.generation != observed_generation {
            return Ok(state.generation);
        }
        let (state, _) = self
            .inner
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.generation == observed_generation
            })
            .map_err(|_| TaskEngineError::state_unavailable())?;
        Ok(state.generation)
    }

    pub(crate) fn wake(&self) -> Result<(), TaskEngineError> {
        let mut state = self.lock_state()?;
        Self::mark_changed(&mut state, &self.inner.changed);
        Ok(())
    }

    pub(crate) fn begin_shutdown(&self) -> Result<Option<u64>, TaskEngineError> {
        let (plan, scheduler, diagnostics) = {
            let mut state = self.lock_state()?;
            state.accepting = false;
            let plan = state.scheduler.begin_shutdown();
            state.deferred_chat.clear();
            Self::mark_changed(&mut state, &self.inner.changed);
            (
                plan,
                Self::scheduler_snapshot(&state),
                state.scheduler.diagnostic_snapshot(),
            )
        };
        let active_task_id = plan.active_task_id();
        plan.cancel_queued();
        self.publish_scheduler(scheduler);
        self.publish_diagnostics(diagnostics);
        Ok(active_task_id)
    }

    fn release_lane(&self, lease: SchedulerLaneLease) -> Result<(), TaskEngineError> {
        let scheduler = {
            let mut state = self.lock_state()?;
            state.scheduler.release_lane(lease)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            Self::scheduler_snapshot(&state)
        };
        self.publish_scheduler(scheduler);
        Ok(())
    }

    fn scheduler_snapshot(state: &TaskEngineState) -> FormalSchedulerSnapshot {
        state
            .scheduler
            .snapshot()
            .with_pending_deferred(!state.deferred_chat.is_empty())
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, TaskEngineState>, TaskEngineError> {
        self.inner
            .state
            .lock()
            .map_err(|_| TaskEngineError::state_unavailable())
    }

    fn ensure_accepting(state: &TaskEngineState) -> Result<(), TaskEngineError> {
        if state.accepting {
            Ok(())
        } else {
            Err(TaskEngineError(
                "任务引擎正在关闭，不能再接收任务".to_string(),
            ))
        }
    }

    fn mark_changed(state: &mut TaskEngineState, changed: &Condvar) {
        state.generation = state.generation.wrapping_add(1);
        changed.notify_all();
    }

    fn publish_current_state(&self) {
        let Ok(state) = self.lock_state() else {
            return;
        };
        let scheduler = Self::scheduler_snapshot(&state);
        let diagnostics = state.scheduler.diagnostic_snapshot();
        drop(state);
        self.publish_scheduler(scheduler);
        self.publish_diagnostics(diagnostics);
    }

    fn publish_scheduler(&self, snapshot: FormalSchedulerSnapshot) {
        if let Some(sink) = self.inner.state_sink.as_ref() {
            sink.publish_scheduler(snapshot);
        }
    }

    fn publish_diagnostics(&self, snapshot: Vec<DiagnosticTaskSnapshot>) {
        if let Some(sink) = self.inner.state_sink.as_ref() {
            sink.publish_diagnostics(snapshot);
        }
    }
}

impl TurtleSoupDeliveryPort for TaskEngineHandle {
    fn deliver_turtle_soup(
        &mut self,
        intent: TurtleSoupDeliveryIntent,
    ) -> anyhow::Result<TurtleSoupDeliveryOutcome> {
        let (outcome, scheduler) = {
            let mut state = self.lock_state().map_err(anyhow::Error::from)?;
            Self::ensure_accepting(&state).map_err(anyhow::Error::from)?;
            let outcome =
                TurtleSoupDeliveryPort::deliver_turtle_soup(&mut state.deferred_chat, intent)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::scheduler_snapshot(&state))
        };
        self.publish_scheduler(scheduler);
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::runtime::deferred_chat::{DeferredChatMessage, DeferredChatTarget};
    use crate::runtime::scheduler::{
        DiagnosticTaskCompletion, DiagnosticTaskSubmission, DiagnosticTaskWork,
        FormalTaskCancellationToken, FormalTaskCompletion, FormalTaskExecutionOutcome,
        FormalTaskSubmission, FormalTaskWork,
    };

    struct FormalNoop;

    impl FormalTaskWork for FormalNoop {
        fn execute(
            self: Box<Self>,
            _cancellation: FormalTaskCancellationToken,
        ) -> FormalTaskExecutionOutcome {
            FormalTaskExecutionOutcome::Completed(Ok("formal".to_string()))
        }

        fn cancel(self: Box<Self>) {}
    }

    struct DiagnosticNoop;

    impl DiagnosticTaskWork for DiagnosticNoop {
        fn execute(self: Box<Self>) -> anyhow::Result<String> {
            Ok("diagnostic".to_string())
        }
    }

    struct ObserveCancel(Arc<AtomicBool>);

    impl FormalTaskWork for ObserveCancel {
        fn execute(
            self: Box<Self>,
            _cancellation: FormalTaskCancellationToken,
        ) -> FormalTaskExecutionOutcome {
            FormalTaskExecutionOutcome::Completed(Ok("unexpected execution".to_string()))
        }

        fn cancel(self: Box<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn shared_work_is_taken_in_formal_deferred_diagnostic_order() {
        let engine = TaskEngineHandle::new(None);
        engine
            .enqueue_diagnostic(DiagnosticTaskSubmission::new(
                "diagnostic",
                Box::new(DiagnosticNoop),
            ))
            .unwrap();
        engine
            .enqueue_deferred(DeferredChatMessage {
                text: "deferred".to_string(),
                target: DeferredChatTarget::Primary,
                background_key: None,
                formal_epoch: None,
            })
            .unwrap();
        engine
            .enqueue_formal(FormalTaskSubmission::new(
                "formal",
                None,
                false,
                Box::new(FormalNoop),
            ))
            .unwrap();

        assert!(engine.take_next_deferred().unwrap().is_none());
        assert!(engine.take_next_diagnostic().unwrap().is_none());

        let formal = engine
            .take_next_formal()
            .unwrap()
            .expect("formal task should run first");
        let formal_id = formal.task_id();
        engine
            .complete_formal(
                formal_id,
                FormalTaskCompletion::Succeeded("done".to_string()),
            )
            .unwrap();

        assert!(engine.take_next_diagnostic().unwrap().is_none());
        let (_, deferred_permit) = engine
            .take_next_deferred()
            .unwrap()
            .expect("deferred chat should run second");
        drop(deferred_permit);

        let diagnostic = engine
            .take_next_diagnostic()
            .unwrap()
            .expect("diagnostic task should run last");
        engine
            .complete_diagnostic(
                diagnostic.task_id(),
                DiagnosticTaskCompletion::Succeeded("done".to_string()),
            )
            .unwrap();
    }

    #[test]
    fn queued_formal_task_can_be_canceled_through_the_engine() {
        let engine = TaskEngineHandle::new(None);
        let canceled = Arc::new(AtomicBool::new(false));
        let receipt = match engine
            .enqueue_formal(FormalTaskSubmission::new(
                "cancel me",
                None,
                false,
                Box::new(ObserveCancel(canceled.clone())),
            ))
            .unwrap()
        {
            crate::runtime::scheduler::FormalTaskEnqueueOutcome::Queued(receipt) => receipt,
            crate::runtime::scheduler::FormalTaskEnqueueOutcome::Duplicate => {
                panic!("unexpected duplicate")
            }
        };

        assert_eq!(
            engine.cancel_formal(receipt.task_id).unwrap(),
            crate::runtime::scheduler::FormalTaskCancelOutcome::CanceledBeforeStart
        );
        assert!(canceled.load(Ordering::SeqCst));
        assert_eq!(engine.snapshot().unwrap().tasks()[0].status, "canceled");
    }

    #[test]
    fn queued_deferred_chat_makes_the_engine_non_idle() {
        let engine = TaskEngineHandle::new(None);
        engine
            .enqueue_deferred(DeferredChatMessage {
                text: "deferred".to_string(),
                target: DeferredChatTarget::Primary,
                background_key: None,
                formal_epoch: None,
            })
            .unwrap();

        assert!(!engine.snapshot().unwrap().is_idle());
    }

    #[test]
    fn shutdown_cancels_active_and_queued_work_and_rejects_new_work() {
        let engine = TaskEngineHandle::new(None);
        let active_canceled = Arc::new(AtomicBool::new(false));
        let queued_canceled = Arc::new(AtomicBool::new(false));
        engine
            .enqueue_formal(FormalTaskSubmission::new(
                "active",
                None,
                false,
                Box::new(ObserveCancel(active_canceled.clone())),
            ))
            .unwrap();
        let active = engine.take_next_formal().unwrap().expect("active task");
        engine
            .enqueue_formal(FormalTaskSubmission::new(
                "queued",
                None,
                false,
                Box::new(ObserveCancel(queued_canceled.clone())),
            ))
            .unwrap();

        assert_eq!(engine.begin_shutdown().unwrap(), Some(active.task_id()));
        assert!(queued_canceled.load(Ordering::SeqCst));
        assert!(matches!(
            active.execute(),
            FormalTaskExecutionOutcome::Canceled(_)
        ));
        assert!(active_canceled.load(Ordering::SeqCst));
        assert!(
            engine
                .enqueue_formal(FormalTaskSubmission::new(
                    "late",
                    None,
                    false,
                    Box::new(FormalNoop),
                ))
                .is_err()
        );
    }
}
