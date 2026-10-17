//! 线程安全的沙盒任务与运行记录内存存储。
//!
//! 存储层只负责任务版本与运行生命周期，不执行原子命令；执行器可以在每一步
//! 调用 `record_command_result` 并通过 `is_cancel_requested` 响应取消。

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::debug_sandbox::{
    SandboxCommandResult, SandboxLimits, SandboxRun, SandboxStatus, SandboxTask,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SandboxStoreError {
    InvalidTask(String),
    StateUnavailable,
    Persistence(String),
    CapacityExceeded,
    ResultsIncomplete,
    InvalidResultIndex { expected: usize, actual: usize },
    VersionConflict { expected: u32, actual: u32 },
    TaskNotFound(String),
    RunNotFound(String),
    InvalidRunState { status: SandboxStatus },
}

impl fmt::Display for SandboxStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExceeded => formatter.write_str("沙盒存储容量已满"),
            Self::ResultsIncomplete => formatter.write_str("命令结果尚未全部成功"),
            Self::StateUnavailable => formatter.write_str("沙盒存储状态不可用"),
            Self::Persistence(message) => write!(formatter, "沙盒持久化失败: {message}"),
            Self::InvalidResultIndex { expected, actual } => write!(
                formatter,
                "命令序号错误: expected={expected}, actual={actual}"
            ),
            Self::InvalidTask(message) => write!(formatter, "无效沙盒任务: {message}"),
            Self::VersionConflict { expected, actual } => {
                write!(
                    formatter,
                    "任务版本冲突: expected={expected}, actual={actual}"
                )
            }
            Self::TaskNotFound(id) => write!(formatter, "沙盒任务不存在: {id}"),
            Self::RunNotFound(id) => write!(formatter, "沙盒运行记录不存在: {id}"),
            Self::InvalidRunState { status } => {
                write!(formatter, "运行状态不允许此操作: {status:?}")
            }
        }
    }
}

impl std::error::Error for SandboxStoreError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxTaskSaveOutcome {
    Created,
    Updated,
}

#[derive(Clone, Debug)]
struct RunRecord {
    run: SandboxRun,
    task: SandboxTask,
    cancellation_requested: Arc<AtomicBool>,
}

#[derive(Default)]
struct StoreState {
    tasks: HashMap<String, SandboxTask>,
    runs: HashMap<String, RunRecord>,
}

/// 进程内沙盒存储。所有快照均按值复制，任务更新不会改变已启动运行使用的版本。
/// 最多保存 256 个任务和 1024 条运行记录，容量满时拒绝新增，保留已有证据。
/// 不负责调度或输入许可；调用方仍须使用现有任务引擎和 UI runtime。
#[derive(Clone)]
pub(crate) struct DebugSandboxStore {
    state: Arc<Mutex<StoreState>>,
    limits: SandboxLimits,
    max_tasks: usize,
    max_runs: usize,
}

impl DebugSandboxStore {
    pub(crate) fn new(limits: SandboxLimits) -> Self {
        Self {
            state: Arc::new(Mutex::new(StoreState::default())),
            limits,
            max_tasks: 256,
            max_runs: 1024,
        }
    }

    pub(crate) fn save_task(
        &self,
        task: SandboxTask,
    ) -> Result<SandboxTaskSaveOutcome, SandboxStoreError> {
        self.save_task_with(task, |_| Ok(()))
    }

    pub(crate) fn save_task_with(
        &self,
        task: SandboxTask,
        persist: impl FnOnce(&SandboxTask) -> Result<(), String>,
    ) -> Result<SandboxTaskSaveOutcome, SandboxStoreError> {
        task.validate(self.limits)
            .map_err(SandboxStoreError::InvalidTask)?;
        let mut state = self.lock_state()?;
        let outcome = match state.tasks.get(&task.id) {
            None => SandboxTaskSaveOutcome::Created,
            Some(previous) if task.version > previous.version => SandboxTaskSaveOutcome::Updated,
            Some(previous) => {
                return Err(SandboxStoreError::VersionConflict {
                    expected: previous.version.saturating_add(1),
                    actual: task.version,
                });
            }
        };
        if outcome == SandboxTaskSaveOutcome::Created && state.tasks.len() >= self.max_tasks {
            return Err(SandboxStoreError::CapacityExceeded);
        }
        persist(&task).map_err(SandboxStoreError::Persistence)?;
        state.tasks.insert(task.id.clone(), task);
        Ok(outcome)
    }

    pub(crate) fn task(&self, task_id: &str) -> Result<Option<SandboxTask>, SandboxStoreError> {
        Ok(self.lock_state()?.tasks.get(task_id).cloned())
    }

