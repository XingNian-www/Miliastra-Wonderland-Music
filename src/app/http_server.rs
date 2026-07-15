use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::net::SocketAddr;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
use image::ColorType;
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::oneshot;
use url::form_urlencoded;

use super::ai;
use super::chat_listener::{ChatListenerMode, ChatListenerShared};
use super::command::{self, ParsedCommand, PendingCommand, SongCommand, SongSource, UserCommand};
use super::custom_workflow;
use super::decision_control::{DecisionAction, DecisionControlShared};
#[cfg(test)]
use super::deferred_chat::DeferredChatQueue;
use super::feeluown::FeelUOwnClient;
use super::geometry::parse_rect;
use super::monitor::{MonitorQueueItem, MonitorShared};
use super::queue::{PersistentQueue, QueueItem};
use super::runtime_state::PersistentRuntimeState;
use super::task_tracker::TaskTrackerShared;
use super::web_tools::{WebToolRequest, WebToolShared, WebToolTemplate};
use crate::config::AppConfig;
use crate::features::custom_workflow::CustomWorkflowService;
#[cfg(test)]
use crate::features::entertainment::EntertainmentCoordinator;
#[cfg(test)]
use crate::features::moderation::{ModerationPolicy, ModerationService};
use crate::features::startup::{StartupSource, StartupTask};
use crate::features::turtle_soup::TurtleSoupService;
use crate::features::turtle_soup::repository::{TurtleSoupBankStore, TurtleSoupSubmission};
use crate::features::undercover::{UndercoverCommand, UndercoverService};
use crate::runtime::player_io::{PlayerSearchClient, PlayerSearchClientError};

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

const BODY_ROUTES: &[BodyRouteSpec] = &[BodyRouteSpec {
    path: "/turtle-soup/questions",
    handler: turtle_soup_questions_route,
}];

const SPECIAL_ROUTES: &[&str] = &["/screenshot"];
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
        path: "/player/play-uri",
        json: true,
        mutating: true,
        handler: player_play_uri_route,
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
    pub config: AppConfig,
    pub queue: Arc<Mutex<PersistentQueue>>,
    pub runtime_state: Arc<Mutex<PersistentRuntimeState>>,
    pub monitor: MonitorShared,
    pub chat_listener: ChatListenerShared,
    turtle_soup: TurtleSoupService,
    turtle_soup_bank: TurtleSoupBankStore,
    undercover: UndercoverService,
    custom_workflow: CustomWorkflowService,
    pub history: Arc<Mutex<VecDeque<HistoryItem>>>,
    pub active_connections: Arc<AtomicUsize>,
    pending: Arc<(Mutex<VecDeque<super::TrackedPendingTask>>, Condvar)>,
    task_tracker: TaskTrackerShared,
    decision_control: DecisionControlShared,
    web_tools: WebToolShared,
    latest_frame: Arc<Mutex<Option<Arc<image::DynamicImage>>>>,
    player_search: PlayerSearchClient,
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
    pub(super) fn new(
        config: AppConfig,
        queue: Arc<Mutex<PersistentQueue>>,
        runtime_state: Arc<Mutex<PersistentRuntimeState>>,
        pending: Arc<(Mutex<VecDeque<super::TrackedPendingTask>>, Condvar)>,
        chat_listener: ChatListenerShared,
        turtle_soup: TurtleSoupService,
        undercover: UndercoverService,
        monitor: MonitorShared,
        task_tracker: TaskTrackerShared,
        decision_control: DecisionControlShared,
        web_tools: WebToolShared,
        latest_frame: Arc<Mutex<Option<Arc<image::DynamicImage>>>>,
        player_search: PlayerSearchClient,
    ) -> Self {
        let turtle_soup_bank =
            TurtleSoupBankStore::new(config.turtle_soup.question_bank_path.clone());
        let custom_workflow = custom_workflow::service_from_config(&config);
        Self {
            config,
            queue,
            runtime_state,
            monitor,
            chat_listener,
            turtle_soup,
            turtle_soup_bank,
            undercover,
            custom_workflow,
            history: Arc::new(Mutex::new(VecDeque::new())),
            active_connections: Arc::new(AtomicUsize::new(0)),
            pending,
            task_tracker,
            decision_control,
            web_tools,
            latest_frame,
            player_search,
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
    let client = FeelUOwnClient::new(&state.config.feeluown, &state.config.timing);
    serde_json::to_string(&client.status().map_err(internal_error)?).map_err(internal_error)
}

fn play_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command("继续".to_string(), "继续", UserCommand::Resume),
    )
}

fn pause_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command("暂停".to_string(), "暂停", UserCommand::Pause),
    )
}

fn skip_next_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command("下一首".to_string(), "下一首", UserCommand::Next),
    )
}

fn skip_prev_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command("上一首".to_string(), "上一首", UserCommand::Previous),
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
            UserCommand::Volume(volume.to_string()),
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
        .player_search
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
            .player_search
            .search_candidates(&keyword, &source)
            .map_err(player_search_error)?,
    )
    .map_err(internal_error)
}

