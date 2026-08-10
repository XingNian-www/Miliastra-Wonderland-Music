use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use super::{FormalTaskExecutionContext, PendingTask, PendingTaskExecution};
use crate::features::hall::HallRuntimeState;
use crate::features::playback::{PlaybackRuntimeState, QueueItem};
use crate::features::startup::StartupTask;
use crate::features::turtle_soup::TurtleSoupSnapshot;
use crate::features::undercover::UndercoverSnapshot;
use crate::interfaces::chat::PendingCommand;
use crate::interfaces::http::{HttpQueryPort, HttpTaskPort, WebToolRequest};
use crate::runtime::business::{
    BusinessMutationIntent, BusinessMutationOutcome, BusinessRuntimeHandle,
};
use crate::runtime::chat_listener::ChatListenerMode;
use crate::runtime::decision::DecisionAction;
use crate::runtime::scheduler::{
    DiagnosticTaskCompletion, DiagnosticTaskSnapshot, DiagnosticTaskSubmission, DiagnosticTaskWork,
    FormalTaskCancelOutcome, FormalTaskCancellationToken, FormalTaskCompletion, FormalTaskDedupKey,
    FormalTaskEnqueueOutcome, FormalTaskExecutionOutcome, FormalTaskSubmission, FormalTaskWork,
};
use crate::runtime::task_engine::{TaskEngineError, TaskEngineHandle};
use crate::runtime::ui::UiCoordinator;

const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);
const DROP_SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

struct FormalTaskExecutionState {
    context: Mutex<Option<FormalTaskExecutionContext>>,
}

#[derive(Clone)]
pub(crate) struct FormalTaskExecutionHandle {
    context: Weak<FormalTaskExecutionState>,
}

impl FormalTaskExecutionHandle {
    fn execute(
        &self,
        mut task: PendingTask,
        cancellation: FormalTaskCancellationToken,
    ) -> FormalTaskExecutionOutcome {
        let Some(state) = self.context.upgrade() else {
            return FormalTaskExecutionOutcome::Completed(Err(anyhow!("正式任务执行上下文已停止")));
        };
        let context = match state.context.lock() {
            Ok(context) => context,
            Err(poisoned) => {
                log::error!("正式任务执行上下文锁曾发生异常，继续使用已恢复状态");
                poisoned.into_inner()
            }
        };
        let Some(context) = context.as_ref() else {
            return FormalTaskExecutionOutcome::Completed(Err(anyhow!(
                "正式任务执行上下文尚未初始化"
            )));
        };
        if cancellation.is_requested() {
            task.cancel(&context.business.business);
            return FormalTaskExecutionOutcome::Canceled(
                "任务在开始执行前收到取消请求".to_string(),
            );
        }
        let label = task.label();
        let result = match catch_unwind(AssertUnwindSafe(|| context.execute_pending_task(task))) {
            Ok(Ok(PendingTaskExecution::Completed)) => Ok(format!("{label}执行完成")),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow!("待处理任务执行发生未捕获异常")),
        };
        FormalTaskExecutionOutcome::Completed(result)
    }

    fn execute_diagnostic(&self, request: WebToolRequest) -> Result<String> {
        let state = self
            .context
            .upgrade()
            .ok_or_else(|| anyhow!("应用执行上下文已停止"))?;
        let context = match state.context.lock() {
            Ok(context) => context,
            Err(poisoned) => {
                log::error!("应用执行上下文锁曾发生异常，继续使用已恢复状态");
                poisoned.into_inner()
            }
        };
        let context = context
            .as_ref()
            .ok_or_else(|| anyhow!("应用执行上下文尚未初始化"))?;
        match catch_unwind(AssertUnwindSafe(|| {
            context.execute_web_tool_request(request)
        })) {
            Ok(result) => result,
            Err(_) => Err(anyhow!("Web 工具执行发生未捕获异常")),
        }
    }
}

/// Typed submission boundary shared by chat, HTTP and background producers.
///
/// The private task enum and execution transport stay behind this client so
/// protocol adapters cannot depend on application-executor internals.
#[derive(Clone)]
pub(crate) struct FormalTaskClient {
    executor: FormalTaskExecutionHandle,
    business: BusinessRuntimeHandle,
    task_engine: TaskEngineHandle,
}

impl FormalTaskClient {
    pub(crate) fn new(
        executor: FormalTaskExecutionHandle,
        business: BusinessRuntimeHandle,
        task_engine: TaskEngineHandle,
    ) -> Self {
        Self {
            executor,
            business,
            task_engine,
        }
    }

