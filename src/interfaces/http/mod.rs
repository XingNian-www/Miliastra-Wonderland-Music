use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::net::SocketAddr;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use image::codecs::jpeg::JpegEncoder;
use image::{ColorType, DynamicImage};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
#[cfg(test)]
use serde_json::Value;
use serde_json::json;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::oneshot;
use url::form_urlencoded;

mod tools;

use crate::adapters::login_helper::{
    LoginHelperFailure, LoginHelperManager, LoginManagerStatus, ProviderView,
};
use crate::adapters::player::PlayerRuntimeBackend;
#[cfg(test)]
use crate::config::AppConfig;
#[cfg(test)]
use crate::config::OcrConfig;
use crate::config::{HttpConfig, ScreenConfig, TemplateConfig, TimingConfig};
use crate::features::administration::{
    AdministrationCommand, AdministrationMutationIntent, AdministrationMutationOutcome,
};
use crate::features::command::ModuleCommand;
#[cfg(test)]
use crate::features::custom_workflow::WorkflowDefaults;
use crate::features::custom_workflow::{CustomWorkflowConfig, CustomWorkflowService};
use crate::features::hall::{
    HallCommand, HallMutationIntent, HallMutationOutcome, HallRuntimeState, HallStatePatch,
};
use crate::features::invite::InviteConfig;
use crate::features::moderation::ModerationConfig;
#[cfg(test)]
use crate::features::playback::{ActivePlaybackRequest, PlayerStatus};
use crate::features::playback::{
    MusicPlayerBackend, PlaybackCommand, PlaybackMutationIntent, PlaybackMutationOutcome,
    PlaybackRuntimeState, QueueItem, QueueRemoval, QueueRemoveOutcome,
};
use crate::features::song_request::AiClient;
use crate::features::song_request::{SongCommand, SongSource};
use crate::features::startup::{StartupConfig, StartupSource, StartupTask};
use crate::features::turtle_soup::{
    TurtleSoupMutationIntent, TurtleSoupMutationOutcome, TurtleSoupSnapshot, TurtleSoupSubmission,
};
use crate::features::undercover::{UndercoverCommand, UndercoverSnapshot};
use crate::interfaces::chat::{ConsoleCommandIntent, PendingCommand};
use crate::runtime::business::{BusinessMutationIntent, BusinessMutationOutcome};
use crate::runtime::chat_listener::ChatListenerMode;
use crate::runtime::decision::DecisionAction;
use crate::runtime::monitor::MonitorShared;
use crate::runtime::player_io::PlayerRuntimeHandle;
use crate::runtime::player_io::{PlayerSearchClient, PlayerSearchClientError, SearchCandidate};
use crate::runtime::scheduler::{
    DiagnosticTaskSnapshot, FormalTaskCancelOutcome, FormalTaskEnqueueOutcome,
};
use crate::ui::frame::LatestFrameCache;
use crate::ui::geometry::parse_rect;
use miliastra_playback::{LoginSession, PlayableTrack, PlaybackError, PlaybackHandle, ProviderId};
pub(crate) use tools::{WebToolRequest, WebToolTemplate};
use uuid::Uuid;

pub(crate) trait HttpTaskPort: Send + Sync {
    fn apply_mutation(&self, intent: BusinessMutationIntent) -> Result<BusinessMutationOutcome>;

    fn enqueue_command(&self, pending: PendingCommand) -> Result<FormalTaskEnqueueOutcome>;

    fn enqueue_startup(&self, task: StartupTask) -> Result<FormalTaskEnqueueOutcome>;

    fn enqueue_console_chat(
        &self,
        text: String,
        prefix: String,
    ) -> Result<FormalTaskEnqueueOutcome>;

    fn enqueue_listener_mode(&self, target: ChatListenerMode) -> Result<FormalTaskEnqueueOutcome>;

    fn enqueue_clear_idle_exit(&self) -> Result<FormalTaskEnqueueOutcome>;

    fn enqueue_diagnostic(&self, request: WebToolRequest) -> Result<DiagnosticTaskSnapshot>;

    fn cancel_task(&self, task_id: u64) -> Result<FormalTaskCancelOutcome>;

    fn submit_decision(&self, id: u64, action: DecisionAction) -> Result<()>;
}

pub(crate) trait HttpQueryPort: Send + Sync {
    fn turtle_soup_snapshot(&self) -> Result<TurtleSoupSnapshot>;

    fn undercover_snapshot(&self) -> Result<UndercoverSnapshot>;

    fn diagnostic_task_snapshot(&self, id: u64) -> Result<Option<DiagnosticTaskSnapshot>>;

    fn playback_queue_snapshot(&self) -> Result<Vec<QueueItem>>;

    fn playback_state_snapshot(&self) -> Result<PlaybackRuntimeState>;

    fn hall_state_snapshot(&self) -> Result<HallRuntimeState>;
}

pub(crate) trait HttpHallPort: Send + Sync {
    fn capture_hall_screenshot(&self) -> Result<Arc<DynamicImage>>;
}

pub(crate) trait HttpPlayerPort: Send + Sync {
    fn status(&self) -> Result<crate::features::playback::PlayerStatus>;

    fn search_text(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<String, PlayerSearchClientError>;

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Vec<SearchCandidate>, PlayerSearchClientError>;
}

trait HttpNativePlaybackPort: Send + Sync {
    fn play_track(&self, track: PlayableTrack) -> Result<(), PlaybackError>;
}

impl HttpNativePlaybackPort for PlaybackHandle {
    fn play_track(&self, track: PlayableTrack) -> Result<(), PlaybackError> {
        PlaybackHandle::play(self, track)
    }
}

trait HttpLoginPort: Send + Sync {
    fn providers(&self) -> Result<Vec<ProviderView>, LoginHelperFailure>;
    fn status(&self) -> LoginManagerStatus;
    fn start(&self, provider: ProviderId) -> Result<LoginSession, LoginHelperFailure>;
    fn cancel(&self, session_id: Uuid) -> Result<(), LoginHelperFailure>;
    fn logout(
        &self,
        provider: ProviderId,
    ) -> Result<miliastra_playback::CredentialStatus, LoginHelperFailure>;
}

impl HttpLoginPort for LoginHelperManager {
    fn providers(&self) -> Result<Vec<ProviderView>, LoginHelperFailure> {
        LoginHelperManager::providers(self)
    }

    fn status(&self) -> LoginManagerStatus {
        LoginHelperManager::status(self)
    }

    fn start(&self, provider: ProviderId) -> Result<LoginSession, LoginHelperFailure> {
        LoginHelperManager::start(self, provider)
    }

    fn cancel(&self, session_id: Uuid) -> Result<(), LoginHelperFailure> {
        LoginHelperManager::cancel(self, session_id)
    }

    fn logout(
        &self,
        provider: ProviderId,
    ) -> Result<miliastra_playback::CredentialStatus, LoginHelperFailure> {
        LoginHelperManager::logout(self, provider)
    }
}

pub(crate) trait HttpAiPort: Send + Sync {
    fn recognize(&self, query: &[(String, String)]) -> Result<String>;
    fn match_song(&self, query: &[(String, String)]) -> Result<String>;
    fn pick(&self, query: &[(String, String)]) -> Result<String>;
}

#[derive(Clone)]
struct RuntimeHttpPlayerPort {
    backend: PlayerRuntimeBackend,
    search: PlayerSearchClient,
}

impl HttpPlayerPort for RuntimeHttpPlayerPort {
    fn status(&self) -> Result<crate::features::playback::PlayerStatus> {
        self.backend.status()
    }

    fn search_text(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<String, PlayerSearchClientError> {
        self.search.search_text(keyword, source)
    }

    fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> std::result::Result<Vec<SearchCandidate>, PlayerSearchClientError> {
        self.search.search_candidates(keyword, source)
    }
}

impl HttpAiPort for AiClient {
    fn recognize(&self, query: &[(String, String)]) -> Result<String> {
        self.recognize_with_query(query)
    }

    fn match_song(&self, query: &[(String, String)]) -> Result<String> {
        self.match_with_query(query)
    }

