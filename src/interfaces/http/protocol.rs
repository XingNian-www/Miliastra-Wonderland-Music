use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::net::SocketAddr;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

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

#[path = "tools.rs"]
mod tools;

#[path = "routes/ai.rs"]
mod ai_routes;
#[path = "routes/config.rs"]
mod config_routes;
#[path = "routes/entertainment.rs"]
mod entertainment_routes;
#[path = "routes/operator.rs"]
mod operator_routes;
#[path = "routes/playback.rs"]
mod playback_routes;
#[path = "routes/system.rs"]
mod system_routes;
#[path = "routes/tasks_chat.rs"]
mod tasks_chat_routes;

use ai_routes::*;
use config_routes::*;
use entertainment_routes::*;
use operator_routes::*;
use playback_routes::*;
use system_routes::*;
use tasks_chat_routes::*;

use super::ports::{
    HttpApplicationPorts, HttpCommandError, HttpLoginError, HttpPlayerSearchError, PlayTrackRequest,
};

#[cfg(test)]
use crate::config::AppConfig;
#[cfg(test)]
use crate::config::OcrConfig;
use crate::config::{HttpConfig, ScreenConfig, TemplateConfig, TimingConfig};
#[cfg(test)]
use crate::features::administration::{
    AdministrationMutationIntent, AdministrationMutationOutcome,
};
#[cfg(test)]
use crate::features::command::ModuleCommand;
use crate::features::custom_workflow::CustomWorkflowConfig;
#[cfg(test)]
use crate::features::custom_workflow::CustomWorkflowService;
#[cfg(test)]
use crate::features::custom_workflow::WorkflowDefaults;
use crate::features::hall::{HallCommand, HallMutationIntent, HallMutationOutcome, HallStatePatch};
use crate::features::invite::InviteConfig;
use crate::features::moderation::ModerationConfig;
#[cfg(test)]
use crate::features::playback::ActivePlaybackRequest;
use crate::features::playback::{
    PlaybackCommand, PlaybackMutationIntent, PlaybackMutationOutcome, QueueItem, QueueRemoval,
    QueueRemoveOutcome,
};
#[cfg(test)]
use crate::features::song_request::SongSource;
use crate::features::startup::{StartupConfig, StartupSource, StartupTask};
use crate::features::turtle_soup::{
    TurtleSoupMutationIntent, TurtleSoupMutationOutcome, TurtleSoupSubmission,
};
#[cfg(test)]
use crate::interfaces::chat::PendingCommand;
use crate::runtime::business::{BusinessMutationIntent, BusinessMutationOutcome};
use crate::runtime::chat_listener::ChatListenerMode;
use crate::runtime::decision::DecisionAction;
use crate::runtime::monitor::MonitorShared;
use crate::runtime::scheduler::{FormalTaskCancelOutcome, FormalTaskEnqueueOutcome};
use crate::ui::frame::LatestFrameCache;
use crate::ui::geometry::parse_rect;
use miliastra_playback::{ProviderId, TrackKey};
pub(crate) use tools::{WebToolRequest, WebToolTemplate};
use uuid::Uuid;

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
    BodyRouteSpec {
        path: "/config/validate",
        handler: config_validate_route,
    },
    BodyRouteSpec {
        path: "/config/save",
        handler: config_save_route,
    },
    BodyRouteSpec {
        path: "/config/rollback",
        handler: config_rollback_route,
    },
];

