use std::collections::{HashMap, VecDeque};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use image::ColorType;
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::runtime::Builder;
use url::form_urlencoded;

use super::ai;
use super::command::{self, ParsedCommand, PendingCommand, SongCommand, SongSource, UserCommand};
use super::config::AppConfig;
use super::feeluown::FeelUOwnClient;
use super::monitor::{MonitorQueueItem, MonitorShared};
use super::queue::{PersistentQueue, QueueItem};
use super::runtime_state::PersistentRuntimeState;

const MAX_ACTIVE_CONNECTIONS: usize = 32;
const PAGE: &str = include_str!("page.html");
const KNOWN_ROUTES: &str = "/status, /play, /pause, /skip-next, /skip-prev, /volume, /startup/wonderland, /searchPlay, /searchSource, /search, /open-scheme, /history, /clear-history, /health, /monitor, /screenshot, /queue, /queue/add, /queue/remove, /queue/clear, /state, /state/save, /chat/send, /ai/recognize, /ai/match, /ai/pick, /ai/search";

#[derive(Clone)]
pub struct HttpSharedState {
    pub config: AppConfig,
    pub queue: Arc<Mutex<PersistentQueue>>,
    pub runtime_state: Arc<Mutex<PersistentRuntimeState>>,
    pub monitor: MonitorShared,
    pub history: Arc<Mutex<VecDeque<HistoryItem>>>,
    pub active_connections: Arc<AtomicUsize>,
    pending: Arc<(Mutex<VecDeque<super::PendingTask>>, Condvar)>,
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
}

#[derive(Debug)]
struct AppError {
    status: u16,
    message: String,
}

impl HttpSharedState {
    pub(super) fn new(
        config: AppConfig,
        queue: Arc<Mutex<PersistentQueue>>,
        runtime_state: Arc<Mutex<PersistentRuntimeState>>,
        pending: Arc<(Mutex<VecDeque<super::PendingTask>>, Condvar)>,
        monitor: MonitorShared,
    ) -> Self {
        Self {
            config,
            queue,
            runtime_state,
            monitor,
            history: Arc::new(Mutex::new(VecDeque::new())),
            active_connections: Arc::new(AtomicUsize::new(0)),
            pending,
        }
    }
}

pub fn start(state: HttpSharedState) -> Result<()> {
    let bind_addr = format!("{}:{}", state.config.http.host, state.config.http.port);
    let listener = TcpListener::bind(&bind_addr)
        .with_context(|| format!("启动 HTTP/Web 面板失败: {}", bind_addr))?;
    listener
        .set_nonblocking(true)
        .context("set HTTP listener nonblocking")?;
    log::info!("HTTP/Web 面板已启动: http://{}", bind_addr);
    thread::spawn(move || run_server(listener, state));
    Ok(())
}

fn run_server(listener: TcpListener, state: HttpSharedState) {
    let runtime = match Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("HTTP runtime 启动失败: {error}");
            return;
        }
    };
    runtime.block_on(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("HTTP listener 初始化失败: {error}");
                return;
            }
        };
        let app = Router::new()
            .fallback(any(axum_entry))
            .with_state(Arc::new(state));
        if let Err(error) = axum::serve(listener, app).await {
            log::error!("HTTP/Web 面板运行失败: {error}");
        }
    });
}