    fn pick(&self, query: &[(String, String)]) -> Result<String> {
        self.pick_with_query(query)
    }
}

const MAX_ACTIVE_CONNECTIONS: usize = 32;
const MAX_JSON_BODY_BYTES: usize = 64 * 1024;
const PAGE: &str = include_str!("page.html");
const TOOLS_PAGE: &str = include_str!("tools.html");

type RouteHandler =
    fn(&[(String, String)], &HttpSharedState) -> std::result::Result<String, AppError>;

struct RouteSpec {
    path: &'static str,
    json: bool,
    mutating: bool,
    handler: RouteHandler,
}

type BodyRouteHandler = fn(&[u8], &HttpSharedState) -> std::result::Result<String, AppError>;

struct BodyRouteSpec {
    path: &'static str,
    handler: BodyRouteHandler,
}

const BODY_ROUTES: &[BodyRouteSpec] = &[
    BodyRouteSpec {
        path: "/player/play-track",
        handler: player_play_track_body_route,
    },
    BodyRouteSpec {
        path: "/player/login/start",
        handler: player_login_start_body_route,
    },
    BodyRouteSpec {
        path: "/player/login/cancel",
        handler: player_login_cancel_body_route,
    },
    BodyRouteSpec {
        path: "/player/logout",
        handler: player_logout_body_route,
    },
    BodyRouteSpec {
        path: "/turtle-soup/questions",
        handler: turtle_soup_questions_route,
    },
];

const SPECIAL_ROUTES: &[&str] = &["/screenshot", "/hall-screenshot"];
const ROUTES: &[RouteSpec] = &[
    RouteSpec {
        path: "/status",
        json: true,
        mutating: false,
        handler: status_route,
    },
    RouteSpec {
        path: "/play",
        json: true,
        mutating: true,
        handler: play_route,
    },
    RouteSpec {
        path: "/pause",
        json: true,
        mutating: true,
        handler: pause_route,
    },
    RouteSpec {
        path: "/skip-next",
        json: true,
        mutating: true,
        handler: skip_next_route,
    },
    RouteSpec {
        path: "/skip-prev",
        json: true,
        mutating: true,
        handler: skip_prev_route,
    },
    RouteSpec {
        path: "/volume",
        json: true,
        mutating: true,
        handler: volume_route,
    },
    RouteSpec {
        path: "/startup/game",
        json: true,
        mutating: true,
        handler: startup_game_route,
    },
    RouteSpec {
        path: "/startup/wonderland",
        json: true,
        mutating: true,
        handler: startup_wonderland_route,
    },
    RouteSpec {
        path: "/startup/enter-wonderland",
        json: true,
        mutating: true,
        handler: enter_wonderland_route,
    },
    RouteSpec {
        path: "/searchPlay",
        json: true,
        mutating: true,
        handler: search_play_route,
    },
    RouteSpec {
        path: "/searchSource",
        json: true,
        mutating: true,
        handler: search_source_route,
    },
    RouteSpec {
        path: "/search",
        json: false,
        mutating: false,
        handler: search_route,
    },
    RouteSpec {
        path: "/search/candidates",
        json: true,
        mutating: false,
        handler: search_candidates_route,
    },
    RouteSpec {
        path: "/player/providers",
        json: true,
        mutating: false,
        handler: player_providers_route,
    },
    RouteSpec {
        path: "/player/login/status",
        json: true,
        mutating: false,
        handler: player_login_status_route,
    },
    RouteSpec {
        path: "/queue",
        json: true,
        mutating: false,
        handler: queue_route,
    },
    RouteSpec {
        path: "/queue/add",
        json: true,
        mutating: true,
        handler: queue_add_route,
    },
    RouteSpec {
        path: "/queue/remove",
        json: true,
        mutating: true,
        handler: queue_remove_route,
    },
    RouteSpec {
        path: "/queue/clear",
        json: true,
        mutating: true,
        handler: queue_clear_route,
    },
    RouteSpec {
        path: "/state",
        json: true,
        mutating: false,
        handler: state_route,
    },
    RouteSpec {
        path: "/state/save",
        json: true,
        mutating: true,
        handler: state_save_route,
    },
    RouteSpec {
        path: "/chat/send",
        json: true,
        mutating: true,
        handler: chat_send_route,
    },
    RouteSpec {
        path: "/chat-listener/mode",
        json: true,
        mutating: true,
        handler: chat_listener_mode_route,
    },
    RouteSpec {
        path: "/tasks/cancel",
        json: true,
        mutating: true,
        handler: task_cancel_route,
    },
    RouteSpec {
        path: "/decisions/submit",
        json: true,
        mutating: true,
        handler: decision_submit_route,
    },
    RouteSpec {
        path: "/operator/lyrics",
        json: true,
        mutating: true,
        handler: operator_lyrics_route,
    },
    RouteSpec {
        path: "/operator/hall-detect",
        json: true,
        mutating: true,
        handler: operator_hall_detect_route,
    },
    RouteSpec {
        path: "/operator/hall-time",
        json: true,
        mutating: true,
        handler: operator_hall_time_route,
    },
    RouteSpec {
        path: "/operator/microphone",
        json: true,
        mutating: true,
        handler: operator_microphone_route,
    },
    RouteSpec {
        path: "/operator/commands",
        json: true,
        mutating: true,
        handler: operator_commands_route,
    },
    RouteSpec {
        path: "/operator/idle-exit",
        json: true,
        mutating: true,
        handler: operator_idle_exit_route,
    },
    RouteSpec {
        path: "/operator/workflows",
        json: true,
        mutating: false,
        handler: operator_workflows_route,
    },
    RouteSpec {
        path: "/operator/workflows/run",
        json: true,
        mutating: true,
        handler: operator_workflow_run_route,
    },
    RouteSpec {
        path: "/ai/recognize",
        json: true,
        mutating: true,
        handler: ai_recognize_route,
    },
    RouteSpec {
        path: "/ai/match",
        json: true,
        mutating: true,
        handler: ai_match_route,
    },
    RouteSpec {
        path: "/ai/pick",
        json: true,
        mutating: true,
        handler: ai_pick_route,
    },
    RouteSpec {
        path: "/ai/search",
        json: true,
        mutating: true,
        handler: ai_search_route,
    },
    RouteSpec {
        path: "/history",
        json: true,
        mutating: false,
        handler: history_route,
    },
    RouteSpec {
        path: "/clear-history",
        json: false,
        mutating: true,
        handler: clear_history_route,
    },
    RouteSpec {
        path: "/monitor",
        json: true,
        mutating: false,
        handler: monitor_route,
    },
    RouteSpec {
        path: "/turtle-soup",
        json: true,
        mutating: false,
        handler: turtle_soup_route,
    },
    RouteSpec {
        path: "/turtle-soup/start",
        json: true,
        mutating: true,
        handler: turtle_soup_start_route,
    },
    RouteSpec {
        path: "/turtle-soup/end",
        json: true,
        mutating: true,
        handler: turtle_soup_end_route,
    },
    RouteSpec {
        path: "/undercover",
        json: true,
        mutating: false,
        handler: undercover_route,
    },
    RouteSpec {
        path: "/undercover/start",
        json: true,
        mutating: true,
        handler: undercover_start_route,
    },
    RouteSpec {
        path: "/undercover/end",
        json: true,
        mutating: true,
        handler: undercover_end_route,
    },
    RouteSpec {
        path: "/tools/task",
        json: true,
        mutating: false,
        handler: tool_task_route,
    },
    RouteSpec {
        path: "/tools/templates",
        json: true,
        mutating: false,
        handler: tool_templates_route,
    },
    RouteSpec {
        path: "/tools/ocr",
        json: true,
        mutating: true,
        handler: tool_ocr_route,
    },
    RouteSpec {
        path: "/tools/scan-chat",
        json: true,
        mutating: true,
        handler: tool_scan_chat_route,
    },
    RouteSpec {
        path: "/tools/ui-state",
        json: true,
        mutating: true,
        handler: tool_ui_state_route,
    },
    RouteSpec {
        path: "/tools/hall-name",
        json: true,
        mutating: true,
        handler: tool_hall_name_route,
    },
    RouteSpec {
        path: "/tools/template",
        json: true,
        mutating: true,
        handler: tool_template_route,
    },
    RouteSpec {
        path: "/tools/click",
        json: true,
        mutating: true,
        handler: tool_click_route,
    },
    RouteSpec {
        path: "/tools/key",
        json: true,
        mutating: true,
        handler: tool_key_route,
    },
    RouteSpec {
        path: "/tools/chat-change-samples",
        json: true,
        mutating: true,
        handler: tool_chat_change_samples_route,
    },
    RouteSpec {
        path: "/tools/panel-benchmark",
        json: true,
        mutating: true,
        handler: tool_panel_benchmark_route,
    },
    RouteSpec {
        path: "/tools/ocr-backends",
        json: true,
        mutating: true,
        handler: tool_ocr_backends_route,
    },
    RouteSpec {
        path: "/tools/ai-preview",
        json: true,
        mutating: true,
        handler: tool_ai_preview_route,
    },
    RouteSpec {
        path: "/health",
        json: false,
        mutating: false,
        handler: health_route,
    },
];

#[derive(Clone)]
pub struct HttpSharedState {
    config: HttpInterfaceConfig,
    pub monitor: MonitorShared,
    custom_workflow: CustomWorkflowService,
    pub history: Arc<Mutex<VecDeque<HistoryItem>>>,
    pub active_connections: Arc<AtomicUsize>,
    formal_tasks: Arc<dyn HttpTaskPort>,
    queries: Arc<dyn HttpQueryPort>,
    hall: Arc<dyn HttpHallPort>,
    latest_frame: Arc<Mutex<LatestFrameCache>>,
    player: Arc<dyn HttpPlayerPort>,
    native_playback: Arc<dyn HttpNativePlaybackPort>,
    login: Arc<dyn HttpLoginPort>,
    ai: Arc<dyn HttpAiPort>,
}

#[derive(Clone)]
pub(crate) struct HttpInterfaceConfig {
    http: HttpConfig,
    screen: ScreenConfig,
    templates: TemplateConfig,
    moderation: ModerationConfig,
    startup: StartupConfig,
    invite: InviteConfig,
    timing: TimingConfig,
    custom_workflows: CustomWorkflowConfig,
}

impl HttpInterfaceConfig {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        http: HttpConfig,
        screen: ScreenConfig,
        templates: TemplateConfig,
        moderation: ModerationConfig,
        startup: StartupConfig,
        invite: InviteConfig,
        timing: TimingConfig,
        custom_workflows: CustomWorkflowConfig,
    ) -> Self {
        Self {
            http,
            screen,
            templates,
            moderation,
            startup,
            invite,
            timing,
            custom_workflows,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    time: String,
    command: String,
    query: HashMap<String, String>,
    result: String,
    ok: bool,
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Debug)]
struct AppError {
    status: u16,
    message: String,
}

#[derive(Clone, Copy, Debug)]
struct EnqueueReceipt {
    task_id: u64,
    position: usize,
}

impl HttpSharedState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: HttpInterfaceConfig,
        custom_workflow: CustomWorkflowService,
        formal_tasks: Arc<dyn HttpTaskPort>,
        queries: Arc<dyn HttpQueryPort>,
        monitor: MonitorShared,
        hall: Arc<dyn HttpHallPort>,
        latest_frame: Arc<Mutex<LatestFrameCache>>,
        player_search: PlayerSearchClient,
        player_runtime: PlayerRuntimeHandle,
        native_playback: PlaybackHandle,
        login: LoginHelperManager,
        ai: AiClient,
    ) -> Self {
        let player = Arc::new(RuntimeHttpPlayerPort {
            backend: PlayerRuntimeBackend::new(player_runtime),
            search: player_search,
        });
        Self::new_with_ports(
            config,
            custom_workflow,
            formal_tasks,
            queries,
            monitor,
            hall,
            latest_frame,
            player,
            Arc::new(native_playback),
            Arc::new(login),
            Arc::new(ai),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_ports(
        config: HttpInterfaceConfig,
        custom_workflow: CustomWorkflowService,
        formal_tasks: Arc<dyn HttpTaskPort>,
        queries: Arc<dyn HttpQueryPort>,
        monitor: MonitorShared,
        hall: Arc<dyn HttpHallPort>,
        latest_frame: Arc<Mutex<LatestFrameCache>>,
        player: Arc<dyn HttpPlayerPort>,
        native_playback: Arc<dyn HttpNativePlaybackPort>,
        login: Arc<dyn HttpLoginPort>,
        ai: Arc<dyn HttpAiPort>,
    ) -> Self {
        Self {
            config,
            monitor,
            custom_workflow,
            history: Arc::new(Mutex::new(VecDeque::new())),
            active_connections: Arc::new(AtomicUsize::new(0)),
            formal_tasks,
            queries,
            hall,
            latest_frame,
            player,
            native_playback,
            login,
            ai,
        }
    }
}

pub struct HttpServer {
    #[cfg(test)]
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<Result<()>>>,
}

impl HttpServer {
    #[cfg(test)]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shutdown(mut self) -> Result<()> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| anyhow!("HTTP server thread panicked"))?
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown_inner() {
            log::error!("HTTP/Web 面板关闭失败: {error:#}");
        }
    }
}

pub fn start(state: HttpSharedState) -> Result<HttpServer> {
    if !is_loopback_host(&state.config.http.host)
        && state.config.http.access_token.trim().is_empty()
    {
        return Err(anyhow!(
            "HTTP 监听地址不是本机地址，必须设置 http.access_token 后才能启动"
        ));
    }
    let bind_addr = format!("{}:{}", state.config.http.host, state.config.http.port);
    let listener = TcpListener::bind(&bind_addr)
        .with_context(|| format!("启动 HTTP/Web 面板失败: {}", bind_addr))?;
    let local_addr = listener
        .local_addr()
        .context("read HTTP listener address")?;
    listener
        .set_nonblocking(true)
        .context("set HTTP listener nonblocking")?;
    let runtime = Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .context("启动 HTTP runtime")?;
    let listener = {
        let _runtime_guard = runtime.enter();
        tokio::net::TcpListener::from_std(listener).context("初始化 HTTP listener")?
    };
    let (shutdown, shutdown_receiver) = oneshot::channel();
    let worker = thread::Builder::new()
        .name("http-server".to_string())
        .spawn(move || run_server(runtime, listener, state, shutdown_receiver))
        .context("启动 HTTP server thread")?;
    log::info!("HTTP/Web 面板已启动: http://{}", local_addr);
    Ok(HttpServer {
        #[cfg(test)]
        local_addr,
        shutdown: Some(shutdown),
        worker: Some(worker),
    })
}

fn run_server(
    runtime: Runtime,
    listener: tokio::net::TcpListener,
    state: HttpSharedState,
    shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    runtime.block_on(async move {
        let app = Router::new()
            .fallback(any(axum_entry))
            .layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES))
            .with_state(Arc::new(state));
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown.await;
            })
            .await
            .context("HTTP/Web 面板运行失败")
    })
}