const ROUTES: &[RouteSpec] = &[
    RouteSpec {
        path: "/status",
        json: true,
        mutating: false,
        handler: status_route,
    },
    RouteSpec {
        path: "/playback/insights",
        json: true,
        mutating: false,
        handler: playback_insights_route,
    },
    RouteSpec {
        path: "/playback/cache/tracks",
        json: true,
        mutating: false,
        handler: playback_cache_tracks_route,
    },
    RouteSpec {
        path: "/playback/statistics/reset",
        json: true,
        mutating: true,
        handler: playback_statistics_reset_route,
    },
    RouteSpec {
        path: "/playback/cache/invalidate",
        json: true,
        mutating: true,
        handler: playback_cache_invalidate_route,
    },
    RouteSpec {
        path: "/playback/song/delete",
        json: true,
        mutating: true,
        handler: playback_song_delete_route,
    },
    RouteSpec {
        path: "/playback/seek",
        json: true,
        mutating: true,
        handler: playback_seek_route,
    },
    RouteSpec {
        path: "/playback/mode",
        json: true,
        mutating: true,
        handler: playback_mode_route,
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
        path: "/player/login/refresh",
        json: true,
        mutating: true,
        handler: player_login_refresh_route,
    },
    RouteSpec {
        path: "/player/kugou/status",
        json: true,
        mutating: false,
        handler: player_kugou_status_route,
    },
    RouteSpec {
        path: "/player/account/status",
        json: true,
        mutating: false,
        handler: player_account_status_route,
    },
    RouteSpec {
        path: "/player/account/refresh",
        json: true,
        mutating: true,
        handler: player_account_refresh_route,
    },
    RouteSpec {
        path: "/player/kugou/claim-vip",
        json: true,
        mutating: true,
        handler: player_kugou_claim_vip_route,
    },
    RouteSpec {
        path: "/player/kugou/upgrade-vip",
        json: true,
        mutating: true,
        handler: player_kugou_upgrade_vip_route,
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
        path: "/config",
        json: true,
        mutating: false,
        handler: config_route,
    },
    RouteSpec {
        path: "/config/schema",
        json: true,
        mutating: false,
        handler: config_schema_route,
    },
    RouteSpec {
        path: "/config/section",
        json: true,
        mutating: false,
        handler: config_section_route,
    },
    RouteSpec {
        path: "/config/revisions",
        json: true,
        mutating: false,
        handler: config_revisions_route,
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
    pub history: Arc<Mutex<VecDeque<HistoryItem>>>,
    pub active_connections: Arc<AtomicUsize>,
    pub active_reload_blockers: Arc<AtomicUsize>,
    pub reload_draining: Arc<AtomicBool>,
    application: HttpApplicationPorts,
    latest_frame: Arc<Mutex<LatestFrameCache>>,
    /// 配置中心共享句柄（阶段 5 起由 HTTP 配置接口持有）。
    pub config_store: crate::config::SharedConfigStore,
    /// 热更新共享句柄集合（阶段 7）：配置保存/回滚成功后 apply，
    /// 使 schema 中标 Live 的字段立即作用于运行态。
    pub live_configs: crate::config::LiveConfigs,
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
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
        monitor: MonitorShared,
        latest_frame: Arc<Mutex<LatestFrameCache>>,
        config_store: crate::config::SharedConfigStore,
        live_configs: crate::config::LiveConfigs,
        application: HttpApplicationPorts,
    ) -> Self {
        Self {
            config,
            monitor,
            history: Arc::new(Mutex::new(VecDeque::new())),
            active_connections: Arc::new(AtomicUsize::new(0)),
            active_reload_blockers: Arc::new(AtomicUsize::new(0)),
            reload_draining: Arc::new(AtomicBool::new(false)),
            application,
            latest_frame,
            config_store,
            live_configs,
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
        // 给 worker 有限时间退出；keep-alive 连接未断开时不能无限阻塞程序退出。
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if worker.is_finished() {
                return worker
                    .join()
                    .map_err(|_| anyhow!("HTTP server thread panicked"))?;
            }
            if Instant::now() >= deadline {
                log::warn!("HTTP server 5 秒内未退出，放弃等待");
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
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
    let blocks_idle_reload = request_blocks_idle_reload(uri.path());
    let active = state.active_connections.fetch_add(1, Ordering::SeqCst);
    let _guard = ActiveConnectionGuard {
        counter: state.active_connections.clone(),
    };
    let _reload_blocker_guard = if blocks_idle_reload {
        match try_begin_reload_blocking_request(&state) {
            Some(guard) => Some(guard),
            None => {
                return plain_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "配置重载排空中，请稍后重试".to_string(),
                    Vec::new(),
                );
            }
        }
    } else {
        None
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
        Err(error) => {
            // 内部错误详情只记日志,响应体不包含 panic 位置/内部路径。
            log::error!("HTTP 请求处理失败(worker panic): {error:#}");
            plain_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "错误: HTTP请求处理失败".to_string(),
                default_cors_headers(&fallback_host, fallback_port),
            )
        }
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
            message: "未知接口".to_string(),
        })
    }
}

fn route_spec(path: &str) -> Option<&'static RouteSpec> {
    ROUTES.iter().find(|route| route.path == path)
}

fn body_route_spec(path: &str) -> Option<&'static BodyRouteSpec> {
    BODY_ROUTES.iter().find(|route| route.path == path)
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

fn push_history(request: &Request, result: &str, ok: bool, state: &HttpSharedState) {
    if request.path.starts_with("/tools/")
        || matches!(
            request.path.as_str(),
            "/history"
                | "/clear-history"
                | "/monitor"
                | "/status"
                | "/playback/insights"
                | "/playback/cache/tracks"
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
        if !part.is_empty()
            && part != "qqmusic"
            && part != "netease"
            && part != "bilibili"
            && part != "kugou"
        {
            return Err(bad_request(
                "source参数只允许qqmusic、netease、bilibili或kugou",
            ));
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
    header_value(request, "x-miliastra-token").is_some_and(|value| {
        // 常数时间比较，避免长度/逐字节时序侧信道泄露 token 信息。
        value.len() == expected.len()
            && value
                .bytes()
                .zip(expected.bytes())
                .fold(0_u8, |diff, (actual, want)| diff | (actual ^ want))
                == 0
    })
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

fn request_blocks_idle_reload(path: &str) -> bool {
    is_mutating_route(path) || matches!(path, "/hall-screenshot" | "/search" | "/search/candidates")
}

fn try_begin_reload_blocking_request(state: &HttpSharedState) -> Option<ActiveConnectionGuard> {
    if state.reload_draining.load(Ordering::SeqCst) {
        return None;
    }
    state.active_reload_blockers.fetch_add(1, Ordering::SeqCst);
    let guard = ActiveConnectionGuard {
        counter: state.active_reload_blockers.clone(),
    };
    if state.reload_draining.load(Ordering::SeqCst) {
        drop(guard);
        None
    } else {
        Some(guard)
    }
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
    // 内部错误详情只写日志，响应体用通用信息，避免泄漏内部路径/错误链。
    log::error!("HTTP 内部错误: {error:#}");
    AppError {
        status: 500,
        message: "内部错误".to_string(),
    }
}

fn command_error(error: HttpCommandError) -> AppError {
    AppError {
        status: error.status,
        message: error.message,
    }
}

fn player_search_error(error: HttpPlayerSearchError) -> AppError {
    // 搜索失败原因面向用户可读（如“队列已满/无结果”），直接透传，不走 internal_error。
    AppError {
        status: 500,
        message: error.to_string(),
    }
}

fn login_http_error(error: HttpLoginError) -> AppError {
    let status = match error.code.as_str() {
        "unsupported_provider" | "invalid_helper_provider" | "invalid_helper_credential" => 400,
        "provider_auth_required" | "relogin_required" => 401,
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
#[path = "protocol/tests.rs"]
mod tests;
