use std::collections::VecDeque;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::features::playback::PlaybackControllerSnapshot as MonitorPlaybackController;
use crate::features::turtle_soup::TurtleSoupSnapshot;
use crate::features::undercover::UndercoverSnapshot;
use crate::runtime::business::{BusinessOperationalSnapshot, BusinessStateSink};
use crate::runtime::decision::DecisionSnapshot;
use crate::runtime::scheduler::{DiagnosticTaskSnapshot, FormalTaskSnapshot};
use crate::runtime::ui::{UiRoutineProgress, UiRoutineProgressSink, UiRoutineProgressStage};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OcrSnapshot {
    pub(crate) markers: usize,
    pub(crate) messages: Vec<String>,
    pub(crate) marker_ms: u128,
    pub(crate) ocr_ms: u128,
    pub(crate) total_ms: u128,
    pub(crate) source: String,
    pub(crate) captured_at_ms: u64,
}

impl OcrSnapshot {
    pub(crate) fn new(
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
pub(crate) struct MonitorQueueItem {
    pub(crate) id: u64,
    pub(crate) keyword: String,
    pub(crate) source: String,
    pub(crate) prefer_accompaniment: bool,
    pub(crate) friend_username: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitorChatListener {
    pub(crate) mode: String,
    pub(crate) pending_mode: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitorOperationalState {
    pub(crate) ui_state: String,
    pub(crate) scanner_paused: bool,
    pub(crate) commands_enabled: bool,
    pub(crate) idle_exit_remaining_seconds: Option<u64>,
    pub(crate) hall_remaining_minutes: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitorUiRoutineProgress {
    pub(crate) operation_id: u64,
    pub(crate) stage: &'static str,
    pub(crate) recipient_index: Option<usize>,
    pub(crate) recipient_count: Option<usize>,
    pub(crate) message_index: Option<usize>,
    pub(crate) message_count: Option<usize>,
    pub(crate) action_index: Option<usize>,
    pub(crate) action_count: Option<usize>,
}

impl From<UiRoutineProgress> for MonitorUiRoutineProgress {
    fn from(progress: UiRoutineProgress) -> Self {
        let mut snapshot = Self {
            operation_id: progress.operation_id().get(),
            stage: "unknown",
            recipient_index: None,
            recipient_count: None,
            message_index: None,
            message_count: None,
            action_index: None,
            action_count: None,
        };
        match progress.stage() {
            UiRoutineProgressStage::NormalizingStart => snapshot.stage = "normalizing_start",
            UiRoutineProgressStage::LocatingFriend {
                recipient_index,
                recipient_count,
            } => {
                snapshot.stage = "locating_friend";
                snapshot.recipient_index = Some(*recipient_index);
                snapshot.recipient_count = Some(*recipient_count);
            }
            UiRoutineProgressStage::SendingFriendMessage {
                recipient_index,
                recipient_count,
                message_index,
                message_count,
            } => {
                snapshot.stage = "sending_friend_message";
                snapshot.recipient_index = Some(*recipient_index);
                snapshot.recipient_count = Some(*recipient_count);
                snapshot.message_index = Some(*message_index);
                snapshot.message_count = Some(*message_count);
            }
            UiRoutineProgressStage::ExecutingCustomAction {
                operation_index,
                operation_count,
            } => {
                snapshot.stage = "executing_custom_action";
                snapshot.action_index = Some(*operation_index);
                snapshot.action_count = Some(*operation_count);
            }
            UiRoutineProgressStage::ConfirmingUi => snapshot.stage = "confirming_ui",
            UiRoutineProgressStage::RecoveringResidency => snapshot.stage = "recovering_residency",
        }
        snapshot
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitorSnapshot {
    pub(crate) logs: Vec<String>,
    pub(crate) ocr: Option<OcrSnapshot>,
    pub(crate) queue: Vec<MonitorQueueItem>,
    pub(crate) commands: Vec<String>,
    pub(crate) status: String,
    pub(crate) playback_controller: MonitorPlaybackController,
    pub(crate) chat_listener: MonitorChatListener,
    pub(crate) operational: MonitorOperationalState,
    pub(crate) ui_routine: Option<MonitorUiRoutineProgress>,
    pub(crate) pending_tasks: Vec<String>,
    pub(crate) web_tools: Vec<DiagnosticTaskSnapshot>,
    pub(crate) tasks: Vec<FormalTaskSnapshot>,
    pub(crate) decision: Option<DecisionSnapshot>,
    pub(crate) turtle_soup: TurtleSoupSnapshot,
    pub(crate) undercover: UndercoverSnapshot,
}

#[derive(Debug)]
pub(crate) enum MonitorEvent {
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
    ScannerPaused(bool),
    BusinessOperational(BusinessOperationalSnapshot),
    UiState(String),
    UiRoutineProgress(MonitorUiRoutineProgress),
    Scheduler {
        pending_tasks: Vec<String>,
        tasks: Vec<FormalTaskSnapshot>,
    },
    Diagnostics(Vec<DiagnosticTaskSnapshot>),
    Decision(Option<DecisionSnapshot>),
    TurtleSoup(TurtleSoupSnapshot),
    Undercover(UndercoverSnapshot),
    HallRemainingMinutes(Option<u32>),
}

enum MonitorMessage {
    Event(Box<MonitorEvent>),
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
    ui_routine: Option<MonitorUiRoutineProgress>,
    pending_tasks: Vec<String>,
    web_tools: Vec<DiagnosticTaskSnapshot>,
    tasks: Vec<FormalTaskSnapshot>,
    decision: Option<DecisionSnapshot>,
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
            MonitorEvent::ScannerPaused(paused) => state.operational.scanner_paused = paused,
            MonitorEvent::BusinessOperational(snapshot) => {
                state.operational.commands_enabled = snapshot.commands_enabled();
                state.operational.idle_exit_remaining_seconds =
                    snapshot.idle_exit_remaining_seconds();
            }
            MonitorEvent::UiState(ui_state) => state.operational.ui_state = ui_state,
            MonitorEvent::UiRoutineProgress(progress) => state.ui_routine = Some(progress),
            MonitorEvent::Scheduler {
                pending_tasks,
                tasks,
            } => {
                state.pending_tasks = pending_tasks;
                state.tasks = tasks;
            }
            MonitorEvent::Diagnostics(web_tools) => state.web_tools = web_tools,
            MonitorEvent::Decision(decision) => state.decision = decision,
            MonitorEvent::TurtleSoup(snapshot) => state.turtle_soup = snapshot,
            MonitorEvent::Undercover(snapshot) => state.undercover = snapshot,
            MonitorEvent::HallRemainingMinutes(minutes) => {
                state.operational.hall_remaining_minutes = minutes;
            }
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
            ui_routine: state.ui_routine.clone(),
            pending_tasks: state.pending_tasks.clone(),
            web_tools: state.web_tools.clone(),
            tasks: state.tasks.clone(),
            decision: state.decision.clone(),
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
                ui_routine: None,
                pending_tasks: Vec::new(),
                web_tools: Vec::new(),
                tasks: Vec::new(),
                decision: None,
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
                        MonitorMessage::Event(event) => projection.apply(*event),
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

    pub(crate) fn publish(&self, event: MonitorEvent) {
        self.publish_async(event);
    }

    pub(crate) fn publish_async(&self, event: MonitorEvent) {
        let _ = self
            .runtime
            .events
            .send(MonitorMessage::Event(Box::new(event)));
    }

    pub(crate) fn snapshot(&self) -> MonitorSnapshot {
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

    fn publish_playback_queue(&self, queue: Vec<crate::features::playback::QueueItem>) {
        self.publish_async(MonitorEvent::Queue(
            queue
                .into_iter()
                .map(|item| MonitorQueueItem {
                    id: item.id,
                    keyword: item.keyword,
                    source: item.source,
                    prefer_accompaniment: item.prefer_accompaniment,
                    friend_username: item.friend_username,
                })
                .collect(),
        ));
    }

    fn publish_hall_remaining_minutes(&self, minutes: Option<u32>) {
        self.publish_async(MonitorEvent::HallRemainingMinutes(minutes));
    }

    fn publish_scheduler(&self, snapshot: crate::runtime::scheduler::FormalSchedulerSnapshot) {
        self.publish_async(MonitorEvent::Scheduler {
            pending_tasks: snapshot.pending_labels().to_vec(),
            tasks: snapshot.tasks().to_vec(),
        });
    }

    fn publish_chat_listener(&self, snapshot: crate::runtime::chat_listener::ChatListenerSnapshot) {
        self.publish_async(MonitorEvent::ChatListener {
            mode: snapshot.display_mode(),
            pending_mode: snapshot
                .pending_mode
                .map(|mode| mode.label().to_string())
                .unwrap_or_default(),
        });
    }

    fn publish_decision(&self, snapshot: Option<DecisionSnapshot>) {
        self.publish_async(MonitorEvent::Decision(snapshot));
    }

    fn publish_operational(&self, snapshot: BusinessOperationalSnapshot) {
        self.publish_async(MonitorEvent::BusinessOperational(snapshot));
    }

    fn publish_diagnostics(&self, snapshot: Vec<DiagnosticTaskSnapshot>) {
        self.publish_async(MonitorEvent::Diagnostics(snapshot));
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
        ui_routine: None,
        pending_tasks: Vec::new(),
        web_tools: Vec::new(),
        tasks: Vec::new(),
        decision: None,
        turtle_soup: TurtleSoupSnapshot::default(),
        undercover: UndercoverSnapshot::default(),
    }
}

impl MonitorLogSink {
    pub(crate) fn push(&self, line: String) {
        self.shared.publish(MonitorEvent::Log(line));
    }
}

impl UiRoutineProgressSink for MonitorShared {
    fn publish(&self, progress: UiRoutineProgress) {
        self.publish_async(MonitorEvent::UiRoutineProgress(progress.into()));
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
    use anyhow::{Result, bail};
    use image::DynamicImage;

    use super::*;
    use crate::runtime::ui::{
        UiDevice, UiRoutine, UiRoutineContext, UiRoutineProgressStage, UiRuntime, sealed,
    };

    struct ProgressRoutine;

    impl sealed::UiRoutineSealed for ProgressRoutine {}

    impl UiRoutine for ProgressRoutine {
        type Output = ();

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            context.publish_progress(UiRoutineProgressStage::SendingFriendMessage {
                recipient_index: 2,
                recipient_count: 3,
                message_index: 1,
                message_count: 2,
            });
        }
    }

    struct UnusedDevice;

    impl UiDevice for UnusedDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            bail!("progress routine should not capture")
        }
    }

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
        monitor.publish(MonitorEvent::ScannerPaused(true));
        BusinessStateSink::publish_operational(
            &monitor,
            BusinessOperationalSnapshot::new(false, Some(12)),
        );
        BusinessStateSink::publish_hall_remaining_minutes(&monitor, Some(8));
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

    #[test]
    fn hall_state_event_updates_only_the_owned_projection_field() {
        let monitor = MonitorShared::new(20);
        monitor.publish(MonitorEvent::ScannerPaused(true));
        BusinessStateSink::publish_operational(
            &monitor,
            BusinessOperationalSnapshot::new(false, Some(12)),
        );

        BusinessStateSink::publish_hall_remaining_minutes(&monitor, Some(9));
        let snapshot = monitor.snapshot();

        assert_eq!(snapshot.operational.hall_remaining_minutes, Some(9));
        assert!(snapshot.operational.scanner_paused);
        assert!(!snapshot.operational.commands_enabled);
        assert_eq!(snapshot.operational.idle_exit_remaining_seconds, Some(12));
        monitor.shutdown();
    }

    #[test]
    fn ui_runtime_progress_enters_the_monitor_without_friend_or_message_text() {
        let monitor = MonitorShared::new(20);
        let runtime = UiRuntime::start_with_progress(UnusedDevice, 1, Arc::new(monitor.clone()))
            .expect("UI runtime should start");
        let operation = runtime.handle().submit(ProgressRoutine).unwrap();
        let operation_id = operation.id().get();
        operation.wait().unwrap();

        let snapshot = monitor.snapshot();
        let progress = snapshot.ui_routine.expect("progress should be projected");
        assert_eq!(progress.operation_id, operation_id);
        assert_eq!(progress.stage, "sending_friend_message");
        assert_eq!(progress.recipient_index, Some(2));
        assert_eq!(progress.recipient_count, Some(3));
        assert_eq!(progress.message_index, Some(1));
        assert_eq!(progress.message_count, Some(2));
        let serialized = serde_json::to_string(&progress).unwrap();
        assert!(!serialized.contains("nickname"));
        assert!(!serialized.contains("messageText"));

        runtime.shutdown().unwrap();
        monitor.shutdown();
    }
}