    pub(crate) fn tasks(&self) -> Result<Vec<SandboxTask>, SandboxStoreError> {
        let mut tasks = self
            .lock_state()?
            .tasks
            .values()
            .cloned()
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(tasks)
    }

    /// 按任务当前版本创建运行快照；运行期间保存任务的新版本不会影响该快照。
    pub(crate) fn start_run(
        &self,
        task_id: &str,
        expected_version: Option<u32>,
    ) -> Result<SandboxRun, SandboxStoreError> {
        let mut state = self.lock_state()?;
        let task = state
            .tasks
            .get(task_id)
            .ok_or_else(|| SandboxStoreError::TaskNotFound(task_id.to_string()))?;
        if let Some(expected) = expected_version {
            if task.version != expected {
                return Err(SandboxStoreError::VersionConflict {
                    expected,
                    actual: task.version,
                });
            }
        }
        let task = task.clone();
        Self::insert_run(&mut state, task, self.max_runs)
    }

    pub(crate) fn start_run_snapshot(
        &self,
        task: SandboxTask,
    ) -> Result<SandboxRun, SandboxStoreError> {
        task.validate(self.limits)
            .map_err(SandboxStoreError::InvalidTask)?;
        let mut state = self.lock_state()?;
        Self::insert_run(&mut state, task, self.max_runs)
    }

    fn insert_run(
        state: &mut StoreState,
        task: SandboxTask,
        max_runs: usize,
    ) -> Result<SandboxRun, SandboxStoreError> {
        if state.runs.len() >= max_runs {
            return Err(SandboxStoreError::CapacityExceeded);
        }
        let run = SandboxRun {
            run_id: Uuid::new_v4().to_string(),
            task_id: task.id.clone(),
            task_version: task.version,
            status: SandboxStatus::Running,
            results: Vec::new(),
        };
        state.runs.insert(
            run.run_id.clone(),
            RunRecord {
                run: run.clone(),
                task,
                cancellation_requested: Arc::new(AtomicBool::new(false)),
            },
        );
        Ok(run)
    }

    pub(crate) fn release_completed(&self, run_id: &str) -> Result<(), SandboxStoreError> {
        let mut state = self.lock_state()?;
        let record = state
            .runs
            .get(run_id)
            .ok_or_else(|| SandboxStoreError::RunNotFound(run_id.to_owned()))?;
        if record.run.status == SandboxStatus::Running {
            return Err(SandboxStoreError::InvalidRunState {
                status: SandboxStatus::Running,
            });
        }
        state.runs.remove(run_id);
        Ok(())
    }

    pub(crate) fn run(&self, run_id: &str) -> Result<Option<SandboxRun>, SandboxStoreError> {
        Ok(self
            .lock_state()?
            .runs
            .get(run_id)
            .map(|record| record.run.clone()))
    }

    pub(crate) fn run_task(&self, run_id: &str) -> Result<SandboxTask, SandboxStoreError> {
        self.lock_state()?
            .runs
            .get(run_id)
            .map(|record| record.task.clone())
            .ok_or_else(|| SandboxStoreError::RunNotFound(run_id.to_string()))
    }

    pub(crate) fn runs(&self) -> Result<Vec<SandboxRun>, SandboxStoreError> {
        let mut runs = self
            .lock_state()?
            .runs
            .values()
            .map(|record| record.run.clone())
            .collect::<Vec<_>>();
        runs.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        Ok(runs)
    }

    pub(crate) fn record_command_result(
        &self,
        run_id: &str,
        result: SandboxCommandResult,
    ) -> Result<SandboxRun, SandboxStoreError> {
        let mut state = self.lock_state()?;
        let record = state
            .runs
            .get_mut(run_id)
            .ok_or_else(|| SandboxStoreError::RunNotFound(run_id.to_string()))?;
        if record.run.status != SandboxStatus::Running {
            return Err(SandboxStoreError::InvalidRunState {
                status: record.run.status,
            });
        }
        let expected = record.run.results.len();
        if result.index != expected || expected >= record.task.commands.len() {
            return Err(SandboxStoreError::InvalidResultIndex {
                expected,
                actual: result.index,
            });
        }
        let failed = !result.ok;
        record.run.results.push(result);
        if failed {
            record.run.status = if record.cancellation_requested.load(Ordering::Acquire) {
                SandboxStatus::Cancelled
            } else {
                SandboxStatus::Failed
            };
        }
        Ok(record.run.clone())
    }