    pub(crate) fn enqueue_command(
        &self,
        pending: PendingCommand,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        self.enqueue(PendingTask::Command(Box::new(pending)))
    }

    pub(crate) fn enqueue_startup(
        &self,
        task: StartupTask,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        self.enqueue(PendingTask::Startup(task))
    }

    pub(crate) fn enqueue_console_chat(
        &self,
        text: String,
        prefix: String,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        self.enqueue(PendingTask::ConsoleChat { text, prefix })
    }

    pub(crate) fn enqueue_listener_mode(
        &self,
        target: ChatListenerMode,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        self.enqueue(PendingTask::SetChatListenerMode { target })
    }

    pub(crate) fn enqueue_clear_idle_exit(
        &self,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        self.enqueue(PendingTask::ClearIdleExit)
    }

    pub(crate) fn enqueue_diagnostic(
        &self,
        request: WebToolRequest,
    ) -> Result<DiagnosticTaskSnapshot, TaskEngineError> {
        self.task_engine
            .enqueue_diagnostic(diagnostic_task_submission(self.executor.clone(), request))
    }

    pub(super) fn enqueue(
        &self,
        task: PendingTask,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        self.task_engine.enqueue_formal(formal_task_submission(
            self.executor.clone(),
            self.business.clone(),
            task,
        ))
    }
}

impl HttpTaskPort for FormalTaskClient {
    fn apply_mutation(&self, intent: BusinessMutationIntent) -> Result<BusinessMutationOutcome> {
        Ok(self.business.apply_mutation(intent)?)
    }

    fn playback_queue_contains(&self, item: QueueItem) -> Result<bool> {
        Ok(self.business.playback_queue_contains(item)?)
    }

    fn enqueue_command(&self, pending: PendingCommand) -> Result<FormalTaskEnqueueOutcome> {
        Ok(FormalTaskClient::enqueue_command(self, pending)?)
    }

    fn enqueue_startup(&self, task: StartupTask) -> Result<FormalTaskEnqueueOutcome> {
        Ok(FormalTaskClient::enqueue_startup(self, task)?)
    }

    fn enqueue_console_chat(
        &self,
        text: String,
        prefix: String,
    ) -> Result<FormalTaskEnqueueOutcome> {
        Ok(FormalTaskClient::enqueue_console_chat(self, text, prefix)?)
    }

    fn enqueue_listener_mode(&self, target: ChatListenerMode) -> Result<FormalTaskEnqueueOutcome> {
        Ok(FormalTaskClient::enqueue_listener_mode(self, target)?)
    }

    fn enqueue_clear_idle_exit(&self) -> Result<FormalTaskEnqueueOutcome> {
        Ok(FormalTaskClient::enqueue_clear_idle_exit(self)?)
    }

    fn enqueue_diagnostic(&self, request: WebToolRequest) -> Result<DiagnosticTaskSnapshot> {
        Ok(FormalTaskClient::enqueue_diagnostic(self, request)?)
    }

    fn cancel_task(&self, task_id: u64) -> Result<FormalTaskCancelOutcome> {
        Ok(self.task_engine.cancel_formal(task_id)?)
    }

    fn submit_decision(&self, id: u64, action: DecisionAction) -> Result<()> {
        Ok(self.business.submit_decision(id, action)?)
    }
}

impl HttpQueryPort for FormalTaskClient {
    fn turtle_soup_snapshot(&self) -> Result<TurtleSoupSnapshot> {
        Ok(self.business.turtle_soup_snapshot()?)
    }

    fn undercover_snapshot(&self) -> Result<UndercoverSnapshot> {
        Ok(self.business.undercover_snapshot()?)
    }

    fn diagnostic_task_snapshot(&self, id: u64) -> Result<Option<DiagnosticTaskSnapshot>> {
        Ok(self.task_engine.diagnostic_task_snapshot(id)?)
    }

    fn playback_queue_snapshot(&self) -> Result<Vec<QueueItem>> {
        Ok(self.business.playback_queue_snapshot()?)
    }

    fn playback_state_snapshot(&self) -> Result<PlaybackRuntimeState> {
        Ok(self.business.playback_state_snapshot()?)
    }