fn player_play_uri_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let uri = normalize_fuo_uri(query_value_or(query, "url", "uri"))?;
    let keyword = normalize_optional_text(
        query_value_or(query, "keyword", "title")
            .or_else(|| query_value(query, "text"))
            .or_else(|| query_value(query, "name")),
        "keyword",
    )?;
    let keyword = if keyword.is_empty() {
        uri.clone()
    } else {
        keyword
    };
    let source =
        normalize_source(query_value(query, "source").or_else(|| source_from_fuo_uri(&uri)))?;
    let prefer_accompaniment = parse_bool(query_value_or(
        query,
        "preferAccompaniment",
        "accompaniment",
    ));
    let mut queue = state
        .queue
        .lock()
        .map_err(|_| internal_message("音乐播放队列锁已损坏"))?;
    if !queue
        .push(QueueItem {
            id: 0,
            keyword: keyword.clone(),
            source,
            prefer_accompaniment,
            ai_original_text: String::new(),
            uri: uri.trim().to_string(),
            friend_username: String::new(),
            dedup_bypass: true,
        })
        .map_err(internal_error)?
    {
        return Err(AppError {
            status: 400,
            message: "音乐播放队列已满".to_string(),
        });
    }
    sync_monitor_queue(&state.monitor, &queue);
    Ok(
        json!({ "ok": true, "queued": true, "size": queue.len(), "keyword": keyword, "uri": uri })
            .to_string(),
    )
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
    let queued = state.chat_listener.request_mode(mode);
    let snapshot = state.chat_listener.snapshot();
    if !queued {
        return Ok(json!({
            "ok": true,
            "queued": false,
            "mode": snapshot.mode,
            "pendingMode": snapshot.pending_mode,
        })
        .to_string());
    }
    let receipt = match enqueue_pending_task(
        state,
        super::PendingTask::SetChatListenerMode { target: mode },
    ) {
        Ok(receipt) => receipt,
        Err(error) => {
            state.chat_listener.cancel_mode_request(mode);
            sync_chat_listener_monitor(state);
            return Err(error);
        }
    };
    sync_chat_listener_monitor(state);
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
    let (lock, cvar) = &*state.pending;
    let mut pending = lock
        .lock()
        .map_err(|_| internal_message("待处理任务队列锁已损坏"))?;
    let Some(index) = pending.iter().position(|task| task.id == task_id) else {
        return Err(AppError {
            status: 409,
            message: "任务已开始、已结束或不存在，不能撤销".to_string(),
        });
    };
    let task = pending
        .remove(index)
        .ok_or_else(|| internal_message("待处理任务撤销失败"))?;
    let label = task.label();
    drop(pending);
    let sync_listener = task.cancel(&state.chat_listener);
    if sync_listener {
        sync_chat_listener_monitor(state);
    }
    state
        .task_tracker
        .cancel(task_id, format!("{}已由控制台撤销", label));
    cvar.notify_all();
    Ok(json!({ "ok": true, "taskId": task_id, "canceled": true }).to_string())
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
        .decision_control
        .submit(id, action)
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
        remote_control_command("歌词".to_string(), "歌词", UserCommand::Lyrics),
    )
}

fn operator_hall_detect_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command("大厅检测".to_string(), "大厅检测", UserCommand::HallDetect),
    )
}

fn operator_hall_time_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    enqueue_remote_command(
        state,
        remote_control_command("大厅时间".to_string(), "大厅时间", UserCommand::HallTime),
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
            UserCommand::Microphone {
                username: "控制台".to_string(),
            },
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
            UserCommand::EnableCommands {
                username: "控制台".to_string(),
            },
        )
    } else {
        (
            "禁用".to_string(),
            UserCommand::DisableCommands {
                username: "控制台".to_string(),
            },
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
                let receipt = enqueue_pending_task(state, super::PendingTask::ClearIdleExit)?;
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
            UserCommand::IdleExit { minutes },
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
            UserCommand::CustomWorkflow(prepared.command),
        ),
    )
}

fn ai_recognize_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    ai::recognize_with_query(&state.config.ai, &state.config.timing, query).map_err(ai_route_error)
}

fn ai_match_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    ai::match_with_query(&state.config.ai, &state.config.timing, query).map_err(ai_route_error)
}

fn ai_pick_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    ai::pick_with_query(&state.config.ai, &state.config.timing, query).map_err(ai_route_error)
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
    serde_json::to_string(&state.turtle_soup.snapshot()).map_err(internal_error)
}

fn turtle_soup_start_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    match query_value(query, "id")
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(id) => state.turtle_soup.start_by_id_from_web(id),
        None => state.turtle_soup.start_random_from_web(),
    }
    .map_err(turtle_soup_error)?;
    serde_json::to_string(&json!({
        "ok": true,
        "turtleSoup": state.turtle_soup.snapshot(),
    }))
    .map_err(internal_error)
}

fn turtle_soup_end_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let ended = state
        .turtle_soup
        .end_from_web()
        .map_err(turtle_soup_error)?;
    if !ended {
        return Err(AppError {
            status: 409,
            message: "当前没有可结束的海龟汤".to_string(),
        });
    }
    serde_json::to_string(&json!({
        "ok": true,
        "turtleSoup": state.turtle_soup.snapshot(),
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
    let receipt = state
        .turtle_soup_bank
        .append(submission)
        .map_err(internal_error)?;
    serde_json::to_string(&receipt).map_err(internal_error)
}

fn undercover_route(
    _query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let snapshot = state
        .undercover
        .snapshot(std::time::Instant::now())
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
            UserCommand::Undercover(UndercoverCommand::Start),
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
            UserCommand::Undercover(UndercoverCommand::End),
        ),
    )
}

fn tool_task_route(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let id = parse_tool_id(query)?;
    let snapshot = state
        .web_tools
        .snapshot(id)
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
        json!({ "name": "wonderland-enter-button", "label": "千星前往大厅", "region": state.config.startup.wonderland_enter_button_region, "threshold": state.config.startup.wonderland_enter_button_threshold }),
        json!({ "name": "paimon-menu", "label": "派蒙主界面", "region": state.config.startup.main_ui_region, "threshold": state.config.startup.template_threshold }),
        json!({ "name": "wonderland-close", "label": "千星主页关闭按钮", "region": state.config.startup.wonderland_close_region, "threshold": state.config.startup.template_threshold }),
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
    let snapshot = state.web_tools.enqueue(request).map_err(|error| AppError {
        status: if error.to_string().contains("任务过多") {
            429
        } else {
            500
        },
        message: error.to_string(),
    })?;
    let (_, cvar) = &*state.pending;
    cvar.notify_one();
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
    pending: PendingCommand,
) -> std::result::Result<String, AppError> {
    let command = pending.parsed.raw.clone();
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
        [super::PendingTask::Startup(StartupTask::start_game(
            StartupSource::REMOTE_CONSOLE,
        ))],
    )
}