async fn axum_entry(
    State(state): State<Arc<HttpSharedState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
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
    let request = request_from_axum(method, uri, headers);
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

fn request_from_axum(method: Method, uri: Uri, headers: HeaderMap) -> Request {
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

    enforce_method(&request, state)?;

    if request.path == "/" && request.query.is_empty() {
        return Ok(body_response(
            StatusCode::OK,
            "text/html; charset=utf-8",
            PAGE.to_string(),
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

    let routed = route(&request.path, &request.query, state);
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
    match path {
        "/status" => {
            let client = FeelUOwnClient::new(&state.config.feeluown, &state.config.timing);
            serde_json::to_string(&client.status().map_err(internal_error)?).map_err(internal_error)
        }
        "/play" => enqueue_remote_command(
            state,
            remote_control_command("继续".to_string(), "继续", UserCommand::Resume),
        ),
        "/pause" => enqueue_remote_command(
            state,
            remote_control_command("暂停".to_string(), "暂停", UserCommand::Pause),
        ),
        "/skip-next" => enqueue_remote_command(
            state,
            remote_control_command("下一首".to_string(), "下一首", UserCommand::Next),
        ),
        "/skip-prev" => enqueue_remote_command(
            state,
            remote_control_command("上一首".to_string(), "上一首", UserCommand::Previous),
        ),
        "/volume" => {
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
        "/startup/wonderland" => enqueue_startup_wonderland(state),
        "/searchPlay" => enqueue_remote_song(query, state, false),
        "/searchSource" => enqueue_remote_song(query, state, false),
        "/search" => {
            let keyword = normalize_keyword(query_value(query, "keyword"))?;
            let source = normalize_optional_source(query_value(query, "source"))?;
            let client = FeelUOwnClient::new(&state.config.feeluown, &state.config.timing);
            client.search(&keyword, &source).map_err(internal_error)
        }
        "/open-scheme" => {
            let uri = normalize_fuo_uri(query_value_or(query, "url", "uri"))?;
            let client = FeelUOwnClient::new(&state.config.feeluown, &state.config.timing);
            client.play_uri(uri.trim()).map_err(internal_error)?;
            set_pause_flags(state, false, false)?;
            Ok("已打开 FeelUOwn URI".to_string())
        }
        "/queue" => queue_json(state),
        "/queue/add" => queue_add(query, state),
        "/queue/remove" => queue_remove(query, state),
        "/queue/clear" => queue_clear(state),
        "/state" => state_json(state),
        "/state/save" => state_save(query, state),
        "/chat/send" => chat_send(query, state),
        "/ai/recognize" => ai::recognize_with_query(&state.config.ai, &state.config.timing, query)
            .map_err(|error| AppError {
                status: if is_client_error(&error.to_string()) {
                    400
                } else {
                    500
                },
                message: error.to_string(),
            }),
        "/ai/match" => {
            ai::match_with_query(&state.config.ai, &state.config.timing, query).map_err(|error| {
                AppError {
                    status: if is_client_error(&error.to_string()) {
                        400
                    } else {
                        500
                    },
                    message: error.to_string(),
                }
            })
        }
        "/ai/pick" => {
            ai::pick_with_query(&state.config.ai, &state.config.timing, query).map_err(|error| {
                AppError {
                    status: if is_client_error(&error.to_string()) {
                        400
                    } else {
                        500
                    },
                    message: error.to_string(),
                }
            })
        }
        "/ai/search" => enqueue_remote_song(query, state, true),
        "/history" => history_json(state),
        "/clear-history" => clear_history(state),
        "/monitor" => monitor_json(state),
        "/health" => Ok("OK".to_string()),
        _ => Err(AppError {
            status: 404,
            message: format!("未知接口，可用: {}", KNOWN_ROUTES),
        }),
    }
}

fn enqueue_remote_command(
    state: &HttpSharedState,
    pending: PendingCommand,
) -> std::result::Result<String, AppError> {
    let command = pending.parsed.raw.clone();
    let queued = enqueue_pending_command(state, pending)?;
    Ok(json!({
        "ok": true,
        "queued": queued > 0,
        "duplicate": queued == 0,
        "position": queued,
        "command": command,
    })
    .to_string())
}

fn enqueue_startup_wonderland(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let position = enqueue_pending_task(
        state,
        super::PendingTask::StartAndEnterWonderland {
            source: "远程指挥台",
        },
    )?;
    Ok(json!({
        "ok": true,
        "queued": true,
        "position": position,
        "task": "自动启动并进入千星",
    })
    .to_string())
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
    Ok(json!({
        "ok": true,
        "queued": queued > 0,
        "duplicate": queued == 0,
        "position": queued,
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
            _ => return Err(bad_request("远程点歌source只允许qqmusic或netease")),
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
            keyword,
            source,
            prefer_accompaniment: prefer,
            ai_original_text,
            uri,
            friend_username: String::new(),
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
    if let Some(index_text) = query_value(query, "index") {
        if !index_text.is_empty() {
            let index = index_text
                .parse::<usize>()
                .map_err(|_| bad_request("无效的队列索引"))?;
            if index >= queue.len() {
                return Err(bad_request("无效的队列索引"));
            }
            queue.remove_indexes(&[index]).map_err(internal_error)?;
        } else if !queue.is_empty() {
            queue.remove_indexes(&[0]).map_err(internal_error)?;
        }
    } else if !queue.is_empty() {
        queue.remove_indexes(&[0]).map_err(internal_error)?;
    }
    sync_monitor_queue(&state.monitor, &queue);
    Ok(json!({ "ok": true, "size": queue.len() }).to_string())
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

fn set_pause_flags(
    state: &HttpSharedState,
    paused_by_command: bool,
    paused_for_pending_playback: bool,
) -> std::result::Result<(), AppError> {
    let mut runtime = state
        .runtime_state
        .lock()
        .map_err(|_| internal_message("状态锁已损坏"))?;
    runtime.state_mut().paused_by_command = paused_by_command;
    runtime.state_mut().paused_for_pending_playback = paused_for_pending_playback;
    runtime.save().map_err(internal_error)
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
    let image = super::window::capture_game(&state.config.window).map_err(internal_error)?;
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
    }
    serde_json::to_string(&value).map_err(internal_error)
}

fn chat_send(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let text = normalize_required_text(query_value(query, "text"), "text")?;
    let message = format!("[控制台]: {}", text);
    let position = enqueue_pending_task(state, super::PendingTask::ConsoleChat { text })?;
    Ok(json!({ "ok": true, "queued": true, "position": position, "message": message }).to_string())
}

fn pending_task_labels(state: &HttpSharedState) -> std::result::Result<Vec<String>, AppError> {
    let (lock, _) = &*state.pending;
    let guard = lock
        .lock()
        .map_err(|_| internal_message("待处理任务队列锁已损坏"))?;
    Ok(guard.iter().map(super::PendingTask::label).collect())
}

fn enqueue_pending_task(
    state: &HttpSharedState,
    task: super::PendingTask,
) -> std::result::Result<usize, AppError> {
    let (lock, cvar) = &*state.pending;
    let mut guard = lock
        .lock()
        .map_err(|_| internal_message("待处理任务队列锁已损坏"))?;
    guard.push_back(task);
    let position = guard.len();
    cvar.notify_one();
    Ok(position)
}

fn enqueue_pending_command(
    state: &HttpSharedState,
    pending: PendingCommand,
) -> std::result::Result<usize, AppError> {
    let (lock, cvar) = &*state.pending;
    let mut guard = lock
        .lock()
        .map_err(|_| internal_message("待处理任务队列锁已损坏"))?;
    if guard
        .iter()
        .any(|task| task.same_lock_command(&pending.parsed))
    {
        return Ok(0);
    }
    guard.push_back(super::PendingTask::Command(Box::new(pending)));
    let position = guard.len();
    cvar.notify_one();
    Ok(position)
}

fn push_history(request: &Request, result: &str, ok: bool, state: &HttpSharedState) {
    if matches!(
        request.path.as_str(),
        "/history" | "/clear-history" | "/monitor" | "/screenshot" | "/favicon.ico"
    ) {
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
        .get("currentSongIsRequested")
        .and_then(serde_json::Value::as_bool)
    {
        state.current_song_is_requested = value;
    }
    if let Some(value) = patch
        .get("lastRequestedUri")
        .and_then(serde_json::Value::as_str)
    {
        state.last_requested_uri = value.to_string();
    }
    if let Some(value) = patch
        .get("lastRequestedSong")
        .and_then(serde_json::Value::as_str)
    {
        state.last_requested_song = value.to_string();
    }
    if let Some(value) = patch
        .get("lastRequestedKeyword")
        .and_then(serde_json::Value::as_str)
    {
        state.last_requested_keyword = value.to_string();
    }
    if let Some(value) = patch
        .get("lastRequestedSource")
        .and_then(serde_json::Value::as_str)
    {
        state.last_requested_source = value.to_string();
    }
    if let Some(value) = patch
        .get("lastRequestedPreferAccompaniment")
        .and_then(serde_json::Value::as_bool)
    {
        state.last_requested_prefer_accompaniment = value;
    }
    if let Some(value) = patch
        .get("pausedByCommand")
        .and_then(serde_json::Value::as_bool)
    {
        state.paused_by_command = value;
    }
    if let Some(value) = patch
        .get("pausedForPendingPlayback")
        .and_then(serde_json::Value::as_bool)
    {
        state.paused_for_pending_playback = value;
    }
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
        if !part.is_empty() && part != "qqmusic" && part != "netease" {
            return Err(bad_request("source参数只允许qqmusic或netease"));
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

fn normalize_optional_text(
    value: Option<&str>,
    name: &str,
) -> std::result::Result<String, AppError> {
    Ok(assert_no_control_chars(value.unwrap_or(""), name)?
        .trim()
        .to_string())
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
            {
                "***".to_string()
            } else {
                value.clone()
            };
            (key.clone(), value)
        })
        .collect()
}

fn current_time_text() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    seconds.to_string()
}

fn is_json_route(path: &str) -> bool {
    matches!(
        path,
        "/status"
            | "/play"
            | "/pause"
            | "/skip-next"
            | "/skip-prev"
            | "/volume"
            | "/startup/wonderland"
            | "/queue"
            | "/queue/add"
            | "/queue/remove"
            | "/queue/clear"
            | "/searchPlay"
            | "/searchSource"
            | "/state"
            | "/state/save"
            | "/chat/send"
            | "/ai/recognize"
            | "/ai/match"
            | "/ai/pick"
            | "/ai/search"
            | "/history"
            | "/monitor"
    )
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
    Ok(())
}

fn is_mutating_route(path: &str) -> bool {
    matches!(
        path,
        "/play"
            | "/pause"
            | "/skip-next"
            | "/skip-prev"
            | "/volume"
            | "/startup/wonderland"
            | "/searchPlay"
            | "/searchSource"
            | "/open-scheme"
            | "/queue/add"
            | "/queue/remove"
            | "/queue/clear"
            | "/state/save"
            | "/chat/send"
            | "/ai/recognize"
            | "/ai/match"
            | "/ai/pick"
            | "/ai/search"
            | "/clear-history"
    )
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
    if let Some(origin_value) = origin {
        if !is_same_origin(origin_value, &request_host) {
            return false;
        }
    }
    if origin.is_none() {
        if let Some(fetch_site_value) = fetch_site {
            if fetch_site_value != "same-origin" && fetch_site_value != "none" {
                return false;
            }
        }
    }
    true
}

fn allowed_request_host(
    value: &str,
    configured_host: &str,
    configured_port: u16,
) -> Option<String> {
    let Some((host, port)) = parse_host_header(value) else {
        return None;
    };
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
    if let Some(origin) = header_value(request, "origin") {
        if is_same_origin(origin, &request_host) {
            return vec![
                (
                    "Access-Control-Allow-Origin".to_string(),
                    origin.to_string(),
                ),
                ("Vary".to_string(), "Origin".to_string()),
            ];
        }
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
        "Content-Type".to_string(),
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

fn internal_message(message: &str) -> AppError {
    internal_error(anyhow!(message.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_send_requires_post() {
        assert!(is_mutating_route("/chat/send"));
    }

    #[test]
    fn remote_song_routes_are_queued_json_post_routes() {
        assert!(is_mutating_route("/searchPlay"));
        assert!(is_mutating_route("/ai/search"));
        assert!(is_json_route("/searchPlay"));
        assert!(is_json_route("/ai/search"));
    }

    #[test]
    fn playback_control_routes_are_queued_json_post_routes() {
        for route in ["/play", "/pause", "/skip-next", "/skip-prev", "/volume"] {
            assert!(is_mutating_route(route), "{route} should require POST");
            assert!(is_json_route(route), "{route} should return queued JSON");
        }
    }

    #[test]
    fn startup_wonderland_route_is_queued_json_post_route() {
        assert!(is_mutating_route("/startup/wonderland"));
        assert!(is_json_route("/startup/wonderland"));
        assert!(PAGE.contains("call('/startup/wonderland','POST')"));
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
}