    fn hall_state_snapshot(&self) -> Result<HallRuntimeState> {
        Ok(self.business.hall_state_snapshot()?)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FormalTaskShutdownReport {
    timed_out: bool,
    active_task_id: Option<u64>,
}

impl FormalTaskShutdownReport {
    pub(crate) const fn timed_out(self) -> bool {
        self.timed_out
    }

    pub(crate) const fn active_task_id(self) -> Option<u64> {
        self.active_task_id
    }
}

/// Owns the one task worker and the application execution context.
/// Queue order, task history and shared-lane state live in `TaskEngineHandle`.
pub(crate) struct FormalTaskRuntime {
    client: FormalTaskClient,
    context: Option<Arc<FormalTaskExecutionState>>,
    task_engine: TaskEngineHandle,
    stop_requested: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    finished: Receiver<()>,
}

impl FormalTaskRuntime {
    pub(crate) fn start(
        business: BusinessRuntimeHandle,
        task_engine: TaskEngineHandle,
        running: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
        post_settle: Duration,
        build_context: impl FnOnce(FormalTaskClient) -> FormalTaskExecutionContext,
    ) -> Result<Self> {
        let context = Arc::new(FormalTaskExecutionState {
            context: Mutex::new(None),
        });
        let handle = FormalTaskExecutionHandle {
            context: Arc::downgrade(&context),
        };
        let client = FormalTaskClient::new(handle, business.clone(), task_engine.clone());
        let execution_context = build_context(client.clone());
        let coordinator = execution_context.coordinator.clone();
        *context
            .context
            .lock()
            .map_err(|_| anyhow!("初始化正式任务执行上下文失败"))? = Some(execution_context);
        Self::start_worker(
            task_engine,
            running,
            paused,
            post_settle,
            client,
            Some(context),
            Some(coordinator),
        )
    }

    #[cfg(test)]
    fn start_without_application(
        business: BusinessRuntimeHandle,
        task_engine: TaskEngineHandle,
        running: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
        post_settle: Duration,
        coordinator: Option<UiCoordinator>,
    ) -> Result<Self> {
        let client = FormalTaskClient::new(
            FormalTaskExecutionHandle {
                context: Weak::new(),
            },
            business.clone(),
            task_engine.clone(),
        );
        Self::start_worker(
            task_engine,
            running,
            paused,
            post_settle,
            client,
            None,
            coordinator,
        )
    }

    fn start_worker(
        task_engine: TaskEngineHandle,
        running: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
        post_settle: Duration,
        client: FormalTaskClient,
        context: Option<Arc<FormalTaskExecutionState>>,
        coordinator: Option<UiCoordinator>,
    ) -> Result<Self> {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let worker_engine = task_engine.clone();
        let worker_stop = Arc::clone(&stop_requested);
        let (finished_sender, finished) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("formal-task-runtime".to_string())
            .spawn(move || {
                log::info!("正式任务运行时已启动");
                if let Err(error) = run_task_loop(
                    worker_engine,
                    running,
                    paused,
                    worker_stop,
                    post_settle,
                    coordinator,
                ) {
                    log::error!("正式任务运行时异常退出: {error:#}");
                }
                let _ = finished_sender.send(());
            })
            .map_err(|error| anyhow!("启动正式任务运行时失败: {error}"))?;
        Ok(Self {
            client,
            context,
            task_engine,
            stop_requested,
            worker: Some(worker),
            finished,
        })
    }

    pub(crate) fn client(&self) -> FormalTaskClient {
        self.client.clone()
    }

    pub(crate) fn shutdown(mut self) -> Result<FormalTaskShutdownReport> {
        self.stop(DEFAULT_SHUTDOWN_GRACE)
    }

    #[cfg(test)]
    fn shutdown_with_timeout(mut self, timeout: Duration) -> Result<FormalTaskShutdownReport> {
        self.stop(timeout)
    }

    fn stop(&mut self, timeout: Duration) -> Result<FormalTaskShutdownReport> {
        let Some(worker) = self.worker.take() else {
            return Ok(FormalTaskShutdownReport::default());
        };
        let prepare_result = self.task_engine.begin_shutdown();
        let active_task_id = prepare_result.as_ref().ok().copied().flatten();
        self.stop_requested.store(true, AtomicOrdering::SeqCst);
        if let Err(error) = self.task_engine.wake() {
            log::debug!("唤醒正在关闭的任务引擎失败: {error}");
        }
        let timed_out = match self.finished.recv_timeout(timeout) {
            Ok(()) => {
                worker
                    .join()
                    .map_err(|_| anyhow!("正式任务运行时线程 panic"))?;
                false
            }
            Err(RecvTimeoutError::Disconnected) => {
                worker
                    .join()
                    .map_err(|_| anyhow!("正式任务运行时线程 panic"))?;
                false
            }
            Err(RecvTimeoutError::Timeout) => {
                drop(worker);
                true
            }
        };
        prepare_result?;
        self.context.take();
        Ok(FormalTaskShutdownReport {
            timed_out,
            active_task_id,
        })
    }
}

impl Drop for FormalTaskRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.stop(DROP_SHUTDOWN_GRACE) {
            log::error!("正式任务运行时关闭失败: {error:#}");
        }
    }
}