async fn axum_entry(
    State(state): State<Arc<HttpSharedState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let active = state.active_connections.fetch_add(1, Ordering::SeqCst);
    let _guard = ActiveConnectionGuard {
        counter: state.active_connections.clone(),
    };
    if active >= MAX_ACTIVE_CONNECTIONS {
        return plain_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "服务繁忙，请稍后再试".to_string(),
            Vec::new(),
        );
    }
    let request = request_from_axum(method, uri, headers, body);
    let fallback_host = state.config.http.host.clone();
    let fallback_port = state.config.http.port;
    let state_for_handler = Arc::clone(&state);
    match tokio::task::spawn_blocking(move || handle_request(request, &state_for_handler)).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => plain_response(
            status_code(error.status),
            format!("错误: {}", error.message),
            default_cors_headers(&fallback_host, fallback_port),
        ),
        Err(error) => plain_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("错误: HTTP请求处理失败: {error}"),
            default_cors_headers(&fallback_host, fallback_port),
        ),
    }
}

struct ActiveConnectionGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

fn request_from_axum(method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Request {
    let query = uri
        .query()
        .map(|query| {
            form_urlencoded::parse(query.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default();
    Request {
        method: method.as_str().to_string(),
        path: uri.path().to_string(),
        query,
        headers,
        body: body.to_vec(),
    }
}

fn handle_request(
    request: Request,
    state: &HttpSharedState,
) -> std::result::Result<Response, AppError> {
    if !is_allowed_origin(&request, &state.config.http.host, state.config.http.port) {
        return Err(AppError {
            status: 403,
            message: "不允许的请求来源".to_string(),
        });
    }

    if request.method == "OPTIONS" {
        return Ok(empty_response(
            StatusCode::NO_CONTENT,
            options_headers(&request, &state.config.http.host, state.config.http.port),
        ));
    }

    if requires_access_token(&state.config.http, &request.path)
        && !has_valid_access_token(&request, &state.config.http.access_token)
    {
        return Err(AppError {
            status: 401,
            message: "需要有效的 Web 访问令牌".to_string(),
        });
    }

    enforce_method(&request, state)?;

    if request.path == "/" && request.query.is_empty() {
        return Ok(body_response(
            StatusCode::OK,
            "text/html; charset=utf-8",
            PAGE.to_string(),
            cors_headers(&request, &state.config.http.host, state.config.http.port),
        ));
    }
    if request.path == "/tools" && request.query.is_empty() {
        if request.method != "GET" {
            return Err(method_not_allowed("工具页面仅支持GET请求"));
        }
        return Ok(body_response(
            StatusCode::OK,
            "text/html; charset=utf-8",
            TOOLS_PAGE.to_string(),
            cors_headers(&request, &state.config.http.host, state.config.http.port),
        ));
    }
    if request.path == "/favicon.ico" {
        return Ok(empty_response(
            StatusCode::NO_CONTENT,
            cors_headers(&request, &state.config.http.host, state.config.http.port),
        ));
    }
    if request.path == "/screenshot" {
        return screenshot_response(&request, state);
    }
    if request.path == "/hall-screenshot" {
        return hall_screenshot_response(&request, state);
    }

    let routed = if let Some(spec) = body_route_spec(&request.path) {
        if request.body.len() > MAX_JSON_BODY_BYTES {
            Err(AppError {
                status: 413,
                message: format!("JSON请求体不能超过{}字节", MAX_JSON_BODY_BYTES),
            })
        } else {
            (spec.handler)(&request.body, state)
        }
    } else {
        route(&request.path, &request.query, state)
    };
    let (body, ok) = match routed {
        Ok(body) => (body, true),
        Err(error) => {
            push_history(&request, &error.message, false, state);
            return Ok(plain_response(
                status_code(error.status),
                format!("错误: {}", error.message),
                cors_headers(&request, &state.config.http.host, state.config.http.port),
            ));
        }
    };
    push_history(&request, &body, ok, state);
    let content_type = if is_json_route(&request.path) {
        "application/json; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    };
    Ok(body_response(
        StatusCode::OK,
        content_type,
        body,
        cors_headers(&request, &state.config.http.host, state.config.http.port),
    ))
}

fn route(
    path: &str,
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    if let Some(spec) = route_spec(path) {
        (spec.handler)(query, state)
    } else {
        Err(AppError {
            status: 404,
            message: format!("未知接口，可用: {}", known_routes()),
        })
    }
}

fn route_spec(path: &str) -> Option<&'static RouteSpec> {
    ROUTES.iter().find(|route| route.path == path)
}

fn body_route_spec(path: &str) -> Option<&'static BodyRouteSpec> {
    BODY_ROUTES.iter().find(|route| route.path == path)
}

fn known_routes() -> String {
    ROUTES
        .iter()
        .map(|route| route.path)
        .chain(BODY_ROUTES.iter().map(|route| route.path))
        .chain(SPECIAL_ROUTES.iter().copied())
        .collect::<Vec<_>>()
        .join(", ")
}

fn status_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let mut status = state.player.status().map_err(internal_error)?;
    if let Ok(playback) = state.queries.playback_state_snapshot()
        && let Some(request) = playback.active_request
    {
        let active_uri = request
            .track
            .as_ref()
            .map(|track| track.track_ref.key.to_string())
            .unwrap_or_default();
        if !status.current_uri.trim().is_empty()
            && !active_uri.is_empty()
            && status.current_uri.trim() == active_uri
        {
            status.requester = request.requester;
        }
    }
    serde_json::to_string(&status).map_err(internal_error)
}

fn play_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "继续".to_string(),
            "继续",
            ModuleCommand::Playback(PlaybackCommand::Resume),
        ),
    )
}

fn pause_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "暂停".to_string(),
            "暂停",
            ModuleCommand::Playback(PlaybackCommand::Pause),
        ),
    )
}

fn skip_next_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "下一首".to_string(),
            "下一首",
            ModuleCommand::Playback(PlaybackCommand::Next),
        ),
    )
}

fn skip_prev_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "上一首".to_string(),
            "上一首",
            ModuleCommand::Playback(PlaybackCommand::Previous),
        ),
    )
}

fn volume_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let volume =
        query_value(query, "volume").ok_or_else(|| bad_request("volume参数必须是0-100"))?;
    if !is_valid_volume(volume) {
        return Err(bad_request("volume参数必须是0-100"));
    }
    enqueue_remote_command(
        state,
        remote_control_command(
            format!("音量 {}", volume),
            "音量",
            ModuleCommand::Playback(PlaybackCommand::Volume(volume.to_string())),
        ),
    )
}

fn startup_game_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_startup_game(state)
}

fn startup_wonderland_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_startup_wonderland(state)
}

fn enter_wonderland_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_enter_wonderland(state)
}

fn search_play_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_song(query, state, false)
}

fn search_source_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_song(query, state, false)
}

fn search_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let source = normalize_optional_source(query_value(query, "source"))?;
    state
        .player
        .search_text(&keyword, &source)
        .map_err(player_search_error)
}

fn search_candidates_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let source = normalize_optional_source(query_value(query, "source"))?;
    serde_json::to_string(
        &state
            .player
            .search_candidates(&keyword, &source)
            .map_err(player_search_error)?,
    )
    .map_err(internal_error)
}

fn player_providers_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(&state.login.providers().map_err(login_http_error)?)
        .map_err(internal_error)
}

fn player_login_status_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(&state.login.status()).map_err(internal_error)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginProviderRequest {
    provider: ProviderId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginCancelRequest {
    session_id: Uuid,
}

fn player_play_track_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let track = parse_json_body::<PlayableTrack>(body, "结构化曲目")?;
    validate_playable_track(&track)?;
    let current_uri = track.track_ref.key.to_string();
    state
        .native_playback
        .play_track(track.clone())
        .map_err(playback_http_error)?;
    Ok(json!({
        "ok": true,
        "played": true,
        "currentUri": current_uri,
        "track": track,
    })
    .to_string())
}

fn player_login_start_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request = parse_json_body::<LoginProviderRequest>(body, "登录请求")?;
    let session = state
        .login
        .start(request.provider)
        .map_err(login_http_error)?;
    Ok(json!({
        "ok": true,
        "sessionId": session.session_id,
        "provider": session.provider,
    })
    .to_string())
}

fn player_login_cancel_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request = parse_json_body::<LoginCancelRequest>(body, "取消登录请求")?;
    state
        .login
        .cancel(request.session_id)
        .map_err(login_http_error)?;
    Ok(json!({ "ok": true, "sessionId": request.session_id }).to_string())
}

fn player_logout_body_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let request = parse_json_body::<LoginProviderRequest>(body, "退出登录请求")?;
    let status = state
        .login
        .logout(request.provider)
        .map_err(login_http_error)?;
    serde_json::to_string(&json!({ "ok": true, "credential": status })).map_err(internal_error)
}

fn parse_json_body<T: DeserializeOwned>(
    body: &[u8],
    label: &str,
) -> std::result::Result<T, AppError> {
    if body.is_empty() {
        return Err(bad_request(&format!("{label}不能为空")));
    }
    serde_json::from_slice(body).map_err(|error| bad_request(&format!("{label} JSON无效: {error}")))
}

fn validate_playable_track(track: &PlayableTrack) -> std::result::Result<(), AppError> {
    const MAX_ID_BYTES: usize = 512;
    const MAX_TEXT_BYTES: usize = 1024;
    const MAX_ARTISTS: usize = 32;
    const MAX_DURATION_MS: u64 = 24 * 60 * 60 * 1000;

    let id = track.track_ref.key.id.as_str();
    if id.trim().is_empty()
        || id != id.trim()
        || id.contains('/')
        || id.len() > MAX_ID_BYTES
        || id.chars().any(char::is_control)
    {
        return Err(bad_request("trackRef.key.id字段无效"));
    }
    validate_track_text(
        &track.metadata.title,
        "metadata.title",
        true,
        MAX_TEXT_BYTES,
    )?;
    if track.metadata.artists.len() > MAX_ARTISTS {
        return Err(bad_request("metadata.artists数量过多"));
    }
    for artist in &track.metadata.artists {
        validate_track_text(artist, "metadata.artists", true, MAX_TEXT_BYTES)?;
    }
    if let Some(album) = track.metadata.album.as_deref() {
        validate_track_text(album, "metadata.album", false, MAX_TEXT_BYTES)?;
    }
    if track
        .metadata
        .duration_ms
        .is_some_and(|duration| duration == 0 || duration > MAX_DURATION_MS)
    {
        return Err(bad_request("metadata.durationMs字段无效"));
    }
    if let Some(locator) = track.track_ref.resolver_locator.as_ref() {
        validate_locator_identity(track.track_ref.key.provider, id, locator.as_str())?;
    }
    Ok(())
}

fn validate_track_text(
    value: &str,
    field: &str,
    required: bool,
    max_bytes: usize,
) -> std::result::Result<(), AppError> {
    if (required && value.trim().is_empty())
        || value != value.trim()
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
    {
        return Err(bad_request(&format!("{field}字段无效")));
    }
    Ok(())
}

fn validate_locator_identity(
    provider: ProviderId,
    track_id: &str,
    locator: &str,
) -> std::result::Result<(), AppError> {
    let mut parts = locator.split(':');
    let locator_provider = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    let locator_track = parts.next().unwrap_or_default();
    if locator_provider != provider.as_str() {
        return Err(bad_request("resolverLocator平台与trackRef不一致"));
    }
    if !matches!(version, "v1" | "v2") || locator_track.is_empty() {
        return Err(bad_request("resolverLocator格式无效"));
    }
    if locator_track != track_id {
        return Err(bad_request("resolverLocator曲目与trackRef不一致"));
    }
    Ok(())
}