    pub(crate) fn finish_run(
        &self,
        run_id: &str,
        status: SandboxStatus,
    ) -> Result<SandboxRun, SandboxStoreError> {
        if !matches!(
            status,
            SandboxStatus::Passed | SandboxStatus::Failed | SandboxStatus::Cancelled
        ) {
            return Err(SandboxStoreError::InvalidRunState { status });
        }
        let mut state = self.lock_state()?;
        let record = state
            .runs
            .get_mut(run_id)
            .ok_or_else(|| SandboxStoreError::RunNotFound(run_id.to_string()))?;
        if record.run.status != SandboxStatus::Running {
            return Err(SandboxStoreError::InvalidRunState {
                status: record.run.status,
            });
        }
        if status == SandboxStatus::Passed
            && !record.cancellation_requested.load(Ordering::Acquire)
            && (record.run.results.len() != record.task.commands.len()
                || record.run.results.iter().any(|result| !result.ok))
        {
            return Err(SandboxStoreError::ResultsIncomplete);
        }
        record.run.status = if record.cancellation_requested.load(Ordering::Acquire) {
            SandboxStatus::Cancelled
        } else {
            status
        };
        Ok(record.run.clone())
    }

    /// 请求取消运行；执行器可通过轮询标志尽快停止，最终状态由 finish_run 固化。
    pub(crate) fn cancel_run(&self, run_id: &str) -> Result<SandboxRun, SandboxStoreError> {
        let mut state = self.lock_state()?;
        let record = state
            .runs
            .get_mut(run_id)
            .ok_or_else(|| SandboxStoreError::RunNotFound(run_id.to_string()))?;
        match record.run.status {
            SandboxStatus::Running => {
                record.cancellation_requested.store(true, Ordering::Release);
                Ok(record.run.clone())
            }
            status => Err(SandboxStoreError::InvalidRunState { status }),
        }
    }

    pub(crate) fn cancellation_signal(
        &self,
        run_id: &str,
    ) -> Result<Arc<AtomicBool>, SandboxStoreError> {
        self.lock_state()?
            .runs
            .get(run_id)
            .map(|record| record.cancellation_requested.clone())
            .ok_or_else(|| SandboxStoreError::RunNotFound(run_id.to_string()))
    }

    pub(crate) fn is_cancel_requested(&self, run_id: &str) -> Result<bool, SandboxStoreError> {
        let state = self.lock_state()?;
        let record = state
            .runs
            .get(run_id)
            .ok_or_else(|| SandboxStoreError::RunNotFound(run_id.to_string()))?;
        Ok(record.cancellation_requested.load(Ordering::Acquire)
            || record.run.status == SandboxStatus::Cancelled)
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, StoreState>, SandboxStoreError> {
        self.state
            .lock()
            .map_err(|_| SandboxStoreError::StateUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_sandbox::{AtomicCommand, SandboxCommandResult};
    use std::sync::Arc;
    use std::thread;

    fn task(id: &str, version: u32) -> SandboxTask {
        SandboxTask {
            id: id.into(),
            version,
            description: String::new(),
            commands: vec![AtomicCommand::Capture],
            capture_after_each: false,
        }
    }

    fn result(index: usize) -> SandboxCommandResult {
        SandboxCommandResult {
            index,
            ok: true,
            status: "ok".into(),
            error: None,
            frame_sequence: None,
        }
    }

    #[test]
    fn release_completed_rejects_running_and_removes_terminal_record() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        let run = store.start_run_snapshot(task("cleanup", 1)).unwrap();
        assert!(matches!(
            store.release_completed(&run.run_id),
            Err(SandboxStoreError::InvalidRunState {
                status: SandboxStatus::Running
            })
        ));
        store.record_command_result(&run.run_id, result(0)).unwrap();
        store
            .finish_run(&run.run_id, SandboxStatus::Passed)
            .unwrap();
        store.release_completed(&run.run_id).unwrap();
        assert!(store.run(&run.run_id).unwrap().is_none());
    }

    #[test]
    fn saves_tasks_with_monotonic_versions() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        assert_eq!(
            store.save_task(task("a", 1)).unwrap(),
            SandboxTaskSaveOutcome::Created
        );
        assert_eq!(
            store.save_task(task("a", 2)).unwrap(),
            SandboxTaskSaveOutcome::Updated
        );
        let error = store.save_task(task("a", 2)).unwrap_err();
        assert!(matches!(
            error,
            SandboxStoreError::VersionConflict {
                expected: 3,
                actual: 2
            }
        ));
    }

    #[test]
    fn runs_use_copy_on_write_task_snapshots_and_record_results() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store.save_task(task("a", 1)).unwrap();
        let run = store.start_run("a", Some(1)).unwrap();
        let mut edited = task("a", 2);
        edited.commands = vec![AtomicCommand::Wait { milliseconds: 1 }];
        store.save_task(edited).unwrap();
        assert_eq!(
            store.run_task(&run.run_id).unwrap().commands,
            vec![AtomicCommand::Capture]
        );
        assert_eq!(store.run(&run.run_id).unwrap().unwrap().task_version, 1);
        store.record_command_result(&run.run_id, result(0)).unwrap();
        let finished = store
            .finish_run(&run.run_id, SandboxStatus::Passed)
            .unwrap();
        assert_eq!(finished.status, SandboxStatus::Passed);
        assert_eq!(finished.results.len(), 1);
    }