fn run_task_loop(
    task_engine: TaskEngineHandle,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
    post_settle: Duration,
    coordinator: Option<UiCoordinator>,
) -> Result<()> {
    let mut generation = task_engine.generation()?;
    while running.load(AtomicOrdering::SeqCst) && !stop_requested.load(AtomicOrdering::SeqCst) {
        if !paused.load(AtomicOrdering::SeqCst)
            && let Some(task) = task_engine.take_next_formal()?
        {
            if paused.load(AtomicOrdering::SeqCst) {
                task_engine.restore_formal(task)?;
            } else {
                let _ui_lease = coordinator.as_ref().map(UiCoordinator::acquire_formal);
                let succeeded = execute_formal_task(&task_engine, task)?;
                generation = task_engine.generation()?;
                if succeeded {
                    wait_for_post_settle(
                        &task_engine,
                        &running,
                        &stop_requested,
                        &mut generation,
                        post_settle,
                    )?;
                }
            }
            continue;
        }
        if let Some(task) = task_engine.take_next_diagnostic()? {
            execute_diagnostic_task(&task_engine, task)?;
            generation = task_engine.generation()?;
            continue;
        }
        generation = task_engine.wait_for_change(generation)?;
    }
    Ok(())
}

fn execute_formal_task(
    task_engine: &TaskEngineHandle,
    task: crate::runtime::scheduler::FormalTaskLease,
) -> Result<bool> {
    let task_id = task.task_id();
    let task_label = task.label().to_string();
    log::info!("待处理任务开始: {task_label}");
    let outcome = match catch_unwind(AssertUnwindSafe(|| task.execute())) {
        Ok(outcome) => outcome,
        Err(_) => {
            FormalTaskExecutionOutcome::Completed(Err(anyhow!("待处理任务执行发生未捕获异常")))
        }
    };
    match outcome {
        FormalTaskExecutionOutcome::Completed(Ok(result)) => {
            task_engine.complete_formal(task_id, FormalTaskCompletion::Succeeded(result))?;
            log::info!("待处理任务完成: {task_label}");
            Ok(true)
        }
        FormalTaskExecutionOutcome::Completed(Err(error)) => {
            task_engine.complete_formal(
                task_id,
                FormalTaskCompletion::Failed(format!("错误: {error:#}")),
            )?;
            log::error!("待处理任务执行异常: {error:#}");
            Ok(false)
        }
        FormalTaskExecutionOutcome::Canceled(reason) => {
            task_engine.complete_formal(task_id, FormalTaskCompletion::Canceled(reason.clone()))?;
            log::info!("待处理任务已取消: {task_label}; {reason}");
            Ok(false)
        }
    }
}

fn execute_diagnostic_task(
    task_engine: &TaskEngineHandle,
    task: crate::runtime::scheduler::DiagnosticTaskLease,
) -> Result<()> {
    let task_id = task.task_id();
    let label = task.label().to_string();
    let completion = match catch_unwind(AssertUnwindSafe(|| task.execute())) {
        Ok(Ok(result)) => DiagnosticTaskCompletion::Succeeded(result),
        Ok(Err(error)) => {
            log::error!("Web 工具执行失败 {label}: {error:#}");
            DiagnosticTaskCompletion::Failed(format!("{error:#}"))
        }
        Err(_) => {
            log::error!("Web 工具执行发生未捕获异常: {label}");
            DiagnosticTaskCompletion::Failed("Web 工具执行发生未捕获异常".to_string())
        }
    };
    task_engine.complete_diagnostic(task_id, completion)?;
    Ok(())
}

fn wait_for_post_settle(
    task_engine: &TaskEngineHandle,
    running: &AtomicBool,
    stop_requested: &AtomicBool,
    generation: &mut u64,
    duration: Duration,
) -> Result<()> {
    let deadline = Instant::now() + duration;
    while running.load(AtomicOrdering::SeqCst) && !stop_requested.load(AtomicOrdering::SeqCst) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        *generation = task_engine.wait_for_change_timeout(*generation, remaining)?;
    }
    Ok(())
}