fn queue_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    queue_json(state)
}

fn queue_add_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    queue_add(query, state)
}

fn queue_remove_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    queue_remove(query, state)
}

fn queue_clear_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    queue_clear(state)
}

fn state_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state_json(state)
}

fn state_save_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state_save(query, state)
}

fn chat_send_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    chat_send(query, state)
}

fn chat_listener_mode_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let mode = match normalize_required_text(query_value(query, "mode"), "mode")?.as_str() {
        "primary" | "一级" => ChatListenerMode::Primary,
        "secondary" | "二级" => ChatListenerMode::Secondary,
        _ => {
            return Err(AppError {
                status: 400,
                message: "mode 仅支持 primary 或 secondary".to_string(),
            });
        }
    };
    let BusinessMutationOutcome::Administration(
        AdministrationMutationOutcome::ChatListenerModeRequested { queued, snapshot },
    ) = state
        .formal_tasks
        .apply_mutation(BusinessMutationIntent::Administration(
            AdministrationMutationIntent::RequestChatListenerMode(mode),
        ))
        .map_err(internal_error)?
    else {
        unreachable!("chat listener mode intent returned a different outcome")
    };
    if !queued {
        return Ok(json!({
            "ok": true,
            "queued": false,
            "mode": snapshot.mode,
            "pendingMode": snapshot.pending_mode,
        })
        .to_string());
    }
    let receipt = match required_enqueue_receipt(state.formal_tasks.enqueue_listener_mode(mode)) {
        Ok(receipt) => receipt,
        Err(error) => {
            let _ = state
                .formal_tasks
                .apply_mutation(BusinessMutationIntent::Administration(
                    AdministrationMutationIntent::CancelChatListenerModeRequest(mode),
                ));
            return Err(error);
        }
    };
    Ok(json!({
        "ok": true,
        "queued": true,
        "taskId": receipt.task_id,
        "position": receipt.position,
        "mode": snapshot.mode,
        "pendingMode": mode,
    })
    .to_string())
}

fn task_cancel_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let task_id = normalize_required_text(query_value(query, "id"), "id")?
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| bad_request("无效的任务ID"))?;
    let outcome = state
        .formal_tasks
        .cancel_task(task_id)
        .map_err(internal_error)?;
    match outcome {
        FormalTaskCancelOutcome::CanceledBeforeStart => Ok(json!({
            "ok": true,
            "taskId": task_id,
            "canceled": true,
            "cancellationRequested": false,
        })
        .to_string()),
        FormalTaskCancelOutcome::CancellationRequested => Ok(json!({
            "ok": true,
            "taskId": task_id,
            "canceled": false,
            "cancellationRequested": true,
        })
        .to_string()),
        FormalTaskCancelOutcome::AlreadyFinished => Err(AppError {
            status: 409,
            message: "任务已经结束，最终结果不会再改变".to_string(),
        }),
        FormalTaskCancelOutcome::NotFound => Err(AppError {
            status: 404,
            message: "没有找到该任务".to_string(),
        }),
    }
}

fn decision_submit_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let id = normalize_required_text(query_value(query, "id"), "id")?
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| bad_request("无效的决策ID"))?;
    let action_text = normalize_required_text(query_value(query, "action"), "action")?;
    let action = DecisionAction::parse(&action_text)
        .ok_or_else(|| bad_request("action仅支持confirm、skip、switch_source或ai"))?;
    state
        .formal_tasks
        .submit_decision(id, action)
        .map_err(|error| AppError {
            status: 409,
            message: error.to_string(),
        })?;
    Ok(json!({ "ok": true, "decisionId": id, "submitted": action_text }).to_string())
}

fn operator_lyrics_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "歌词".to_string(),
            "歌词",
            ModuleCommand::Playback(PlaybackCommand::Lyrics),
        ),
    )
}

fn operator_hall_detect_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "大厅检测".to_string(),
            "大厅检测",
            ModuleCommand::Hall(HallCommand::Detect),
        ),
    )
}

fn operator_hall_time_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "大厅时间".to_string(),
            "大厅时间",
            ModuleCommand::Hall(HallCommand::Time),
        ),
    )
}

fn operator_microphone_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "麦克风".to_string(),
            "麦克风",
            ModuleCommand::Hall(HallCommand::ToggleMicrophone {
                username: "控制台".to_string(),
            }),
        ),
    )
}

fn operator_commands_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let enabled = match normalize_required_text(query_value(query, "enabled"), "enabled")?
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "on" | "enable" | "enabled" => true,
        "0" | "false" | "off" | "disable" | "disabled" => false,
        _ => return Err(bad_request("enabled参数必须是1或0")),
    };
    let (raw, command) = if enabled {
        (
            "启用".to_string(),
            ModuleCommand::Administration(AdministrationCommand::SetCommandsEnabled {
                enabled: true,
                username: "控制台".to_string(),
            }),
        )
    } else {
        (
            "禁用".to_string(),
            ModuleCommand::Administration(AdministrationCommand::SetCommandsEnabled {
                enabled: false,
                username: "控制台".to_string(),
            }),
        )
    };
    enqueue_remote_command(state, remote_control_command(raw.clone(), &raw, command))
}

fn operator_idle_exit_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    if let Some(enabled) = query_value(query, "enabled") {
        match enabled.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "off" | "disabled" => {
                let receipt =
                    required_enqueue_receipt(state.formal_tasks.enqueue_clear_idle_exit())?;
                return Ok(json!({
                    "ok": true,
                    "queued": true,
                    "taskId": receipt.task_id,
                    "position": receipt.position,
                    "command": "取消闲置退出"
                })
                .to_string());
            }
            "1" | "true" | "on" | "enabled" => {}
            _ => return Err(bad_request("enabled参数必须是1或0")),
        }
    }
    let minutes = normalize_required_text(query_value(query, "minutes"), "minutes")?
        .parse::<u32>()
        .ok()
        .filter(|minutes| (15..=1440).contains(minutes))
        .ok_or_else(|| bad_request("minutes参数必须在15到1440之间"))?;
    enqueue_remote_command(
        state,
        remote_control_command(
            format!("闲置退出 {minutes}"),
            "闲置退出",
            ModuleCommand::Administration(AdministrationCommand::IdleExit { minutes }),
        ),
    )
}

fn operator_workflows_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let workflows = state
        .custom_workflow
        .list()
        .into_iter()
        .map(|workflow| {
            json!({
                "name": workflow.name,
                "commands": workflow.commands,
                "allowArgs": workflow.allow_args,
                "confirmBeforeRun": workflow.confirm_before_run,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&workflows).map_err(internal_error)
}

fn operator_workflow_run_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    if !state.custom_workflow.enabled() {
        return Err(bad_request("自定义工作流未启用"));
    }
    let name = normalize_required_text(query_value(query, "name"), "name")?;
    let args = normalize_optional_text(query_value(query, "args"), "args")?;
    let prepared = state
        .custom_workflow
        .prepare_remote(&name, &args)
        .map_err(|error| bad_request(&error.to_string()))?;
    enqueue_remote_command(
        state,
        remote_control_command(
            prepared.raw,
            &prepared.matched,
            ModuleCommand::CustomWorkflow(prepared.command),
        ),
    )
}

fn ai_recognize_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state.ai.recognize(query).map_err(ai_route_error)
}

fn ai_match_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state.ai.match_song(query).map_err(ai_route_error)
}

fn ai_pick_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    state.ai.pick(query).map_err(ai_route_error)
}

fn ai_search_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_song(query, state, true)
}

fn history_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    history_json(state)
}

fn clear_history_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    clear_history(state)
}

fn monitor_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    monitor_json(state)
}

fn turtle_soup_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    serde_json::to_string(
        &state
            .queries
            .turtle_soup_snapshot()
            .map_err(internal_error)?,
    )
    .map_err(internal_error)
}

fn turtle_soup_start_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let puzzle_id = query_value(query, "id")
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    let BusinessMutationOutcome::TurtleSoup(outcome) = state
        .formal_tasks
        .apply_mutation(BusinessMutationIntent::TurtleSoup(
            TurtleSoupMutationIntent::Start { puzzle_id },
        ))
        .map_err(internal_error)?
    else {
        unreachable!("turtle soup start intent returned a different outcome")
    };
    let TurtleSoupMutationOutcome::Started(snapshot) = *outcome else {
        unreachable!("turtle soup start intent returned a different outcome")
    };
    serde_json::to_string(&json!({
        "ok": true,
        "turtleSoup": snapshot,
    }))
    .map_err(internal_error)
}

fn turtle_soup_end_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let BusinessMutationOutcome::TurtleSoup(outcome) = state
        .formal_tasks
        .apply_mutation(BusinessMutationIntent::TurtleSoup(
            TurtleSoupMutationIntent::End,
        ))
        .map_err(internal_error)?
    else {
        unreachable!("turtle soup end intent returned a different outcome")
    };
    let TurtleSoupMutationOutcome::Ended { ended, snapshot } = *outcome else {
        unreachable!("turtle soup end intent returned a different outcome")
    };
    if !ended {
        return Err(AppError {
            status: 409,
            message: "当前没有可结束的海龟汤".to_string(),
        });
    }
    serde_json::to_string(&json!({
        "ok": true,
        "turtleSoup": snapshot,
    }))
    .map_err(internal_error)
}

fn turtle_soup_questions_route(
    body: &[u8],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let submission =
        serde_json::from_slice::<TurtleSoupSubmission>(body).map_err(|error| AppError {
            status: 400,
            message: format!("海龟汤提交JSON无效: {error}"),
        })?;
    if submission.title.trim().is_empty()
        || submission.surface.trim().is_empty()
        || submission.bottom.trim().is_empty()
    {
        return Err(bad_request("海龟汤标题、汤面和汤底不能为空"));
    }
    let BusinessMutationOutcome::TurtleSoup(outcome) = state
        .formal_tasks
        .apply_mutation(BusinessMutationIntent::TurtleSoup(
            TurtleSoupMutationIntent::AppendPuzzle(submission),
        ))
        .map_err(internal_error)?
    else {
        unreachable!("turtle soup append intent returned a different outcome")
    };
    let TurtleSoupMutationOutcome::PuzzleAppended(receipt) = *outcome else {
        unreachable!("turtle soup append intent returned a different outcome")
    };
    serde_json::to_string(&receipt).map_err(internal_error)
}

fn undercover_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let snapshot = state
        .queries
        .undercover_snapshot()
        .map_err(internal_error)?;
    serde_json::to_string(&snapshot).map_err(internal_error)
}

fn undercover_start_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "卧底开局".to_string(),
            "卧底",
            ModuleCommand::Undercover(UndercoverCommand::Start),
        ),
    )
}

fn undercover_end_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command(
            "卧底结束".to_string(),
            "卧底",
            ModuleCommand::Undercover(UndercoverCommand::End),
        ),
    )
}

fn tool_task_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let id = parse_tool_id(query)?;
    let snapshot = state
        .queries
        .diagnostic_task_snapshot(id)
        .map_err(internal_error)?
        .ok_or_else(|| AppError {
            status: 404,
            message: "Web 工具任务不存在或已过期".to_string(),
        })?;
    serde_json::to_string(&snapshot).map_err(internal_error)
}

