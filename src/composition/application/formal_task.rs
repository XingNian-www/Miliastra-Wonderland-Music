use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use anyhow::{Result, anyhow};

use super::{ApplicationRuntime, PendingTask, PendingTaskExecution};
use crate::features::startup::StartupTask;
use crate::interfaces::chat::PendingCommand;
use crate::interfaces::http::{HttpTaskPort, WebToolRequest};
use crate::runtime::business::{BusinessRuntimeError, BusinessRuntimeHandle};
use crate::runtime::chat_listener::ChatListenerMode;
use crate::runtime::scheduler::{
    DiagnosticTaskSnapshot, DiagnosticTaskSubmission, DiagnosticTaskWork, FormalTaskDedupKey,
    FormalTaskEnqueueOutcome, FormalTaskSubmission, FormalTaskWork,
};

const EXECUTION_QUEUE_CAPACITY: usize = 8;

enum ExecutionMessage {
    Execute {
        task: PendingTask,
        response: SyncSender<Result<String, String>>,
    },
    ExecuteDiagnostic {
        request: WebToolRequest,
        response: SyncSender<Result<String, String>>,
    },
    Shutdown(SyncSender<()>),
}

#[derive(Clone)]
pub(crate) struct FormalTaskExecutionHandle {
    sender: SyncSender<ExecutionMessage>,
}

impl FormalTaskExecutionHandle {
    fn execute(&self, task: PendingTask) -> Result<String> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(ExecutionMessage::Execute { task, response })
            .map_err(|_| anyhow!("正式任务执行运行时已停止"))?;
        receiver
            .recv()
            .map_err(|_| anyhow!("正式任务执行运行时未返回结果"))?
            .map_err(anyhow::Error::msg)
    }

    fn execute_diagnostic(&self, request: WebToolRequest) -> Result<String> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.sender
            .send(ExecutionMessage::ExecuteDiagnostic { request, response })
            .map_err(|_| anyhow!("应用执行运行时已停止"))?;
        receiver
            .recv()
            .map_err(|_| anyhow!("应用执行运行时未返回诊断结果"))?
            .map_err(anyhow::Error::msg)
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
}

impl FormalTaskClient {
    pub(crate) fn new(
        executor: FormalTaskExecutionHandle,
        business: BusinessRuntimeHandle,
    ) -> Self {
        Self { executor, business }
    }

    pub(crate) fn enqueue_command(
        &self,
        pending: PendingCommand,
    ) -> Result<FormalTaskEnqueueOutcome, BusinessRuntimeError> {
        self.enqueue(PendingTask::Command(Box::new(pending)))
    }

    pub(crate) fn enqueue_startup(
        &self,
        task: StartupTask,
    ) -> Result<FormalTaskEnqueueOutcome, BusinessRuntimeError> {
        self.enqueue(PendingTask::Startup(task))
    }

    pub(crate) fn enqueue_console_chat(
        &self,
        text: String,
        prefix: String,
    ) -> Result<FormalTaskEnqueueOutcome, BusinessRuntimeError> {
        self.enqueue(PendingTask::ConsoleChat { text, prefix })
    }

    pub(crate) fn enqueue_listener_mode(
        &self,
        target: ChatListenerMode,
    ) -> Result<FormalTaskEnqueueOutcome, BusinessRuntimeError> {
        self.enqueue(PendingTask::SetChatListenerMode { target })
    }

    pub(crate) fn enqueue_clear_idle_exit(
        &self,
    ) -> Result<FormalTaskEnqueueOutcome, BusinessRuntimeError> {
        self.enqueue(PendingTask::ClearIdleExit)
    }

    pub(crate) fn enqueue_diagnostic(
        &self,
        request: WebToolRequest,
    ) -> Result<DiagnosticTaskSnapshot, BusinessRuntimeError> {
        self.business
            .enqueue_diagnostic_task(diagnostic_task_submission(self.executor.clone(), request))
    }

    pub(super) fn enqueue(
        &self,
        task: PendingTask,
    ) -> Result<FormalTaskEnqueueOutcome, BusinessRuntimeError> {
        self.business.enqueue_formal_task(formal_task_submission(
            self.executor.clone(),
            self.business.clone(),
            task,
        ))
    }
}

impl HttpTaskPort for FormalTaskClient {
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
}

pub(crate) struct FormalTaskExecutionRuntime {
    handle: FormalTaskExecutionHandle,
    worker: Option<JoinHandle<()>>,
}

impl FormalTaskExecutionRuntime {
    pub(crate) fn start(
        build_app: impl FnOnce(FormalTaskExecutionHandle) -> ApplicationRuntime,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(EXECUTION_QUEUE_CAPACITY);
        let handle = FormalTaskExecutionHandle { sender };
        let app = build_app(handle.clone());
        let worker = thread::Builder::new()
            .name("formal-task-execution".to_string())
            .spawn(move || run_execution_loop(app, receiver))
            .map_err(|error| anyhow!("启动正式任务执行运行时失败: {error}"))?;
        Ok(Self {
            handle,
            worker: Some(worker),
        })
    }

    pub(crate) fn handle(&self) -> FormalTaskExecutionHandle {
        self.handle.clone()
    }

    pub(crate) fn shutdown(mut self) -> Result<()> {
        self.stop()
    }

    fn stop(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let (response, receiver) = mpsc::sync_channel(0);
        let _ = self
            .handle
            .sender
            .send(ExecutionMessage::Shutdown(response));
        let _ = receiver.recv();
        worker
            .join()
            .map_err(|_| anyhow!("正式任务执行运行时线程 panic"))
    }
}

impl Drop for FormalTaskExecutionRuntime {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            log::error!("正式任务执行运行时关闭失败: {error:#}");
        }
    }
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
    fn execute(self: Box<Self>) -> Result<String> {
        self.executor.execute(self.task)
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

fn run_execution_loop(mut app: ApplicationRuntime, receiver: Receiver<ExecutionMessage>) {
    while let Ok(message) = receiver.recv() {
        match message {
            ExecutionMessage::Execute { task, response } => {
                let label = task.label();
                let result = match catch_unwind(AssertUnwindSafe(|| app.execute_pending_task(task)))
                {
                    Ok(Ok(PendingTaskExecution::Completed)) => Ok(format!("{label}执行完成")),
                    Ok(Err(error)) => Err(format!("{error:#}")),
                    Err(_) => Err("待处理任务执行发生未捕获异常".to_string()),
                };
                let _ = response.send(result);
            }
            ExecutionMessage::ExecuteDiagnostic { request, response } => {
                let result = match catch_unwind(AssertUnwindSafe(|| {
                    app.execute_web_tool_request(request)
                })) {
                    Ok(Ok(result)) => Ok(result),
                    Ok(Err(error)) => Err(format!("{error:#}")),
                    Err(_) => Err("Web 工具执行发生未捕获异常".to_string()),
                };
                let _ = response.send(result);
            }
            ExecutionMessage::Shutdown(response) => {
                let _ = response.send(());
                break;
            }
        }
    }
}