struct AppFormalTaskWork {
    executor: FormalTaskExecutionHandle,
    business: BusinessRuntimeHandle,
    task: PendingTask,
}

struct AppDiagnosticTaskWork {
    executor: FormalTaskExecutionHandle,
    request: WebToolRequest,
}

impl DiagnosticTaskWork for AppDiagnosticTaskWork {
    fn execute(self: Box<Self>) -> Result<String> {
        self.executor.execute_diagnostic(self.request)
    }
}

impl FormalTaskWork for AppFormalTaskWork {
    fn execute(
        self: Box<Self>,
        cancellation: FormalTaskCancellationToken,
    ) -> FormalTaskExecutionOutcome {
        self.executor.execute(self.task, cancellation)
    }

    fn cancel(self: Box<Self>) {
        let mut task = self.task;
        task.cancel(&self.business);
    }
}

pub(crate) fn formal_task_submission(
    executor: FormalTaskExecutionHandle,
    business: BusinessRuntimeHandle,
    task: PendingTask,
) -> FormalTaskSubmission {
    let label = task.label();
    let dedup_key = task.dedup_key().map(FormalTaskDedupKey::new);
    let playback_related = task.is_playback_task();
    FormalTaskSubmission::new(
        label,
        dedup_key,
        playback_related,
        Box::new(AppFormalTaskWork {
            executor,
            business,
            task,
        }),
    )
}

