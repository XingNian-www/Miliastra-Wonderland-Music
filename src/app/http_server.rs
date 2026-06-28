use std::collections::{HashMap, VecDeque};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::Response;
use axum::routing::any;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::runtime::Builder;
use url::form_urlencoded;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM};
use windows::Win32::Security::Authentication::Identity::{GetUserNameExW, NameSamCompatible};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::BOOL;

use super::ai;
use super::config::AppConfig;
use super::feeluown::FeelUOwnClient;
use super::queue::{PersistentQueue, QueueItem};
use super::runtime_state::PersistentRuntimeState;

const MAX_ACTIVE_CONNECTIONS: usize = 32;
const PAGE: &str = include_str!("page.html");
const KNOWN_ROUTES: &str = "/status, /play, /pause, /skip-next, /skip-prev, /volume, /searchPlay, /searchSource, /search, /open-scheme, /history, /clear-history, /health, /admin-status, /restart-admin, /active-window, /queue, /queue/add, /queue/remove, /queue/clear, /state, /state/save, /ai/recognize, /ai/match, /ai/pick";

#[derive(Clone)]
pub struct HttpSharedState {
    pub config: AppConfig,
    pub queue: Arc<Mutex<PersistentQueue>>,
    pub runtime_state: Arc<Mutex<PersistentRuntimeState>>,
    pub history: Arc<Mutex<VecDeque<HistoryItem>>>,
    pub active_connections: Arc<AtomicUsize>,
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
    pub fn new(
        config: AppConfig,
        queue: Arc<Mutex<PersistentQueue>>,
        runtime_state: Arc<Mutex<PersistentRuntimeState>>,
    ) -> Self {
        Self {
            config,
            queue,
            runtime_state,
            history: Arc::new(Mutex::new(VecDeque::new())),
            active_connections: Arc::new(AtomicUsize::new(0)),
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
    let response = match handle_request(request, &state) {
        Ok(response) => response,
        Err(error) => plain_response(
            status_code(error.status),
            format!("错误: {}", error.message),
            default_cors_headers(&state.config.http.host, state.config.http.port),
        ),
    };
    response
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
    let client = FeelUOwnClient::new(&state.config.feeluown, &state.config.timing);
    match path {
        "/status" => {
            serde_json::to_string(&client.status().map_err(internal_error)?).map_err(internal_error)
        }
        "/play" => {
            client.play().map_err(internal_error)?;
            Ok("已恢复播放".to_string())
        }
        "/pause" => {
            client.pause().map_err(internal_error)?;
            Ok("已暂停".to_string())
        }
        "/skip-next" => {
            client.next().map_err(internal_error)?;
            Ok("下一首".to_string())
        }
        "/skip-prev" => {
            client.previous().map_err(internal_error)?;
            Ok("上一首".to_string())
        }
        "/volume" => {
            let volume =
                query_value(query, "volume").ok_or_else(|| bad_request("volume参数必须是0-100"))?;
            if !is_valid_volume(volume) {
                return Err(bad_request("volume参数必须是0-100"));
            }
            client.set_volume(volume).map_err(internal_error)?;
            Ok(format!("音量已设置为 {}", volume))
        }
        "/searchPlay" => {
            let keyword = normalize_keyword(query_value(query, "keyword"))?;
            let source = normalize_source(query_value(query, "source"))?;
            let prefer = parse_bool(query_value_or(
                query,
                "preferAccompaniment",
                "accompaniment",
            ));
            let result = client
                .play_keyword(&keyword, &source, prefer)
                .map_err(|error| AppError {
                    status: if error.to_string().contains("平台无对应歌曲音源") {
                        404
                    } else {
                        500
                    },
                    message: error.to_string(),
                })?;
            Ok(result.message)
        }
        "/searchSource" => {
            let keyword = normalize_keyword(query_value(query, "keyword"))?;
            let source = normalize_source(query_value(query, "source"))?;
            let prefer = parse_bool(query_value_or(
                query,
                "preferAccompaniment",
                "accompaniment",
            ));
            client
                .play_keyword(&keyword, &source, prefer)
                .map_err(internal_error)?;
            Ok(format!(
                "正在搜索: {} ({}){}",
                keyword,
                if source.is_empty() { "默认" } else { &source },
                if prefer { " (伴奏优先)" } else { "" }
            ))
        }
        "/search" => {
            let keyword = normalize_keyword(query_value(query, "keyword"))?;
            let source = normalize_optional_source(query_value(query, "source"))?;
            client.search(&keyword, &source).map_err(internal_error)
        }
        "/open-scheme" => {
            let uri = normalize_fuo_uri(query_value_or(query, "url", "uri"))?;
            client.play_uri(uri.trim()).map_err(internal_error)?;
            Ok("已打开 FeelUOwn URI".to_string())
        }
        "/queue" => queue_json(state),
        "/queue/add" => queue_add(query, state),
        "/queue/remove" => queue_remove(query, state),
        "/queue/clear" => queue_clear(state),
        "/state" => state_json(state),
        "/state/save" => state_save(query, state),
        "/admin-status" => admin_status_json(),
        "/restart-admin" => Ok(json!({
            "ok": false,
            "supported": true,
            "reason": "程序不会自动以管理员权限重启"
        })
        .to_string()),
        "/active-window" => active_window_json(query, state),
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
        "/history" => history_json(state),
        "/clear-history" => clear_history(state),
        "/health" => Ok("OK".to_string()),
        _ => Err(AppError {
            status: 404,
            message: format!("未知接口，可用: {}", KNOWN_ROUTES),
        }),
    }
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
        })
        .map_err(internal_error)?
    {
        return Err(AppError {
            status: 400,
            message: "队列已满".to_string(),
        });
    }
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
    Ok(json!({ "ok": true, "size": queue.len() }).to_string())
}

