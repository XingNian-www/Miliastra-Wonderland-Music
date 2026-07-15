use std::collections::VecDeque;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::features::turtle_soup::TurtleSoupSnapshot;
use crate::features::undercover::UndercoverSnapshot;
use crate::runtime::business::BusinessStateSink;

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
    pub(super) turtle_soup: TurtleSoupSnapshot,
    pub(super) undercover: UndercoverSnapshot,
}

#[derive(Debug)]
pub(super) enum MonitorEvent {
    Log(String),
    Ocr(OcrSnapshot),
    Queue(Vec<MonitorQueueItem>),
    Command(String),
    Status(String),
    PlaybackController(MonitorPlaybackController),
    ChatListener {
        mode: String,
        pending_mode: String,
    },
    Operational {
        scanner_paused: bool,
        commands_enabled: bool,
        idle_exit_remaining_seconds: Option<u64>,
        hall_remaining_minutes: Option<u32>,
    },
    UiState(String),
    TurtleSoup(TurtleSoupSnapshot),
    Undercover(UndercoverSnapshot),
}

enum MonitorMessage {
    Event {
        event: Box<MonitorEvent>,
        applied: SyncSender<()>,
    },
    Snapshot(SyncSender<MonitorSnapshot>),
    Shutdown(SyncSender<()>),
}

struct MonitorRuntime {
    events: Sender<MonitorMessage>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Clone)]
pub(crate) struct MonitorShared {
    runtime: Arc<MonitorRuntime>,
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
    turtle_soup: TurtleSoupSnapshot,
    undercover: UndercoverSnapshot,
}

struct MonitorProjection {
    state: MonitorState,
}

impl MonitorProjection {
    fn apply(&mut self, event: MonitorEvent) {
        let state = &mut self.state;
        match event {
            MonitorEvent::Log(line) => {
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
            MonitorEvent::Ocr(snapshot) => state.ocr = Some(snapshot),
            MonitorEvent::Queue(queue) => state.queue = queue,
            MonitorEvent::Command(command) => {
                state.commands.push_back(command);
                while state.commands.len() > 20 {
                    state.commands.pop_front();
                }
            }
            MonitorEvent::Status(status) => state.status = status,
            MonitorEvent::PlaybackController(snapshot) => state.playback_controller = snapshot,
            MonitorEvent::ChatListener { mode, pending_mode } => {
                state.chat_listener = MonitorChatListener { mode, pending_mode };
            }
            MonitorEvent::Operational {
                scanner_paused,
                commands_enabled,
                idle_exit_remaining_seconds,
                hall_remaining_minutes,
            } => {
                state.operational.scanner_paused = scanner_paused;
                state.operational.commands_enabled = commands_enabled;
                state.operational.idle_exit_remaining_seconds = idle_exit_remaining_seconds;
                state.operational.hall_remaining_minutes = hall_remaining_minutes;
            }
            MonitorEvent::UiState(ui_state) => state.operational.ui_state = ui_state,
            MonitorEvent::TurtleSoup(snapshot) => state.turtle_soup = snapshot,
            MonitorEvent::Undercover(snapshot) => state.undercover = snapshot,
        }
    }

    fn snapshot(&self) -> MonitorSnapshot {
        let state = &self.state;
        MonitorSnapshot {
            logs: state.logs.iter().cloned().collect(),
            ocr: state.ocr.clone(),
            queue: state.queue.clone(),
            commands: state.commands.iter().cloned().collect(),
            status: state.status.clone(),
            playback_controller: state.playback_controller.clone(),
            chat_listener: state.chat_listener.clone(),
            operational: state.operational.clone(),
            turtle_soup: state.turtle_soup.clone(),
            undercover: state.undercover.clone(),
        }
    }
}

impl MonitorShared {
    pub(crate) fn new(log_limit: usize) -> Self {
        let mut projection = MonitorProjection {
            state: MonitorState {
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
                turtle_soup: TurtleSoupSnapshot::default(),
                undercover: UndercoverSnapshot::default(),
            },
        };
        let (events, receiver) = mpsc::channel::<MonitorMessage>();
        let worker = thread::Builder::new()
            .name("monitor-projection".to_string())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        MonitorMessage::Event { event, applied } => {
                            projection.apply(*event);
                            let _ = applied.send(());
                        }
                        MonitorMessage::Snapshot(response) => {
                            let _ = response.send(projection.snapshot());
                        }
                        MonitorMessage::Shutdown(response) => {
                            let _ = response.send(());
                            break;
                        }
                    }
                }
            })
            .expect("启动监控投影线程失败");
        Self {
            runtime: Arc::new(MonitorRuntime {
                events,
                worker: Mutex::new(Some(worker)),
            }),
        }
    }

    pub(crate) fn log_sink(&self) -> MonitorLogSink {
        MonitorLogSink {
            shared: self.clone(),
        }
    }

    pub(super) fn publish(&self, event: MonitorEvent) {
        let (applied, wait) = mpsc::sync_channel(0);
        if self
            .runtime
            .events
            .send(MonitorMessage::Event {
                event: Box::new(event),
                applied,
            })
            .is_ok()
        {
            let _ = wait.recv();
        }
    }

    pub(super) fn publish_async(&self, event: MonitorEvent) {
        let (applied, _wait) = mpsc::sync_channel(0);
        let _ = self.runtime.events.send(MonitorMessage::Event {
            event: Box::new(event),
            applied,
        });
    }

    pub(super) fn snapshot(&self) -> MonitorSnapshot {
        let (response, receiver) = mpsc::sync_channel(1);
        if self
            .runtime
            .events
            .send(MonitorMessage::Snapshot(response))
            .is_err()
        {
            return unavailable_snapshot();
        }
        receiver.recv().unwrap_or_else(|_| unavailable_snapshot())
    }

    pub(crate) fn shutdown(&self) {
        self.runtime.shutdown();
    }
}