pub(crate) fn diagnostic_task_submission(
    executor: FormalTaskExecutionHandle,
    request: WebToolRequest,
) -> DiagnosticTaskSubmission {
    let label = request.label();
    DiagnosticTaskSubmission::new(label, Box::new(AppDiagnosticTaskWork { executor, request }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::features::card_games::{CardGameService, LandlordConfig};
    use crate::features::idiom_chain::IdiomChainService;
    use crate::runtime::business::BusinessRuntime;
    use crate::runtime::scheduler::{
        FormalTaskCancellationToken, FormalTaskExecutionOutcome, FormalTaskSubmission,
        FormalTaskWork,
    };

    struct NotifyWork(mpsc::Sender<()>);

    impl FormalTaskWork for NotifyWork {
        fn execute(
            self: Box<Self>,
            _cancellation: FormalTaskCancellationToken,
        ) -> FormalTaskExecutionOutcome {
            self.0.send(()).expect("execution observer");
            FormalTaskExecutionOutcome::Completed(Ok("done".to_string()))
        }

        fn cancel(self: Box<Self>) {}
    }

    struct ObserveShutdownCancellation {
        started: mpsc::Sender<()>,
        observed: Arc<AtomicBool>,
    }

    struct IgnoreShutdownCancellation {
        started: mpsc::Sender<()>,
    }

    impl FormalTaskWork for IgnoreShutdownCancellation {
        fn execute(
            self: Box<Self>,
            _cancellation: FormalTaskCancellationToken,
        ) -> FormalTaskExecutionOutcome {
            self.started.send(()).expect("start observer");
            thread::sleep(Duration::from_millis(500));
            FormalTaskExecutionOutcome::Completed(Ok("late completion".to_string()))
        }

        fn cancel(self: Box<Self>) {}
    }

    impl FormalTaskWork for ObserveShutdownCancellation {
        fn execute(
            self: Box<Self>,
            cancellation: FormalTaskCancellationToken,
        ) -> FormalTaskExecutionOutcome {
            self.started.send(()).expect("start observer");
            let deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < deadline {
                if cancellation.is_requested() {
                    self.observed.store(true, Ordering::SeqCst);
                    return FormalTaskExecutionOutcome::Canceled(
                        "shutdown cancellation observed".to_string(),
                    );
                }
                thread::yield_now();
            }
            FormalTaskExecutionOutcome::Completed(Err(anyhow!(
                "shutdown cancellation was not observed"
            )))
        }

        fn cancel(self: Box<Self>) {}
    }

    fn business_runtime() -> (BusinessRuntime, TaskEngineHandle) {
        let task_engine = TaskEngineHandle::new(None);
        let runtime = BusinessRuntime::start_with_task_engine(
            8,
            IdiomChainService::from_entries_for_test(
                &["画蛇添足", "足智多谋", "谋事在人", "人山人海"],
                Some(Duration::from_secs(300)),
            ),
            CardGameService::new(LandlordConfig::default()),
            task_engine.clone(),
        )
        .expect("business runtime");
        (runtime, task_engine)
    }

    #[test]
    fn enqueued_formal_task_starts_without_a_polling_interval() {
        let (business_runtime, task_engine) = business_runtime();
        let business = business_runtime.handle();
        let task_runtime = FormalTaskRuntime::start_without_application(
            business.clone(),
            task_engine.clone(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Duration::ZERO,
            None,
        )
        .expect("formal task runtime");
        let (executed, execution) = mpsc::channel();

        task_engine
            .enqueue_formal(FormalTaskSubmission::new(
                "event-driven",
                None,
                false,
                Box::new(NotifyWork(executed)),
            ))
            .expect("enqueue formal task");

        execution
            .recv_timeout(Duration::from_secs(1))
            .expect("formal task should start directly after enqueue");

        task_runtime
            .shutdown()
            .expect("formal task runtime shutdown");
        business_runtime.shutdown().expect("business shutdown");
    }

    #[test]
    fn formal_task_ui_lease_covers_post_settle() {
        let (business_runtime, task_engine) = business_runtime();
        let business = business_runtime.handle();
        let coordinator = UiCoordinator::new();
        let task_runtime = FormalTaskRuntime::start_without_application(
            business.clone(),
            task_engine.clone(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Duration::from_millis(150),
            Some(coordinator.clone()),
        )
        .expect("formal task runtime");
        let (executed, execution) = mpsc::channel();

        task_engine
            .enqueue_formal(FormalTaskSubmission::new(
                "settle lease",
                None,
                false,
                Box::new(NotifyWork(executed)),
            ))
            .expect("enqueue formal task");
        execution
            .recv_timeout(Duration::from_secs(1))
            .expect("formal task execution");

        assert!(!coordinator.scan_may_run());
        thread::sleep(Duration::from_millis(50));
        assert!(!coordinator.scan_may_run());
        let deadline = Instant::now() + Duration::from_secs(1);
        while !coordinator.scan_may_run() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(coordinator.scan_may_run());

        task_runtime
            .shutdown()
            .expect("formal task runtime shutdown");
        business_runtime.shutdown().expect("business shutdown");
    }

    #[test]
    fn formal_task_runtime_requests_cancellation_from_the_running_task_on_shutdown() {
        let (business_runtime, task_engine) = business_runtime();
        let business = business_runtime.handle();
        let task_runtime = FormalTaskRuntime::start_without_application(
            business.clone(),
            task_engine.clone(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Duration::ZERO,
            None,
        )
        .expect("formal task runtime");
        let (started, start) = mpsc::channel();
        let observed = Arc::new(AtomicBool::new(false));
        task_engine
            .enqueue_formal(FormalTaskSubmission::new(
                "cancel on shutdown",
                None,
                false,
                Box::new(ObserveShutdownCancellation {
                    started,
                    observed: observed.clone(),
                }),
            ))
            .expect("enqueue formal task");
        start
            .recv_timeout(Duration::from_secs(1))
            .expect("formal task start");

        task_runtime
            .shutdown()
            .expect("formal task runtime shutdown");

        assert!(observed.load(Ordering::SeqCst));
        business_runtime.shutdown().expect("business shutdown");
    }

    #[test]
    fn formal_task_runtime_shutdown_has_a_time_limit_when_work_ignores_cancellation() {
        let (business_runtime, task_engine) = business_runtime();
        let business = business_runtime.handle();
        let task_runtime = FormalTaskRuntime::start_without_application(
            business.clone(),
            task_engine.clone(),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(false)),
            Duration::ZERO,
            None,
        )
        .expect("formal task runtime");
        let (started, start) = mpsc::channel();
        task_engine
            .enqueue_formal(FormalTaskSubmission::new(
                "ignore shutdown",
                None,
                false,
                Box::new(IgnoreShutdownCancellation { started }),
            ))
            .expect("enqueue formal task");
        start
            .recv_timeout(Duration::from_secs(1))
            .expect("formal task start");
        let shutdown_started = Instant::now();

        let report = task_runtime
            .shutdown_with_timeout(Duration::from_millis(20))
            .expect("bounded formal task runtime shutdown");

        assert!(report.timed_out());
        assert!(shutdown_started.elapsed() < Duration::from_millis(200));
        business_runtime.shutdown().expect("business shutdown");
    }
}
