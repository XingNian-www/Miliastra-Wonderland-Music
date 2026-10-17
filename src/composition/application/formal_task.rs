use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use super::{ApplicationRuntime, FormalTaskExecutionContext, PendingTask, PendingTaskExecution};
use crate::config::LiveConfigs;
use crate::features::command::RoutedCommand;
use crate::features::hall::HallRuntimeState;
use crate::features::playback::{PlaybackRuntimeState, QueueItem, SongRequestLeaderboardEntry};
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
/// 正式任务超时后给任务线程的宽限等待窗口。
const FORMAL_TASK_JOIN_GRACE: Duration = Duration::from_secs(2);

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

/// 聊天、HTTP 和后台生产者共享的类型化提交边界。
///
/// 私有任务枚举和执行传输封装在此客户端之后，协议适配器不依赖应用执行器内部实现。
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
        self.enqueue_with_optional_source(task, None)
    }

    pub(super) fn enqueue_with_command_source(
        &self,
        task: PendingTask,
        source: &RoutedCommand,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        self.enqueue_with_optional_source(task, Some(source))
    }

    fn enqueue_with_optional_source(
        &self,
        task: PendingTask,
        source: Option<&RoutedCommand>,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        self.enqueue_with_observer(task, source, crate::debug_read_only::observer())
    }

    fn enqueue_with_observer(
        &self,
        task: PendingTask,
        source: Option<&RoutedCommand>,
        observer: &crate::debug_read_only::ReadOnlyObserver,
    ) -> Result<FormalTaskEnqueueOutcome, TaskEngineError> {
        // Observation is optional even if serialization panics. Normal queue semantics
        // and the task itself never depend on the read-only switch or evidence health.
        let observation = catch_unwind(AssertUnwindSafe(|| {
            observer
                .push_enabled()
                .then(|| read_only_task_parameters_with_source(&task, source))
        }))
        .ok()
        .flatten();
        let outcome = self.task_engine.enqueue_formal(formal_task_submission(
            self.executor.clone(),
            self.business.clone(),
            task,
        ))?;
        if let (FormalTaskEnqueueOutcome::Queued(receipt), Some((payload, captured_at))) =
            (outcome, observation)
        {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                observer.record_trigger(receipt.task_id, payload, captured_at);
                // The projection may finish before this receipt, or exceed the
                // bounded pre-receipt cache. Query the authoritative task source
                // without changing publication order; stale lookups cannot regress it.
                if let Ok((generation, Some(status))) = self
                    .task_engine
                    .formal_task_status_revision(receipt.task_id)
                {
                    observer.record_formal_status_at(generation, &status);
                }
            }));
        }
        Ok(outcome)
    }
}

impl ApplicationRuntime {
    pub(super) fn push_pending_task_with_command_source(
        &self,
        task: PendingTask,
        source: &RoutedCommand,
    ) -> Result<()> {
        let tasks = self
            .business
            .formal_tasks
            .as_ref()
            .ok_or_else(|| anyhow!("正式任务执行运行时尚未启动"))?;
        // Queued alone emits evidence inside FormalTaskClient; Duplicate stays silent.
        tasks.enqueue_with_command_source(task, source)?;
        Ok(())
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

    fn song_request_leaderboard(
        &self,
        since_ms: Option<u64>,
        limit: usize,
    ) -> Result<Vec<SongRequestLeaderboardEntry>> {
        Ok(self.business.song_request_leaderboard(since_ms, limit)?)
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

/// 持有唯一任务工作线程和应用执行上下文。
/// 队列顺序、任务历史和共享通道状态由 `TaskEngineHandle` 持有。
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
        live_configs: LiveConfigs,
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
            PostSettleConfig::Live(live_configs),
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
            PostSettleConfig::Fixed(post_settle),
            client,
            None,
            coordinator,
        )
    }

