use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecisionAction {
    Confirm,
    Skip,
    SwitchSource,
    Ai,
}

impl DecisionAction {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "confirm" | "确认" => Some(Self::Confirm),
            "skip" | "跳过" => Some(Self::Skip),
            "switch_source" | "switch-source" | "换源" => Some(Self::SwitchSource),
            "ai" => Some(Self::Ai),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DecisionSnapshot {
    pub(crate) id: u64,
    pub(crate) label: String,
    pub(crate) allow_switch_source: bool,
    pub(crate) allow_ai: bool,
    pub(crate) deadline_at_ms: u64,
}

struct DecisionRecord {
    id: u64,
    label: String,
    allow_switch_source: bool,
    allow_ai: bool,
    deadline_at_ms: u64,
    submitted: bool,
    delivery: SyncSender<DecisionAction>,
}

pub(crate) struct DecisionState {
    next_id: u64,
    current: Option<DecisionRecord>,
}

impl DecisionState {
    pub(crate) const fn new() -> Self {
        Self {
            next_id: 1,
            current: None,
        }
    }

    pub(crate) fn begin(
        &mut self,
        label: String,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout: Duration,
        delivery: SyncSender<DecisionAction>,
    ) -> Result<u64, String> {
        self.expire_current();
        if self.current.is_some() {
            return Err("已有等待中的 Web 决策".to_string());
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.current = Some(DecisionRecord {
            id,
            label,
            allow_switch_source,
            allow_ai,
            deadline_at_ms: current_unix_millis().saturating_add(timeout.as_millis() as u64),
            submitted: false,
            delivery,
        });
        Ok(id)
    }

    pub(crate) fn snapshot(&mut self) -> Option<DecisionSnapshot> {
        self.expire_current();
        self.current.as_ref().map(|record| DecisionSnapshot {
            id: record.id,
            label: record.label.clone(),
            allow_switch_source: record.allow_switch_source,
            allow_ai: record.allow_ai,
            deadline_at_ms: record.deadline_at_ms,
        })
    }

    pub(crate) fn submit(&mut self, id: u64, action: DecisionAction) -> Result<(), String> {
        self.expire_current();
        let record = self
            .current
            .as_mut()
            .filter(|record| record.id == id)
            .ok_or_else(|| "决策已结束、已过期或不存在".to_string())?;
        if action == DecisionAction::SwitchSource && !record.allow_switch_source {
            return Err("当前决策不允许换源".to_string());
        }
        if action == DecisionAction::Ai && !record.allow_ai {
            return Err("当前决策不允许切换 AI".to_string());
        }
        if record.submitted {
            return Err("当前决策已经提交".to_string());
        }
        match record.delivery.try_send(action) {
            Ok(()) => {
                record.submitted = true;
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err("当前决策已经提交".to_string()),
            Err(TrySendError::Disconnected(_)) => {
                self.current = None;
                Err("决策已结束、已过期或不存在".to_string())
            }
        }
    }

    pub(crate) fn finish(&mut self, id: u64) {
        if self.current.as_ref().is_some_and(|record| record.id == id) {
            self.current = None;
        }
    }

    fn expire_current(&mut self) {
        if self
            .current
            .as_ref()
            .is_some_and(|record| current_unix_millis() > record.deadline_at_ms)
        {
            self.current = None;
        }
    }
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
