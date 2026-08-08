use super::*;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::ops::{Deref, DerefMut};
use std::time::Duration;

use crate::composition::application::http_facade::ApplicationHttpCommandFacade;
use crate::features::hall::HallRuntimeState;
use crate::features::playback::{PlaybackRuntimeState, PlayerStatus, test_candidate, test_track};
use crate::features::turtle_soup::{TurtleSoupAppendReceipt, TurtleSoupSnapshot};
use crate::features::undercover::UndercoverSnapshot;
use crate::interfaces::http::{
    HttpAiPort, HttpHallPort, HttpLoginPort, HttpLoginStatus, HttpPlayerPort, HttpProviderView,
    HttpQueryPort, HttpTaskPort,
};
use crate::runtime::chat_listener::ChatListenerState;
use crate::runtime::player_io::SearchCandidate;
use crate::runtime::scheduler::{DiagnosticTaskSnapshot, FormalTaskReceipt};
use miliastra_playback::LoginSession;

fn custom_workflow_service_from_config_parts(
    config: &CustomWorkflowConfig,
    timing: &TimingConfig,
    ocr: &OcrConfig,
) -> CustomWorkflowService {
    CustomWorkflowService::new(
        config.clone(),
        WorkflowDefaults {
            default_timeout_ms: timing.workflow.default_timeout_ms,
            default_poll_ms: timing.workflow.default_poll_ms,
            default_step_wait_ms: timing.workflow.default_step_wait_ms,
            decision_timeout_ms: timing.decision.timeout_ms,
            decision_poll_ms: timing.decision.poll_ms,
            after_activate_ms: timing.input.after_activate_ms,
            clipboard_hold_ms: timing.input.text_ms,
            stability_mean_threshold: ocr.change_mean_threshold,
            stability_changed_ratio_threshold: ocr.change_pixel_threshold,
        },
    )
}

#[derive(Clone, Debug)]
struct RecordedFormalTask {
    id: u64,
    kind: RecordedFormalTaskKind,
    queued: bool,
    running: bool,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
enum RecordedFormalTaskKind {
    Command(Box<PendingCommand>),
    Startup(StartupTask),
    ConsoleChat { text: String, prefix: String },
    ListenerMode(ChatListenerMode),
    ClearIdleExit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RecordedMutation {
    PlaybackPush(Box<QueueItem>),
    PlaybackRemove(QueueRemoval),
    PlaybackClear,
    HallPatch(HallStatePatch),
    TurtleSoupStart(Option<String>),
    TurtleSoupEnd,
    TurtleSoupAppend {
        title: String,
        surface: String,
        bottom: String,
        adjudication_notes: String,
        enabled: bool,
    },
    ChatListenerModeRequest(ChatListenerMode),
    ChatListenerModeCancel(ChatListenerMode),
}

struct RecordingHttpState {
    next_id: u64,
    next_queue_id: u64,
    formal_tasks: Vec<RecordedFormalTask>,
    diagnostic_tasks: HashMap<u64, DiagnosticTaskSnapshot>,
    diagnostic_requests: Vec<(u64, WebToolRequest)>,
    cancellation_requests: Vec<u64>,
    decisions: Vec<(u64, DecisionAction)>,
    mutations: Vec<RecordedMutation>,
    queue: Vec<QueueItem>,
    playback: PlaybackRuntimeState,
    hall: HallRuntimeState,
    hall_screenshot_requests: usize,
    hall_screenshot_error: bool,
    listener_enqueue_error: bool,
    turtle_soup: TurtleSoupSnapshot,
    turtle_soup_submissions: Vec<TurtleSoupSubmission>,
    undercover: UndercoverSnapshot,
    listener: ChatListenerState,
}

impl RecordingHttpState {
    fn new() -> Self {
        Self {
            next_id: 1,
            next_queue_id: 1,
            formal_tasks: Vec::new(),
            diagnostic_tasks: HashMap::new(),
            diagnostic_requests: Vec::new(),
            cancellation_requests: Vec::new(),
            decisions: Vec::new(),
            mutations: Vec::new(),
            queue: Vec::new(),
            playback: PlaybackRuntimeState::default(),
            hall: HallRuntimeState::default(),
            hall_screenshot_requests: 0,
            hall_screenshot_error: false,
            listener_enqueue_error: false,
            turtle_soup: TurtleSoupSnapshot::default(),
            turtle_soup_submissions: Vec::new(),
            undercover: UndercoverSnapshot::default(),
            listener: ChatListenerState::new(),
        }
    }

    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

struct RecordingHttpPort {
    state: Mutex<RecordingHttpState>,
}

impl RecordingHttpPort {
    fn new() -> Self {
        Self {
            state: Mutex::new(RecordingHttpState::new()),
        }
    }

    fn enqueue_kind(&self, kind: RecordedFormalTaskKind) -> Result<FormalTaskEnqueueOutcome> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        let id = state.allocate_id();
        let position = state.formal_tasks.iter().filter(|task| task.queued).count() + 1;
        state.formal_tasks.push(RecordedFormalTask {
            id,
            kind,
            queued: true,
            running: false,
        });
        Ok(FormalTaskEnqueueOutcome::Queued(FormalTaskReceipt {
            task_id: id,
            position,
        }))
    }

    fn formal_tasks(&self) -> Vec<RecordedFormalTask> {
        self.state
            .lock()
            .expect("recording port")
            .formal_tasks
            .clone()
    }

    fn mutations(&self) -> Vec<RecordedMutation> {
        self.state.lock().expect("recording port").mutations.clone()
    }

    fn diagnostic_requests(&self) -> Vec<(u64, WebToolRequest)> {
        self.state
            .lock()
            .expect("recording port")
            .diagnostic_requests
            .clone()
    }

    fn decisions(&self) -> Vec<(u64, DecisionAction)> {
        self.state.lock().expect("recording port").decisions.clone()
    }

    fn cancellation_requests(&self) -> Vec<u64> {
        self.state
            .lock()
            .expect("recording port")
            .cancellation_requests
            .clone()
    }

    fn mark_started(&self, id: u64) {
        let mut state = self.state.lock().expect("recording port");
        let task = state
            .formal_tasks
            .iter_mut()
            .find(|task| task.id == id)
            .expect("recorded task");
        task.queued = false;
        task.running = true;
    }

    fn hall_screenshot_requests(&self) -> usize {
        self.state
            .lock()
            .expect("recording port")
            .hall_screenshot_requests
    }

    fn fail_hall_screenshot(&self) {
        self.state
            .lock()
            .expect("recording port")
            .hall_screenshot_error = true;
    }

