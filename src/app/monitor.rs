use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct OcrSnapshot {
    pub(super) markers: usize,
    pub(super) messages: Vec<String>,
    pub(super) marker_ms: u128,
    pub(super) ocr_ms: u128,
    pub(super) total_ms: u128,
    pub(super) source: String,
    pub(super) captured_at_ms: u64,
}

impl OcrSnapshot {
    pub(super) fn new(
        markers: usize,
        messages: Vec<String>,
        marker_ms: u128,
        ocr_ms: u128,
        total_ms: u128,
        source: impl Into<String>,
    ) -> Self {
        Self {
            markers,
            messages,
            marker_ms,
            ocr_ms,
            total_ms,
            source: source.into(),
            captured_at_ms: current_unix_millis(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MonitorQueueItem {
    pub(super) id: u64,
    pub(super) keyword: String,
    pub(super) source: String,
    pub(super) prefer_accompaniment: bool,
    pub(super) friend_username: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MonitorPlaybackController {
    pub(super) state: String,
    pub(super) pause_reason: String,
    pub(super) active_keyword: String,
    pub(super) active_uri: String,
    pub(super) last_observation_reliability: String,
    pub(super) backend_status: String,
    pub(super) current_uri: String,
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) progress: f64,
    pub(super) duration: f64,
    pub(super) observed_at_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MonitorChatListener {
    pub(super) mode: String,
    pub(super) pending_mode: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MonitorOperationalState {
    pub(super) ui_state: String,
    pub(super) scanner_paused: bool,
    pub(super) commands_enabled: bool,
    pub(super) idle_exit_remaining_seconds: Option<u64>,
    pub(super) hall_remaining_minutes: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MonitorSnapshot {
    pub(super) logs: Vec<String>,
    pub(super) ocr: Option<OcrSnapshot>,
    pub(super) queue: Vec<MonitorQueueItem>,
    pub(super) commands: Vec<String>,
    pub(super) status: String,
    pub(super) playback_controller: MonitorPlaybackController,
    pub(super) chat_listener: MonitorChatListener,
    pub(super) operational: MonitorOperationalState,
}

#[derive(Clone)]
pub(crate) struct MonitorShared {
    state: Arc<Mutex<MonitorState>>,
}

#[derive(Clone)]
pub(crate) struct MonitorLogSink {
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
    playback_controller: MonitorPlaybackController,
    chat_listener: MonitorChatListener,
    operational: MonitorOperationalState,
}

impl MonitorShared {
    pub(crate) fn new(log_limit: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(MonitorState {
                logs: VecDeque::new(),
                log_limit: log_limit.max(20),
                ocr: None,
                queue: Vec::new(),
                commands: VecDeque::new(),
                status: "启动中".to_string(),
                playback_controller: MonitorPlaybackController::default(),
                chat_listener: MonitorChatListener::default(),
                operational: MonitorOperationalState {
                    commands_enabled: true,
                    ..MonitorOperationalState::default()
                },
            })),
        }
    }

    pub(crate) fn log_sink(&self) -> MonitorLogSink {
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

    pub(super) fn set_playback_controller(&self, snapshot: MonitorPlaybackController) {
        if let Ok(mut state) = self.state.lock() {
            state.playback_controller = snapshot;
        }
    }

    pub(super) fn set_chat_listener(&self, mode: impl Into<String>, pending_mode: Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.chat_listener = MonitorChatListener {
                mode: mode.into(),
                pending_mode: pending_mode.unwrap_or_default(),
            };
        }
    }

    pub(super) fn set_operational(
        &self,
        scanner_paused: bool,
        commands_enabled: bool,
        idle_exit_remaining_seconds: Option<u64>,
        hall_remaining_minutes: Option<u32>,
    ) {
        if let Ok(mut state) = self.state.lock() {
            state.operational.scanner_paused = scanner_paused;
            state.operational.commands_enabled = commands_enabled;
            state.operational.idle_exit_remaining_seconds = idle_exit_remaining_seconds;
            state.operational.hall_remaining_minutes = hall_remaining_minutes;
        }
    }

    pub(super) fn set_ui_state(&self, ui_state: impl Into<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.operational.ui_state = ui_state.into();
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
                playback_controller: MonitorPlaybackController::default(),
                chat_listener: MonitorChatListener::default(),
                operational: MonitorOperationalState::default(),
            },
            |state| MonitorSnapshot {
                logs: state.logs.iter().cloned().collect(),
                ocr: state.ocr.clone(),
                queue: state.queue.clone(),
                commands: state.commands.iter().cloned().collect(),
                status: state.status.clone(),
                playback_controller: state.playback_controller.clone(),
                chat_listener: state.chat_listener.clone(),
                operational: state.operational.clone(),
            },
        )
    }
}

impl MonitorLogSink {
    pub(super) fn push(&self, line: String) {
        self.shared.push_log(line);
    }
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