fn enqueue_enter_wonderland(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    enqueue_startup_task_response(
        state,
        "进入千星",
        [super::PendingTask::Startup(StartupTask::enter_wonderland(
            StartupSource::REMOTE_CONSOLE,
        ))],
    )
}

fn enqueue_startup_wonderland(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    enqueue_startup_task_response(
        state,
        "启动游戏并进入千星",
        [
            super::PendingTask::Startup(StartupTask::start_game(StartupSource::REMOTE_CONSOLE)),
            super::PendingTask::Startup(StartupTask::enter_wonderland(
                StartupSource::REMOTE_CONSOLE,
            )),
        ],
    )
}

fn enqueue_startup_task_response<const N: usize>(
    state: &HttpSharedState,
    task_label: &'static str,
    tasks: [super::PendingTask; N],
) -> std::result::Result<String, AppError> {
    let receipts = enqueue_pending_tasks(state, tasks)?;
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

fn remote_control_command(raw: String, matched: &str, command: UserCommand) -> PendingCommand {
    let parsed = ParsedCommand {
        matched: matched.to_string(),
        user_command: format!("@{}", raw),
        raw,
        message_type: "控制台".to_string(),
        username: "控制台".to_string(),
        command,
    };
    PendingCommand {
        lock_key: command::lock_key(&parsed),
        parsed,
    }
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
    let pending = remote_song_command(keyword, source, prefer_accompaniment, ai_assisted)?;
    let command = pending.parsed.raw.clone();
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

fn remote_song_command(
    keyword: String,
    source: String,
    prefer_accompaniment: bool,
    ai_assisted: bool,
) -> std::result::Result<PendingCommand, AppError> {
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
    let user_command = if prefer_accompaniment {
        format!("@{} {} 伴奏", prefix, keyword)
    } else {
        format!("@{} {}", prefix, keyword)
    };
    let raw = if prefer_accompaniment {
        format!("{} {} 伴奏", prefix, keyword)
    } else {
        format!("{} {}", prefix, keyword)
    };
    let parsed = ParsedCommand {
        matched: prefix.to_string(),
        raw,
        user_command,
        message_type: "控制台".to_string(),
        username: "控制台".to_string(),
        command: UserCommand::Song(SongCommand {
            keyword,
            source: song_source,
            prefix: prefix.to_string(),
            prefer_accompaniment,
            ai_assisted,
            friend_username: String::new(),
        }),
    };
    Ok(PendingCommand {
        lock_key: command::lock_key(&parsed),
        parsed,
    })
}

fn queue_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let queue = state
        .queue
        .lock()
        .map_err(|_| internal_message("队列锁已损坏"))?;
    serde_json::to_string(queue.items()).map_err(internal_error)
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
    let uri = normalize_optional_text(query_value(query, "uri"), "uri")?;
    let mut queue = state
        .queue
        .lock()
        .map_err(|_| internal_message("队列锁已损坏"))?;
    if !queue
        .push(QueueItem {
            id: 0,
            keyword,
            source,
            prefer_accompaniment: prefer,
            ai_original_text,
            uri,
            friend_username: String::new(),
            dedup_bypass: true,
        })
        .map_err(internal_error)?
    {
        return Err(AppError {
            status: 400,
            message: "队列已满".to_string(),
        });
    }
    sync_monitor_queue(&state.monitor, &queue);
    Ok(json!({ "ok": true, "size": queue.len() }).to_string())
}

fn queue_remove(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let mut queue = state
        .queue
        .lock()
        .map_err(|_| internal_message("队列锁已损坏"))?;
    let removed = if let Some(id_text) = query_value(query, "id").filter(|value| !value.is_empty())
    {
        let id = id_text
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| bad_request("无效的队列项ID"))?;
        queue
            .remove_id(id)
            .map_err(internal_error)?
            .ok_or_else(|| AppError {
                status: 409,
                message: "队列已发生变化，请刷新后重试".to_string(),
            })?
    } else if let Some(index_text) = query_value(query, "index") {
        if !index_text.is_empty() {
            let index = index_text
                .parse::<usize>()
                .map_err(|_| bad_request("无效的队列索引"))?;
            if index >= queue.len() {
                return Err(bad_request("无效的队列索引"));
            }
            queue
                .remove_indexes(&[index])
                .map_err(internal_error)?
                .into_iter()
                .next()
                .ok_or_else(|| internal_message("队列删除结果为空"))?
        } else if !queue.is_empty() {
            queue
                .remove_indexes(&[0])
                .map_err(internal_error)?
                .into_iter()
                .next()
                .ok_or_else(|| internal_message("队首删除结果为空"))?
        } else {
            return Err(bad_request("队列为空"));
        }
    } else if !queue.is_empty() {
        queue
            .remove_indexes(&[0])
            .map_err(internal_error)?
            .into_iter()
            .next()
            .ok_or_else(|| internal_message("队首删除结果为空"))?
    } else {
        return Err(bad_request("队列为空"));
    };
    sync_monitor_queue(&state.monitor, &queue);
    Ok(json!({
        "ok": true,
        "size": queue.len(),
        "removed": {
            "index": removed.0,
            "id": removed.1.id,
            "keyword": removed.1.keyword,
        }
    })
    .to_string())
}

