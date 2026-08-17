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

#[derive(Clone, Debug)]
pub(crate) struct TaskEngineProjection {
    pub(crate) generation: u64,
    pub(crate) scheduler: FormalSchedulerSnapshot,
    pub(crate) diagnostics: Arc<[DiagnosticTaskSnapshot]>,
}

pub(crate) trait TaskEngineStateSink: Send + Sync {
    fn publish_task_engine(&self, _projection: TaskEngineProjection) {}
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
    diagnostics: Arc<[DiagnosticTaskSnapshot]>,
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

#[derive(Clone)]
pub(crate) struct BusinessTaskPort {
    engine: TaskEngineHandle,
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
                    diagnostics: Arc::from(Vec::<DiagnosticTaskSnapshot>::new()),
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

    pub(crate) fn business_port(&self) -> BusinessTaskPort {
        BusinessTaskPort {
            engine: self.clone(),
        }
    }

    pub(crate) fn enqueue_formal(
        &self,
        submission: FormalTaskSubmission,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        let (outcome, projection) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let outcome = state.scheduler.enqueue(submission)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::projection(&state))
        };
        self.publish_projection(projection);
        Ok(outcome)
    }

    pub(crate) fn enqueue_deferred(
        &self,
        item: impl Into<DeferredChatItem>,
    ) -> Result<EnqueueOutcome, TaskEngineError> {
        let (outcome, projection) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let outcome = state
                .deferred_chat
                .enqueue(item)
                .map_err(|error| TaskEngineError(error.to_string()))?;
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::projection(&state))
        };
        self.publish_projection(projection);
        Ok(outcome)
    }

    pub(crate) fn requeue_deferred_front(
        &self,
        item: DeferredChatItem,
    ) -> Result<EnqueueOutcome, TaskEngineError> {
        let (outcome, projection) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let outcome = state.deferred_chat.requeue_front(item);
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::projection(&state))
        };
        self.publish_projection(projection);
        Ok(outcome)
    }

    pub(crate) fn requeue_deferred_back(
        &self,
        item: DeferredChatItem,
    ) -> Result<EnqueueOutcome, TaskEngineError> {
        let (outcome, projection) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let outcome = state.deferred_chat.requeue_back(item);
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::projection(&state))
        };
        self.publish_projection(projection);
        Ok(outcome)
    }

    pub(crate) fn enqueue_diagnostic(
        &self,
        submission: DiagnosticTaskSubmission,
    ) -> Result<DiagnosticTaskSnapshot, TaskEngineError> {
        let (task, projection) = {
            let mut state = self.lock_state()?;
            Self::ensure_accepting(&state)?;
            let task = state.scheduler.enqueue_diagnostic(submission)?;
            Self::refresh_diagnostics(&mut state);
            Self::mark_changed(&mut state, &self.inner.changed);
            (task, Self::projection(&state))
        };
        self.publish_projection(projection);
        Ok(task)
    }

    pub(crate) fn take_next_formal(&self) -> Result<Option<FormalTaskLease>, TaskEngineError> {
        let (task, projection) = {
            let mut state = self.lock_state()?;
            let task = state.scheduler.take_next();
            let projection = task.as_ref().map(|_| {
                Self::mark_changed(&mut state, &self.inner.changed);
                Self::projection(&state)
            });
            (task, projection)
        };
        if let Some(projection) = projection {
            self.publish_projection(projection);
        }
        Ok(task)
    }

    pub(crate) fn restore_formal(&self, lease: FormalTaskLease) -> Result<(), TaskEngineError> {
        let (shutdown_plan, projection) = {
            let mut state = self.lock_state()?;
            state.scheduler.restore(lease)?;
            let shutdown_plan = if state.accepting {
                None
            } else {
                Some(state.scheduler.begin_shutdown())
            };
            Self::refresh_diagnostics(&mut state);
            Self::mark_changed(&mut state, &self.inner.changed);
            (shutdown_plan, Self::projection(&state))
        };
        if let Some(plan) = shutdown_plan {
            plan.cancel_queued();
        }
        self.publish_projection(projection);
        Ok(())
    }

    pub(crate) fn take_next_deferred(
        &self,
    ) -> Result<Option<(DeferredChatItem, TaskEnginePermit)>, TaskEngineError> {
        let (item, lease, projection) = {
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
            (item, lease, Self::projection(&state))
        };
        self.publish_projection(projection);
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
        let (task, projection) = {
            let mut state = self.lock_state()?;
            if !state.deferred_chat.is_empty() {
                return Ok(None);
            }
            let task = state.scheduler.take_next_diagnostic();
            let Some(_) = task.as_ref() else {
                return Ok(None);
            };
            Self::refresh_diagnostics(&mut state);
            Self::mark_changed(&mut state, &self.inner.changed);
            (task, Self::projection(&state))
        };
        self.publish_projection(projection);
        Ok(task)
    }

    pub(crate) fn complete_formal(
        &self,
        task_id: u64,
        completion: FormalTaskCompletion,
    ) -> Result<(), TaskEngineError> {
        let projection = {
            let mut state = self.lock_state()?;
            state.scheduler.complete(task_id, completion)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            Self::projection(&state)
        };
        self.publish_projection(projection);
        Ok(())
    }

    pub(crate) fn complete_diagnostic(
        &self,
        task_id: u64,
        completion: DiagnosticTaskCompletion,
    ) -> Result<(), TaskEngineError> {
        let projection = {
            let mut state = self.lock_state()?;
            state.scheduler.complete_diagnostic(task_id, completion)?;
            Self::refresh_diagnostics(&mut state);
            Self::mark_changed(&mut state, &self.inner.changed);
            Self::projection(&state)
        };
        self.publish_projection(projection);
        Ok(())
    }

    pub(crate) fn cancel_formal(
        &self,
        task_id: u64,
    ) -> Result<FormalTaskCancelOutcome, TaskEngineError> {
        let (action, projection) = {
            let mut state = self.lock_state()?;
            let action = state.scheduler.cancel(task_id);
            let projection = if matches!(action, FormalTaskCancelAction::NotFound) {
                None
            } else {
                Self::mark_changed(&mut state, &self.inner.changed);
                Some(Self::projection(&state))
            };
            (action, projection)
        };
        if let Some(projection) = projection {
            self.publish_projection(projection);
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
        let (plan, projection) = {
            let mut state = self.lock_state()?;
            state.accepting = false;
            let plan = state.scheduler.begin_shutdown();
            state.deferred_chat.clear();
            Self::refresh_diagnostics(&mut state);
            Self::mark_changed(&mut state, &self.inner.changed);
            (plan, Self::projection(&state))
        };
        let active_task_id = plan.active_task_id();
        plan.cancel_queued();
        self.publish_projection(projection);
        Ok(active_task_id)
    }

    /// 仅当所有任务通道仍为空闲时，原子停止接收新工作。
    ///
    /// 闲置配置重载不能把“检查空闲”和“停止接单”拆成两次加锁，否则两步之间
    /// 新入队的任务可能在调用方已经决定关停后被取消。返回 `false` 表示状态已不再
    /// 空闲或任务引擎已经进入关停流程，调用方应保持当前进程运行。
    pub(crate) fn begin_shutdown_if_idle(&self) -> Result<bool, TaskEngineError> {
        let projection = {
            let mut state = self.lock_state()?;
            if !state.accepting || !Self::scheduler_snapshot(&state).is_idle() {
                return Ok(false);
            }
            state.accepting = false;
            Self::mark_changed(&mut state, &self.inner.changed);
            Self::projection(&state)
        };
        self.publish_projection(projection);
        Ok(true)
    }

    fn release_lane(&self, lease: SchedulerLaneLease) -> Result<(), TaskEngineError> {
        let projection = {
            let mut state = self.lock_state()?;
            state.scheduler.release_lane(lease)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            Self::projection(&state)
        };
        self.publish_projection(projection);
        Ok(())
    }

    fn scheduler_snapshot(state: &TaskEngineState) -> FormalSchedulerSnapshot {
        state
            .scheduler
            .snapshot()
            .with_pending_deferred(!state.deferred_chat.is_empty())
    }

    fn projection(state: &TaskEngineState) -> TaskEngineProjection {
        TaskEngineProjection {
            generation: state.generation,
            scheduler: Self::scheduler_snapshot(state),
            diagnostics: Arc::clone(&state.diagnostics),
        }
    }

    fn refresh_diagnostics(state: &mut TaskEngineState) {
        state.diagnostics = Arc::from(state.scheduler.diagnostic_snapshot());
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
        let projection = Self::projection(&state);
        drop(state);
        self.publish_projection(projection);
    }

    fn publish_projection(&self, projection: TaskEngineProjection) {
        if let Some(sink) = self.inner.state_sink.as_ref() {
            sink.publish_task_engine(projection);
        }
    }
}