fn tool_templates_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let marker_threshold = state.config.templates.marker_threshold;
    let mut templates = vec![
        json!({ "name": "blue-marker", "label": "蓝色聊天标志", "region": state.config.screen.chat_rect, "threshold": marker_threshold }),
        json!({ "name": "yellow-marker", "label": "黄色聊天标志", "region": state.config.screen.chat_rect, "threshold": marker_threshold }),
        json!({ "name": "pink-marker", "label": "粉色聊天标志", "region": state.config.screen.chat_rect, "threshold": marker_threshold }),
        json!({ "name": "friend", "label": "好友按钮", "region": state.config.screen.friend_rect, "threshold": marker_threshold }),
        json!({ "name": "secondary-back", "label": "二级聊天返回按钮", "region": state.config.screen.secondary_back_rect, "threshold": marker_threshold }),
        json!({ "name": "secondary-hall", "label": "二级当前大厅", "region": state.config.screen.secondary_hall_rect, "threshold": marker_threshold }),
        json!({ "name": "invite-view-star", "label": "邀请查看千星", "region": state.config.invite.view_star_region, "threshold": marker_threshold }),
        json!({ "name": "invite-goto-hall", "label": "邀请前往大厅", "region": state.config.invite.goto_hall_region, "threshold": marker_threshold }),
        json!({ "name": "invite-enter-hall", "label": "邀请进入大厅", "region": state.config.invite.enter_hall_region, "threshold": marker_threshold }),
        json!({ "name": "friend-panel", "label": "好友面板", "region": state.config.moderation.friend_panel_region, "threshold": marker_threshold }),
        json!({ "name": "friend-search-panel", "label": "好友搜索面板", "region": state.config.moderation.search_panel_region, "threshold": marker_threshold }),
        json!({ "name": "friend-more-settings", "label": "好友更多设置", "region": state.config.moderation.more_settings_region, "threshold": marker_threshold }),
        json!({ "name": "friend-block-chat", "label": "屏蔽聊天", "region": state.config.moderation.block_chat_region, "threshold": marker_threshold }),
        json!({ "name": "friend-blacklist", "label": "拉黑", "region": state.config.moderation.blacklist_region, "threshold": marker_threshold }),
        json!({ "name": "friend-confirm", "label": "好友操作确认", "region": state.config.moderation.confirm_region, "threshold": marker_threshold }),
        json!({ "name": "wonderland-confirm", "label": "千星确认按钮", "region": state.config.startup.wonderland_confirm_region, "threshold": state.config.startup.wonderland_confirm_threshold }),
        json!({ "name": "paimon-menu", "label": "派蒙主界面", "region": state.config.startup.main_ui_region, "threshold": state.config.startup.template_threshold }),
        json!({ "name": "wonderland-map-star", "label": "千星地图入口", "region": state.config.startup.wonderland_map_star_region, "threshold": state.config.startup.template_threshold }),
    ];
    let mut custom = state
        .config
        .custom_workflows
        .templates
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    custom.sort();
    templates.extend(custom.into_iter().map(|name| {
        json!({
            "name": name,
            "label": format!("自定义: {name}"),
            "region": null,
            "threshold": state.config.custom_workflows.default_threshold,
        })
    }));
    serde_json::to_string(&templates).map_err(internal_error)
}

fn tool_ocr_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let rect = query_value(query, "rect")
        .filter(|value| !value.trim().is_empty())
        .map(parse_rect)
        .transpose()
        .map_err(|error| bad_request(&format!("rect参数无效: {error}")))?;
    enqueue_web_tool(state, WebToolRequest::Ocr { rect })
}

fn tool_scan_chat_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_web_tool(state, WebToolRequest::ScanChat)
}

fn tool_ui_state_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_web_tool(state, WebToolRequest::UiState)
}

fn tool_hall_name_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_web_tool(state, WebToolRequest::HallName)
}

fn tool_template_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let name = normalize_required_text(query_value(query, "template"), "template")?;
    let template = WebToolTemplate::parse(&name, &state.config.custom_workflows.templates)
        .map_err(|error| bad_request(&error.to_string()))?;
    let rect = query_value(query, "rect")
        .filter(|value| !value.trim().is_empty())
        .map(parse_rect)
        .transpose()
        .map_err(|error| bad_request(&format!("rect参数无效: {error}")))?;
    let threshold = query_value(query, "threshold")
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value
                .trim()
                .parse::<f32>()
                .map_err(|_| bad_request("threshold参数必须是0到1之间的小数"))
        })
        .transpose()?;
    if threshold.is_some_and(|value| !(0.0..=1.0).contains(&value)) {
        return Err(bad_request("threshold参数必须是0到1之间的小数"));
    }
    enqueue_web_tool(
        state,
        WebToolRequest::MatchTemplate {
            template,
            rect,
            threshold,
            click: parse_bool(query_value(query, "click")),
        },
    )
}

fn tool_click_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let x = parse_coordinate(query_value(query, "x"), "x")?;
    let y = parse_coordinate(query_value(query, "y"), "y")?;
    enqueue_web_tool(state, WebToolRequest::Click { x, y })
}

fn tool_key_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let key = normalize_required_text(query_value(query, "key"), "key")?;
    if key.chars().count() > 40 {
        return Err(bad_request("key参数过长"));
    }
    enqueue_web_tool(state, WebToolRequest::Key { key })
}

fn tool_chat_change_samples_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let samples = parse_bounded_u32(query_value(query, "samples"), "samples", 1, 30, 10)?;
    let interval_ms = parse_bounded_u64(
        query_value(query, "intervalMs"),
        "intervalMs",
        50,
        5_000,
        state.config.timing.loop_idle_ms,
    )?;
    enqueue_web_tool(
        state,
        WebToolRequest::ChatChangeSamples {
            samples,
            interval_ms,
        },
    )
}

fn tool_panel_benchmark_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let rounds = parse_bounded_u32(query_value(query, "rounds"), "rounds", 1, 10, 3)?;
    enqueue_web_tool(state, WebToolRequest::PanelResponseBenchmark { rounds })
}

fn tool_ocr_backends_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_web_tool(state, WebToolRequest::OcrBackendProbe)
}

fn tool_ai_preview_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let prefer_accompaniment = parse_bool(query_value_or(
        query,
        "preferAccompaniment",
        "accompaniment",
    ));
    enqueue_web_tool(
        state,
        WebToolRequest::AiSearchPreview {
            keyword,
            prefer_accompaniment,
        },
    )
}

fn enqueue_web_tool(
    state: &HttpSharedState,
    request: WebToolRequest,
) -> std::result::Result<String, AppError> {
    let snapshot = state
        .formal_tasks
        .enqueue_diagnostic(request)
        .map_err(|error| AppError {
            status: if error.to_string().contains("任务过多") {
                429
            } else {
                500
            },
            message: error.to_string(),
        })?;
    serde_json::to_string(&snapshot).map_err(internal_error)
}

fn parse_tool_id(query: &[(String, String)]) -> std::result::Result<u64, AppError> {
    query_value(query, "id")
        .ok_or_else(|| bad_request("缺少id参数"))?
        .parse::<u64>()
        .map_err(|_| bad_request("id参数无效"))
}

fn parse_coordinate(value: Option<&str>, name: &str) -> std::result::Result<i32, AppError> {
    normalize_required_text(value, name)?
        .parse::<i32>()
        .map_err(|_| bad_request(&format!("{}参数必须是整数", name)))
}

fn parse_bounded_u32(
    value: Option<&str>,
    name: &str,
    min: u32,
    max: u32,
    default: u32,
) -> std::result::Result<u32, AppError> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(default);
    };
    let parsed = value
        .parse::<u32>()
        .map_err(|_| bad_request(&format!("{}参数必须是整数", name)))?;
    if (min..=max).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(bad_request(&format!(
            "{}参数必须在{}到{}之间",
            name, min, max
        )))
    }
}

fn parse_bounded_u64(
    value: Option<&str>,
    name: &str,
    min: u64,
    max: u64,
    default: u64,
) -> std::result::Result<u64, AppError> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(default);
    };
    let parsed = value
        .parse::<u64>()
        .map_err(|_| bad_request(&format!("{}参数必须是整数", name)))?;
    if (min..=max).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(bad_request(&format!(
            "{}参数必须在{}到{}之间",
            name, min, max
        )))
    }
}

fn health_route(
    _query: &[(String, String)],
    _state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    Ok("OK".to_string())
}

fn ai_route_error(error: anyhow::Error) -> AppError {
    AppError {
        status: if is_client_error(&error.to_string()) {
            400
        } else {
            500
        },
        message: error.to_string(),
    }
}

fn enqueue_remote_command(
    state: &HttpSharedState,
    intent: ConsoleCommandIntent,
) -> std::result::Result<String, AppError> {
    let pending = intent.into_pending();
    let command = pending.routed.raw.clone();
    let queued = enqueue_pending_command(state, pending)?;
    let task_id = queued.map(|receipt| receipt.task_id);
    let position = queued.map_or(0, |receipt| receipt.position);
    Ok(json!({
        "ok": true,
        "queued": queued.is_some(),
        "duplicate": queued.is_none(),
        "taskId": task_id,
        "position": position,
        "command": command,
    })
    .to_string())
}

fn enqueue_startup_game(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    enqueue_startup_task_response(
        state,
        "启动游戏",
        [StartupTask::start_game(StartupSource::REMOTE_CONSOLE)],
    )
}

fn enqueue_enter_wonderland(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    enqueue_startup_task_response(
        state,
        "进入千星",
        [StartupTask::enter_wonderland(StartupSource::REMOTE_CONSOLE)],
    )
}

fn enqueue_startup_wonderland(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    enqueue_startup_task_response(
        state,
        "启动游戏并进入千星",
        [
            StartupTask::start_game(StartupSource::REMOTE_CONSOLE),
            StartupTask::enter_wonderland(StartupSource::REMOTE_CONSOLE),
        ],
    )
}

fn enqueue_startup_task_response<const N: usize>(
    state: &HttpSharedState,
    task_label: &'static str,
    tasks: [StartupTask; N],
) -> std::result::Result<String, AppError> {
    let mut receipts = Vec::with_capacity(N);
    for task in tasks {
        receipts.push(required_enqueue_receipt(
            state.formal_tasks.enqueue_startup(task),
        )?);
    }
    let positions = receipts
        .iter()
        .map(|receipt| receipt.position)
        .collect::<Vec<_>>();
    let task_ids = receipts
        .iter()
        .map(|receipt| receipt.task_id)
        .collect::<Vec<_>>();
    let mut response = json!({
        "ok": true,
        "queued": true,
        "task": task_label,
    });
    if let Some(object) = response.as_object_mut() {
        if receipts.len() == 1 {
            object.insert("position".to_string(), json!(positions[0]));
            object.insert("taskId".to_string(), json!(task_ids[0]));
        } else {
            object.insert("positions".to_string(), json!(positions));
            object.insert("taskIds".to_string(), json!(task_ids));
        }
    }
    Ok(response.to_string())
}

fn remote_control_command(
    raw: String,
    matched: &str,
    command: ModuleCommand,
) -> ConsoleCommandIntent {
    ConsoleCommandIntent::new(raw, matched, command)
}

fn enqueue_remote_song(
    query: &[(String, String)],
    state: &HttpSharedState,
    ai_assisted: bool,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let source = normalize_source(query_value(query, "source"))?;
    let prefer_accompaniment = parse_bool(query_value_or(
        query,
        "preferAccompaniment",
        "accompaniment",
    ));
    enqueue_remote_command(
        state,
        remote_song_command(keyword, source, prefer_accompaniment, ai_assisted)?,
    )
}