fn queue_clear(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let mut queue = state
        .queue
        .lock()
        .map_err(|_| internal_message("队列锁已损坏"))?;
    queue.clear().map_err(internal_error)?;
    sync_monitor_queue(&state.monitor, &queue);
    Ok(json!({ "ok": true }).to_string())
}

fn sync_monitor_queue(monitor: &MonitorShared, queue: &PersistentQueue) {
    monitor.set_queue(
        queue
            .items()
            .iter()
            .map(|item| MonitorQueueItem {
                id: item.id,
                keyword: item.keyword.clone(),
                source: item.source.clone(),
                prefer_accompaniment: item.prefer_accompaniment,
                friend_username: item.friend_username.clone(),
            })
            .collect(),
    );
}

fn state_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let runtime = state
        .runtime_state
        .lock()
        .map_err(|_| internal_message("状态锁已损坏"))?;
    let mut value = serde_json::to_value(runtime.state()).map_err(internal_error)?;
    if let Value::Object(object) = &mut value {
        object.insert(
            "hallRemainingMinutesNow".to_string(),
            runtime
                .state()
                .hall_remaining_minutes_now()
                .map(Value::from)
                .unwrap_or(Value::Null),
        );
    }
    serde_json::to_string(&value).map_err(internal_error)
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
    let mut runtime = state
        .runtime_state
        .lock()
        .map_err(|_| internal_message("状态锁已损坏"))?;
    apply_runtime_patch(runtime.state_mut(), patch);
    runtime.save().map_err(internal_error)?;
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
    let image = state
        .latest_frame
        .lock()
        .map_err(|_| internal_message("主扫描画面缓存锁已损坏"))?
        .clone()
        .ok_or_else(|| AppError {
            status: 503,
            message: "尚未获取主扫描画面，请稍后重试".to_string(),
        })?;
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
        cors_headers(request, &state.config.http.host, state.config.http.port),
    ))
}

fn monitor_json(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let mut value = serde_json::to_value(state.monitor.snapshot()).map_err(internal_error)?;
    if let Value::Object(object) = &mut value {
        object.insert(
            "pendingTasks".to_string(),
            json!(pending_task_labels(state)?),
        );
        object.insert(
            "chatListener".to_string(),
            serde_json::to_value(state.chat_listener.snapshot()).map_err(internal_error)?,
        );
        object.insert(
            "webTools".to_string(),
            serde_json::to_value(state.web_tools.recent().map_err(internal_error)?)
                .map_err(internal_error)?,
        );
        object.insert(
            "tasks".to_string(),
            serde_json::to_value(state.task_tracker.recent().map_err(internal_error)?)
                .map_err(internal_error)?,
        );
        object.insert(
            "decision".to_string(),
            serde_json::to_value(state.decision_control.snapshot().map_err(internal_error)?)
                .map_err(internal_error)?,
        );
        object.insert(
            "turtleSoup".to_string(),
            serde_json::to_value(state.turtle_soup.snapshot()).map_err(internal_error)?,
        );
        object.insert(
            "undercover".to_string(),
            serde_json::to_value(
                state
                    .undercover
                    .snapshot(std::time::Instant::now())
                    .map_err(internal_error)?,
            )
            .map_err(internal_error)?,
        );
    }
    serde_json::to_string(&value).map_err(internal_error)
}

fn sync_chat_listener_monitor(state: &HttpSharedState) {
    let snapshot = state.chat_listener.snapshot();
    state.monitor.set_chat_listener(
        snapshot.display_mode(),
        snapshot.pending_mode.map(|mode| mode.label().to_string()),
    );
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
    let receipt = enqueue_pending_task(state, super::PendingTask::ConsoleChat { text, prefix })?;
    Ok(json!({
        "ok": true,
        "queued": true,
        "taskId": receipt.task_id,
        "position": receipt.position,
        "message": message
    })
    .to_string())
}

fn pending_task_labels(state: &HttpSharedState) -> std::result::Result<Vec<String>, AppError> {
    let (lock, _) = &*state.pending;
    let guard = lock
        .lock()
        .map_err(|_| internal_message("待处理任务队列锁已损坏"))?;
    Ok(guard.iter().map(super::TrackedPendingTask::label).collect())
}

fn enqueue_pending_task(
    state: &HttpSharedState,
    task: super::PendingTask,
) -> std::result::Result<EnqueueReceipt, AppError> {
    let (lock, cvar) = &*state.pending;
    let mut guard = lock
        .lock()
        .map_err(|_| internal_message("待处理任务队列锁已损坏"))?;
    let task_id = state
        .task_tracker
        .create(task.label())
        .map_err(internal_error)?;
    guard.push_back(super::TrackedPendingTask { id: task_id, task });
    let position = guard.len();
    cvar.notify_one();
    Ok(EnqueueReceipt { task_id, position })
}

fn enqueue_pending_tasks<const N: usize>(
    state: &HttpSharedState,
    tasks: [super::PendingTask; N],
) -> std::result::Result<Vec<EnqueueReceipt>, AppError> {
    let (lock, cvar) = &*state.pending;
    let mut guard = lock
        .lock()
        .map_err(|_| internal_message("待处理任务队列锁已损坏"))?;
    let mut receipts = Vec::with_capacity(N);
    for task in tasks {
        let task_id = state
            .task_tracker
            .create(task.label())
            .map_err(internal_error)?;
        guard.push_back(super::TrackedPendingTask { id: task_id, task });
        receipts.push(EnqueueReceipt {
            task_id,
            position: guard.len(),
        });
    }
    cvar.notify_one();
    Ok(receipts)
}