fn queue_clear(state: &HttpSharedState) -> std::result::Result<String, AppError> {
    let mut queue = state
        .queue
        .lock()
        .map_err(|_| internal_message("队列锁已损坏"))?;
    queue.clear().map_err(internal_error)?;
    Ok(json!({ "ok": true }).to_string())
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

fn active_window_json(
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let query_target = query_value(query, "target");
    let target = query_target.unwrap_or(&state.config.window.target_process);
    let auto_activate = state.config.window.auto_activate_window && query_target.is_none();
    active_window_status(
        target,
        auto_activate,
        state.config.timing.active_after_activate_ms,
    )
    .map_err(internal_error)
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

fn push_history(request: &Request, result: &str, ok: bool, state: &HttpSharedState) {
    if matches!(
        request.path.as_str(),
        "/history"
            | "/clear-history"
            | "/active-window"
            | "/admin-status"
            | "/restart-admin"
            | "/favicon.ico"
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
            let value = if key.eq_ignore_ascii_case("apiKey") || key.eq_ignore_ascii_case("token") {
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

fn active_window_status(
    target: &str,
    auto_activate: bool,
    after_activate_ms: u64,
) -> Result<String> {
    if target.trim().is_empty() {
        return Ok(json!({
            "supported": true,
            "enabled": false,
            "active": true,
            "reason": "no-target",
        })
        .to_string());
    }
    let target_process = target_process_name(target);
    let target_process_label = strip_exe_suffix(target.trim());
    let target_window = find_window_by_process(&target_process)?;
    let target_process_id = target_window
        .map(|window| window.process_id)
        .unwrap_or_default();

    let mut foreground = foreground_window_info();
    let mut active = if target_process_id != 0 {
        foreground.process_id == target_process_id
    } else {
        normalize_process_name(&foreground.process) == target_process
    };
    let mut activated = false;
    let mut show_window_result = false;
    let mut send_input_alt_result = 0_u32;
    let mut set_foreground_result = false;

    if !active && auto_activate {
        if let Some(target_window) = target_window {
            show_window_result = unsafe { ShowWindow(target_window.hwnd, SW_RESTORE).as_bool() };
            send_input_alt_result = send_alt_keypress();
            set_foreground_result = unsafe { SetForegroundWindow(target_window.hwnd).as_bool() };
            thread::sleep(Duration::from_millis(after_activate_ms));

            foreground = foreground_window_info();
            active = foreground.process_id == target_process_id;
            activated = active;
        }
    }

    Ok(json!({
        "supported": true,
        "enabled": true,
        "active": active,
        "activated": activated,
        "showWindow": show_window_result,
        "sendInputAlt": send_input_alt_result,
        "setForeground": set_foreground_result,
        "autoClick": false,
        "title": foreground.title,
        "process": foreground.process,
        "processId": foreground.process_id,
        "target": target,
        "targetProcess": target_process_label,
        "targetProcessId": target_process_id,
    })
    .to_string())
}

fn admin_status_json() -> std::result::Result<String, AppError> {
    match admin_status() {
        Ok((is_admin, user)) => Ok(json!({
            "supported": true,
            "isAdmin": is_admin,
            "user": user,
        })
        .to_string()),
        Err(error) => Ok(json!({
            "supported": false,
            "isAdmin": false,
            "reason": error.to_string(),
        })
        .to_string()),
    }
}

#[derive(Clone, Copy)]
struct ProcessWindow {
    hwnd: HWND,
    process_id: u32,
}

#[derive(Debug)]
struct ForegroundWindowInfo {
    title: String,
    process: String,
    process_id: u32,
}

struct SearchState {
    target: String,
    found: Option<ProcessWindow>,
}

fn foreground_window_info() -> ForegroundWindowInfo {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return ForegroundWindowInfo {
            title: String::new(),
            process: String::new(),
            process_id: 0,
        };
    }

    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    ForegroundWindowInfo {
        title: window_title(hwnd),
        process: process_name(process_id)
            .map(|name| strip_exe_suffix(&name).to_string())
            .unwrap_or_default(),
        process_id,
    }
}

fn window_title(hwnd: HWND) -> String {
    let mut buffer = vec![0_u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..len as usize])
}

fn find_window_by_process(target_process: &str) -> Result<Option<ProcessWindow>> {
    let mut state = SearchState {
        target: target_process.to_string(),
        found: None,
    };
    unsafe {
        EnumWindows(
            Some(enum_windows_proc),
            LPARAM((&mut state as *mut SearchState) as isize),
        )
    }
    .context("EnumWindows failed")?;
    Ok(state.found)
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut SearchState) };
    if !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return true.into();
    }

    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id == 0 {
        return true.into();
    }

    if process_name(process_id)
        .map(|name| normalize_process_name(&name) == state.target)
        .unwrap_or(false)
    {
        state.found = Some(ProcessWindow { hwnd, process_id });
        return false.into();
    }
    true.into()
}