    fn start_worker(
        task_engine: TaskEngineHandle,
        running: Arc<AtomicBool>,
        paused: Arc<AtomicBool>,
        post_settle: PostSettleConfig,
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
                // stop_requested 已置位，任务线程完成当前任务后即退出；
                // 宽限等待后仍超时才分离线程，避免 shutdown 无限阻塞。
                let joined = self.finished.recv_timeout(FORMAL_TASK_JOIN_GRACE).is_ok();
                if joined {
                    let _ = worker.join();
                } else {
                    drop(worker);
                }
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
    post_settle: PostSettleConfig,
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
                        post_settle.current(),
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

enum PostSettleConfig {
    #[cfg(test)]
    Fixed(Duration),
    Live(LiveConfigs),
}

impl PostSettleConfig {
    fn current(&self) -> Duration {
        match self {
            #[cfg(test)]
            Self::Fixed(duration) => *duration,
            Self::Live(configs) => {
                Duration::from_millis(configs.snapshot().timing.command.post_settle_ms)
            }
        }
    }
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

pub(super) fn read_only_task_parameters_with_source(
    task: &PendingTask,
    source: Option<&RoutedCommand>,
) -> (serde_json::Value, Option<Instant>) {
    let (mut payload, captured_at) = read_only_task_parameters(task);
    if let Some(source) = source {
        payload["sourceCommand"] = crate::debug_read_only::command_parameters(source);
        payload["conversion"] = serde_json::json!({"from": "routedCommand",
            "commandFamily": payload["sourceCommand"]["commandFamily"], "to": payload["kind"]});
        return (payload, crate::debug_read_only::command_capture(source));
    }
    (payload, captured_at)
}

/// Explicit operational fields only: never serialize an application context or configuration.
fn read_only_task_parameters(task: &PendingTask) -> (serde_json::Value, Option<Instant>) {
    use serde_json::json;
    match task {
        PendingTask::Command(pending) => (
            crate::debug_read_only::command_parameters(&pending.routed),
            crate::debug_read_only::command_capture(&pending.routed),
        ),
        PendingTask::AdvanceQueue { reason } => {
            (json!({"kind": "advanceQueue", "reason": reason}), None)
        }
        PendingTask::ConsoleChat { text, prefix } => (
            json!({"kind": "consoleChat",
            "text": crate::privacy::redacted_chat_text(text), "prefix": prefix}),
            None,
        ),
        PendingTask::Startup(task) => (
            json!({"kind": "startup", "parameters": {
            "kind": format!("{:?}", task.kind())}}),
            None,
        ),
        PendingTask::ClearIdleExit => (json!({"kind": "clearIdleExit"}), None),
        PendingTask::ModerationResult(_) => (
            json!({"kind": "moderationResult",
            "parametersOmitted": "private moderation context"}),
            None,
        ),
        PendingTask::SetChatListenerMode { target } => (
            json!({"kind": "setChatListenerMode",
            "target": target.label()}),
            None,
        ),
        PendingTask::SecondaryUnread { hit, discard_only } => (
            json!({"kind": "secondaryUnread",
            "discardOnly": discard_only,
            "indicator": {"x": hit.indicator.x, "y": hit.indicator.y},
            "rowClick": {"x": hit.row_click.x, "y": hit.row_click.y}}),
            None,
        ),
        PendingTask::RestoreSecondaryHall => (json!({"kind": "restoreSecondaryHall"}), None),
        PendingTask::TurtleSoupQuestion {
            question,
            observed_at,
        } => (
            json!({"kind": "turtleSoupQuestion",
            "question": crate::privacy::redacted_chat_text(&question.question)}),
            Some(*observed_at),
        ),
        PendingTask::CardGameEffect(_) => (
            json!({"kind": "cardGameEffect",
            "parametersOmitted": "private game effect context"}),
            None,
        ),
        PendingTask::UndercoverEffect(_) => (
            json!({"kind": "undercoverEffect",
            "parametersOmitted": "private game effect context"}),
            None,
        ),
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

    fn listener_source() -> RoutedCommand {
        use crate::features::administration::{AdministrationCommand, ChatListenerModeCommand};
        use crate::features::command::{CommandAuthority, ModuleCommand};
        use crate::observation::chat::{
            BubbleSequence, ChatIdentity, ObservedChatMessageId, VisualSessionId,
        };
        let mut source = RoutedCommand::console(
            "值守模式",
            "值守模式二级",
            ModuleCommand::Administration(AdministrationCommand::ChatListenerMode(
                ChatListenerModeCommand::Secondary,
            )),
        );
        source.message_type = "pink".into();
        source.username = "香菜".into();
        source.authority = CommandAuthority::Friend;
        source.observation.message_text = Some("香菜：@值守模式二级".into());
        source.observation.message_y = Some(420);
        source.observation.message_id = Some(ObservedChatMessageId::new(
            VisualSessionId::new(7),
            ChatIdentity::Friend(Arc::from("最好的京子")),
            BubbleSequence::new(11),
        ));
        source
    }

    fn evidence_client(
        business: BusinessRuntimeHandle,
        engine: TaskEngineHandle,
    ) -> FormalTaskClient {
        FormalTaskClient::new(
            FormalTaskExecutionHandle {
                context: Weak::new(),
            },
            business,
            engine,
        )
    }

    #[test]
    fn read_only_converted_task_keeps_original_typed_parameters_and_instant() {
        let source = listener_source();
        let task = PendingTask::SetChatListenerMode {
            target: ChatListenerMode::Secondary,
        };
        let (payload, at) = read_only_task_parameters_with_source(&task, Some(&source));
        assert_eq!(at, source.observation.captured_at);
        assert_eq!(payload["kind"], "setChatListenerMode");
        assert_eq!(payload["target"], ChatListenerMode::Secondary.label());
        assert_eq!(payload["sourceCommand"]["raw"], "值守模式二级");
        assert_eq!(
            payload["sourceCommand"]["originalText"],
            "香菜：@值守模式二级"
        );
        assert_eq!(payload["sourceCommand"]["nickname"], "香菜");
        assert_eq!(payload["sourceCommand"]["messageType"], "pink");
        assert_eq!(payload["sourceCommand"]["messageId"]["bubbleSequence"], 11);
        assert_eq!(
            payload["sourceCommand"]["parameters"],
            serde_json::json!(source.command)
        );
        assert_eq!(payload["conversion"]["commandFamily"], "administration");
    }

    #[test]
    fn read_only_queue_emits_only_queued_not_duplicate_or_error() {
        use crate::debug_read_only::{ReadOnlyObserver, ReadOnlyState};
        let (runtime, engine) = business_runtime();
        let client = evidence_client(runtime.handle(), engine.clone());
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Push);
        let source = listener_source();
        let command = || {
            PendingTask::Command(Box::new(PendingCommand {
                lock_key: "test-readonly-source".into(),
                routed: source.clone(),
            }))
        };
        let FormalTaskEnqueueOutcome::Queued(receipt) = client
            .enqueue_with_observer(command(), None, &observer)
            .unwrap()
        else {
            panic!("first command must queue");
        };
        let evidence = observer.read_entry("readonly-1").unwrap();
        assert_eq!(evidence["taskId"], receipt.task_id);
        assert_eq!(evidence["task"]["nickname"], "香菜");
        assert_eq!(
            client
                .enqueue_with_observer(command(), None, &observer)
                .unwrap(),
            FormalTaskEnqueueOutcome::Duplicate
        );
        assert_eq!(observer.bus().latest_sequence(), 1);
        engine.begin_shutdown().unwrap();
        assert!(
            client
                .enqueue_with_observer(command(), None, &observer)
                .is_err()
        );
        assert_eq!(observer.bus().latest_sequence(), 1);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn read_only_listener_noop_is_silent_and_switch_never_controls_business() {
        use crate::debug_read_only::{ReadOnlyObserver, ReadOnlyState};
        for (enabled, mode) in [
            (false, ReadOnlyState::Push),
            (true, ReadOnlyState::Active),
            (true, ReadOnlyState::Push),
        ] {
            let (runtime, engine) = business_runtime();
            let business = runtime.handle();
            let client = evidence_client(business.clone(), engine);
            let observer = ReadOnlyObserver::default();
            observer.set_enabled_state(enabled, mode);
            let source = listener_source();
            for _ in 0..2 {
                let (queued, _) = business
                    .request_chat_listener_mode(ChatListenerMode::Secondary)
                    .unwrap();
                if queued {
                    assert!(matches!(
                        client
                            .enqueue_with_observer(
                                PendingTask::SetChatListenerMode {
                                    target: ChatListenerMode::Secondary
                                },
                                Some(&source),
                                &observer,
                            )
                            .unwrap(),
                        FormalTaskEnqueueOutcome::Queued(_)
                    ));
                }
            }
            assert_eq!(
                business.chat_listener_snapshot().unwrap().pending_mode,
                Some(ChatListenerMode::Secondary)
            );
            assert_eq!(
                observer.bus().latest_sequence(),
                u64::from(enabled && mode == ReadOnlyState::Push)
            );
            runtime.shutdown().unwrap();
        }
    }

    #[test]
    fn read_only_poisoned_observer_does_not_block_business_enqueue() {
        let (runtime, engine) = business_runtime();
        let client = evidence_client(runtime.handle(), engine);
        let observer = crate::debug_read_only::ReadOnlyObserver::default();
        observer.poison_for_test();
        assert!(matches!(
            client
                .enqueue_with_observer(PendingTask::ClearIdleExit, None, &observer)
                .unwrap(),
            FormalTaskEnqueueOutcome::Queued(_)
        ));
        assert_eq!(observer.bus().latest_sequence(), 0);
        runtime.shutdown().unwrap();
    }

    #[test]
    #[ignore = "requires the local real screenshot fixture; no synthetic frames"]
    fn read_only_converted_task_real_source_receipt_and_export() {
        use crate::debug_read_only::{ReadOnlyObserver, ReadOnlyState};
        use crate::runtime::ui::{CaptureFrame, UiDevice, UiRuntime};
        struct FixtureDevice(image::DynamicImage);
        impl UiDevice for FixtureDevice {
            fn capture(&mut self) -> anyhow::Result<image::DynamicImage> {
                Ok(self.0.clone())
            }
        }
        let image = image::open(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/ui/secondary-with-hall-row-1920x1080.png"),
        )
        .unwrap();
        let ui = UiRuntime::start(FixtureDevice(image), 2).unwrap();
        let capture = || {
            Arc::new(
                ui.handle()
                    .submit_external(CaptureFrame)
                    .unwrap()
                    .wait()
                    .unwrap()
                    .unwrap(),
            )
        };
        let first = capture();
        let newer = capture();
        let mut source = listener_source();
        source.observation.captured_at = Some(first.captured_at());
        let (runtime, engine) = business_runtime();
        let client = evidence_client(runtime.handle(), engine);
        let observer = ReadOnlyObserver::default();
        observer.set_enabled_state(true, ReadOnlyState::Push);
        observer.cache_frame(first.clone());
        observer.cache_frame(newer);
        let FormalTaskEnqueueOutcome::Queued(receipt) = client
            .enqueue_with_observer(
                PendingTask::SetChatListenerMode {
                    target: ChatListenerMode::Secondary,
                },
                Some(&source),
                &observer,
            )
            .unwrap()
        else {
            panic!("queue required");
        };
        let evidence = observer.read_entry("readonly-1").unwrap();
        assert_eq!(evidence["taskId"], receipt.task_id);
        assert_eq!(evidence["task"]["sourceCommand"]["raw"], "值守模式二级");
        assert_eq!(evidence["task"]["sourceCommand"]["nickname"], "香菜");
        assert_eq!(evidence["source"]["frameId"], first.id());
        assert_eq!(evidence["source"]["canvas"], "rawGame");
        assert!(
            evidence["source"]["imagePngBase64"]
                .as_str()
                .unwrap()
                .starts_with("iVBOR")
        );
        // The command text is an injected hook input. The image is an actual local
        // game fixture, but this test does not claim that OCR saw this command.
        if let Some(directory) = std::env::var_os("MILIASTRA_READONLY_EVIDENCE_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join("converted-listener.json"),
                serde_json::to_vec_pretty(&evidence).unwrap(),
            )
            .unwrap();
            first
                .image()
                .save(directory.join("converted-listener-source.png"))
                .unwrap();
            std::fs::write(directory.join("converted-provenance.json"), serde_json::to_vec_pretty(&serde_json::json!({
                "kind": "formal_task_source_binding_fixture", "actualGameCaptureDuringTest": false,
                "actualBusinessCommandExecuted": false, "commandTextInjected": true,
                "fixture": "tests/fixtures/ui/secondary-with-hall-row-1920x1080.png",
                "checks": ["real queue receipt taskID", "source and converted typed parameters", "exact source Instant", "raw screenshot export"]
            })).unwrap()).unwrap();
        }
        runtime.shutdown().unwrap();
        ui.shutdown().unwrap();
    }

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
        // 超时后先给任务 FORMAL_TASK_JOIN_GRACE 宽限收尾，再放弃线程。
        assert!(shutdown_started.elapsed() < FORMAL_TASK_JOIN_GRACE + Duration::from_millis(500));
        business_runtime.shutdown().expect("business shutdown");
    }
}