fn enqueue_pending_command(
    state: &HttpSharedState,
    pending: PendingCommand,
) -> std::result::Result<Option<EnqueueReceipt>, AppError> {
    let (lock, cvar) = &*state.pending;
    let mut guard = lock
        .lock()
        .map_err(|_| internal_message("待处理任务队列锁已损坏"))?;
    if guard
        .iter()
        .any(|task| task.same_lock_command(&pending.parsed))
    {
        return Ok(None);
    }
    let task = super::PendingTask::Command(Box::new(pending));
    let task_id = state
        .task_tracker
        .create(task.label())
        .map_err(internal_error)?;
    guard.push_back(super::TrackedPendingTask { id: task_id, task });
    let position = guard.len();
    cvar.notify_one();
    Ok(Some(EnqueueReceipt { task_id, position }))
}

fn push_history(request: &Request, result: &str, ok: bool, state: &HttpSharedState) {
    if request.path.starts_with("/tools/")
        || matches!(
            request.path.as_str(),
            "/history" | "/clear-history" | "/monitor" | "/screenshot" | "/favicon.ico"
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

fn apply_runtime_patch(
    state: &mut super::runtime_state::RuntimeState,
    patch: HashMap<String, serde_json::Value>,
) {
    if let Some(value) = patch
        .get("hallRemainingMinutes")
        .and_then(serde_json::Value::as_u64)
    {
        state.hall_remaining_minutes = u32::try_from(value).ok();
    }
    if patch
        .get("hallRemainingMinutes")
        .is_some_and(serde_json::Value::is_null)
    {
        state.hall_remaining_minutes = None;
    }
    if let Some(value) = patch
        .get("hallRemainingUpdatedAt")
        .and_then(serde_json::Value::as_u64)
    {
        state.hall_remaining_updated_at = Some(value);
    }
    if patch
        .get("hallRemainingUpdatedAt")
        .is_some_and(serde_json::Value::is_null)
    {
        state.hall_remaining_updated_at = None;
    }
    if let Some(value) = patch
        .get("hallExpiringWarningSent")
        .and_then(serde_json::Value::as_bool)
    {
        state.hall_expiring_warning_sent = value;
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

fn normalize_fuo_uri(value: Option<&str>) -> std::result::Result<String, AppError> {
    let uri = assert_no_control_chars(value.unwrap_or(""), "url")?
        .trim()
        .to_string();
    if uri.is_empty() {
        return Err(bad_request("缺少url或uri参数"));
    }
    if !uri.starts_with("fuo://") {
        return Err(bad_request("只允许打开fuo://链接"));
    }
    Ok(uri)
}

fn source_from_fuo_uri(uri: &str) -> Option<&'static str> {
    let rest = uri.strip_prefix("fuo://")?;
    let source = rest.split('/').next().unwrap_or("");
    match source {
        "qqmusic" => Some("qqmusic"),
        "netease" => Some("netease"),
        "bilibili" => Some("bilibili"),
        _ => None,
    }
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
    super::logger::format_time(SystemTime::now())
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
        "/" | "/tools" | "/screenshot" | "/favicon.ico"
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

fn internal_message(message: &str) -> AppError {
    internal_error(anyhow!(message.to_string()))
}

fn turtle_soup_error(error: anyhow::Error) -> AppError {
    AppError {
        status: 409,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::ops::{Deref, DerefMut};
    use std::time::Duration;

    use crate::runtime::identity::BusinessOperationIdAllocator;
    use crate::runtime::player::RawPlayerSample;
    use crate::runtime::player_io::{
        ControlDispatchOutcome, PickedCandidate, PlayerControl, PlayerControlPort,
        PlayerObservationPort, PlayerObservationReadError, PlayerRuntime, PlayerSearchClient,
        PlayerSearchError, PlayerSearchPort, SearchCandidate,
    };

    struct HttpTestObservationPort;

    impl PlayerObservationPort for HttpTestObservationPort {
        fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
            Ok(RawPlayerSample::default())
        }
    }

    struct HttpTestControlPort;

    impl PlayerControlPort for HttpTestControlPort {
        fn dispatch(&mut self, _control: &PlayerControl) -> ControlDispatchOutcome {
            ControlDispatchOutcome::acknowledged("ok")
        }
    }

    struct HttpTestSearchPort {
        fail: bool,
    }

    impl HttpTestSearchPort {
        const fn successful() -> Self {
            Self { fail: false }
        }

        const fn failing() -> Self {
            Self { fail: true }
        }

        fn fail_if_requested(&self) -> Result<(), PlayerSearchError> {
            if self.fail {
                Err(PlayerSearchError::new("backend failed"))
            } else {
                Ok(())
            }
        }
    }

    impl PlayerSearchPort for HttpTestSearchPort {
        fn search_text(
            &mut self,
            keyword: &str,
            source: &str,
        ) -> Result<String, PlayerSearchError> {
            self.fail_if_requested()?;
            Ok(format!("raw search: {keyword} [{source}]"))
        }

        fn search_candidates(
            &mut self,
            keyword: &str,
            source: &str,
        ) -> Result<Vec<SearchCandidate>, PlayerSearchError> {
            self.fail_if_requested()?;
            Ok(vec![SearchCandidate::new(
                format!("{keyword} result"),
                format!("fuo://{source}/songs/1"),
            )])
        }

        fn search_and_pick(
            &mut self,
            keyword: &str,
            source: &str,
            _prefer_accompaniment: bool,
        ) -> Result<Option<PickedCandidate>, PlayerSearchError> {
            self.fail_if_requested()?;
            Ok(Some(PickedCandidate::new(
                SearchCandidate::new(
                    format!("{keyword} result"),
                    format!("fuo://{source}/songs/1"),
                ),
                "candidate listing",
            )))
        }
    }

    struct HttpTestState {
        state: HttpSharedState,
        _player_runtime: PlayerRuntime,
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
        let mut stream = TcpStream::connect(address).expect("connect to HTTP server");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let token_header = access_token
            .map(|token| format!("X-Miliastra-Token: {token}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: localhost\r\n{token_header}Connection: close\r\n\r\n"
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
            candidates.body,
            r#"[{"text":"song result","uri":"fuo://netease/songs/1"}]"#
        );
        assert_eq!(
            serde_json::from_str::<Value>(&candidates.body).expect("candidate JSON"),
            json!([{
                "text": "song result",
                "uri": "fuo://netease/songs/1"
            }])
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
        let mut state = test_state_with_search_port(HttpTestSearchPort::failing());
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

    struct TestModerationCommandPort {
        listener: ChatListenerShared,
    }

    impl crate::features::moderation::ModerationCommandPort for TestModerationCommandPort {
        fn send_hall(&mut self, _message: &str) -> Result<()> {
            Ok(())
        }

        fn prepare_vote_hold(
            &mut self,
        ) -> Result<Box<dyn crate::features::moderation::ModerationPrimaryHold>> {
            Ok(Box::new(super::super::TemporaryPrimaryHold::new(
                self.listener.clone(),
            )?))
        }
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
        state.custom_workflow = custom_workflow::service_from_config(&state.config);

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
        assert_eq!(pending_task_labels(&state).unwrap(), ["控制台命令: 入口 5"]);
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
        assert!(!state.config.turtle_soup.question_bank_path.exists());
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
        assert!(
            pending_task_labels(&state)
                .expect("pending labels")
                .is_empty()
        );
        assert_eq!(
            state
                .web_tools
                .snapshot(id)
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
                "wonderland-enter-button",
                state.config.startup.wonderland_enter_button_region,
                state.config.startup.wonderland_enter_button_threshold,
            ),
            (
                "paimon-menu",
                state.config.startup.main_ui_region,
                state.config.startup.template_threshold,
            ),
            (
                "wonderland-close",
                state.config.startup.wonderland_close_region,
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
            serde_yaml::from_str(include_str!("../../config.yaml")).expect("default config");
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
                "text": "晴天 result",
                "uri": "fuo://netease/songs/1"
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
    fn player_play_uri_route_pushes_music_queue_item() {
        let state = test_state();
        let body = player_play_uri_route(
            &[
                ("uri".to_string(), "fuo://netease/songs/123".to_string()),
                ("title".to_string(), "测试歌曲".to_string()),
            ],
            &state,
        )
        .expect("play uri route succeeds");

        let value: Value = serde_json::from_str(&body).expect("json response");
        assert_eq!(value["ok"], true);
        assert_eq!(value["queued"], true);
        assert_eq!(value["size"], 1);
        assert_eq!(value["keyword"], "测试歌曲");
        assert_eq!(value["uri"], "fuo://netease/songs/123");

        let queue = state.queue.lock().expect("queue lock");
        let item = queue.front().expect("queued item");
        assert_eq!(item.keyword, "测试歌曲");
        assert_eq!(item.source, "netease");
        assert_eq!(item.uri, "fuo://netease/songs/123");
        assert!(item.dedup_bypass);
        assert!(item.friend_username.is_empty());
    }

    #[test]
    fn source_from_fuo_uri_supports_known_music_sources() {
        assert_eq!(
            source_from_fuo_uri("fuo://qqmusic/songs/1"),
            Some("qqmusic")
        );
        assert_eq!(
            source_from_fuo_uri("fuo://netease/songs/1"),
            Some("netease")
        );
        assert_eq!(source_from_fuo_uri("fuo://local/songs/1"), None);
    }

    #[test]
    fn remote_next_builds_console_game_command() {
        let pending = remote_control_command("下一首".to_string(), "下一首", UserCommand::Next);

        assert_eq!(pending.parsed.message_type, "控制台");
        assert_eq!(pending.parsed.username, "控制台");
        assert_eq!(pending.parsed.raw, "下一首");
        assert_eq!(pending.parsed.user_command, "@下一首");
        assert!(matches!(pending.parsed.command, UserCommand::Next));
    }

    #[test]
    fn remote_volume_builds_console_game_command() {
        let pending = remote_control_command(
            "音量 60".to_string(),
            "音量",
            UserCommand::Volume("60".to_string()),
        );

        assert_eq!(pending.parsed.raw, "音量 60");
        assert_eq!(pending.parsed.user_command, "@音量 60");
        assert!(matches!(
            pending.parsed.command,
            UserCommand::Volume(ref volume) if volume == "60"
        ));
    }

    #[test]
    fn refresh_button_runs_full_uncached_refresh() {
        assert!(PAGE.contains("onclick=\"refreshAll()\""));
        assert!(PAGE.contains("async function refreshAll()"));
        assert!(PAGE.contains("refreshPlayer()"));
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
        assert_eq!(
            pending_task_labels(&state).expect("pending labels"),
            vec!["控制台命令: 继续"]
        );
        let tasks = state.task_tracker.recent().expect("task snapshots");
        assert_eq!(tasks[0].id, task_id);
        assert_eq!(tasks[0].status, "queued");
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
        assert!(
            pending_task_labels(&state)
                .expect("pending labels")
                .is_empty()
        );
        let task = state
            .task_tracker
            .recent()
            .expect("task snapshots")
            .into_iter()
            .find(|task| task.id == task_id)
            .expect("canceled task");
        assert_eq!(task.status, "canceled");
        assert!(task.result.expect("cancel result").contains("控制台撤销"));
    }

    #[test]
    fn started_formal_task_returns_conflict_instead_of_being_canceled() {
        let state = test_state();
        let body = pause_route(&[], &state).expect("pause route");
        let response: Value = serde_json::from_str(&body).expect("pause response json");
        let task_id = response["taskId"].as_u64().expect("task id");
        let (lock, _) = &*state.pending;
        let started = lock
            .lock()
            .expect("pending queue")
            .pop_front()
            .expect("queued task");
        assert_eq!(started.id, task_id);
        state.task_tracker.mark_running(task_id);

        let error = task_cancel_route(&[("id".to_string(), task_id.to_string())], &state)
            .expect_err("started task must not be canceled");

        assert_eq!(error.status, 409);
        assert!(error.message.contains("不能撤销"));
        let task = state
            .task_tracker
            .recent()
            .expect("task snapshots")
            .into_iter()
            .find(|task| task.id == task_id)
            .expect("running task");
        assert_eq!(task.status, "running");
    }

    #[test]
    fn canceling_secondary_recovery_releases_unread_claim() {
        let state = test_state();
        state
            .chat_listener
            .complete_mode_switch(ChatListenerMode::Secondary);
        assert!(state.chat_listener.claim_unread_task());
        let receipt = enqueue_pending_task(&state, super::super::PendingTask::RestoreSecondaryHall)
            .expect("enqueue recovery");

        task_cancel_route(&[("id".to_string(), receipt.task_id.to_string())], &state)
            .expect("cancel recovery");

        assert!(!state.chat_listener.snapshot().unread_task_pending);
    }

    #[test]
    fn canceling_moderation_result_releases_workflow_and_listener_hold() {
        let state = test_state();
        state
            .chat_listener
            .complete_mode_switch(ChatListenerMode::Secondary);
        let command = command::ModerationCommand {
            action: command::ModerationAction::Blacklist,
            uid: "123456789".to_string(),
            requester: "测试用户".to_string(),
        };
        let moderation = ModerationService::new(ModerationPolicy::new(
            std::time::Duration::from_secs(120),
            std::time::Duration::from_secs(2),
            3,
            3,
        ));
        let mut port = TestModerationCommandPort {
            listener: state.chat_listener.clone(),
        };
        let crate::features::moderation::ModerationStart::Started(work) = moderation
            .start(&command, &mut port)
            .expect("start moderation")
        else {
            panic!("moderation should start");
        };
        let receipt = enqueue_pending_task(
            &state,
            super::super::PendingTask::ModerationResult(work.finish(false)),
        )
        .expect("enqueue moderation result");
        assert!(state.chat_listener.snapshot().temporary_primary);

        task_cancel_route(&[("id".to_string(), receipt.task_id.to_string())], &state)
            .expect("cancel moderation result");

        assert!(!moderation.is_active(&command).unwrap());
        assert!(!state.chat_listener.snapshot().temporary_primary);
    }

    #[test]
    fn canceling_card_game_timeout_releases_entertainment_session() {
        let state = test_state();
        let entertainment = EntertainmentCoordinator::new();
        let service = crate::features::card_games::CardGameService::new(
            crate::features::card_games::LandlordConfig {
                lobby_timeout_seconds: 1,
                ..crate::features::card_games::LandlordConfig::default()
            },
            entertainment.clone(),
        );
        let idiom_chain = crate::features::idiom_chain::IdiomChainService::from_entries_for_test(
            &["画蛇添足", "足智多谋"],
            entertainment.clone(),
            None,
        );
        let runtime = crate::runtime::business::BusinessRuntime::start(8, idiom_chain, service)
            .expect("start business runtime");
        let business = runtime.handle();
        let started_at = std::time::Instant::now();
        let verification = match business
            .begin_card_game(
                "甲",
                &crate::features::card_games::LandlordCommand::Start,
                started_at,
            )
            .expect("begin card game")
        {
            crate::features::card_games::CardGameCommandStart::Suspended(request) => request,
            crate::features::card_games::CardGameCommandStart::Completed(_) => {
                panic!("start should require verification")
            }
        };
        assert!(matches!(
            business
                .claim_card_game_effect(verification.key)
                .expect("claim verification"),
            crate::features::card_games::CardGameEffectClaim::Claimed
        ));
        let hall = match business
            .resume_card_game(
                verification.key,
                crate::features::card_games::CardGameEffectResult::FriendVerify(Ok(true)),
            )
            .expect("resume verification")
        {
            crate::features::card_games::CardGameResume::Suspended(request) => request,
            other => panic!("verified start should announce lobby: {other:?}"),
        };
        assert!(matches!(
            business
                .claim_card_game_effect(hall.key)
                .expect("claim hall announcement"),
            crate::features::card_games::CardGameEffectClaim::Claimed
        ));
        assert!(matches!(
            business
                .resume_card_game(
                    hall.key,
                    crate::features::card_games::CardGameEffectResult::HallDelivery(Ok(())),
                )
                .expect("resume hall announcement"),
            crate::features::card_games::CardGameResume::Completed(_)
        ));
        let outcome = business
            .tick_card_game(started_at + std::time::Duration::from_secs(2), true)
            .expect("tick card game")
            .expect("lobby timeout");
        let action = outcome.action();
        let request = outcome.into_request();
        let receipt = enqueue_pending_task(
            &state,
            super::super::PendingTask::CardGameEffect(super::super::QueuedCardGameEffect::new(
                business.clone(),
                action,
                request,
            )),
        )
        .expect("enqueue card game delivery");

        task_cancel_route(&[("id".to_string(), receipt.task_id.to_string())], &state)
            .expect("cancel card game delivery");

        assert_eq!(entertainment.active(), None);
        assert!(!business.abort_card_game().expect("query remaining game"));
        runtime.shutdown().expect("shutdown business runtime");
    }

    #[test]
    fn web_decision_submission_reaches_active_song_decision() {
        let state = test_state();
        let session = state
            .decision_control
            .begin(
                "点歌候选确认",
                true,
                false,
                std::time::Duration::from_secs(1),
            )
            .expect("decision session");
        let id = state
            .decision_control
            .snapshot()
            .expect("decision snapshot")
            .expect("active decision")
            .id;

        decision_submit_route(
            &[
                ("id".to_string(), id.to_string()),
                ("action".to_string(), "switch_source".to_string()),
            ],
            &state,
        )
        .expect("decision route");

        assert_eq!(
            session
                .wait(std::time::Duration::from_millis(1))
                .expect("decision wait"),
            Some(DecisionAction::SwitchSource)
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

        let labels = pending_task_labels(&state).expect("pending labels");
        assert_eq!(
            labels,
            vec![
                "控制台发言: [控制台]: 你好",
                "控制台发言: [远程] 你好",
                "控制台发言: 你好",
            ]
        );
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
                .expect("remote song command");

        assert_eq!(pending.parsed.message_type, "控制台");
        assert_eq!(pending.parsed.username, "控制台");
        assert_eq!(pending.parsed.raw, "点歌 晴天 伴奏");
        match pending.parsed.command {
            UserCommand::Song(song) => {
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
            .expect("remote ai song command");

        assert_eq!(pending.parsed.raw, "AI点歌 晴天");
        match pending.parsed.command {
            UserCommand::Song(song) => {
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
        .expect("remote bilibili song command");

        assert_eq!(pending.parsed.raw, "B站点歌 耀斑 HOYO-MiX");
        match pending.parsed.command {
            UserCommand::Song(song) => {
                assert_eq!(song.source, SongSource::Bilibili);
                assert_eq!(song.prefix, "B站点歌");
            }
            _ => panic!("expected song command"),
        }
    }

    #[test]
    fn queue_removal_by_id_survives_automatic_front_shift() {
        let state = test_state();
        let third_id = {
            let mut queue = state.queue.lock().expect("queue lock");
            for keyword in ["第一首", "第二首", "第三首"] {
                queue
                    .push(QueueItem {
                        keyword: keyword.to_string(),
                        ..QueueItem::default()
                    })
                    .expect("queue push");
            }
            let third_id = queue.items()[2].id;
            assert_eq!(
                queue.shift().expect("queue shift").unwrap().keyword,
                "第一首"
            );
            third_id
        };

        let body = queue_remove(&[("id".to_string(), third_id.to_string())], &state)
            .expect("remove by id");
        let response: Value = serde_json::from_str(&body).expect("remove response json");

        assert_eq!(response["removed"]["id"], third_id);
        assert_eq!(response["removed"]["keyword"], "第三首");
        let queue = state.queue.lock().expect("queue lock");
        assert_eq!(queue.items().len(), 1);
        assert_eq!(queue.items()[0].keyword, "第二首");
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
        assert_eq!(
            state.chat_listener.snapshot().mode,
            ChatListenerMode::Primary
        );
        assert_eq!(
            state.chat_listener.snapshot().pending_mode,
            Some(ChatListenerMode::Secondary)
        );
        assert_eq!(
            pending_task_labels(&state).expect("pending labels"),
            vec!["切换二级监听"]
        );
    }

    fn test_state() -> HttpTestState {
        test_state_with_search_port(HttpTestSearchPort::successful())
    }

    fn test_state_with_search_port(search_port: impl PlayerSearchPort) -> HttpTestState {
        let mut config: AppConfig =
            serde_yaml::from_str(include_str!("../../config.yaml")).expect("default config");
        let suffix = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mwm-http-test-{suffix}"));
        std::fs::create_dir_all(&dir).expect("test dir");
        config.state.queue_path = dir.join("queue.json");
        config.state.runtime_state_path = dir.join("runtime-state.json");
        config.turtle_soup.question_bank_path = dir.join("turtle-soup.yaml");

        let queue = Arc::new(Mutex::new(
            PersistentQueue::load(config.state.queue_path.clone(), config.queue.max_size)
                .expect("queue"),
        ));
        let runtime_state = Arc::new(Mutex::new(
            PersistentRuntimeState::load(config.state.runtime_state_path.clone())
                .expect("runtime state"),
        ));
        let pending = Arc::new((
            Mutex::new(VecDeque::<super::super::TrackedPendingTask>::new()),
            Condvar::new(),
        ));
        let monitor = MonitorShared::new(20);
        let turtle_soup = TurtleSoupService::new(
            config.turtle_soup.clone(),
            EntertainmentCoordinator::new(),
            DeferredChatQueue::new(32),
        );
        let undercover =
            UndercoverService::new(config.undercover.clone(), EntertainmentCoordinator::new());
        let player_runtime_config = config.player_runtime_config().expect("player config");
        let player_runtime = PlayerRuntime::start(
            HttpTestObservationPort,
            HttpTestControlPort,
            search_port,
            player_runtime_config,
        )
        .expect("player runtime");
        let player_search =
            PlayerSearchClient::new(player_runtime.handle(), BusinessOperationIdAllocator::new());
        HttpTestState {
            state: HttpSharedState::new(
                config,
                queue,
                runtime_state,
                pending,
                ChatListenerShared::new(),
                turtle_soup,
                undercover,
                monitor,
                TaskTrackerShared::new(),
                DecisionControlShared::new(),
                WebToolShared::new(),
                Arc::new(Mutex::new(None)),
                player_search,
            ),
            _player_runtime: player_runtime,
        }
    }
}