fn remote_song_command(
    keyword: String,
    source: String,
    prefer_accompaniment: bool,
    ai_assisted: bool,
) -> std::result::Result<ConsoleCommandIntent, AppError> {
    let contains_accompaniment = keyword.contains("伴奏");
    let keyword = keyword
        .replace("伴奏", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if keyword.trim().is_empty() {
        return Err(bad_request("缺少keyword参数"));
    }

    let prefer_accompaniment = prefer_accompaniment || contains_accompaniment;
    let (prefix, song_source) = if ai_assisted {
        ("AI点歌", SongSource::All)
    } else {
        match source.as_str() {
            "qqmusic" => ("点歌", SongSource::QqMusic),
            "netease" => ("网易点歌", SongSource::Netease),
            "bilibili" => ("B站点歌", SongSource::Bilibili),
            _ => {
                return Err(bad_request(
                    "远程点歌source只允许qqmusic、netease或bilibili",
                ));
            }
        }
    };
    let raw = if prefer_accompaniment {
        format!("{} {} 伴奏", prefix, keyword)
    } else {
        format!("{} {}", prefix, keyword)
    };
    let command = ModuleCommand::SongRequest(SongCommand {
        keyword,
        source: song_source,
        prefix: prefix.to_string(),
        prefer_accompaniment,
        ai_assisted,
        friend_username: String::new(),
    });
    Ok(ConsoleCommandIntent::new(raw, prefix, command))
}

fn queue_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    serde_json::to_string(
        &state
            .queries
            .playback_queue_snapshot()
            .map_err(internal_error)?,
    )
    .map_err(internal_error)
}

fn queue_add(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let keyword = normalize_keyword(query_value(query, "keyword"))?;
    let source = normalize_source(query_value(query, "source"))?;
    let prefer = parse_bool(query_value_or(
        query,
        "preferAccompaniment",
        "accompaniment",
    ));
    let ai_original_text =
        normalize_optional_text(query_value(query, "aiOriginalText"), "aiOriginalText")?;
    let requester = requester_from_query(query)?;
    let BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Pushed(pushed)) = state
        .formal_tasks
        .apply_mutation(BusinessMutationIntent::Playback(
            PlaybackMutationIntent::Push(Box::new(QueueItem {
                id: 0,
                keyword,
                source,
                prefer_accompaniment: prefer,
                ai_original_text,
                track: None,
                friend_username: String::new(),
                requester,
                dedup_bypass: true,
                candidate_snapshot: Vec::new(),
            })),
        ))
        .map_err(internal_error)?
    else {
        unreachable!("playback queue push intent returned a different outcome")
    };
    if !pushed.accepted {
        return Err(AppError {
            status: 400,
            message: "队列已满".to_string(),
        });
    }
    Ok(json!({ "ok": true, "size": pushed.size }).to_string())
}

fn requester_from_query(query: &[(String, String)]) -> std::result::Result<String, AppError> {
    let requester = normalize_optional_text(query_value(query, "requester"), "requester")?;
    Ok(if requester.is_empty() {
        "WEB/API".to_string()
    } else {
        requester
    })
}

fn queue_remove(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let removal = if let Some(id_text) = query_value(query, "id").filter(|value| !value.is_empty())
    {
        let id = id_text
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| bad_request("无效的队列项ID"))?;
        QueueRemoval::Id(id)
    } else if let Some(index_text) = query_value(query, "index") {
        if !index_text.is_empty() {
            let index = index_text
                .parse::<usize>()
                .map_err(|_| bad_request("无效的队列索引"))?;
            QueueRemoval::Index(index)
        } else {
            QueueRemoval::Front
        }
    } else {
        QueueRemoval::Front
    };
    let BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Removed(removed)) = state
        .formal_tasks
        .apply_mutation(BusinessMutationIntent::Playback(
            PlaybackMutationIntent::Remove(removal),
        ))
        .map_err(internal_error)?
    else {
        unreachable!("playback queue remove intent returned a different outcome")
    };
    let QueueRemoveOutcome::Removed { index, item, size } = removed else {
        return Err(match removed {
            QueueRemoveOutcome::MissingId => AppError {
                status: 409,
                message: "队列已发生变化，请刷新后重试".to_string(),
            },
            QueueRemoveOutcome::InvalidIndex => bad_request("无效的队列索引"),
            QueueRemoveOutcome::Empty => bad_request("队列为空"),
            QueueRemoveOutcome::Removed { .. } => unreachable!(),
        });
    };
    Ok(json!({
        "ok": true,
        "size": size,
        "removed": {
            "index": index,
            "id": item.id,
            "keyword": item.keyword,
        }
    })
    .to_string())
}

fn queue_clear(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let BusinessMutationOutcome::Playback(PlaybackMutationOutcome::Cleared) = state
        .formal_tasks
        .apply_mutation(BusinessMutationIntent::Playback(
            PlaybackMutationIntent::Clear,
        ))
        .map_err(internal_error)?
    else {
        unreachable!("playback queue clear intent returned a different outcome")
    };
    Ok(json!({ "ok": true }).to_string())
}

fn state_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let mut playback = serde_json::to_value(
        state
            .queries
            .playback_state_snapshot()
            .map_err(internal_error)?,
    )
    .map_err(internal_error)?;
    if let Some(object) = playback.as_object_mut() {
        object.remove("previousRequests");
    }
    let hall = state
        .queries
        .hall_state_snapshot()
        .map_err(internal_error)?;
    serde_json::to_string(&json!({
        "playback": playback,
        "hallRemainingMinutes": hall.remaining_minutes,
        "hallRemainingUpdatedAt": hall.remaining_updated_at,
        "hallExpiringWarningSent": hall.expiring_warning_sent,
        "hallRemainingMinutesNow": hall.remaining_minutes_now(),
    }))
    .map_err(internal_error)
}

fn state_save(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let text = query_value(query, "json").unwrap_or("{}");
    let patch: HashMap<String, serde_json::Value> =
        serde_json::from_str(text).map_err(|error| AppError {
            status: 400,
            message: format!("json参数无效: {}", error),
        })?;
    let BusinessMutationOutcome::Hall(HallMutationOutcome::StatePatched) = state
        .formal_tasks
        .apply_mutation(BusinessMutationIntent::Hall(
            HallMutationIntent::PatchState(hall_state_patch(&patch)),
        ))
        .map_err(internal_error)?
    else {
        unreachable!("runtime state patch intent returned a different outcome")
    };
    Ok(json!({ "ok": true }).to_string())
}

fn history_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let history = state
        .history
        .lock()
        .map_err(|_| internal_message("历史锁已损坏"))?;
    serde_json::to_string(&*history).map_err(internal_error)
}

fn clear_history(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    state
        .history
        .lock()
        .map_err(|_| internal_message("历史锁已损坏"))?
        .clear();
    Ok("命令记录已清空".to_string())
}

fn screenshot_response(
    request: &Request,
    state: &HttpSharedState,
) -> std::result::Result<Response, AppError> {
    let quality = parse_jpeg_quality(query_value(&request.query, "quality"))?;
    cached_screenshot_response(
        request,
        quality,
        &state.latest_frame,
        "尚未获取主扫描画面，请稍后重试",
        &state.config.http.host,
        state.config.http.port,
    )
}

fn hall_screenshot_response(
    request: &Request,
    state: &HttpSharedState,
) -> std::result::Result<Response, AppError> {
    let quality = parse_jpeg_quality(query_value(&request.query, "quality"))?;
    let image = state
        .hall
        .capture_hall_screenshot()
        .map_err(|error| AppError {
            status: 503,
            message: format!("主动检测大厅失败: {error:#}"),
        })?;
    encoded_screenshot_response(
        request,
        quality,
        &image,
        &state.config.http.host,
        state.config.http.port,
    )
}

fn cached_screenshot_response(
    request: &Request,
    quality: u8,
    cache: &Arc<Mutex<LatestFrameCache>>,
    unavailable_message: &str,
    host: &str,
    port: u16,
) -> std::result::Result<Response, AppError> {
    let image = cache
        .lock()
        .map_err(|_| internal_message("截图缓存锁已损坏"))?
        .image()
        .ok_or_else(|| AppError {
            status: 503,
            message: unavailable_message.to_string(),
        })?;
    encoded_screenshot_response(request, quality, &image, host, port)
}

fn encoded_screenshot_response(
    request: &Request,
    quality: u8,
    image: &DynamicImage,
    host: &str,
    port: u16,
) -> std::result::Result<Response, AppError> {
    let rgb = image.to_rgb8();
    let mut bytes = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut bytes, quality);
    encoder
        .encode(&rgb, rgb.width(), rgb.height(), ColorType::Rgb8.into())
        .map_err(internal_error)?;
    Ok(bytes_response(
        StatusCode::OK,
        "image/jpeg",
        bytes,
        cors_headers(request, host, port),
    ))
}

fn monitor_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    serde_json::to_string(&state.monitor.snapshot()).map_err(internal_error)
}

fn chat_send(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let text = normalize_required_text(query_value(query, "text"), "text")?;
    let use_prefix = parse_bool_default(
        query_value(query, "usePrefix")
            .or_else(|| query_value(query, "prefixEnabled"))
            .or_else(|| query_value(query, "withPrefix")),
        true,
    );
    let prefix = if use_prefix {
        normalize_optional_raw_text(
            query_value(query, "prefix").or(Some("[控制台]: ")),
            "prefix",
        )?
    } else {
        String::new()
    };
    let message = format!("{}{}", prefix, text);
    let receipt = required_enqueue_receipt(state.formal_tasks.enqueue_console_chat(text, prefix))?;
    Ok(json!({
        "ok": true,
        "queued": true,
        "taskId": receipt.task_id,
        "position": receipt.position,
        "message": message
    })
    .to_string())
}

fn required_enqueue_receipt(
    outcome: Result<FormalTaskEnqueueOutcome, impl std::fmt::Display>,
) -> std::result::Result<EnqueueReceipt, AppError> {
    let outcome = outcome.map_err(internal_error)?;
    match outcome {
        FormalTaskEnqueueOutcome::Queued(receipt) => Ok(EnqueueReceipt {
            task_id: receipt.task_id,
            position: receipt.position,
        }),
        FormalTaskEnqueueOutcome::Duplicate => Err(AppError {
            status: 409,
            message: "任务已在待执行范围内".to_string(),
        }),
    }
}

fn enqueue_pending_command(
    state: &HttpSharedState,
    pending: PendingCommand,
) -> std::result::Result<Option<EnqueueReceipt>, AppError> {
    let outcome = state
        .formal_tasks
        .enqueue_command(pending)
        .map_err(internal_error)?;
    Ok(match outcome {
        FormalTaskEnqueueOutcome::Queued(receipt) => Some(EnqueueReceipt {
            task_id: receipt.task_id,
            position: receipt.position,
        }),
        FormalTaskEnqueueOutcome::Duplicate => None,
    })
}

fn push_history(request: &Request, result: &str, ok: bool, state: &HttpSharedState) {
    if request.path.starts_with("/tools/")
        || matches!(
            request.path.as_str(),
            "/history"
                | "/clear-history"
                | "/monitor"
                | "/screenshot"
                | "/hall-screenshot"
                | "/favicon.ico"
        )
    {
        return;
    }
    if let Ok(mut history) = state.history.lock() {
        history.push_front(HistoryItem {
            time: current_time_text(),
            command: request.path.clone(),
            query: sanitized_query(&request.query),
            result: result.to_string(),
            ok,
        });
        while history.len() > 30 {
            history.pop_back();
        }
    }
}