impl TurtleSoupDeliveryPort for TaskEngineHandle {
    fn deliver_turtle_soup(
        &mut self,
        intent: TurtleSoupDeliveryIntent,
    ) -> anyhow::Result<TurtleSoupDeliveryOutcome> {
        let (outcome, projection) = {
            let mut state = self.lock_state().map_err(anyhow::Error::from)?;
            Self::ensure_accepting(&state).map_err(anyhow::Error::from)?;
            let outcome =
                TurtleSoupDeliveryPort::deliver_turtle_soup(&mut state.deferred_chat, intent)?;
            Self::mark_changed(&mut state, &self.inner.changed);
            (outcome, Self::projection(&state))
        };
        self.publish_projection(projection);
        Ok(outcome)
    }
}

impl BusinessTaskPort {
    pub(crate) fn is_idle(&self) -> Result<bool, TaskEngineError> {
        self.engine.is_idle()
    }
}

impl TurtleSoupDeliveryPort for BusinessTaskPort {
    fn deliver_turtle_soup(
        &mut self,
        intent: TurtleSoupDeliveryIntent,
    ) -> anyhow::Result<TurtleSoupDeliveryOutcome> {
        TurtleSoupDeliveryPort::deliver_turtle_soup(&mut self.engine, intent)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::runtime::deferred_chat::{DeferredChatMessage, DeferredChatTarget};
    use crate::runtime::scheduler::{
        DiagnosticTaskCompletion, DiagnosticTaskSubmission, DiagnosticTaskWork,
        FormalTaskCancelOutcome, FormalTaskCancellationToken, FormalTaskCompletion,
        FormalTaskDedupKey, FormalTaskEnqueueOutcome, FormalTaskExecutionOutcome,
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

    struct CountCancel(Arc<AtomicUsize>);

    impl FormalTaskWork for CountCancel {
        fn execute(
            self: Box<Self>,
            _cancellation: FormalTaskCancellationToken,
        ) -> FormalTaskExecutionOutcome {
            FormalTaskExecutionOutcome::Completed(Ok("unexpected execution".to_string()))
        }

        fn cancel(self: Box<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn formal_submission(label: &str, dedup_key: Option<&str>) -> FormalTaskSubmission {
        FormalTaskSubmission::new(
            label,
            dedup_key.map(FormalTaskDedupKey::new),
            false,
            Box::new(FormalNoop),
        )
    }

    #[test]
    fn formal_tasks_keep_fifo_order_and_reject_a_queued_duplicate() {
        let engine = TaskEngineHandle::new(None);

        let first = engine
            .enqueue_formal(formal_submission("first", Some("same")))
            .unwrap();
        let second = engine
            .enqueue_formal(formal_submission("second", Some("other")))
            .unwrap();
        let duplicate = engine
            .enqueue_formal(formal_submission("duplicate", Some("same")))
            .unwrap();

        let FormalTaskEnqueueOutcome::Queued(first) = first else {
            panic!("first task should be queued");
        };
        let FormalTaskEnqueueOutcome::Queued(second) = second else {
            panic!("second task should be queued");
        };
        assert_eq!(first.task_id, 1);
        assert_eq!(first.position, 1);
        assert_eq!(second.task_id, 2);
        assert_eq!(second.position, 2);
        assert_eq!(duplicate, FormalTaskEnqueueOutcome::Duplicate);
        assert_eq!(
            engine.snapshot().unwrap().pending_labels(),
            &["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn restored_formal_task_keeps_its_turn_and_only_queued_work_is_canceled() {
        let engine = TaskEngineHandle::new(None);
        let canceled = Arc::new(AtomicBool::new(false));
        let FormalTaskEnqueueOutcome::Queued(first) = engine
            .enqueue_formal(formal_submission("first", Some("first")))
            .unwrap()
        else {
            panic!("first task should be queued");
        };
        let FormalTaskEnqueueOutcome::Queued(second) = engine
            .enqueue_formal(FormalTaskSubmission::new(
                "second",
                Some(FormalTaskDedupKey::new("second")),
                false,
                Box::new(ObserveCancel(canceled.clone())),
            ))
            .unwrap()
        else {
            panic!("second task should be queued");
        };

        let active = engine.take_next_formal().unwrap().expect("first task");
        assert_eq!(active.task_id(), first.task_id);
        engine.restore_formal(active).unwrap();
        assert_eq!(
            engine.snapshot().unwrap().pending_labels(),
            &["first".to_string(), "second".to_string()]
        );

        let active = engine.take_next_formal().unwrap().expect("restored task");
        let active_id = active.task_id();
        let result = match active.execute() {
            FormalTaskExecutionOutcome::Completed(Ok(result)) => result,
            _ => panic!("restored task should complete"),
        };
        engine
            .complete_formal(active_id, FormalTaskCompletion::Succeeded(result))
            .unwrap();
        assert_eq!(
            engine.cancel_formal(second.task_id).unwrap(),
            FormalTaskCancelOutcome::CanceledBeforeStart
        );
        assert!(canceled.load(Ordering::SeqCst));

        let snapshot = engine.snapshot().unwrap();
        assert!(snapshot.pending_labels().is_empty());
        assert_eq!(snapshot.tasks()[0].id, second.task_id);
        assert_eq!(snapshot.tasks()[0].status, "canceled");
        assert_eq!(snapshot.tasks()[1].id, first.task_id);
        assert_eq!(snapshot.tasks()[1].status, "completed");
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
        let diagnostic_id = diagnostic.task_id();
        let result = diagnostic.execute().unwrap();
        engine
            .complete_diagnostic(diagnostic_id, DiagnosticTaskCompletion::Succeeded(result))
            .unwrap();
        let snapshot = engine
            .diagnostic_task_snapshot(diagnostic_id)
            .unwrap()
            .expect("diagnostic history");
        assert_eq!(snapshot.status, "completed");
        assert_eq!(snapshot.result.as_deref(), Some("diagnostic"));
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
    fn running_formal_task_records_a_cancellation_request_until_completion() {
        let engine = TaskEngineHandle::new(None);
        let FormalTaskEnqueueOutcome::Queued(receipt) = engine
            .enqueue_formal(formal_submission("running", Some("running")))
            .unwrap()
        else {
            panic!("formal task should be queued");
        };
        let active = engine.take_next_formal().unwrap().expect("running task");

        assert_eq!(
            engine.cancel_formal(receipt.task_id).unwrap(),
            FormalTaskCancelOutcome::CancellationRequested
        );
        let snapshot = engine.snapshot().unwrap();
        let task = snapshot
            .tasks()
            .iter()
            .find(|task| task.id == receipt.task_id)
            .expect("running task history");
        assert_eq!(task.status, "running");
        assert!(task.cancellation_requested);

        let reason = match active.execute() {
            FormalTaskExecutionOutcome::Canceled(reason) => reason,
            FormalTaskExecutionOutcome::Completed(_) => {
                panic!("cancellation should be observed before work starts")
            }
        };
        engine
            .complete_formal(receipt.task_id, FormalTaskCompletion::Canceled(reason))
            .unwrap();
    }

    #[test]
    fn cancel_reports_finished_and_unknown_tasks_without_changing_history() {
        let engine = TaskEngineHandle::new(None);
        let FormalTaskEnqueueOutcome::Queued(receipt) = engine
            .enqueue_formal(formal_submission("finished", Some("finished")))
            .unwrap()
        else {
            panic!("formal task should be queued");
        };
        let active = engine.take_next_formal().unwrap().expect("finished task");
        let result = match active.execute() {
            FormalTaskExecutionOutcome::Completed(Ok(result)) => result,
            _ => panic!("formal task should complete"),
        };
        engine
            .complete_formal(receipt.task_id, FormalTaskCompletion::Succeeded(result))
            .unwrap();

        assert_eq!(
            engine.cancel_formal(receipt.task_id).unwrap(),
            FormalTaskCancelOutcome::AlreadyFinished
        );
        assert_eq!(
            engine.cancel_formal(99_999).unwrap(),
            FormalTaskCancelOutcome::NotFound
        );
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
        let queued_cancellations = Arc::new(AtomicUsize::new(0));
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
                Box::new(CountCancel(queued_cancellations.clone())),
            ))
            .unwrap();

        assert_eq!(engine.begin_shutdown().unwrap(), Some(active.task_id()));
        assert_eq!(queued_cancellations.load(Ordering::SeqCst), 1);
        assert_eq!(engine.begin_shutdown().unwrap(), Some(active.task_id()));
        assert_eq!(queued_cancellations.load(Ordering::SeqCst), 1);
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

    #[test]
    fn idle_shutdown_atomically_stops_accepting_new_work() {
        let engine = TaskEngineHandle::new(None);

        assert!(engine.begin_shutdown_if_idle().unwrap());
        assert!(!engine.begin_shutdown_if_idle().unwrap());
        assert!(
            engine
                .enqueue_formal(formal_submission("late", None))
                .is_err()
        );
    }

    #[test]
    fn idle_shutdown_refuses_pending_work_without_closing_the_engine() {
        let engine = TaskEngineHandle::new(None);
        engine
            .enqueue_formal(formal_submission("pending", None))
            .unwrap();

        assert!(!engine.begin_shutdown_if_idle().unwrap());
        assert!(
            engine
                .enqueue_formal(formal_submission("still accepted", None))
                .is_ok()
        );
    }
}
