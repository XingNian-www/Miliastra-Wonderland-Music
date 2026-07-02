use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OcrSnapshot {
    pub(super) markers: usize,
    pub(super) messages: Vec<String>,
    pub(super) marker_ms: u128,
    pub(super) ocr_ms: u128,
    pub(super) total_ms: u128,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MonitorQueueItem {
    pub(super) keyword: String,
    pub(super) source: String,
    pub(super) prefer_accompaniment: bool,
    pub(super) friend_username: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MonitorSnapshot {
    pub(super) logs: Vec<String>,
    pub(super) ocr: Option<OcrSnapshot>,
    pub(super) queue: Vec<MonitorQueueItem>,
    pub(super) commands: Vec<String>,
    pub(super) status: String,
}

#[derive(Clone)]
pub(super) struct MonitorShared {
    state: Arc<Mutex<MonitorState>>,
}

#[derive(Clone)]
pub(super) struct MonitorLogSink {
    shared: MonitorShared,
}

#[derive(Debug)]
struct MonitorState {
    logs: VecDeque<String>,
    log_limit: usize,
    ocr: Option<OcrSnapshot>,
    queue: Vec<MonitorQueueItem>,
    commands: VecDeque<String>,
    status: String,
}

impl MonitorShared {
    pub(super) fn new(log_limit: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(MonitorState {
                logs: VecDeque::new(),
                log_limit: log_limit.max(20),
                ocr: None,
                queue: Vec::new(),
                commands: VecDeque::new(),
                status: "启动中".to_string(),
            })),
        }
    }

    pub(super) fn log_sink(&self) -> MonitorLogSink {
        MonitorLogSink {
            shared: self.clone(),
        }
    }

    pub(super) fn push_log(&self, line: String) {
        if let Ok(mut state) = self.state.lock() {
            let mut pushed = false;
            for part in line.lines() {
                state.logs.push_back(part.to_string());
                pushed = true;
                while state.logs.len() > state.log_limit {
                    state.logs.pop_front();
                }
            }
            if !pushed {
                state.logs.push_back(String::new());
                while state.logs.len() > state.log_limit {
                    state.logs.pop_front();
                }
            }
        }
    }

    pub(super) fn set_ocr(&self, snapshot: OcrSnapshot) {
        if let Ok(mut state) = self.state.lock() {
            state.ocr = Some(snapshot);
        }
    }

    pub(super) fn set_queue(&self, queue: Vec<MonitorQueueItem>) {
        if let Ok(mut state) = self.state.lock() {
            state.queue = queue;
        }
    }

    pub(super) fn push_command(&self, command: String) {
        if let Ok(mut state) = self.state.lock() {
            state.commands.push_back(command);
            while state.commands.len() > 20 {
                state.commands.pop_front();
            }
        }
    }

    pub(super) fn set_status(&self, status: impl Into<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.status = status.into();
        }
    }

    pub(super) fn snapshot(&self) -> MonitorSnapshot {
        self.state.lock().map_or_else(
            |_| MonitorSnapshot {
                logs: Vec::new(),
                ocr: None,
                queue: Vec::new(),
                commands: Vec::new(),
                status: "监控状态不可用".to_string(),
            },
            |state| MonitorSnapshot {
                logs: state.logs.iter().cloned().collect(),
                ocr: state.ocr.clone(),
                queue: state.queue.clone(),
                commands: state.commands.iter().cloned().collect(),
                status: state.status.clone(),
            },
        )
    }
}

impl MonitorLogSink {
    pub(super) fn push(&self, line: String) {
        self.shared.push_log(line);
    }
}