impl MonitorRuntime {
    fn shutdown(&self) {
        let worker = self.worker.lock().ok().and_then(|mut worker| worker.take());
        let Some(worker) = worker else {
            return;
        };
        let (ack, done) = mpsc::sync_channel(0);
        if self.runtime_send(MonitorMessage::Shutdown(ack)).is_ok() {
            let _ = done.recv();
        }
        if worker.join().is_err() {
            log::error!("监控投影线程退出时 panic");
        }
    }

    fn runtime_send(&self, message: MonitorMessage) -> std::result::Result<(), ()> {
        self.events.send(message).map_err(|_| ())
    }
}

impl Drop for MonitorRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl BusinessStateSink for MonitorShared {
    fn publish_turtle_soup(&self, snapshot: TurtleSoupSnapshot) {
        self.publish_async(MonitorEvent::TurtleSoup(snapshot));
    }

    fn publish_undercover(&self, snapshot: UndercoverSnapshot) {
        self.publish_async(MonitorEvent::Undercover(snapshot));
    }
}

fn unavailable_snapshot() -> MonitorSnapshot {
    MonitorSnapshot {
        logs: Vec::new(),
        ocr: None,
        queue: Vec::new(),
        commands: Vec::new(),
        status: "监控状态不可用".to_string(),
        playback_controller: MonitorPlaybackController::default(),
        chat_listener: MonitorChatListener::default(),
        operational: MonitorOperationalState::default(),
        turtle_soup: TurtleSoupSnapshot::default(),
        undercover: UndercoverSnapshot::default(),
    }
}

impl MonitorLogSink {
    pub(super) fn push(&self, line: String) {
        self.shared.publish(MonitorEvent::Log(line));
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
    fn typed_events_update_the_single_monitor_projection() {
        let monitor = MonitorShared::new(20);
        monitor.publish(MonitorEvent::Queue(vec![MonitorQueueItem {
            id: 9,
            keyword: "晴天".to_string(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
            friend_username: String::new(),
        }]));
        monitor.publish(MonitorEvent::ChatListener {
            mode: "二级".to_string(),
            pending_mode: "一级".to_string(),
        });
        monitor.publish(MonitorEvent::Operational {
            scanner_paused: true,
            commands_enabled: false,
            idle_exit_remaining_seconds: Some(12),
            hall_remaining_minutes: Some(8),
        });
        monitor.publish(MonitorEvent::UiState("secondary:chat".to_string()));
        monitor.publish(MonitorEvent::TurtleSoup(TurtleSoupSnapshot {
            enabled: true,
            ..TurtleSoupSnapshot::default()
        }));
        monitor.publish(MonitorEvent::Undercover(UndercoverSnapshot {
            enabled: true,
            phase: "lobby",
            ..UndercoverSnapshot::default()
        }));

        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.queue.len(), 1);
        assert_eq!(snapshot.queue[0].id, 9);
        assert_eq!(snapshot.chat_listener.mode, "二级");
        assert_eq!(snapshot.chat_listener.pending_mode, "一级");
        assert!(snapshot.operational.scanner_paused);
        assert!(!snapshot.operational.commands_enabled);
        assert_eq!(snapshot.operational.idle_exit_remaining_seconds, Some(12));
        assert_eq!(snapshot.operational.hall_remaining_minutes, Some(8));
        assert_eq!(snapshot.operational.ui_state, "secondary:chat");
        assert!(snapshot.turtle_soup.enabled);
        assert!(snapshot.undercover.enabled);
        monitor.shutdown();
    }
}
