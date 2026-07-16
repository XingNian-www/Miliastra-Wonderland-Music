use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

const MAX_TASK_RECORDS: usize = 60;
const MAX_TASK_RESULT_CHARS: usize = 4 * 1024;
const MAX_DIAGNOSTIC_RECORDS: usize = 30;
const MAX_QUEUED_DIAGNOSTICS: usize = 10;
const MAX_DIAGNOSTIC_RESULT_CHARS: usize = 48 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FormalTaskDedupKey(String);

impl FormalTaskDedupKey {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

pub(crate) trait FormalTaskWork: Send {
    fn execute(self: Box<Self>) -> anyhow::Result<String>;

    fn cancel(self: Box<Self>);
}

pub(crate) trait DiagnosticTaskWork: Send {
    fn execute(self: Box<Self>) -> anyhow::Result<String>;
}

pub(crate) struct DiagnosticTaskSubmission {
    label: String,
    work: Box<dyn DiagnosticTaskWork>,
}

impl DiagnosticTaskSubmission {
    pub(crate) fn new(label: impl Into<String>, work: Box<dyn DiagnosticTaskWork>) -> Self {
        Self {
            label: label.into(),
            work,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticTaskSnapshot {
    pub(crate) id: u64,
    pub(crate) label: String,
    pub(crate) status: String,
    pub(crate) result: Option<String>,
}

struct DiagnosticTaskRecord {
    id: u64,
    label: String,
    status: &'static str,
    result: Option<String>,
}

struct QueuedDiagnosticTask {
    id: u64,
    label: String,
    work: Box<dyn DiagnosticTaskWork>,
}

pub(crate) struct DiagnosticTaskLease {
    id: u64,
    label: String,
    work: Box<dyn DiagnosticTaskWork>,
}

impl DiagnosticTaskLease {
    pub(crate) const fn task_id(&self) -> u64 {
        self.id
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn execute(self) -> anyhow::Result<String> {
        self.work.execute()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticTaskCompletion {
    Succeeded(String),
    Failed(String),
}

pub(crate) struct FormalTaskSubmission {
    label: String,
    dedup_key: Option<FormalTaskDedupKey>,
    playback_related: bool,
    work: Box<dyn FormalTaskWork>,
}

impl FormalTaskSubmission {
    pub(crate) fn new(
        label: impl Into<String>,
        dedup_key: Option<FormalTaskDedupKey>,
        playback_related: bool,
        work: Box<dyn FormalTaskWork>,
    ) -> Self {
        Self {
            label: label.into(),
            dedup_key,
            playback_related,
            work,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FormalTaskReceipt {
    pub(crate) task_id: u64,
    pub(crate) position: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormalTaskEnqueueOutcome {
    Queued(FormalTaskReceipt),
    Duplicate,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FormalSchedulerSnapshot {
    pending_labels: Vec<String>,
    tasks: Vec<FormalTaskSnapshot>,
    active_lane: Option<SchedulerLane>,
    active_playback_related: bool,
    pending_playback_related: bool,
    pending_diagnostic: bool,
}

impl FormalSchedulerSnapshot {
    pub(crate) fn pending_labels(&self) -> &[String] {
        &self.pending_labels
    }

    pub(crate) fn tasks(&self) -> &[FormalTaskSnapshot] {
        &self.tasks
    }

    pub(crate) const fn is_busy(&self) -> bool {
        self.active_lane.is_some()
    }

    pub(crate) const fn is_idle(&self) -> bool {
        self.active_lane.is_none() && self.pending_labels.is_empty() && !self.pending_diagnostic
    }

    pub(crate) const fn active_playback_related(&self) -> bool {
        self.active_playback_related
    }

    pub(crate) const fn pending_playback_related(&self) -> bool {
        self.pending_playback_related
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FormalTaskSnapshot {
    pub(crate) id: u64,
    pub(crate) label: String,
    pub(crate) status: String,
    pub(crate) queued_at_ms: u64,
    pub(crate) started_at_ms: Option<u64>,
    pub(crate) finished_at_ms: Option<u64>,
    pub(crate) elapsed_ms: u64,
    pub(crate) result: Option<String>,
}

#[derive(Clone, Debug)]
struct FormalTaskRecord {
    id: u64,
    label: String,
    status: &'static str,
    queued_at_ms: u64,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
    result: Option<String>,
}

struct QueuedFormalTask {
    id: u64,
    label: String,
    dedup_key: Option<FormalTaskDedupKey>,
    playback_related: bool,
    work: Box<dyn FormalTaskWork>,
}

pub(crate) struct FormalTaskLease {
    id: u64,
    label: String,
    dedup_key: Option<FormalTaskDedupKey>,
    playback_related: bool,
    work: Box<dyn FormalTaskWork>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchedulerLane {
    Formal,
    Deferred,
    Diagnostic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SchedulerLaneLease {
    id: u64,
    lane: SchedulerLane,
}

enum ActiveSchedulerWork {
    Formal {
        id: u64,
        label: String,
        playback_related: bool,
    },
    External(SchedulerLaneLease),
    Diagnostic {
        id: u64,
    },
}

impl FormalTaskLease {
    pub(crate) const fn task_id(&self) -> u64 {
        self.id
    }

    pub(crate) fn label(&self) -> &str {
        &self.label
    }

    pub(crate) fn execute(self) -> anyhow::Result<String> {
        self.work.execute()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FormalTaskCompletion {
    Succeeded(String),
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FormalTaskCancelOutcome {
    Canceled,
    NotQueued,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FormalSchedulerError(String);

impl std::fmt::Display for FormalSchedulerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub(crate) struct FormalScheduler {
    next_id: u64,
    next_lane_lease_id: u64,
    queued: VecDeque<QueuedFormalTask>,
    active: Option<ActiveSchedulerWork>,
    records: VecDeque<FormalTaskRecord>,
    diagnostic_queued: VecDeque<QueuedDiagnosticTask>,
    diagnostic_records: VecDeque<DiagnosticTaskRecord>,
}

impl FormalScheduler {
    pub(crate) fn new() -> Self {
        Self {
            next_id: 1,
            next_lane_lease_id: 1,
            queued: VecDeque::new(),
            active: None,
            records: VecDeque::new(),
            diagnostic_queued: VecDeque::new(),
            diagnostic_records: VecDeque::new(),
        }
    }

    pub(crate) fn enqueue_diagnostic(
        &mut self,
        submission: DiagnosticTaskSubmission,
    ) -> Result<DiagnosticTaskSnapshot, FormalSchedulerError> {
        if self.diagnostic_queued.len() >= MAX_QUEUED_DIAGNOSTICS {
            return Err(FormalSchedulerError(
                "Web 工具任务过多，请等待现有任务完成".to_string(),
            ));
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let label = submission.label;
        self.diagnostic_queued.push_back(QueuedDiagnosticTask {
            id,
            label: label.clone(),
            work: submission.work,
        });
        self.diagnostic_records.push_front(DiagnosticTaskRecord {
            id,
            label: label.clone(),
            status: "queued",
            result: None,
        });
        trim_diagnostic_records(&mut self.diagnostic_records);
        Ok(DiagnosticTaskSnapshot {
            id,
            label,
            status: "queued".to_string(),
            result: None,
        })
    }

    pub(crate) fn take_next_diagnostic(&mut self) -> Option<DiagnosticTaskLease> {
        if self.active.is_some() || !self.queued.is_empty() {
            return None;
        }
        let task = self.diagnostic_queued.pop_front()?;
        self.active = Some(ActiveSchedulerWork::Diagnostic { id: task.id });
        if let Some(record) = self
            .diagnostic_records
            .iter_mut()
            .find(|record| record.id == task.id)
        {
            record.status = "running";
        }
        Some(DiagnosticTaskLease {
            id: task.id,
            label: task.label,
            work: task.work,
        })
    }

    pub(crate) fn complete_diagnostic(
        &mut self,
        task_id: u64,
        completion: DiagnosticTaskCompletion,
    ) -> Result<(), FormalSchedulerError> {
        match self.active.as_ref() {
            Some(ActiveSchedulerWork::Diagnostic { id, .. }) if *id == task_id => {}
            Some(ActiveSchedulerWork::Diagnostic { id, .. }) => {
                return Err(FormalSchedulerError(format!(
                    "诊断任务完成顺序不一致: active={id} actual={task_id}"
                )));
            }
            Some(_) => {
                return Err(FormalSchedulerError(format!(
                    "其他调度通道仍在执行，不能完成诊断任务: task_id={task_id}"
                )));
            }
            None => {
                return Err(FormalSchedulerError(format!(
                    "诊断任务没有活动租约: task_id={task_id}"
                )));
            }
        }
        self.active = None;
        if let Some(record) = self
            .diagnostic_records
            .iter_mut()
            .find(|record| record.id == task_id)
        {
            match completion {
                DiagnosticTaskCompletion::Succeeded(result) => {
                    record.status = "completed";
                    record.result = Some(limit_diagnostic_result(result));
                }
                DiagnosticTaskCompletion::Failed(error) => {
                    record.status = "failed";
                    record.result = Some(limit_diagnostic_result(format!("错误: {error}")));
                }
            }
        }
        trim_diagnostic_records(&mut self.diagnostic_records);
        Ok(())
    }

    pub(crate) fn diagnostic_snapshot(&self) -> Vec<DiagnosticTaskSnapshot> {
        self.diagnostic_records
            .iter()
            .take(8)
            .map(|record| DiagnosticTaskSnapshot {
                id: record.id,
                label: record.label.clone(),
                status: record.status.to_string(),
                result: record.result.clone(),
            })
            .collect()
    }

    pub(crate) fn diagnostic_task_snapshot(&self, id: u64) -> Option<DiagnosticTaskSnapshot> {
        self.diagnostic_records
            .iter()
            .find(|record| record.id == id)
            .map(|record| DiagnosticTaskSnapshot {
                id: record.id,
                label: record.label.clone(),
                status: record.status.to_string(),
                result: record.result.clone(),
            })
    }

    pub(crate) fn enqueue(&mut self, submission: FormalTaskSubmission) -> FormalTaskEnqueueOutcome {
        if submission.dedup_key.as_ref().is_some_and(|key| {
            self.queued
                .iter()
                .any(|task| task.dedup_key.as_ref() == Some(key))
        }) {
            return FormalTaskEnqueueOutcome::Duplicate;
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let label = submission.label;
        self.queued.push_back(QueuedFormalTask {
            id,
            label: label.clone(),
            dedup_key: submission.dedup_key,
            playback_related: submission.playback_related,
            work: submission.work,
        });
        self.records.push_front(FormalTaskRecord {
            id,
            label,
            status: "queued",
            queued_at_ms: current_unix_millis(),
            started_at_ms: None,
            finished_at_ms: None,
            result: None,
        });
        trim_records(&mut self.records);
        FormalTaskEnqueueOutcome::Queued(FormalTaskReceipt {
            task_id: id,
            position: self.queued.len(),
        })
    }

    pub(crate) fn snapshot(&self) -> FormalSchedulerSnapshot {
        let now = current_unix_millis();
        FormalSchedulerSnapshot {
            pending_labels: self.queued.iter().map(|task| task.label.clone()).collect(),
            tasks: self
                .records
                .iter()
                .take(30)
                .map(|record| record_snapshot(record, now))
                .collect(),
            active_lane: self.active.as_ref().map(|active| match active {
                ActiveSchedulerWork::Formal { .. } => SchedulerLane::Formal,
                ActiveSchedulerWork::External(lease) => lease.lane,
                ActiveSchedulerWork::Diagnostic { .. } => SchedulerLane::Diagnostic,
            }),
            active_playback_related: matches!(
                self.active.as_ref(),
                Some(&ActiveSchedulerWork::Formal {
                    playback_related: true,
                    ..
                })
            ),
            pending_playback_related: self.queued.iter().any(|task| task.playback_related),
            pending_diagnostic: !self.diagnostic_queued.is_empty(),
        }
    }

    pub(crate) fn contains_dedup_key(&self, key: &FormalTaskDedupKey) -> bool {
        self.queued
            .iter()
            .any(|task| task.dedup_key.as_ref() == Some(key))
    }

    pub(crate) fn take_next(&mut self) -> Option<FormalTaskLease> {
        if self.active.is_some() {
            return None;
        }
        let queued = self.queued.pop_front()?;
        self.active = Some(ActiveSchedulerWork::Formal {
            id: queued.id,
            label: queued.label.clone(),
            playback_related: queued.playback_related,
        });
        self.update_record(queued.id, |record| {
            record.status = "running";
            record.started_at_ms = Some(current_unix_millis());
            record.finished_at_ms = None;
            record.result = None;
        });
        Some(FormalTaskLease {
            id: queued.id,
            label: queued.label,
            dedup_key: queued.dedup_key,
            playback_related: queued.playback_related,
            work: queued.work,
        })
    }

    pub(crate) fn restore(&mut self, lease: FormalTaskLease) -> Result<(), FormalSchedulerError> {
        self.require_active(lease.id)?;
        self.active = None;
        self.update_record(lease.id, |record| {
            record.status = "queued";
            record.started_at_ms = None;
            record.finished_at_ms = None;
            record.result = None;
        });
        self.queued.push_front(QueuedFormalTask {
            id: lease.id,
            label: lease.label,
            dedup_key: lease.dedup_key,
            playback_related: lease.playback_related,
            work: lease.work,
        });
        Ok(())
    }

    pub(crate) fn complete(
        &mut self,
        task_id: u64,
        completion: FormalTaskCompletion,
    ) -> Result<(), FormalSchedulerError> {
        self.require_active(task_id)?;
        self.active = None;
        self.update_record(task_id, |record| {
            record.finished_at_ms = Some(current_unix_millis());
            match completion {
                FormalTaskCompletion::Succeeded(result) => {
                    record.status = "completed";
                    record.result = Some(limit_result(result));
                }
                FormalTaskCompletion::Failed(error) => {
                    record.status = "failed";
                    record.result = Some(limit_result(error));
                }
            }
        });
        Ok(())
    }

    pub(crate) fn cancel_queued(&mut self, task_id: u64) -> Option<Box<dyn FormalTaskWork>> {
        let index = self.queued.iter().position(|task| task.id == task_id)?;
        let task = self
            .queued
            .remove(index)
            .expect("queued task index was found");
        self.update_record(task_id, |record| {
            record.status = "canceled";
            record.finished_at_ms = Some(current_unix_millis());
            record.result = Some("任务已在执行前取消".to_string());
        });
        Some(task.work)
    }

    pub(crate) fn try_acquire_lane(
        &mut self,
        lane: SchedulerLane,
    ) -> Result<Option<SchedulerLaneLease>, FormalSchedulerError> {
        if lane == SchedulerLane::Formal {
            return Err(FormalSchedulerError(
                "正式通道只能通过正式任务租约取得".to_string(),
            ));
        }
        if self.active.is_some() || !self.queued.is_empty() {
            return Ok(None);
        }
        let lease = SchedulerLaneLease {
            id: self.next_lane_lease_id,
            lane,
        };
        self.next_lane_lease_id = self.next_lane_lease_id.wrapping_add(1).max(1);
        self.active = Some(ActiveSchedulerWork::External(lease));
        Ok(Some(lease))
    }

    pub(crate) fn release_lane(
        &mut self,
        lease: SchedulerLaneLease,
    ) -> Result<(), FormalSchedulerError> {
        match self.active.as_ref() {
            Some(ActiveSchedulerWork::External(active)) if *active == lease => {
                self.active = None;
                Ok(())
            }
            Some(ActiveSchedulerWork::External(active)) => Err(FormalSchedulerError(format!(
                "调度通道租约不一致: active={active:?} actual={lease:?}"
            ))),
            Some(ActiveSchedulerWork::Formal { id, label, .. }) => Err(FormalSchedulerError(
                format!("正式任务仍在执行，不能释放外部通道: active={id}:{label}"),
            )),
            Some(ActiveSchedulerWork::Diagnostic { id, .. }) => Err(FormalSchedulerError(format!(
                "诊断任务仍在执行，不能释放外部通道: active={id}"
            ))),
            None => Err(FormalSchedulerError(format!(
                "调度通道没有活动租约: actual={lease:?}"
            ))),
        }
    }

    fn require_active(&self, task_id: u64) -> Result<(), FormalSchedulerError> {
        match self.active.as_ref() {
            Some(ActiveSchedulerWork::Formal { id, .. }) if *id == task_id => Ok(()),
            Some(ActiveSchedulerWork::Formal {
                id: active_id,
                label,
                ..
            }) => Err(FormalSchedulerError(format!(
                "正式任务完成顺序不一致: active={active_id}:{label} actual={task_id}"
            ))),
            Some(ActiveSchedulerWork::External(lease)) => Err(FormalSchedulerError(format!(
                "外部调度通道仍在执行，不能完成正式任务: active={lease:?} actual={task_id}"
            ))),
            Some(ActiveSchedulerWork::Diagnostic { id, .. }) => Err(FormalSchedulerError(format!(
                "诊断任务仍在执行，不能完成正式任务: active={id} actual={task_id}"
            ))),
            None => Err(FormalSchedulerError(format!(
                "正式任务没有活动租约: task_id={task_id}"
            ))),
        }
    }

    fn update_record(&mut self, id: u64, update: impl FnOnce(&mut FormalTaskRecord)) {
        let Some(record) = self.records.iter_mut().find(|record| record.id == id) else {
            log::warn!("正式任务追踪记录不存在: id={id}");
            return;
        };
        update(record);
        trim_records(&mut self.records);
    }
}

fn record_snapshot(record: &FormalTaskRecord, now: u64) -> FormalTaskSnapshot {
    let elapsed_ms = record.started_at_ms.map_or(0, |started_at| {
        record
            .finished_at_ms
            .unwrap_or(now)
            .saturating_sub(started_at)
    });
    FormalTaskSnapshot {
        id: record.id,
        label: record.label.clone(),
        status: record.status.to_string(),
        queued_at_ms: record.queued_at_ms,
        started_at_ms: record.started_at_ms,
        finished_at_ms: record.finished_at_ms,
        elapsed_ms,
        result: record.result.clone(),
    }
}

fn trim_records(records: &mut VecDeque<FormalTaskRecord>) {
    while records.len() > MAX_TASK_RECORDS {
        let can_drop = records
            .back()
            .is_some_and(|record| record.status != "queued" && record.status != "running");
        if !can_drop {
            break;
        }
        records.pop_back();
    }
}

fn limit_result(value: String) -> String {
    if value.chars().count() <= MAX_TASK_RESULT_CHARS {
        return value;
    }
    format!(
        "{}\n[任务结果过长，已截断]",
        value
            .chars()
            .take(MAX_TASK_RESULT_CHARS)
            .collect::<String>()
    )
}

fn trim_diagnostic_records(records: &mut VecDeque<DiagnosticTaskRecord>) {
    while records.len() > MAX_DIAGNOSTIC_RECORDS {
        let can_drop = records
            .back()
            .is_some_and(|record| record.status != "queued" && record.status != "running");
        if !can_drop {
            break;
        }
        records.pop_back();
    }
}

fn limit_diagnostic_result(value: String) -> String {
    let char_count = value.chars().count();
    if char_count <= MAX_DIAGNOSTIC_RESULT_CHARS {
        return value;
    }
    format!(
        "{}\n\n[结果过长，已截断：原始字符数={char_count}]",
        value
            .chars()
            .take(MAX_DIAGNOSTIC_RESULT_CHARS)
            .collect::<String>()
    )
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