    fn fail_listener_enqueue(&self) {
        self.state
            .lock()
            .expect("recording port")
            .listener_enqueue_error = true;
    }
}

impl HttpTaskPort for RecordingHttpPort {
    fn apply_mutation(&self, intent: BusinessMutationIntent) -> Result<BusinessMutationOutcome> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        Ok(match intent {
            BusinessMutationIntent::Administration(
                AdministrationMutationIntent::RequestChatListenerMode(mode),
            ) => {
                state
                    .mutations
                    .push(RecordedMutation::ChatListenerModeRequest(mode));
                let queued = state.listener.request_mode(mode);
                BusinessMutationOutcome::Administration(
                    AdministrationMutationOutcome::ChatListenerModeRequested {
                        queued,
                        snapshot: state.listener.snapshot(),
                    },
                )
            }
            BusinessMutationIntent::Administration(
                AdministrationMutationIntent::CancelChatListenerModeRequest(mode),
            ) => {
                state
                    .mutations
                    .push(RecordedMutation::ChatListenerModeCancel(mode));
                state.listener.cancel_mode_request(mode);
                BusinessMutationOutcome::Administration(
                    AdministrationMutationOutcome::ChatListenerModeRequestCancelled,
                )
            }
            BusinessMutationIntent::Playback(PlaybackMutationIntent::Push(mut item)) => {
                if item.id == 0 {
                    item.id = state.next_queue_id;
                    state.next_queue_id = state.next_queue_id.wrapping_add(1).max(1);
                }
                state
                    .mutations
                    .push(RecordedMutation::PlaybackPush(item.clone()));
                state.queue.push(*item);
                BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Pushed(
                    crate::features::playback::QueuePushOutcome {
                        accepted: true,
                        size: state.queue.len(),
                    },
                ))
            }
            BusinessMutationIntent::Playback(PlaybackMutationIntent::Remove(removal)) => {
                state
                    .mutations
                    .push(RecordedMutation::PlaybackRemove(removal));
                let index = match removal {
                    QueueRemoval::Id(id) => state.queue.iter().position(|item| item.id == id),
                    QueueRemoval::Index(index) => {
                        Some(index).filter(|index| *index < state.queue.len())
                    }
                    QueueRemoval::Front => (!state.queue.is_empty()).then_some(0),
                };
                let removed = match index {
                    Some(index) => {
                        let item = state.queue.remove(index);
                        QueueRemoveOutcome::Removed {
                            index,
                            item: Box::new(item),
                            size: state.queue.len(),
                        }
                    }
                    None => match removal {
                        QueueRemoval::Id(_) => QueueRemoveOutcome::MissingId,
                        QueueRemoval::Index(_) => QueueRemoveOutcome::InvalidIndex,
                        QueueRemoval::Front => QueueRemoveOutcome::Empty,
                    },
                };
                BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Removed(removed))
            }
            BusinessMutationIntent::Playback(PlaybackMutationIntent::Clear) => {
                state.mutations.push(RecordedMutation::PlaybackClear);
                state.queue.clear();
                BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Cleared)
            }
            BusinessMutationIntent::Hall(HallMutationIntent::PatchState(patch)) => {
                state
                    .mutations
                    .push(RecordedMutation::HallPatch(patch.clone()));
                if let Some(value) = patch.remaining_minutes {
                    state.hall.remaining_minutes = value;
                }
                if let Some(value) = patch.remaining_updated_at {
                    state.hall.remaining_updated_at = value;
                }
                if let Some(value) = patch.expiring_warning_sent {
                    state.hall.expiring_warning_sent = value;
                }
                BusinessMutationOutcome::Hall(HallMutationOutcome::StatePatched)
            }
            BusinessMutationIntent::TurtleSoup(TurtleSoupMutationIntent::Start { puzzle_id }) => {
                state
                    .mutations
                    .push(RecordedMutation::TurtleSoupStart(puzzle_id));
                state.turtle_soup.enabled = true;
                BusinessMutationOutcome::TurtleSoup(Box::new(TurtleSoupMutationOutcome::Started(
                    state.turtle_soup.clone(),
                )))
            }
            BusinessMutationIntent::TurtleSoup(TurtleSoupMutationIntent::End) => {
                state.mutations.push(RecordedMutation::TurtleSoupEnd);
                let ended = state.turtle_soup.enabled;
                state.turtle_soup = TurtleSoupSnapshot::default();
                BusinessMutationOutcome::TurtleSoup(Box::new(TurtleSoupMutationOutcome::Ended {
                    ended,
                    snapshot: state.turtle_soup.clone(),
                }))
            }
            BusinessMutationIntent::TurtleSoup(TurtleSoupMutationIntent::AppendPuzzle(
                submission,
            )) => {
                state.mutations.push(RecordedMutation::TurtleSoupAppend {
                    title: submission.title.clone(),
                    surface: submission.surface.clone(),
                    bottom: submission.bottom.clone(),
                    adjudication_notes: submission.adjudication_notes.clone(),
                    enabled: submission.enabled,
                });
                state.turtle_soup_submissions.push(submission);
                let position = state.turtle_soup_submissions.len();
                BusinessMutationOutcome::TurtleSoup(Box::new(
                    TurtleSoupMutationOutcome::PuzzleAppended(TurtleSoupAppendReceipt {
                        id: format!("soup-{position:04}"),
                        position,
                        total: position,
                    }),
                ))
            }
        })
    }

    fn enqueue_command(&self, pending: PendingCommand) -> Result<FormalTaskEnqueueOutcome> {
        {
            let state = self
                .state
                .lock()
                .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
            if state.formal_tasks.iter().any(|task| {
                task.queued
                    && matches!(
                        &task.kind,
                        RecordedFormalTaskKind::Command(command)
                            if command.lock_key == pending.lock_key
                    )
            }) {
                return Ok(FormalTaskEnqueueOutcome::Duplicate);
            }
        }
        self.enqueue_kind(RecordedFormalTaskKind::Command(Box::new(pending)))
    }

    fn enqueue_startup(&self, task: StartupTask) -> Result<FormalTaskEnqueueOutcome> {
        self.enqueue_kind(RecordedFormalTaskKind::Startup(task))
    }

    fn enqueue_console_chat(
        &self,
        text: String,
        prefix: String,
    ) -> Result<FormalTaskEnqueueOutcome> {
        self.enqueue_kind(RecordedFormalTaskKind::ConsoleChat { text, prefix })
    }

    fn enqueue_listener_mode(&self, target: ChatListenerMode) -> Result<FormalTaskEnqueueOutcome> {
        if self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .listener_enqueue_error
        {
            return Err(anyhow!("listener enqueue failed"));
        }
        self.enqueue_kind(RecordedFormalTaskKind::ListenerMode(target))
    }

    fn enqueue_clear_idle_exit(&self) -> Result<FormalTaskEnqueueOutcome> {
        self.enqueue_kind(RecordedFormalTaskKind::ClearIdleExit)
    }

    fn enqueue_diagnostic(&self, request: WebToolRequest) -> Result<DiagnosticTaskSnapshot> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        let id = state.allocate_id();
        let snapshot = DiagnosticTaskSnapshot {
            id,
            label: request.label(),
            status: "queued".to_string(),
            result: None,
        };
        state.diagnostic_requests.push((id, request));
        state.diagnostic_tasks.insert(id, snapshot.clone());
        Ok(snapshot)
    }

    fn cancel_task(&self, task_id: u64) -> Result<FormalTaskCancelOutcome> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        state.cancellation_requests.push(task_id);
        let Some(task) = state
            .formal_tasks
            .iter_mut()
            .find(|task| task.id == task_id)
        else {
            return Ok(FormalTaskCancelOutcome::NotFound);
        };
        if task.running {
            return Ok(FormalTaskCancelOutcome::CancellationRequested);
        }
        if !task.queued {
            return Ok(FormalTaskCancelOutcome::AlreadyFinished);
        }
        task.queued = false;
        Ok(FormalTaskCancelOutcome::CanceledBeforeStart)
    }

    fn submit_decision(&self, id: u64, action: DecisionAction) -> Result<()> {
        self.state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .decisions
            .push((id, action));
        Ok(())
    }
}

impl HttpQueryPort for RecordingHttpPort {
    fn turtle_soup_snapshot(&self) -> Result<TurtleSoupSnapshot> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .turtle_soup
            .clone())
    }

    fn undercover_snapshot(&self) -> Result<UndercoverSnapshot> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .undercover
            .clone())
    }

    fn diagnostic_task_snapshot(&self, id: u64) -> Result<Option<DiagnosticTaskSnapshot>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .diagnostic_tasks
            .get(&id)
            .cloned())
    }

    fn playback_queue_snapshot(&self) -> Result<Vec<QueueItem>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .queue
            .clone())
    }

    fn playback_state_snapshot(&self) -> Result<PlaybackRuntimeState> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .playback
            .clone())
    }

    fn hall_state_snapshot(&self) -> Result<HallRuntimeState> {
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?
            .hall
            .clone())
    }
}