fn process_name(process_id: u32) -> Result<String> {
    if process_id == 0 {
        return Ok(String::new());
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .with_context(|| format!("OpenProcess failed for pid {}", process_id))?;
    let _guard = HandleGuard(process);

    let mut buffer = vec![0_u16; 32768];
    let mut len = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    }
    .with_context(|| format!("QueryFullProcessImageNameW failed for pid {}", process_id))?;
    let path = String::from_utf16_lossy(&buffer[..len as usize]);
    Ok(Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or(path))
}

fn admin_status() -> Result<(bool, String)> {
    let process = unsafe { GetCurrentProcess() };
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
        .context("OpenProcessToken failed")?;
    let _guard = HandleGuard(token);

    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0_u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    }
    .context("GetTokenInformation(TokenElevation) failed")?;

    Ok((elevation.TokenIsElevated != 0, current_user_name()))
}

fn current_user_name() -> String {
    let mut len = 0_u32;
    unsafe { GetUserNameExW(NameSamCompatible, None, &mut len) };
    if len > 0 {
        let mut buffer = vec![0_u16; len as usize];
        if unsafe {
            GetUserNameExW(
                NameSamCompatible,
                Some(windows::core::PWSTR(buffer.as_mut_ptr())),
                &mut len,
            )
        } {
            let usable_len = buffer
                .iter()
                .position(|ch| *ch == 0)
                .unwrap_or(len as usize);
            return String::from_utf16_lossy(&buffer[..usable_len]);
        }
    }

    match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(user)) if !domain.is_empty() && !user.is_empty() => {
            format!("{}\\{}", domain, user)
        }
        (_, Ok(user)) => user,
        _ => String::new(),
    }
}

fn send_alt_keypress() -> u32 {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x12),
                    ..Default::default()
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x12),
                    dwFlags: KEYEVENTF_KEYUP,
                    ..Default::default()
                },
            },
        },
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) }
}

fn target_process_name(value: &str) -> String {
    normalize_process_name(strip_exe_suffix(value.trim()))
}

fn normalize_process_name(value: &str) -> String {
    let mut name = value.trim().to_ascii_lowercase();
    if !name.ends_with(".exe") {
        name.push_str(".exe");
    }
    name
}

fn strip_exe_suffix(value: &str) -> &str {
    let suffix_start = value.len().saturating_sub(4);
    if value
        .get(suffix_start..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
    {
        &value[..value.len() - 4]
    } else {
        value
    }
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn is_json_route(path: &str) -> bool {
    matches!(
        path,
        "/status"
            | "/queue"
            | "/queue/add"
            | "/queue/remove"
            | "/queue/clear"
            | "/state"
            | "/state/save"
            | "/admin-status"
            | "/restart-admin"
            | "/active-window"
            | "/ai/recognize"
            | "/ai/match"
            | "/ai/pick"
            | "/history"
    )
}

fn enforce_method(request: &Request, state: &HttpSharedState) -> std::result::Result<(), AppError> {
    if request.method != "GET" && request.method != "POST" {
        return Err(method_not_allowed("只支持GET或POST"));
    }
    let needs_post = is_mutating_route(&request.path)
        || (request.path == "/active-window" && state.config.window.auto_activate_window);
    if needs_post && request.method != "POST" {
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
            | "/searchPlay"
            | "/searchSource"
            | "/open-scheme"
            | "/queue/add"
            | "/queue/remove"
            | "/queue/clear"
            | "/state/save"
            | "/ai/recognize"
            | "/ai/match"
            | "/ai/pick"
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
