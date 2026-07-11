use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use serde::Serialize;

const MAX_TASK_RECORDS: usize = 60;
const MAX_TASK_RESULT_CHARS: usize = 4 * 1024;

#[derive(Clone)]
pub(super) struct TaskTrackerShared {
    inner: Arc<Mutex<TaskTrackerState>>,
}

struct TaskTrackerState {
    next_id: u64,
    records: VecDeque<TaskRecord>,
}

#[derive(Clone, Debug)]
struct TaskRecord {
    id: u64,
    label: String,
    status: &'static str,
    queued_at_ms: u64,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
    result: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskSnapshot {
    pub(super) id: u64,
    pub(super) label: String,
    pub(super) status: String,
    pub(super) queued_at_ms: u64,
    pub(super) started_at_ms: Option<u64>,
    pub(super) finished_at_ms: Option<u64>,
    pub(super) elapsed_ms: u64,
    pub(super) result: Option<String>,
}

impl TaskTrackerShared {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TaskTrackerState {
                next_id: 1,
                records: VecDeque::new(),
            })),
        }
    }

    pub(super) fn create(&self, label: String) -> Result<u64> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| anyhow!("正式任务追踪锁已损坏"))?;
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        state.records.push_front(TaskRecord {
            id,
            label,
            status: "queued",
            queued_at_ms: current_unix_millis(),
            started_at_ms: None,
            finished_at_ms: None,
            result: None,
        });
        trim_records(&mut state.records);
        Ok(id)
    }

    pub(super) fn mark_running(&self, id: u64) {
        self.update_record(id, |record| {
            record.status = "running";
            record.started_at_ms = Some(current_unix_millis());
            record.finished_at_ms = None;
            record.result = None;
        });
    }

    pub(super) fn mark_queued(&self, id: u64) {
        self.update_record(id, |record| {
            record.status = "queued";
            record.started_at_ms = None;
            record.finished_at_ms = None;
            record.result = None;
        });
    }

    pub(super) fn finish_ok(&self, id: u64, result: impl Into<String>) {
        let result = limit_result(result.into());
        self.update_record(id, |record| {
            record.status = "completed";
            record.finished_at_ms = Some(current_unix_millis());
            record.result = Some(result);
        });
    }

    pub(super) fn finish_error(&self, id: u64, error: &anyhow::Error) {
        let result = limit_result(format!("错误: {error:#}"));
        self.update_record(id, |record| {
            record.status = "failed";
            record.finished_at_ms = Some(current_unix_millis());
            record.result = Some(result);
        });
    }

    pub(super) fn cancel(&self, id: u64, result: impl Into<String>) {
        let result = limit_result(result.into());
        self.update_record(id, |record| {
            record.status = "canceled";
            record.finished_at_ms = Some(current_unix_millis());
            record.result = Some(result);
        });
    }

    pub(super) fn recent(&self) -> Result<Vec<TaskSnapshot>> {
        let state = self
            .inner
            .lock()
            .map_err(|_| anyhow!("正式任务追踪锁已损坏"))?;
        let now = current_unix_millis();
        Ok(state
            .records
            .iter()
            .take(30)
            .map(|record| record_snapshot(record, now))
            .collect())
    }

    fn update_record(&self, id: u64, update: impl FnOnce(&mut TaskRecord)) {
        let Ok(mut state) = self.inner.lock() else {
            log::error!("正式任务追踪锁已损坏，无法更新任务: id={id}");
            return;
        };
        let Some(record) = state.records.iter_mut().find(|record| record.id == id) else {
            log::warn!("正式任务追踪记录不存在: id={id}");
            return;
        };
        update(record);
        trim_records(&mut state.records);
    }
}

fn record_snapshot(record: &TaskRecord, now: u64) -> TaskSnapshot {
    let elapsed_ms = record.started_at_ms.map_or(0, |started_at| {
        record
            .finished_at_ms
            .unwrap_or(now)
            .saturating_sub(started_at)
    });
    TaskSnapshot {
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

fn trim_records(records: &mut VecDeque<TaskRecord>) {
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

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_task_lifecycle() {
        let tracker = TaskTrackerShared::new();
        let id = tracker.create("测试任务".to_string()).unwrap();
        tracker.mark_running(id);
        tracker.finish_ok(id, "完成");

        let tasks = tracker.recent().unwrap();
        assert_eq!(tasks[0].id, id);
        assert_eq!(tasks[0].status, "completed");
        assert_eq!(tasks[0].result.as_deref(), Some("完成"));
    }
}