impl HttpHallPort for RecordingHttpPort {
    fn capture_hall_screenshot(&self) -> Result<Arc<DynamicImage>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("recording HTTP port lock poisoned"))?;
        state.hall_screenshot_requests += 1;
        if state.hall_screenshot_error {
            return Err(anyhow!("目标游戏窗口不可用"));
        }
        Ok(Arc::new(DynamicImage::new_rgb8(2, 2)))
    }
}

struct HttpTestPlayerPort {
    fail: bool,
    status: PlayerStatus,
}

impl HttpTestPlayerPort {
    fn successful() -> Self {
        Self {
            fail: false,
            status: PlayerStatus {
                status: String::new(),
                current_uri: String::new(),
                name: String::new(),
                singer: String::new(),
                album_name: String::new(),
                lyric_line_text: String::new(),
                duration: 0.0,
                progress: 0.0,
                playback_rate: 1.0,
                volume: 0,
                requester: String::new(),
                ..PlayerStatus::default()
            },
        }
    }

    fn failing() -> Self {
        Self {
            fail: true,
            ..Self::successful()
        }
    }

    fn with_status(status: PlayerStatus) -> Self {
        Self {
            fail: false,
            status,
        }
    }

    fn fail_if_requested(&self) -> std::result::Result<(), HttpPlayerSearchError> {
        if self.fail {
            Err(HttpPlayerSearchError::new("backend failed"))
        } else {
            Ok(())
        }
    }
}

impl HttpPlayerPort for HttpTestPlayerPort {
    fn status(&self) -> Result<crate::features::playback::PlayerStatus> {
        Ok(self.status.clone())
    }

    fn search_text(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<String, HttpPlayerSearchError> {
        self.fail_if_requested()?;
        Ok(format!("raw search: {keyword} [{source}]"))
    }

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Vec<SearchCandidate>, HttpPlayerSearchError> {
        self.fail_if_requested()?;
        Ok(vec![test_candidate(
            &format!("{keyword} result"),
            &format!("miliastra://track/{source}/1"),
        )])
    }
}

#[derive(Default)]
struct HttpTestLoginPort {
    active: Mutex<Option<LoginSession>>,
}

impl HttpLoginPort for HttpTestLoginPort {
    fn providers(&self) -> Result<Vec<HttpProviderView>, HttpLoginError> {
        Ok(ProviderId::ALL
            .into_iter()
            .map(|provider| {
                let status = miliastra_playback::CredentialStatus::empty(provider.as_str());
                HttpProviderView {
                    provider,
                    configured: status.configured,
                    fields: status
                        .fields
                        .into_iter()
                        .map(|(name, present)| (name.to_owned(), present))
                        .collect(),
                }
            })
            .collect())
    }

    fn status(&self) -> HttpLoginStatus {
        let active = self.active.lock().unwrap().clone();
        HttpLoginStatus {
            active: active.is_some(),
            session_id: active.as_ref().map(|session| session.session_id),
            provider: active.map(|session| session.provider),
            last_error: None,
        }
    }

    fn start(&self, provider: ProviderId) -> Result<LoginSession, HttpLoginError> {
        let mut active = self.active.lock().unwrap();
        if active.is_some() {
            return Err(HttpLoginError {
                code: "login_in_progress",
                message: "已有登录任务正在进行",
            });
        }
        let session = LoginSession {
            session_id: Uuid::new_v4(),
            provider,
        };
        *active = Some(session.clone());
        Ok(session)
    }

    fn cancel(&self, session_id: Uuid) -> Result<(), HttpLoginError> {
        let mut active = self.active.lock().unwrap();
        if active.as_ref().map(|session| session.session_id) != Some(session_id) {
            return Err(HttpLoginError {
                code: "login_session_invalid",
                message: "登录会话无效",
            });
        }
        *active = None;
        Ok(())
    }

    fn logout(
        &self,
        provider: ProviderId,
    ) -> Result<miliastra_playback::CredentialStatus, HttpLoginError> {
        Ok(miliastra_playback::CredentialStatus::empty(
            provider.as_str(),
        ))
    }
}

struct HttpTestAiPort;

impl HttpAiPort for HttpTestAiPort {
    fn recognize(&self, _query: &[(String, String)]) -> Result<String> {
        Ok("test recognition".to_string())
    }

    fn match_song(&self, _query: &[(String, String)]) -> Result<String> {
        Ok("test match".to_string())
    }

    fn pick(&self, _query: &[(String, String)]) -> Result<String> {
        Ok("test pick".to_string())
    }
}

struct HttpTestState {
    state: HttpSharedState,
    recording: Arc<RecordingHttpPort>,
}

impl Deref for HttpTestState {
    type Target = HttpSharedState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for HttpTestState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

struct TestHttpResponse {
    status_line: String,
    headers: HashMap<String, String>,
    body: String,
}

fn start_test_http_server(state: &mut HttpTestState, access_token: &str) -> HttpServer {
    state.config.http.host = "127.0.0.1".to_string();
    state.config.http.port = 0;
    state.config.http.access_token = access_token.to_string();
    start(state.state.clone()).expect("start HTTP server")
}

fn http_get(address: SocketAddr, target: &str, access_token: Option<&str>) -> TestHttpResponse {
    http_request(address, "GET", target, access_token)
}

fn http_post(address: SocketAddr, target: &str, access_token: Option<&str>) -> TestHttpResponse {
    http_request(address, "POST", target, access_token)
}

fn http_post_json(
    address: SocketAddr,
    target: &str,
    body: &str,
    access_token: Option<&str>,
) -> TestHttpResponse {
    http_request_with_body(address, "POST", target, access_token, body)
}

fn http_request(
    address: SocketAddr,
    method: &str,
    target: &str,
    access_token: Option<&str>,
) -> TestHttpResponse {
    http_request_with_body(address, method, target, access_token, "")
}

fn http_request_with_body(
    address: SocketAddr,
    method: &str,
    target: &str,
    access_token: Option<&str>,
    body: &str,
) -> TestHttpResponse {
    let mut stream = TcpStream::connect(address).expect("connect to HTTP server");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let token_header = access_token
        .map(|token| format!("X-Miliastra-Token: {token}\r\n"))
        .unwrap_or_default();
    let content_type = if body.is_empty() {
        String::new()
    } else {
        "Content-Type: application/json\r\n".to_string()
    };
    let request = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\n{token_header}{content_type}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("write HTTP request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read HTTP response");
    let (head, body) = raw.split_once("\r\n\r\n").expect("HTTP response head");
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("HTTP status line").to_string();
    let headers = lines
        .map(|line| line.split_once(':').expect("HTTP response header"))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    TestHttpResponse {
        status_line,
        headers,
        body: body.to_string(),
    }
}

#[test]
fn owned_http_server_serves_and_stops_on_an_ephemeral_port() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "");
    let address = server.local_addr();
    let response = http_get(address, "/", None);

    assert_eq!(response.status_line, "HTTP/1.1 200 OK");
    server.shutdown().expect("shutdown HTTP server");
    assert!(TcpStream::connect(address).is_err());
}