fn hall_state_patch(patch: &HashMap<String, serde_json::Value>) -> HallStatePatch {
    HallStatePatch {
        remaining_minutes: patch.get("hallRemainingMinutes").and_then(|value| {
            if value.is_null() {
                Some(None)
            } else {
                value.as_u64().map(|minutes| u32::try_from(minutes).ok())
            }
        }),
        remaining_updated_at: patch.get("hallRemainingUpdatedAt").and_then(|value| {
            if value.is_null() {
                Some(None)
            } else {
                value.as_u64().map(Some)
            }
        }),
        expiring_warning_sent: patch
            .get("hallExpiringWarningSent")
            .and_then(serde_json::Value::as_bool),
    }
}

fn query_value<'a>(query: &'a [(String, String)], key: &str) -> Option<&'a str> {
    query
        .iter()
        .rev()
        .find(|(item_key, _)| item_key == key)
        .map(|(_, value)| value.as_str())
}

fn query_value_or<'a>(
    query: &'a [(String, String)],
    primary: &str,
    fallback: &str,
) -> Option<&'a str> {
    match query_value(query, primary) {
        Some(value) if !value.is_empty() => Some(value),
        _ => query_value(query, fallback),
    }
}

fn normalize_keyword(value: Option<&str>) -> std::result::Result<String, AppError> {
    let keyword = assert_no_control_chars(value.unwrap_or(""), "keyword")?
        .trim()
        .to_string();
    if keyword.is_empty() {
        Err(bad_request("缺少keyword参数"))
    } else {
        Ok(keyword)
    }
}

fn normalize_source(value: Option<&str>) -> std::result::Result<String, AppError> {
    let raw = value.unwrap_or("qqmusic");
    let raw = if raw.is_empty() { "qqmusic" } else { raw };
    validate_source(raw)
}

fn normalize_optional_source(value: Option<&str>) -> std::result::Result<String, AppError> {
    validate_source(value.unwrap_or(""))
}

fn validate_source(raw: &str) -> std::result::Result<String, AppError> {
    let text = assert_no_control_chars(raw, "source")?.trim().to_string();
    if text.is_empty() {
        return Ok(text);
    }
    for part in text.split(',').map(str::trim) {
        if !part.is_empty() && part != "qqmusic" && part != "netease" && part != "bilibili" {
            return Err(bad_request("source参数只允许qqmusic、netease或bilibili"));
        }
    }
    Ok(text)
}

fn normalize_optional_text(
    value: Option<&str>,
    name: &str,
) -> std::result::Result<String, AppError> {
    Ok(assert_no_control_chars(value.unwrap_or(""), name)?
        .trim()
        .to_string())
}

fn normalize_optional_raw_text(
    value: Option<&str>,
    name: &str,
) -> std::result::Result<String, AppError> {
    assert_no_control_chars(value.unwrap_or(""), name)
}

fn normalize_required_text(
    value: Option<&str>,
    name: &str,
) -> std::result::Result<String, AppError> {
    let text = normalize_optional_text(value, name)?;
    if text.is_empty() {
        Err(bad_request(&format!("缺少{}参数", name)))
    } else {
        Ok(text)
    }
}

fn assert_no_control_chars(value: &str, name: &str) -> std::result::Result<String, AppError> {
    if value.chars().any(char::is_control) {
        Err(bad_request(&format!("{}不能包含控制字符", name)))
    } else {
        Ok(value.to_string())
    }
}

fn is_valid_volume(value: &str) -> bool {
    if value == "100" {
        return true;
    }
    let bytes = value.as_bytes();
    match bytes.len() {
        1 => bytes[0].is_ascii_digit(),
        2 => bytes[0].is_ascii_digit() && bytes[0] != b'0' && bytes[1].is_ascii_digit(),
        _ => false,
    }
}

fn parse_bool(value: Option<&str>) -> bool {
    matches!(value.unwrap_or(""), "1" | "true" | "yes" | "on")
}

fn parse_bool_default(value: Option<&str>, default: bool) -> bool {
    value.map_or(default, |value| parse_bool(Some(value)))
}

fn parse_jpeg_quality(value: Option<&str>) -> std::result::Result<u8, AppError> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(88);
    };
    let quality = value
        .trim()
        .parse::<u8>()
        .map_err(|_| bad_request("quality参数必须是80-95"))?;
    if (80..=95).contains(&quality) {
        Ok(quality)
    } else {
        Err(bad_request("quality参数必须是80-95"))
    }
}

fn is_client_error(message: &str) -> bool {
    message.contains("缺少")
        || message.contains("格式无效")
        || message.contains("只允许")
        || message.contains("控制字符")
        || message.contains("字段无效")
}

fn sanitized_query(query: &[(String, String)]) -> HashMap<String, String> {
    query
        .iter()
        .map(|(key, value)| {
            let value = if key.eq_ignore_ascii_case("apiKey")
                || key.eq_ignore_ascii_case("api_key")
                || key.eq_ignore_ascii_case("token")
                || key.eq_ignore_ascii_case("access_token")
                || key.eq_ignore_ascii_case("authorization")
                || key.eq_ignore_ascii_case("password")
            {
                "***".to_string()
            } else {
                value.clone()
            };
            (key.clone(), value)
        })
        .collect()
}

fn requires_access_token(config: &crate::config::HttpConfig, path: &str) -> bool {
    !config.access_token.trim().is_empty()
        && !matches!(path, "/" | "/tools" | "/favicon.ico" | "/health")
}

fn has_valid_access_token(request: &Request, expected: &str) -> bool {
    header_value(request, "x-miliastra-token").is_some_and(|value| value == expected)
}

fn current_time_text() -> String {
    crate::adapters::logging::format_time(SystemTime::now())
}

fn is_json_route(path: &str) -> bool {
    body_route_spec(path).is_some() || route_spec(path).is_some_and(|route| route.json)
}

fn enforce_method(
    request: &Request,
    _state: &HttpSharedState,
) -> std::result::Result<(), AppError> {
    if request.method != "GET" && request.method != "POST" {
        return Err(method_not_allowed("只支持GET或POST"));
    }
    if is_mutating_route(&request.path) && request.method != "POST" {
        return Err(method_not_allowed("该接口需要POST请求"));
    }
    if matches!(
        request.path.as_str(),
        "/" | "/tools" | "/screenshot" | "/hall-screenshot" | "/favicon.ico"
    ) && request.method != "GET"
    {
        return Err(method_not_allowed("该资源仅支持GET请求"));
    }
    Ok(())
}

fn is_mutating_route(path: &str) -> bool {
    body_route_spec(path).is_some() || route_spec(path).is_some_and(|route| route.mutating)
}

fn status_code(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

fn plain_response(
    status: StatusCode,
    body: String,
    extra_headers: Vec<(String, String)>,
) -> Response {
    body_response(status, "text/plain; charset=utf-8", body, extra_headers)
}

fn empty_response(status: StatusCode, extra_headers: Vec<(String, String)>) -> Response {
    add_headers(
        Response::builder().status(status).body(Body::empty()),
        extra_headers,
    )
}

fn body_response(
    status: StatusCode,
    content_type: &str,
    body: String,
    extra_headers: Vec<(String, String)>,
) -> Response {
    let response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .body(Body::from(body));
    add_headers(response, extra_headers)
}

fn bytes_response(
    status: StatusCode,
    content_type: &str,
    body: Vec<u8>,
    extra_headers: Vec<(String, String)>,
) -> Response {
    let response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .body(Body::from(body));
    add_headers(response, extra_headers)
}

fn add_headers(
    response: std::result::Result<Response, axum::http::Error>,
    headers: Vec<(String, String)>,
) -> Response {
    let mut response = response.unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("HTTP响应构造失败"))
            .unwrap_or_else(|_| Response::new(Body::empty()))
    });
    for (key, value) in headers {
        if let (Ok(key), Ok(value)) = (
            HeaderName::try_from(key.as_str()),
            HeaderValue::from_str(&value),
        ) {
            response.headers_mut().insert(key, value);
        }
    }
    response
}

fn is_allowed_origin(request: &Request, host: &str, port: u16) -> bool {
    let request_host = match header_value(request, "host") {
        Some(value) => match allowed_request_host(value, host, port) {
            Some(value) => value,
            None => return false,
        },
        None => format_host_port(&normalize_host_name(host), port),
    };
    let origin = header_value(request, "origin");
    let fetch_site = header_value(request, "sec-fetch-site");
    if let Some(origin_value) = origin
        && !is_same_origin(origin_value, &request_host)
    {
        return false;
    }
    if origin.is_none()
        && let Some(fetch_site_value) = fetch_site
        && fetch_site_value != "same-origin"
        && fetch_site_value != "none"
    {
        return false;
    }
    true
}

fn allowed_request_host(
    value: &str,
    configured_host: &str,
    configured_port: u16,
) -> Option<String> {
    let (host, port) = parse_host_header(value)?;
    if port.is_some_and(|port| port != configured_port) {
        return None;
    }
    if !is_wildcard_host(configured_host)
        && host != normalize_host_name(configured_host)
        && !(is_loopback_host(configured_host) && is_loopback_host(&host))
    {
        return None;
    }
    Some(format_host_port(&host, port.unwrap_or(configured_port)))
}

fn parse_host_header(value: &str) -> Option<(String, Option<u16>)> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    if let Some(rest) = value.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = normalize_host_name(&rest[..end]);
        let port = match &rest[end + 1..] {
            "" => None,
            value if value.starts_with(':') => Some(parse_host_port(&value[1..])?),
            _ => return None,
        };
        return Some((host, port));
    }

    let colon_count = value
        .as_bytes()
        .iter()
        .filter(|byte| **byte == b':')
        .count();
    if colon_count == 1 {
        let (host, port) = value.rsplit_once(':')?;
        return Some((normalize_host_name(host), Some(parse_host_port(port)?)));
    }
    Some((normalize_host_name(value), None))
}

fn parse_host_port(value: &str) -> Option<u16> {
    if value.is_empty() {
        None
    } else {
        value.parse::<u16>().ok()
    }
}