    #[test]
    fn cancellation_waits_for_executor_and_cannot_be_overwritten() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store.save_task(task("a", 1)).unwrap();
        let run = store.start_run("a", None).unwrap();
        let canceled = store.cancel_run(&run.run_id).unwrap();
        assert_eq!(canceled.status, SandboxStatus::Running);
        assert!(store.is_cancel_requested(&run.run_id).unwrap());
        store.record_command_result(&run.run_id, result(0)).unwrap();
        assert_eq!(
            store
                .finish_run(&run.run_id, SandboxStatus::Passed)
                .unwrap()
                .status,
            SandboxStatus::Cancelled
        );
    }

    #[test]
    fn invalid_results_and_early_success_do_not_change_history() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store.save_task(task("a", 1)).unwrap();
        assert!(store.start_run("a", Some(2)).is_err());
        assert!(store.runs().unwrap().is_empty());
        let run = store.start_run("a", None).unwrap();
        assert!(
            store
                .finish_run(&run.run_id, SandboxStatus::Passed)
                .is_err()
        );
        assert!(store.record_command_result(&run.run_id, result(1)).is_err());
        let mut failed = result(0);
        failed.ok = false;
        failed.error = Some("failed capture".into());
        assert_eq!(
            store
                .record_command_result(&run.run_id, failed)
                .unwrap()
                .status,
            SandboxStatus::Failed
        );
        assert!(store.record_command_result(&run.run_id, result(1)).is_err());
        assert!(
            store
                .finish_run(&run.run_id, SandboxStatus::Passed)
                .is_err()
        );
        assert_eq!(store.run(&run.run_id).unwrap().unwrap().results.len(), 1);
    }

    #[test]
    fn rejects_invalid_tasks_and_handles_maximum_version_without_overflow() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        assert!(store.save_task(task("a", 0)).is_err());
        assert!(store.tasks().unwrap().is_empty());
        store.save_task(task("a", u32::MAX)).unwrap();
        assert!(store.save_task(task("a", u32::MAX)).is_err());
        assert_eq!(store.task("a").unwrap().unwrap().version, u32::MAX);
    }

    #[test]
    fn capacity_limits_reject_new_records_without_dropping_history() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        for id in 0..256 {
            store.save_task(task(&id.to_string(), 1)).unwrap();
        }
        assert_eq!(
            store.save_task(task("overflow", 1)),
            Err(SandboxStoreError::CapacityExceeded)
        );
        store.save_task(task("0", 2)).unwrap();
        for _ in 0..1024 {
            store.start_run("0", None).unwrap();
        }
        assert!(matches!(
            store.start_run("0", None),
            Err(SandboxStoreError::CapacityExceeded)
        ));
        assert_eq!(store.tasks().unwrap().len(), 256);
        assert_eq!(store.runs().unwrap().len(), 1024);
    }

    #[test]
    fn concurrent_same_version_has_exactly_one_winner() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store.save_task(task("shared", 1)).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers = (0..8)
            .map(|_| {
                let store = store.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    store.save_task(task("shared", 2)).is_ok()
                })
            })
            .collect::<Vec<_>>();
        let successes = workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap().then_some(()))
            .count();
        assert_eq!(successes, 1);
    }

    #[test]
    fn concurrent_saves_are_serialized() {
        let store = Arc::new(DebugSandboxStore::new(SandboxLimits::default()));
        store.save_task(task("shared", 1)).unwrap();
        let mut workers = Vec::new();
        for version in 2..=8 {
            let store = Arc::clone(&store);
            workers.push(thread::spawn(move || {
                store.save_task(task("shared", version))
            }));
        }
        for worker in workers {
            let _ = worker.join().unwrap();
        }
        assert_eq!(store.task("shared").unwrap().unwrap().version, 8);
    }
}