#[test]
fn screenshot_rejects_an_invalidated_frame_cache() {
    let state = test_state();
    {
        let mut cache = state.latest_frame.lock().expect("frame cache");
        cache.store(Arc::new(image::DynamicImage::new_rgb8(2, 2)));
        cache.invalidate();
    }
    let request = Request {
        method: "GET".to_string(),
        path: "/screenshot".to_string(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    let error = match screenshot_response(&request, &state) {
        Ok(_) => panic!("invalidated frame must not be served"),
        Err(error) => error,
    };
    assert_eq!(error.status, 503);
}

#[test]
fn hall_screenshot_triggers_detection_and_serves_captured_frame() {
    let state = test_state();
    let request = Request {
        method: "GET".to_string(),
        path: "/hall-screenshot".to_string(),
        query: vec![("quality".to_string(), "88".to_string())],
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    let response = hall_screenshot_response(&request, &state).expect("hall screenshot");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "image/jpeg");
    assert_eq!(state.recording.hall_screenshot_requests(), 1);
}

#[test]
fn hall_screenshot_maps_detection_failure_to_service_unavailable() {
    let state = test_state();
    state.recording.fail_hall_screenshot();
    let request = Request {
        method: "GET".to_string(),
        path: "/hall-screenshot".to_string(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    let error = hall_screenshot_response(&request, &state.state).unwrap_err();

    assert_eq!(error.status, 503);
}

#[test]
fn hall_screenshot_rejects_invalid_quality_before_touching_the_game() {
    let state = test_state();
    let request = Request {
        method: "GET".to_string(),
        path: "/hall-screenshot".to_string(),
        query: vec![("quality".to_string(), "79".to_string())],
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    let error = hall_screenshot_response(&request, &state.state).unwrap_err();

    assert_eq!(error.status, 400);
    assert_eq!(state.recording.hall_screenshot_requests(), 0);
}

#[test]
fn search_routes_preserve_their_contract_over_real_http() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "");
    let address = server.local_addr();

    let text = http_get(address, "/search?keyword=song&source=netease", None);
    assert_eq!(text.status_line, "HTTP/1.1 200 OK");
    assert_eq!(
        text.headers.get("content-type").map(String::as_str),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(text.body, "raw search: song [netease]");

    let candidates = http_get(
        address,
        "/search/candidates?keyword=song&source=netease",
        None,
    );
    assert_eq!(candidates.status_line, "HTTP/1.1 200 OK");
    assert_eq!(
        candidates.headers.get("content-type").map(String::as_str),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&candidates.body).expect("candidate JSON"),
        json!([{
            "trackRef": {"key": {"provider": "netease", "id": "1"}},
            "metadata": {
                "title": "song result",
                "artists": ["测试歌手"],
                "durationMs": 180000
            },
            "eligibility": "unknown",
            "text": "song result",
        }])
    );

    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn native_player_and_login_routes_use_structured_json_without_secrets() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "");
    let address = server.local_addr();

    let providers = http_get(address, "/player/providers", None);
    assert_eq!(providers.status_line, "HTTP/1.1 200 OK");
    let providers: Value = serde_json::from_str(&providers.body).expect("providers JSON");
    assert_eq!(providers.as_array().map(Vec::len), Some(3));
    assert_eq!(providers[0]["configured"], false);
    assert!(providers[0].get("cookies").is_none());

    let track = json!({
        "trackRef": {
            "key": {"provider": "netease", "id": "123"},
            "resolverLocator": "netease:v1:123"
        },
        "metadata": {
            "title": "结构化歌曲",
            "artists": ["测试歌手"],
            "album": "测试专辑",
            "durationMs": 180000
        },
        "requester": "Alice"
    });
    let played = http_post_json(address, "/player/play-track", &track.to_string(), None);
    assert_eq!(played.status_line, "HTTP/1.1 200 OK");
    let played: Value = serde_json::from_str(&played.body).expect("play response JSON");
    assert_eq!(played["currentUri"], "miliastra://track/netease/123");
    assert_eq!(played["queued"], true);
    assert_eq!(played["size"], 1);
    let queue = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("playback queue snapshot");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].keyword, "结构化歌曲");
    assert_eq!(queue[0].source, "netease");
    assert_eq!(queue[0].requester, "Alice");
    assert!(queue[0].dedup_bypass);
    assert_eq!(
        queue[0]
            .track
            .as_ref()
            .map(|track| track.track_ref.key.to_string())
            .as_deref(),
        Some("miliastra://track/netease/123")
    );

    let mismatch = json!({
        "trackRef": {
            "key": {"provider": "netease", "id": "123"},
            "resolverLocator": "qqmusic:v1:123"
        },
        "metadata": {"title": "坏曲目", "artists": []}
    });
    let mismatch = http_post_json(address, "/player/play-track", &mismatch.to_string(), None);
    assert_eq!(mismatch.status_line, "HTTP/1.1 400 Bad Request");
    assert!(mismatch.body.contains("resolverLocator"));
    assert_eq!(
        state
            .application
            .queries
            .playback_queue_snapshot()
            .unwrap()
            .len(),
        1
    );

    let unsupported_version = json!({
        "trackRef": {
            "key": {"provider": "netease", "id": "123"},
            "resolverLocator": "netease:v2:123"
        },
        "metadata": {"title": "坏曲目", "artists": []}
    });
    let unsupported_version = http_post_json(
        address,
        "/player/play-track",
        &unsupported_version.to_string(),
        None,
    );
    assert_eq!(unsupported_version.status_line, "HTTP/1.1 400 Bad Request");
    assert!(unsupported_version.body.contains("resolverLocator"));
    assert_eq!(
        state
            .application
            .queries
            .playback_queue_snapshot()
            .unwrap()
            .len(),
        1
    );

    let session = http_post_json(
        address,
        "/player/login/start",
        r#"{"provider":"qqmusic"}"#,
        None,
    );
    assert_eq!(session.status_line, "HTTP/1.1 200 OK");
    let session: Value = serde_json::from_str(&session.body).expect("login response JSON");
    let session_id = session["sessionId"].as_str().expect("session id");
    let status = http_get(address, "/player/login/status", None);
    let status: Value = serde_json::from_str(&status.body).expect("login status JSON");
    assert_eq!(status["active"], true);
    assert_eq!(status["provider"], "qqmusic");
    let cancel = http_post_json(
        address,
        "/player/login/cancel",
        &json!({"sessionId": session_id}).to_string(),
        None,
    );
    assert_eq!(cancel.status_line, "HTTP/1.1 200 OK");
    let status = http_get(address, "/player/login/status", None);
    let status: Value = serde_json::from_str(&status.body).expect("inactive login status JSON");
    assert_eq!(status["active"], false);

    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn native_play_track_rejects_unknown_fields() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "");
    let body = json!({
        "trackRef": {"key": {"provider": "qqmusic", "id": "1"}},
        "metadata": {"title": "歌曲", "artists": []},
        "credential": "must not be accepted"
    });
    let response = http_post_json(
        server.local_addr(),
        "/player/play-track",
        &body.to_string(),
        None,
    );
    assert_eq!(response.status_line, "HTTP/1.1 400 Bad Request");
    assert!(
        state
            .application
            .queries
            .playback_queue_snapshot()
            .unwrap()
            .is_empty()
    );
    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn search_route_requires_the_configured_token_over_real_http() {
    let mut state = test_state();
    let server = start_test_http_server(&mut state, "secret");

    let response = http_get(
        server.local_addr(),
        "/search?keyword=song&source=netease",
        None,
    );

    assert_eq!(response.status_line, "HTTP/1.1 401 Unauthorized");
    assert_eq!(
        response.headers.get("content-type").map(String::as_str),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(response.body, "错误: 需要有效的 Web 访问令牌");
    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn search_backend_failure_keeps_the_http_error_contract() {
    let mut state = test_state_with_player_port(HttpTestPlayerPort::failing());
    let server = start_test_http_server(&mut state, "");

    let response = http_get(
        server.local_addr(),
        "/search?keyword=song&source=netease",
        None,
    );

    assert_eq!(response.status_line, "HTTP/1.1 500 Internal Server Error");
    assert_eq!(
        response.headers.get("content-type").map(String::as_str),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(response.body, "错误: backend failed");
    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn chat_send_requires_post() {
    assert!(is_mutating_route("/chat/send"));
}

#[test]
fn turtle_soup_routes_have_expected_methods_and_monitor_snapshot() {
    assert!(!is_mutating_route("/turtle-soup"));
    assert!(is_json_route("/turtle-soup"));
    for route in ["/turtle-soup/start", "/turtle-soup/end"] {
        assert!(is_mutating_route(route));
        assert!(is_json_route(route));
    }
    assert!(is_mutating_route("/turtle-soup/questions"));
    assert!(is_json_route("/turtle-soup/questions"));

    let state = test_state();
    let monitor: Value = serde_json::from_str(&monitor_json(&state).unwrap()).unwrap();
    assert_eq!(monitor["turtleSoup"]["enabled"], false);
    assert_eq!(monitor["turtleSoup"]["phase"], "idle");
}

#[test]
fn undercover_routes_are_redacted_json_and_controls_require_post() {
    assert!(!is_mutating_route("/undercover"));
    assert!(is_json_route("/undercover"));
    for route in ["/undercover/start", "/undercover/end"] {
        assert!(is_mutating_route(route));
        assert!(is_json_route(route));
    }

    let state = test_state();
    let monitor: Value = serde_json::from_str(&monitor_json(&state).unwrap()).unwrap();
    assert_eq!(monitor["undercover"]["enabled"], false);
    assert_eq!(monitor["undercover"]["phase"], "idle");
    assert!(monitor["undercover"].get("words").is_none());
    assert!(monitor["undercover"].get("roles").is_none());
}

#[test]
fn custom_workflow_routes_keep_their_list_and_enqueue_contracts() {
    let mut state = test_state();
    state.config.custom_workflows = serde_yaml::from_str(
        r#"
enabled: true
default_threshold: 0.9
wait_template_absent_stable_default: true
max_hold_key_seconds: 10
templates: {}
workflows:
  - enabled: true
    name: example
    commands: ["入口", "别名"]
    allow_args: true
    message_types: [blue]
    confirm_before_run: false
    confirm_message: ""
    confirm_message_types: [blue]
    confirm_timeout_ms: null
    confirm_poll_ms: null
    steps:
      - type: press_key
        key: F
    success_message: ""
"#,
    )
    .expect("custom workflow config");
    let default_config: AppConfig =
        serde_yaml::from_str(include_str!("../../../../config.yaml")).expect("default config");
    state.application.commands = Arc::new(ApplicationHttpCommandFacade::new(
        state.recording.clone(),
        custom_workflow_service_from_config_parts(
            &state.config.custom_workflows,
            &state.config.timing,
            &default_config.ocr,
        ),
    ));

    let listed: Value =
        serde_json::from_str(&operator_workflows_route(&[], &state).unwrap()).unwrap();
    assert_eq!(listed[0]["name"], "example");
    assert_eq!(listed[0]["commands"], json!(["入口", "别名"]));
    assert_eq!(listed[0]["allowArgs"], true);
    assert_eq!(listed[0]["confirmBeforeRun"], false);

    let response: Value = serde_json::from_str(
        &operator_workflow_run_route(
            &[
                ("name".to_string(), "example".to_string()),
                ("args".to_string(), "5".to_string()),
            ],
            &state,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["queued"], true);
    assert_eq!(response["duplicate"], false);
    assert_eq!(response["position"], 1);
    assert_eq!(response["command"], "入口 5");
    assert!(response["taskId"].as_u64().is_some());
    let tasks = state.recording.formal_tasks();
    assert_eq!(tasks.len(), 1);
    assert!(matches!(
        tasks[0].kind,
        RecordedFormalTaskKind::Command(ref pending)
            if pending.routed.raw == "入口 5"
                && matches!(
                    pending.routed.command,
                    ModuleCommand::CustomWorkflow(ref command)
                        if command.workflow == "example" && command.args == "5"
                )
    ));
}

#[test]
fn turtle_soup_question_submission_appends_in_request_order() {
    let state = test_state();
    let first = r#"{"title":"第一题","surface":"第一面","bottom":"第一底"}"#;
    let second =
        r#"{"title":"第二题","surface":"第二面","bottom":"第二底","adjudicationNotes":"备注"}"#;

    let first: Value = serde_json::from_str(
        &turtle_soup_questions_route(first.as_bytes(), &state).expect("first submission"),
    )
    .expect("first receipt");
    let second: Value = serde_json::from_str(
        &turtle_soup_questions_route(second.as_bytes(), &state).expect("second submission"),
    )
    .expect("second receipt");

    assert_eq!(first["id"], "soup-0001");
    assert_eq!(first["position"], 1);
    assert_eq!(second["id"], "soup-0002");
    assert_eq!(second["position"], 2);
}

#[test]
fn turtle_soup_question_submission_rejects_invalid_json() {
    let state = test_state();
    let error = turtle_soup_questions_route(r#"{"title":"缺少内容"}"#.as_bytes(), &state)
        .expect_err("invalid submission");

    assert_eq!(error.status, 400);
    assert!(state.recording.mutations().is_empty());
}

#[test]
fn web_tool_routes_are_queued_json_post_routes() {
    for route in [
        "/tools/ocr",
        "/tools/scan-chat",
        "/tools/ui-state",
        "/tools/hall-name",
        "/tools/template",
        "/tools/click",
        "/tools/key",
        "/tools/chat-change-samples",
        "/tools/panel-benchmark",
        "/tools/ocr-backends",
        "/tools/ai-preview",
    ] {
        assert!(is_mutating_route(route), "{route} should require POST");
        assert!(is_json_route(route), "{route} should return JSON");
    }
    assert!(!is_mutating_route("/tools/task"));
    assert!(is_json_route("/tools/task"));
    assert!(TOOLS_PAGE.contains("Miliastra 高级控制"));
}

#[test]
fn web_tools_wait_outside_the_formal_pending_queue() {
    let state = test_state();
    let body = tool_ui_state_route(&[], &state).expect("tool route succeeds");
    let ticket: Value = serde_json::from_str(&body).expect("tool ticket");
    let id = ticket["id"].as_u64().expect("tool id");

    assert_eq!(ticket["status"], "queued");
    assert!(state.recording.formal_tasks().is_empty());
    let requests = state.recording.diagnostic_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, id);
    assert!(matches!(requests[0].1, WebToolRequest::UiState));
    assert_eq!(
        state
            .application
            .queries
            .diagnostic_task_snapshot(id)
            .expect("tool snapshot")
            .expect("queued tool")
            .label,
        "UI 状态检测"
    );
}

#[test]
fn web_tool_ocr_rejects_malformed_rect_as_client_error() {
    let state = test_state();
    let error = tool_ocr_route(&[("rect".to_string(), "invalid".to_string())], &state)
        .expect_err("invalid rect rejected");

    assert_eq!(error.status, 400);
    assert!(error.message.contains("rect参数无效"));
}

#[test]
fn web_tool_templates_expose_configured_fixed_regions() {
    let state = test_state();
    let body = tool_templates_route(&[], &state).expect("template list");
    let templates: Vec<Value> = serde_json::from_str(&body).expect("template list json");
    let marker_threshold = state.config.templates.marker_threshold;
    let expected = [
        (
            "blue-marker",
            state.config.screen.chat_rect,
            marker_threshold,
        ),
        (
            "yellow-marker",
            state.config.screen.chat_rect,
            marker_threshold,
        ),
        (
            "pink-marker",
            state.config.screen.chat_rect,
            marker_threshold,
        ),
        ("friend", state.config.screen.friend_rect, marker_threshold),
        (
            "secondary-back",
            state.config.screen.secondary_back_rect,
            marker_threshold,
        ),
        (
            "secondary-hall",
            state.config.screen.secondary_hall_rect,
            marker_threshold,
        ),
        (
            "invite-view-star",
            state.config.invite.view_star_region,
            marker_threshold,
        ),
        (
            "invite-goto-hall",
            state.config.invite.goto_hall_region,
            marker_threshold,
        ),
        (
            "invite-enter-hall",
            state.config.invite.enter_hall_region,
            marker_threshold,
        ),
        (
            "friend-panel",
            state.config.moderation.friend_panel_region,
            marker_threshold,
        ),
        (
            "friend-search-panel",
            state.config.moderation.search_panel_region,
            marker_threshold,
        ),
        (
            "friend-more-settings",
            state.config.moderation.more_settings_region,
            marker_threshold,
        ),
        (
            "friend-block-chat",
            state.config.moderation.block_chat_region,
            marker_threshold,
        ),
        (
            "friend-blacklist",
            state.config.moderation.blacklist_region,
            marker_threshold,
        ),
        (
            "friend-confirm",
            state.config.moderation.confirm_region,
            marker_threshold,
        ),
        (
            "wonderland-confirm",
            state.config.startup.wonderland_confirm_region,
            state.config.startup.wonderland_confirm_threshold,
        ),
        (
            "paimon-menu",
            state.config.startup.main_ui_region,
            state.config.startup.template_threshold,
        ),
        (
            "wonderland-map-star",
            state.config.startup.wonderland_map_star_region,
            state.config.startup.template_threshold,
        ),
    ];
    for (name, region, threshold) in expected {
        let template = templates
            .iter()
            .find(|template| template["name"] == name)
            .unwrap_or_else(|| panic!("missing template {name}"));
        assert_eq!(
            template["region"],
            serde_json::to_value(region).expect("template region json"),
            "template region mismatch: {name}"
        );
        let actual_threshold = template["threshold"].as_f64().expect("template threshold") as f32;
        assert!(
            (actual_threshold - threshold).abs() < f32::EPSILON,
            "template threshold mismatch: {name}"
        );
    }
    assert!(TOOLS_PAGE.contains("useConfiguredTemplateRegion"));
}

#[test]
fn remote_http_api_requires_token_when_configured() {
    let mut config: AppConfig =
        serde_yaml::from_str(include_str!("../../../../config.yaml")).expect("default config");
    config.http.host = "0.0.0.0".to_string();
    config.http.access_token = "secret".to_string();
    let request = Request {
        method: "GET".to_string(),
        path: "/monitor".to_string(),
        query: Vec::new(),
        headers: HeaderMap::new(),
        body: Vec::new(),
    };

    assert!(requires_access_token(&config.http, &request.path));
    assert!(!has_valid_access_token(&request, &config.http.access_token));
    assert!(!requires_access_token(&config.http, "/"));
}

#[test]
fn status_route_includes_requester_for_the_matching_active_song() {
    let status = PlayerStatus {
        status: "playing".to_string(),
        current_track: Some(test_track("miliastra://track/qqmusic/1", "晴天 - 周杰伦")),
        current_uri: "miliastra://track/qqmusic/1".to_string(),
        name: "晴天".to_string(),
        ..PlayerStatus::default()
    };
    let state = test_state_with_player_port(HttpTestPlayerPort::with_status(status));
    state
        .recording
        .state
        .lock()
        .expect("recording state")
        .playback
        .active_request = Some(ActivePlaybackRequest {
        track: Some(test_track("miliastra://track/qqmusic/1", "晴天 - 周杰伦")),
        requester: "Alice".to_string(),
        ..ActivePlaybackRequest::default()
    });

    let value: Value = serde_json::from_str(&status_route(&[], &state).expect("status route"))
        .expect("status JSON");
    assert_eq!(value["requester"], "Alice");
}

#[test]
fn remote_song_routes_are_queued_json_post_routes() {
    assert!(is_mutating_route("/searchPlay"));
    assert!(is_mutating_route("/ai/search"));
    assert!(is_json_route("/searchPlay"));
    assert!(is_json_route("/ai/search"));
}

#[test]
fn search_routes_keep_their_existing_response_contracts() {
    let state = test_state();
    let query = [
        ("keyword".to_string(), "晴天".to_string()),
        ("source".to_string(), "netease".to_string()),
    ];

    assert_eq!(
        search_route(&query, &state).unwrap(),
        "raw search: 晴天 [netease]"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&search_candidates_route(&query, &state).unwrap()).unwrap(),
        json!([{
            "trackRef": {"key": {"provider": "netease", "id": "1"}},
            "metadata": {
                "title": "晴天 result",
                "artists": ["测试歌手"],
                "durationMs": 180000
            },
            "eligibility": "unknown",
            "text": "晴天 result",
        }])
    );
}

#[test]
fn search_route_preserves_backend_error_text() {
    let error = player_search_error(HttpPlayerSearchError::new("backend failed"));

    assert_eq!(error.status, 500);
    assert_eq!(error.message, "backend failed");
    assert_eq!(
        player_search_error(HttpPlayerSearchError::new(
            "player search lane queue is full"
        ))
        .message,
        "player search lane queue is full"
    );
}

#[test]
fn playback_control_routes_are_queued_json_post_routes() {
    for route in ["/play", "/pause", "/skip-next", "/skip-prev", "/volume"] {
        assert!(is_mutating_route(route), "{route} should require POST");
        assert!(is_json_route(route), "{route} should return queued JSON");
    }
}

#[test]
fn startup_routes_are_queued_json_post_routes() {
    assert!(is_mutating_route("/startup/game"));
    assert!(is_mutating_route("/startup/wonderland"));
    assert!(is_mutating_route("/startup/enter-wonderland"));
    assert!(is_json_route("/startup/game"));
    assert!(is_json_route("/startup/wonderland"));
    assert!(is_json_route("/startup/enter-wonderland"));
    assert!(PAGE.contains("call('/startup/game','POST')"));
    assert!(PAGE.contains("call('/startup/wonderland','POST')"));
    assert!(PAGE.contains("call('/startup/enter-wonderland','POST')"));
}

#[test]
fn queue_add_defaults_api_requester_when_not_provided() {
    let state = test_state();
    queue_add(
        &[
            ("keyword".to_string(), "测试歌曲".to_string()),
            ("source".to_string(), "qqmusic".to_string()),
        ],
        &state,
    )
    .expect("queue add succeeds");

    let queue = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("playback queue snapshot");
    assert_eq!(queue[0].requester, "WEB/API");
}

#[test]
fn page_displays_song_requester() {
    assert!(PAGE.contains("点歌人"));
    assert!(PAGE.contains("it.requester||it.friendUsername"));
    assert!(PAGE.contains("pc.requester"));
}

#[test]
fn remote_next_builds_console_game_command() {
    let pending = ApplicationHttpCommandFacade::remote_control_command(
        "下一首".to_string(),
        "下一首",
        ModuleCommand::Playback(PlaybackCommand::Next),
    )
    .into_pending();

    assert_eq!(pending.routed.message_type, "控制台");
    assert_eq!(pending.routed.username, "控制台");
    assert_eq!(pending.routed.raw, "下一首");
    assert_eq!(pending.routed.user_command, "@下一首");
    assert!(matches!(
        pending.routed.command,
        ModuleCommand::Playback(PlaybackCommand::Next)
    ));
}

#[test]
fn remote_volume_builds_console_game_command() {
    let pending = ApplicationHttpCommandFacade::remote_control_command(
        "音量 60".to_string(),
        "音量",
        ModuleCommand::Playback(PlaybackCommand::Volume("60".to_string())),
    )
    .into_pending();

    assert_eq!(pending.routed.raw, "音量 60");
    assert_eq!(pending.routed.user_command, "@音量 60");
    assert!(matches!(
        pending.routed.command,
        ModuleCommand::Playback(PlaybackCommand::Volume(ref volume)) if volume == "60"
    ));
}

#[test]
fn refresh_toggle_runs_full_uncached_refresh_when_resumed() {
    assert!(PAGE.contains("id=\"refreshToggle\""));
    assert!(PAGE.contains("onclick=\"toggleRefresh()\""));
    assert!(PAGE.contains("function toggleRefresh()"));
    assert!(PAGE.contains("if(!refreshPaused)refreshAll()"));
    assert!(PAGE.contains("async function refreshAll()"));
    assert!(PAGE.contains(
        "Promise.allSettled([loadMonitor(),loadHistory(),refreshPlayer(),loadLoginState()])"
    ));
    assert!(PAGE.contains("cache:'no-store'"));
    assert!(!PAGE.contains("onclick=\"loadMonitor()\""));
}

#[test]
fn web_inputs_support_enter_submit() {
    assert!(PAGE.contains("function isPlainEnter(e)"));
    assert!(PAGE.contains("!e.isComposing"));
    assert!(PAGE.contains("bindEnter('consoleChatText',sendConsoleChat)"));
    assert!(PAGE.contains("bindEnter('consoleChatPrefix',sendConsoleChat)"));
    assert!(PAGE.contains("bindEnter('keyword',()=>remoteSong(false))"));
    assert!(PAGE.contains("bindEnter('volumeInput',setVolume)"));
    assert!(PAGE.contains("bindEnter('workflowArgs',runWorkflow)"));
    assert!(PAGE.contains("function removeQueueId(id)"));
}

#[test]
fn remote_control_response_includes_trackable_task_id() {
    let state = test_state();
    let body = play_route(&[], &state).expect("play route");
    let response: Value = serde_json::from_str(&body).expect("play response json");
    let task_id = response["taskId"].as_u64().expect("task id");

    assert_eq!(response["queued"], true);
    assert_eq!(response["position"], 1);
    let tasks = state.recording.formal_tasks();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, task_id);
    assert!(matches!(
        tasks[0].kind,
        RecordedFormalTaskKind::Command(ref pending)
            if matches!(
                pending.routed.command,
                ModuleCommand::Playback(PlaybackCommand::Resume)
            )
    ));
}

#[test]
fn waiting_formal_task_can_be_canceled() {
    let state = test_state();
    let body = pause_route(&[], &state).expect("pause route");
    let response: Value = serde_json::from_str(&body).expect("pause response json");
    let task_id = response["taskId"].as_u64().expect("task id");

    let cancel_body = task_cancel_route(&[("id".to_string(), task_id.to_string())], &state)
        .expect("cancel route");
    let canceled: Value = serde_json::from_str(&cancel_body).expect("cancel response json");

    assert_eq!(canceled["canceled"], true);
    assert_eq!(state.recording.cancellation_requests(), [task_id]);
    let tasks = state.recording.formal_tasks();
    assert_eq!(tasks.len(), 1);
    assert!(!tasks[0].queued);
}

#[test]
fn started_formal_task_accepts_a_cancellation_request() {
    let state = test_state();
    let body = pause_route(&[], &state).expect("pause route");
    let response: Value = serde_json::from_str(&body).expect("pause response json");
    let task_id = response["taskId"].as_u64().expect("task id");
    state.recording.mark_started(task_id);

    let body = task_cancel_route(&[("id".to_string(), task_id.to_string())], &state)
        .expect("running task cancellation request");
    let response: Value = serde_json::from_str(&body).expect("cancel response json");

    assert_eq!(response["canceled"], false);
    assert_eq!(response["cancellationRequested"], true);
    assert_eq!(state.recording.cancellation_requests(), [task_id]);
}

#[test]
fn task_cancel_contract_requires_authentication_and_post_before_submitting_id() {
    let mut state = test_state();
    let queued: Value = serde_json::from_str(&pause_route(&[], &state).expect("pause route"))
        .expect("pause response");
    let task_id = queued["taskId"].as_u64().expect("task id");
    let server = start_test_http_server(&mut state, "secret");
    let target = format!("/tasks/cancel?id={task_id}");

    let unauthenticated = http_post(server.local_addr(), &target, None);
    assert_eq!(unauthenticated.status_line, "HTTP/1.1 401 Unauthorized");
    assert!(state.recording.cancellation_requests().is_empty());

    let wrong_method = http_get(server.local_addr(), &target, Some("secret"));
    assert_eq!(wrong_method.status_line, "HTTP/1.1 405 Method Not Allowed");
    assert!(state.recording.cancellation_requests().is_empty());

    let canceled = http_post(server.local_addr(), &target, Some("secret"));
    assert_eq!(canceled.status_line, "HTTP/1.1 200 OK");
    assert_eq!(
        canceled.headers.get("content-type").map(String::as_str),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&canceled.body).expect("cancel response JSON")["ok"],
        true
    );
    assert_eq!(state.recording.cancellation_requests(), [task_id]);

    server.shutdown().expect("shutdown HTTP server");
}

#[test]
fn web_decision_submission_reaches_active_song_decision() {
    let state = test_state();
    let id = 42;

    let body = decision_submit_route(
        &[
            ("id".to_string(), id.to_string()),
            ("action".to_string(), "switch_source".to_string()),
        ],
        &state,
    )
    .expect("decision route");

    let response: Value = serde_json::from_str(&body).expect("decision response");
    assert_eq!(response["decisionId"], id);
    assert_eq!(response["submitted"], "switch_source");
    assert_eq!(
        state.recording.decisions(),
        [(id, DecisionAction::SwitchSource)]
    );
}

#[test]
fn console_chat_prefix_can_be_configured() {
    let state = test_state();

    let default_body = chat_send_route(&[("text".to_string(), "你好".to_string())], &state)
        .expect("default prefix");
    let default_value: Value = serde_json::from_str(&default_body).expect("json response");
    assert_eq!(default_value["message"], "[控制台]: 你好");

    let custom_body = chat_send_route(
        &[
            ("text".to_string(), "你好".to_string()),
            ("prefix".to_string(), "[远程] ".to_string()),
        ],
        &state,
    )
    .expect("custom prefix");
    let custom_value: Value = serde_json::from_str(&custom_body).expect("json response");
    assert_eq!(custom_value["message"], "[远程] 你好");

    let raw_body = chat_send_route(
        &[
            ("text".to_string(), "你好".to_string()),
            ("usePrefix".to_string(), "0".to_string()),
            ("prefix".to_string(), "[远程] ".to_string()),
        ],
        &state,
    )
    .expect("no prefix");
    let raw_value: Value = serde_json::from_str(&raw_body).expect("json response");
    assert_eq!(raw_value["message"], "你好");

    let tasks = state.recording.formal_tasks();
    let messages = tasks
        .iter()
        .map(|task| match &task.kind {
            RecordedFormalTaskKind::ConsoleChat { text, prefix } => {
                format!("{prefix}{text}")
            }
            other => panic!("unexpected task: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(messages, ["[控制台]: 你好", "[远程] 你好", "你好"]);
}

#[test]
fn chat_send_requires_non_empty_text() {
    let error = normalize_required_text(Some("  "), "text").unwrap_err();

    assert_eq!(error.status, 400);
    assert!(error.message.contains("缺少text参数"));
}

#[test]
fn remote_song_command_builds_console_plain_song() {
    let state = test_state();
    state
        .application
        .commands
        .remote_song("晴天 伴奏".to_string(), "qqmusic".to_string(), false, false)
        .expect("remote song command");
    let tasks = state.recording.formal_tasks();
    let RecordedFormalTaskKind::Command(pending) = &tasks[0].kind else {
        panic!("expected song command");
    };

    assert_eq!(pending.routed.message_type, "控制台");
    assert_eq!(pending.routed.username, "控制台");
    assert_eq!(pending.routed.raw, "点歌 晴天 伴奏");
    match &pending.routed.command {
        ModuleCommand::SongRequest(song) => {
            assert_eq!(song.keyword, "晴天");
            assert_eq!(song.source, SongSource::QqMusic);
            assert!(song.prefer_accompaniment);
            assert!(!song.ai_assisted);
            assert!(song.friend_username.is_empty());
        }
        _ => panic!("expected song command"),
    }
}

#[test]
fn remote_song_command_builds_console_ai_song() {
    let state = test_state();
    state
        .application
        .commands
        .remote_song("晴天".to_string(), "qqmusic".to_string(), false, true)
        .expect("remote ai song command");
    let tasks = state.recording.formal_tasks();
    let RecordedFormalTaskKind::Command(pending) = &tasks[0].kind else {
        panic!("expected song command");
    };

    assert_eq!(pending.routed.raw, "AI点歌 晴天");
    match &pending.routed.command {
        ModuleCommand::SongRequest(song) => {
            assert_eq!(song.source, SongSource::All);
            assert!(song.ai_assisted);
        }
        _ => panic!("expected song command"),
    }
}

#[test]
fn remote_song_command_supports_bilibili_source() {
    let state = test_state();
    state
        .application
        .commands
        .remote_song(
            "耀斑 HOYO-MiX".to_string(),
            "bilibili".to_string(),
            false,
            false,
        )
        .expect("remote bilibili song command");
    let tasks = state.recording.formal_tasks();
    let RecordedFormalTaskKind::Command(pending) = &tasks[0].kind else {
        panic!("expected song command");
    };

    assert_eq!(pending.routed.raw, "B站点歌 耀斑 HOYO-MiX");
    match &pending.routed.command {
        ModuleCommand::SongRequest(song) => {
            assert_eq!(song.source, SongSource::Bilibili);
            assert_eq!(song.prefix, "B站点歌");
        }
        _ => panic!("expected song command"),
    }
}

#[test]
fn queue_removal_by_id_survives_automatic_front_shift() {
    let state = test_state();
    for keyword in ["第一首", "第二首", "第三首"] {
        queue_add(
            &[
                ("keyword".to_string(), keyword.to_string()),
                ("source".to_string(), "qqmusic".to_string()),
            ],
            &state,
        )
        .expect("queue push");
    }
    let third_id = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("queue snapshot")[2]
        .id;
    let removed = queue_remove(&[], &state).expect("queue shift");
    let removed: Value = serde_json::from_str(&removed).expect("remove response");
    assert_eq!(removed["removed"]["keyword"], "第一首");

    let body =
        queue_remove(&[("id".to_string(), third_id.to_string())], &state).expect("remove by id");
    let response: Value = serde_json::from_str(&body).expect("remove response json");

    assert_eq!(response["removed"]["id"], third_id);
    assert_eq!(response["removed"]["keyword"], "第三首");
    let queue = state
        .application
        .queries
        .playback_queue_snapshot()
        .expect("queue snapshot");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].keyword, "第二首");
}

#[test]
fn runtime_state_routes_preserve_patch_and_response_contracts() {
    let state = test_state();
    let saved = state_save(
            &[(
                "json".to_string(),
                r#"{"hallRemainingMinutes":42,"hallRemainingUpdatedAt":1234,"hallExpiringWarningSent":true}"#
                    .to_string(),
            )],
            &state,
        )
        .expect("state patch succeeds");
    let saved: Value = serde_json::from_str(&saved).expect("save response json");
    let snapshot: Value = serde_json::from_str(&state_json(&state).expect("state json"))
        .expect("state snapshot json");

    assert_eq!(saved, json!({ "ok": true }));
    assert_eq!(snapshot["hallRemainingMinutes"], 42);
    assert_eq!(snapshot["hallRemainingUpdatedAt"], 1234);
    assert_eq!(snapshot["hallExpiringWarningSent"], true);
    assert!(snapshot.get("hallRemainingMinutesNow").is_some());
    assert!(snapshot["playback"].get("previousRequests").is_none());

    state_save(
        &[(
            "json".to_string(),
            r#"{"hallRemainingMinutes":null,"hallRemainingUpdatedAt":null}"#.to_string(),
        )],
        &state,
    )
    .expect("state clear succeeds");
    let cleared: Value =
        serde_json::from_str(&state_json(&state).expect("state json")).expect("cleared state json");
    assert!(cleared["hallRemainingMinutes"].is_null());
    assert!(cleared["hallRemainingUpdatedAt"].is_null());
}

#[test]
fn remote_plain_song_rejects_multi_source() {
    let state = test_state();
    let error = state
        .application
        .commands
        .remote_song(
            "晴天".to_string(),
            "qqmusic,netease".to_string(),
            false,
            false,
        )
        .unwrap_err();

    assert_eq!(error.status, 400);
    assert!(error.message.contains("source只允许"));
}

#[test]
fn screenshot_quality_is_bounded() {
    assert_eq!(parse_jpeg_quality(None).unwrap(), 88);
    assert_eq!(parse_jpeg_quality(Some("95")).unwrap(), 95);
    assert!(parse_jpeg_quality(Some("79")).is_err());
    assert!(parse_jpeg_quality(Some("96")).is_err());
}

#[test]
fn chat_listener_mode_route_enqueues_secondary_switch() {
    let state = test_state();
    let query = vec![("mode".to_string(), "secondary".to_string())];

    let response = chat_listener_mode_route(&query, &state).expect("listener route");
    let response: Value = serde_json::from_str(&response).expect("listener response json");

    assert_eq!(response["queued"], true);
    assert_eq!(response["mode"], "primary");
    assert_eq!(response["pendingMode"], "secondary");
    assert_eq!(
        state.recording.mutations(),
        [RecordedMutation::ChatListenerModeRequest(
            ChatListenerMode::Secondary
        )]
    );
    let tasks = state.recording.formal_tasks();
    assert_eq!(tasks.len(), 1);
    assert!(matches!(
        tasks[0].kind,
        RecordedFormalTaskKind::ListenerMode(ChatListenerMode::Secondary)
    ));
}

#[test]
fn chat_listener_mode_enqueue_failure_rolls_back_request() {
    let state = test_state();
    state.recording.fail_listener_enqueue();
    let query = vec![("mode".to_string(), "secondary".to_string())];

    let error = chat_listener_mode_route(&query, &state).unwrap_err();

    assert_eq!(error.status, 500);
    assert_eq!(
        state.recording.mutations(),
        [
            RecordedMutation::ChatListenerModeRequest(ChatListenerMode::Secondary),
            RecordedMutation::ChatListenerModeCancel(ChatListenerMode::Secondary),
        ]
    );
    assert!(state.recording.formal_tasks().is_empty());
}

fn test_state() -> HttpTestState {
    test_state_with_player_port(HttpTestPlayerPort::successful())
}

fn test_state_with_player_port(player: impl HttpPlayerPort + 'static) -> HttpTestState {
    let config: AppConfig =
        serde_yaml::from_str(include_str!("../../../../config.yaml")).expect("default config");
    let monitor = MonitorShared::new(20);
    let custom_workflow = custom_workflow_service_from_config_parts(
        &config.custom_workflows,
        &config.timing,
        &config.ocr,
    );
    let recording = Arc::new(RecordingHttpPort::new());
    let login = Arc::new(HttpTestLoginPort::default());
    HttpTestState {
        state: HttpSharedState::new(
            HttpInterfaceConfig::new(
                config.http.clone(),
                config.screen.clone(),
                config.templates.clone(),
                config.moderation.clone(),
                config.startup.clone(),
                config.invite.clone(),
                config.timing.clone(),
                config.custom_workflows.clone(),
            ),
            monitor,
            Arc::new(Mutex::new(LatestFrameCache::default())),
            HttpApplicationPorts::new(
                Arc::new(ApplicationHttpCommandFacade::new(
                    recording.clone(),
                    custom_workflow,
                )),
                recording.clone(),
                recording.clone(),
                recording.clone(),
                Arc::new(player),
                login.clone(),
                Arc::new(HttpTestAiPort),
            ),
        ),
        recording,
    }
}