fn normalize_host_name(value: &str) -> String {
    value.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

fn is_loopback_host(value: &str) -> bool {
    matches!(
        normalize_host_name(value).as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

fn is_wildcard_host(value: &str) -> bool {
    matches!(normalize_host_name(value).as_str(), "0.0.0.0" | "::")
}

fn header_value<'a>(request: &'a Request, key: &'static str) -> Option<&'a str> {
    request
        .headers
        .get(key)
        .and_then(|value| value.to_str().ok())
}

fn cors_headers(request: &Request, host: &str, port: u16) -> Vec<(String, String)> {
    let request_host = header_value(request, "host")
        .and_then(|value| allowed_request_host(value, host, port))
        .unwrap_or_else(|| format_host_port(&normalize_host_name(host), port));
    if let Some(origin) = header_value(request, "origin")
        && is_same_origin(origin, &request_host)
    {
        return vec![
            (
                "Access-Control-Allow-Origin".to_string(),
                origin.to_string(),
            ),
            ("Vary".to_string(), "Origin".to_string()),
        ];
    }
    Vec::new()
}

fn options_headers(request: &Request, host: &str, port: u16) -> Vec<(String, String)> {
    let mut headers = cors_headers(request, host, port);
    headers.push((
        "Access-Control-Allow-Methods".to_string(),
        "GET, POST, OPTIONS".to_string(),
    ));
    headers.push((
        "Access-Control-Allow-Headers".to_string(),
        "Content-Type, X-Miliastra-Token".to_string(),
    ));
    headers
}

fn default_cors_headers(host: &str, port: u16) -> Vec<(String, String)> {
    vec![(
        "Access-Control-Allow-Origin".to_string(),
        format!(
            "http://{}",
            format_host_port(&normalize_host_name(host), port)
        ),
    )]
}

fn is_same_origin(origin: &str, host: &str) -> bool {
    origin == format!("http://{}", host)
}

fn bad_request(message: &str) -> AppError {
    AppError {
        status: 400,
        message: message.to_string(),
    }
}

fn method_not_allowed(message: &str) -> AppError {
    AppError {
        status: 405,
        message: message.to_string(),
    }
}

fn internal_error(error: impl std::fmt::Display) -> AppError {
    AppError {
        status: 500,
        message: error.to_string(),
    }
}

fn player_search_error(error: PlayerSearchClientError) -> AppError {
    let message = match error {
        PlayerSearchClientError::Failed(source) => source.to_string(),
        error => error.to_string(),
    };
    internal_message(&message)
}

fn playback_http_error(error: PlaybackError) -> AppError {
    let code = error.code();
    let (status, message) = match code {
        "track_unavailable" => (409, "曲目当前不可播放"),
        "provider_auth_required" => (401, "该平台尚未登录"),
        "relogin_required" => (401, "登录凭据已失效，请重新登录"),
        "provider_rate_limited" => (429, "平台请求过于频繁"),
        "provider_timeout" => (504, "平台请求超时"),
        "unknown_provider" | "invalid_request" => (400, "曲目请求无效"),
        "playback_busy" => (429, "播放器正忙"),
        "playback_runtime_stopped" => (503, "播放器未运行"),
        _ => (500, "播放操作失败"),
    };
    AppError {
        status,
        message: format!("{code}: {message}"),
    }
}

fn login_http_error(error: LoginHelperFailure) -> AppError {
    let status = match error.code {
        "unsupported_provider" | "invalid_helper_provider" | "invalid_helper_credential" => 400,
        "login_not_active" => 404,
        "login_in_progress" | "login_session_invalid" => 409,
        "login_timeout" | "login_cancel_timeout" => 504,
        "webview_runtime_unavailable" => 503,
        _ => 500,
    };
    AppError {
        status,
        message: format!("{}: {}", error.code, error.message),
    }
}

fn internal_message(message: &str) -> AppError {
    internal_error(anyhow!(message.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::ops::{Deref, DerefMut};
    use std::time::Duration;

    use crate::features::playback::{test_candidate, test_track};
    use crate::features::turtle_soup::TurtleSoupAppendReceipt;
    use crate::runtime::chat_listener::ChatListenerState;
    use crate::runtime::player_io::PlayerSearchError;
    use crate::runtime::scheduler::FormalTaskReceipt;

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
    }

    impl HttpTaskPort for RecordingHttpPort {
        fn apply_mutation(
            &self,
            intent: BusinessMutationIntent,
        ) -> Result<BusinessMutationOutcome> {
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
                BusinessMutationIntent::TurtleSoup(TurtleSoupMutationIntent::Start {
                    puzzle_id,
                }) => {
                    state
                        .mutations
                        .push(RecordedMutation::TurtleSoupStart(puzzle_id));
                    state.turtle_soup.enabled = true;
                    BusinessMutationOutcome::TurtleSoup(Box::new(
                        TurtleSoupMutationOutcome::Started(state.turtle_soup.clone()),
                    ))
                }
                BusinessMutationIntent::TurtleSoup(TurtleSoupMutationIntent::End) => {
                    state.mutations.push(RecordedMutation::TurtleSoupEnd);
                    let ended = state.turtle_soup.enabled;
                    state.turtle_soup = TurtleSoupSnapshot::default();
                    BusinessMutationOutcome::TurtleSoup(Box::new(
                        TurtleSoupMutationOutcome::Ended {
                            ended,
                            snapshot: state.turtle_soup.clone(),
                        },
                    ))
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

        fn enqueue_listener_mode(
            &self,
            target: ChatListenerMode,
        ) -> Result<FormalTaskEnqueueOutcome> {
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

        fn fail_if_requested(&self) -> std::result::Result<(), PlayerSearchClientError> {
            if self.fail {
                Err(PlayerSearchClientError::Failed(PlayerSearchError::new(
                    "backend failed",
                )))
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
        ) -> std::result::Result<String, PlayerSearchClientError> {
            self.fail_if_requested()?;
            Ok(format!("raw search: {keyword} [{source}]"))
        }

        fn search_candidates(
            &self,
            keyword: &str,
            source: &str,
        ) -> std::result::Result<Vec<SearchCandidate>, PlayerSearchClientError> {
            self.fail_if_requested()?;
            Ok(vec![test_candidate(
                &format!("{keyword} result"),
                &format!("miliastra://track/{source}/1"),
            )])
        }
    }

    #[derive(Default)]
    struct HttpTestNativePlaybackPort {
        played: Mutex<Vec<PlayableTrack>>,
    }

    impl HttpNativePlaybackPort for HttpTestNativePlaybackPort {
        fn play_track(&self, track: PlayableTrack) -> Result<(), PlaybackError> {
            self.played.lock().unwrap().push(track);
            Ok(())
        }
    }

    #[derive(Default)]
    struct HttpTestLoginPort {
        active: Mutex<Option<LoginSession>>,
    }

    impl HttpLoginPort for HttpTestLoginPort {
        fn providers(&self) -> Result<Vec<ProviderView>, LoginHelperFailure> {
            Ok(ProviderId::ALL
                .into_iter()
                .map(|provider| {
                    let status = miliastra_playback::CredentialStatus::empty(provider.as_str());
                    ProviderView {
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

        fn status(&self) -> LoginManagerStatus {
            let active = self.active.lock().unwrap().clone();
            LoginManagerStatus {
                active: active.is_some(),
                session_id: active.as_ref().map(|session| session.session_id),
                provider: active.map(|session| session.provider),
                last_error: None,
            }
        }

        fn start(&self, provider: ProviderId) -> Result<LoginSession, LoginHelperFailure> {
            let mut active = self.active.lock().unwrap();
            if active.is_some() {
                return Err(LoginHelperFailure {
                    code: "login_in_progress",
                    message: "已有登录任务正在进行",
                    provider: Some(provider),
                });
            }
            let session = LoginSession {
                session_id: Uuid::new_v4(),
                provider,
            };
            *active = Some(session.clone());
            Ok(session)
        }

        fn cancel(&self, session_id: Uuid) -> Result<(), LoginHelperFailure> {
            let mut active = self.active.lock().unwrap();
            if active.as_ref().map(|session| session.session_id) != Some(session_id) {
                return Err(LoginHelperFailure {
                    code: "login_session_invalid",
                    message: "登录会话无效",
                    provider: None,
                });
            }
            *active = None;
            Ok(())
        }

        fn logout(
            &self,
            provider: ProviderId,
        ) -> Result<miliastra_playback::CredentialStatus, LoginHelperFailure> {
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
        native_playback: Arc<HttpTestNativePlaybackPort>,
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

    fn http_post(
        address: SocketAddr,
        target: &str,
        access_token: Option<&str>,
    ) -> TestHttpResponse {
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
            }
        });
        let played = http_post_json(address, "/player/play-track", &track.to_string(), None);
        assert_eq!(played.status_line, "HTTP/1.1 200 OK");
        let played: Value = serde_json::from_str(&played.body).expect("play response JSON");
        assert_eq!(played["currentUri"], "miliastra://track/netease/123");
        assert_eq!(state.native_playback.played.lock().unwrap().len(), 1);

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
        assert_eq!(state.native_playback.played.lock().unwrap().len(), 1);

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
        assert_eq!(state.native_playback.played.lock().unwrap().len(), 0);
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
            serde_yaml::from_str(include_str!("../../../config.yaml")).expect("default config");
        state.custom_workflow = custom_workflow_service_from_config_parts(
            &state.config.custom_workflows,
            &state.config.timing,
            &default_config.ocr,
        );

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
            let actual_threshold =
                template["threshold"].as_f64().expect("template threshold") as f32;
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
            serde_yaml::from_str(include_str!("../../../config.yaml")).expect("default config");
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
            serde_json::from_str::<Value>(&search_candidates_route(&query, &state).unwrap())
                .unwrap(),
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
        let error = player_search_error(PlayerSearchClientError::Failed(PlayerSearchError::new(
            "backend failed",
        )));

        assert_eq!(error.status, 500);
        assert_eq!(error.message, "backend failed");
        assert_eq!(
            player_search_error(PlayerSearchClientError::QueueFull).message,
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
        let pending = remote_control_command(
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
        let pending = remote_control_command(
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
        let pending =
            remote_song_command("晴天 伴奏".to_string(), "qqmusic".to_string(), false, false)
                .expect("remote song command")
                .into_pending();

        assert_eq!(pending.routed.message_type, "控制台");
        assert_eq!(pending.routed.username, "控制台");
        assert_eq!(pending.routed.raw, "点歌 晴天 伴奏");
        match pending.routed.command {
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
        let pending = remote_song_command("晴天".to_string(), "qqmusic".to_string(), false, true)
            .expect("remote ai song command")
            .into_pending();

        assert_eq!(pending.routed.raw, "AI点歌 晴天");
        match pending.routed.command {
            ModuleCommand::SongRequest(song) => {
                assert_eq!(song.source, SongSource::All);
                assert!(song.ai_assisted);
            }
            _ => panic!("expected song command"),
        }
    }

    #[test]
    fn remote_song_command_supports_bilibili_source() {
        let pending = remote_song_command(
            "耀斑 HOYO-MiX".to_string(),
            "bilibili".to_string(),
            false,
            false,
        )
        .expect("remote bilibili song command")
        .into_pending();

        assert_eq!(pending.routed.raw, "B站点歌 耀斑 HOYO-MiX");
        match pending.routed.command {
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
            .queries
            .playback_queue_snapshot()
            .expect("queue snapshot")[2]
            .id;
        let removed = queue_remove(&[], &state).expect("queue shift");
        let removed: Value = serde_json::from_str(&removed).expect("remove response");
        assert_eq!(removed["removed"]["keyword"], "第一首");

        let body = queue_remove(&[("id".to_string(), third_id.to_string())], &state)
            .expect("remove by id");
        let response: Value = serde_json::from_str(&body).expect("remove response json");

        assert_eq!(response["removed"]["id"], third_id);
        assert_eq!(response["removed"]["keyword"], "第三首");
        let queue = state
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
        let cleared: Value = serde_json::from_str(&state_json(&state).expect("state json"))
            .expect("cleared state json");
        assert!(cleared["hallRemainingMinutes"].is_null());
        assert!(cleared["hallRemainingUpdatedAt"].is_null());
    }

    #[test]
    fn remote_plain_song_rejects_multi_source() {
        let error = remote_song_command(
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

    fn test_state() -> HttpTestState {
        test_state_with_player_port(HttpTestPlayerPort::successful())
    }

    fn test_state_with_player_port(player: impl HttpPlayerPort + 'static) -> HttpTestState {
        let config: AppConfig =
            serde_yaml::from_str(include_str!("../../../config.yaml")).expect("default config");
        let monitor = MonitorShared::new(20);
        let custom_workflow = custom_workflow_service_from_config_parts(
            &config.custom_workflows,
            &config.timing,
            &config.ocr,
        );
        let recording = Arc::new(RecordingHttpPort::new());
        let native_playback = Arc::new(HttpTestNativePlaybackPort::default());
        let login = Arc::new(HttpTestLoginPort::default());
        HttpTestState {
            state: HttpSharedState::new_with_ports(
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
                custom_workflow,
                recording.clone(),
                recording.clone(),
                monitor,
                recording.clone(),
                Arc::new(Mutex::new(LatestFrameCache::default())),
                Arc::new(player),
                native_playback.clone(),
                login.clone(),
                Arc::new(HttpTestAiPort),
            ),
            recording,
            native_playback,
        }
    }
}
