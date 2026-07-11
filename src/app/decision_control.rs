use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use serde::Serialize;

#[derive(Clone)]
pub(super) struct DecisionControlShared {
    inner: Arc<(Mutex<DecisionState>, Condvar)>,
}

struct DecisionState {
    next_id: u64,
    current: Option<DecisionRecord>,
}

struct DecisionRecord {
    id: u64,
    label: String,
    allow_switch_source: bool,
    allow_ai: bool,
    deadline_at_ms: u64,
    submitted: Option<DecisionAction>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DecisionAction {
    Confirm,
    Skip,
    SwitchSource,
    Ai,
}

impl DecisionAction {
    pub(super) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "confirm" | "确认" => Some(Self::Confirm),
            "skip" | "跳过" => Some(Self::Skip),
            "switch_source" | "switch-source" | "换源" => Some(Self::SwitchSource),
            "ai" => Some(Self::Ai),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DecisionSnapshot {
    pub(super) id: u64,
    pub(super) label: String,
    pub(super) allow_switch_source: bool,
    pub(super) allow_ai: bool,
    pub(super) deadline_at_ms: u64,
}

pub(super) struct DecisionSession {
    shared: DecisionControlShared,
    id: u64,
}

impl DecisionControlShared {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(DecisionState {
                    next_id: 1,
                    current: None,
                }),
                Condvar::new(),
            )),
        }
    }

    pub(super) fn begin(
        &self,
        label: impl Into<String>,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout: Duration,
    ) -> Result<DecisionSession> {
        let (lock, _) = &*self.inner;
        let mut state = lock.lock().map_err(|_| anyhow!("Web 决策状态锁已损坏"))?;
        if state.current.is_some() {
            bail!("已有等待中的 Web 决策");
        }
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        state.current = Some(DecisionRecord {
            id,
            label: label.into(),
            allow_switch_source,
            allow_ai,
            deadline_at_ms: current_unix_millis().saturating_add(timeout.as_millis() as u64),
            submitted: None,
        });
        Ok(DecisionSession {
            shared: self.clone(),
            id,
        })
    }

    pub(super) fn snapshot(&self) -> Result<Option<DecisionSnapshot>> {
        let (lock, _) = &*self.inner;
        let state = lock.lock().map_err(|_| anyhow!("Web 决策状态锁已损坏"))?;
        let now = current_unix_millis();
        Ok(state
            .current
            .as_ref()
            .filter(|record| now <= record.deadline_at_ms)
            .map(|record| DecisionSnapshot {
                id: record.id,
                label: record.label.clone(),
                allow_switch_source: record.allow_switch_source,
                allow_ai: record.allow_ai,
                deadline_at_ms: record.deadline_at_ms,
            }))
    }

    pub(super) fn submit(&self, id: u64, action: DecisionAction) -> Result<()> {
        let (lock, cvar) = &*self.inner;
        let mut state = lock.lock().map_err(|_| anyhow!("Web 决策状态锁已损坏"))?;
        if state
            .current
            .as_ref()
            .is_some_and(|record| record.id == id && current_unix_millis() > record.deadline_at_ms)
        {
            state.current = None;
            cvar.notify_all();
            bail!("决策已过期");
        }
        let record = state
            .current
            .as_mut()
            .filter(|record| record.id == id)
            .ok_or_else(|| anyhow!("决策已结束、已过期或不存在"))?;
        if action == DecisionAction::SwitchSource && !record.allow_switch_source {
            bail!("当前决策不允许换源");
        }
        if action == DecisionAction::Ai && !record.allow_ai {
            bail!("当前决策不允许切换 AI");
        }
        if record.submitted.is_some() {
            bail!("当前决策已经提交");
        }
        record.submitted = Some(action);
        cvar.notify_all();
        Ok(())
    }

    fn finish(&self, id: u64) {
        let (lock, cvar) = &*self.inner;
        let Ok(mut state) = lock.lock() else {
            log::error!("Web 决策状态锁已损坏，无法结束决策: id={id}");
            return;
        };
        if state.current.as_ref().is_some_and(|record| record.id == id) {
            state.current = None;
            cvar.notify_all();
        }
    }
}

impl DecisionSession {
    pub(super) fn wait(&self, timeout: Duration) -> Result<Option<DecisionAction>> {
        let (lock, cvar) = &*self.shared.inner;
        let state = lock.lock().map_err(|_| anyhow!("Web 决策状态锁已损坏"))?;
        let (mut state, _) = cvar
            .wait_timeout_while(state, timeout, |state| {
                state
                    .current
                    .as_ref()
                    .is_some_and(|record| record.id == self.id && record.submitted.is_none())
            })
            .map_err(|_| anyhow!("Web 决策等待状态已损坏"))?;
        Ok(state
            .current
            .as_mut()
            .filter(|record| record.id == self.id)
            .and_then(|record| record.submitted.take()))
    }
}

impl Drop for DecisionSession {
    fn drop(&mut self) {
        self.shared.finish(self.id);
    }
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
    fn submits_allowed_decision_to_active_session() {
        let shared = DecisionControlShared::new();
        let session = shared
            .begin("候选确认", true, false, Duration::from_secs(1))
            .unwrap();
        let id = shared.snapshot().unwrap().unwrap().id;
        shared.submit(id, DecisionAction::SwitchSource).unwrap();

        assert_eq!(
            session.wait(Duration::from_millis(1)).unwrap(),
            Some(DecisionAction::SwitchSource)
        );
    }

    #[test]
    fn rejects_decision_after_deadline() {
        let shared = DecisionControlShared::new();
        let _session = shared
            .begin("候选确认", false, false, Duration::from_millis(0))
            .unwrap();
        let id = {
            let (lock, _) = &*shared.inner;
            let mut state = lock.lock().unwrap();
            let record = state.current.as_mut().unwrap();
            record.deadline_at_ms = current_unix_millis().saturating_sub(1);
            record.id
        };

        assert!(shared.submit(id, DecisionAction::Confirm).is_err());
        assert!(shared.snapshot().unwrap().is_none());
    }
}
