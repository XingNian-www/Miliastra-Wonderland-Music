use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::ai;
use super::config::AppConfig;
use super::feeluown::FeelUOwnClient;
use super::notification;
use super::queue::{PersistentQueue, QueueItem};
use super::runtime_state::PersistentRuntimeState;

const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_CONNECTIONS: usize = 32;
const PAGE: &str = include_str!("page.html");
const KNOWN_ROUTES: &str = "/status, /play, /pause, /skip-next, /skip-prev, /volume, /searchPlay, /searchSource, /search, /open-scheme, /history, /clear-history, /health, /admin-status, /restart-admin, /active-window, /notify, /queue, /queue/add, /queue/remove, /queue/clear, /state, /state/save, /ai/recognize, /ai/match";

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
    headers: Vec<(String, String)>,
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
    log::info!("HTTP/Web 面板已启动: http://{}", bind_addr);
    thread::spawn(move || accept_loop(listener, state));
    Ok(())
}

fn accept_loop(listener: TcpListener, state: HttpSharedState) {
    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let active = state.active_connections.fetch_add(1, Ordering::SeqCst);
                if active >= MAX_ACTIVE_CONNECTIONS {
                    state.active_connections.fetch_sub(1, Ordering::SeqCst);
                    let response = http_response(
                        503,
                        "text/plain; charset=utf-8",
                        "服务繁忙，请稍后再试",
                        Vec::new(),
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                let state = state.clone();
                let counter = state.active_connections.clone();
                thread::spawn(move || {
                    let _guard = ActiveConnectionGuard { counter };
                    handle_connection(stream, state);
                });
            }
            Err(error) => log::error!("HTTP 连接失败: {error}"),
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

fn handle_connection(mut stream: TcpStream, state: HttpSharedState) {
    let response =
        match read_request(&mut stream).and_then(|request| handle_request(request, &state)) {
            Ok(response) => response,
            Err(error) => http_response(
                error.status,
                "text/plain; charset=utf-8",
                &format!("错误: {}", error.message),
                default_cors_headers(&state.config.http.host, state.config.http.port),
            ),
        };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

fn read_request(stream: &mut TcpStream) -> std::result::Result<Request, AppError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(internal_error)?;
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 2048];
    loop {
        let size = stream.read(&mut chunk).map_err(internal_error)?;
        if size == 0 {
            if buffer.is_empty() {
                return Err(bad_request("空请求"));
            }
            break;
        }
        buffer.extend_from_slice(&chunk[..size]);
        if buffer.len() > MAX_REQUEST_HEADER_BYTES {
            return Err(bad_request("请求头过大"));
        }
        if find_header_end(&buffer).is_some() {
            break;
        }
    }

    let header_end = find_header_end(&buffer).unwrap_or(buffer.len());
    let text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = text.split("\r\n");
    let first = lines.next().ok_or_else(|| bad_request("无效请求"))?;
    let mut first_parts = first.split_whitespace();
    let method = first_parts.next().unwrap_or("").to_string();
    let target = first_parts.next().unwrap_or("/");
    let (path, query) = parse_target(target)?;
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            line.split_once(':')
                .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    Ok(Request {
        method,
        path,
        query,
        headers,
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn handle_request(
    request: Request,
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    if !is_allowed_origin(&request, &state.config.http.host, state.config.http.port) {
        return Err(AppError {
            status: 403,
            message: "不允许的请求来源".to_string(),
        });
    }

    if request.method == "OPTIONS" {
        return Ok(http_response(
            204,
            "text/plain; charset=utf-8",
            "",
            options_headers(&request, &state.config.http.host, state.config.http.port),
        ));
    }

    enforce_method(&request, state)?;

    if request.path == "/" && request.query.is_empty() {
        return Ok(http_response(
            200,
            "text/html; charset=utf-8",
            PAGE,
            cors_headers(&request, &state.config.http.host, state.config.http.port),
        ));
    }
    if request.path == "/favicon.ico" {
        return Ok(http_response(
            204,
            "text/plain; charset=utf-8",
            "",
            cors_headers(&request, &state.config.http.host, state.config.http.port),
        ));
    }

    let routed = route(&request.path, &request.query, state);
    let (body, ok) = match routed {
        Ok(body) => (body, true),
        Err(error) => {
            push_history(&request, &error.message, false, state);
            return Ok(http_response(
                error.status,
                "text/plain; charset=utf-8",
                &format!("错误: {}", error.message),
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
    Ok(http_response(
        200,
        content_type,
        &body,
        cors_headers(&request, &state.config.http.host, state.config.http.port),
    ))
}

fn route(
    path: &str,
    query: &[(String, String)],
    state: &HttpSharedState,
) -> std::result::Result<String, AppError> {
    let client = FeelUOwnClient::new(&state.config.feeluown);
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
        "/admin-status" => admin_status_json(state.config.window.active_check_timeout_ms),
        "/restart-admin" => Ok(json!({
            "ok": false,
            "supported": true,
            "reason": "程序不会自动以管理员权限重启"
        })
        .to_string()),
        "/active-window" => active_window_json(query, state),
        "/ai/recognize" => {
            ai::recognize_with_query(&state.config.ai, query).map_err(|error| AppError {
                status: if is_client_error(&error.to_string()) {
                    400
                } else {
                    500
                },
                message: error.to_string(),
            })
        }
        "/ai/match" => ai::match_with_query(&state.config.ai, query).map_err(|error| AppError {
            status: if is_client_error(&error.to_string()) {
                400
            } else {
                500
            },
            message: error.to_string(),
        }),
        "/notify" => {
            let title = query_value(query, "title").unwrap_or("点歌命令待处理");
            let message = query_value(query, "message").unwrap_or("");
            Ok(
                json!({ "ok": notification::send_windows_notification(title, message) })
                    .to_string(),
            )
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
        state.config.window.active_check_timeout_ms,
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
            | "/notify"
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

fn parse_target(target: &str) -> std::result::Result<(String, Vec<(String, String)>), AppError> {
    if target.trim().is_empty() {
        return Err(bad_request("无效请求URL"));
    }
    let target = normalize_request_target(target)?;
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    if path.is_empty() || !path.starts_with('/') {
        return Err(bad_request("无效请求URL"));
    }
    let path = path.to_string();
    let query = if query.is_empty() {
        Vec::new()
    } else {
        query
            .split('&')
            .filter(|part| !part.is_empty())
            .map(|part| {
                let (key, value) = part.split_once('=').unwrap_or((part, ""));
                Ok((percent_decode(key)?, percent_decode(value)?))
            })
            .collect::<std::result::Result<Vec<_>, AppError>>()?
    };
    Ok((path, query))
}

fn normalize_request_target(target: &str) -> std::result::Result<String, AppError> {
    if target.starts_with('/') {
        return Ok(target.to_string());
    }
    if let Some(after_scheme) = target.strip_prefix("http://") {
        let path_start = after_scheme.find('/').unwrap_or(after_scheme.len());
        let path = &after_scheme[path_start..];
        return Ok(if path.is_empty() { "/" } else { path }.to_string());
    }
    if let Some(after_scheme) = target.strip_prefix("https://") {
        let path_start = after_scheme.find('/').unwrap_or(after_scheme.len());
        let path = &after_scheme[path_start..];
        return Ok(if path.is_empty() { "/" } else { path }.to_string());
    }
    Err(bad_request("无效请求URL"))
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

fn active_window_status(target: &str, auto_activate: bool, timeout_ms: u64) -> Result<String> {
    if target.trim().is_empty() {
        return Ok(json!({
            "supported": true,
            "enabled": false,
            "active": true,
            "reason": "no-target",
        })
        .to_string());
    }
    let payload = json!({ "target": target, "autoActivate": auto_activate }).to_string();
    let script = r#"
Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class Win32Window {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern uint SendInput(uint cInputs, INPUT[] pInputs, int cbSize);
}

[StructLayout(LayoutKind.Sequential)]
public struct INPUT {
  public uint type;
  public INPUTUNION u;
}

[StructLayout(LayoutKind.Explicit)]
public struct INPUTUNION {
  [FieldOffset(0)] public MOUSEINPUT mi;
  [FieldOffset(0)] public KEYBDINPUT ki;
  [FieldOffset(0)] public HARDWAREINPUT hi;
}

[StructLayout(LayoutKind.Sequential)]
public struct MOUSEINPUT {
  public int dx;
  public int dy;
  public uint mouseData;
  public uint dwFlags;
  public uint time;
  public IntPtr dwExtraInfo;
}

[StructLayout(LayoutKind.Sequential)]
public struct KEYBDINPUT {
  public ushort wVk;
  public ushort wScan;
  public uint dwFlags;
  public uint time;
  public IntPtr dwExtraInfo;
}

[StructLayout(LayoutKind.Sequential)]
public struct HARDWAREINPUT {
  public uint uMsg;
  public ushort wParamL;
  public ushort wParamH;
}
"@
$inputData = [Console]::In.ReadToEnd() | ConvertFrom-Json
$target = [string]$inputData.target
$autoActivate = [bool]$inputData.autoActivate
$handle = [Win32Window]::GetForegroundWindow()
$builder = New-Object System.Text.StringBuilder 512
[void][Win32Window]::GetWindowText($handle, $builder, $builder.Capacity)
$processId = 0
[void][Win32Window]::GetWindowThreadProcessId($handle, [ref]$processId)
$processName = ""
try { $processName = (Get-Process -Id $processId -ErrorAction Stop).ProcessName } catch {}
$title = $builder.ToString()
$targetProcessName = if ($target.EndsWith(".exe", [StringComparison]::OrdinalIgnoreCase)) { $target.Substring(0, $target.Length - 4) } else { $target }
$targetProcess = Get-Process -Name $targetProcessName -ErrorAction SilentlyContinue | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
$targetProcessId = if ($targetProcess) { [int]$targetProcess.Id } else { 0 }
$active = if ($targetProcessId -ne 0) { [int]$processId -eq $targetProcessId } else { $processName.Equals($targetProcessName, [StringComparison]::OrdinalIgnoreCase) }
$activated = $false
$showWindowResult = $false
$sendInputAltResult = 0
$setForegroundResult = $false
if (-not $active -and $autoActivate -and $targetProcess) {
  $targetHandle = $targetProcess.MainWindowHandle
  $showWindowResult = [Win32Window]::ShowWindow($targetHandle, 9)
  $inputs = New-Object 'INPUT[]' 2
  $inputs[0].type = 1
  $inputs[0].u.ki.wVk = 0x12
  $inputs[1].type = 1
  $inputs[1].u.ki.wVk = 0x12
  $inputs[1].u.ki.dwFlags = 0x0002
  $sendInputAltResult = [Win32Window]::SendInput(2, $inputs, [Runtime.InteropServices.Marshal]::SizeOf([type]'INPUT'))
  $setForegroundResult = [Win32Window]::SetForegroundWindow($targetHandle)
  Start-Sleep -Milliseconds 200
  $handle = [Win32Window]::GetForegroundWindow()
  $builder = New-Object System.Text.StringBuilder 512
  [void][Win32Window]::GetWindowText($handle, $builder, $builder.Capacity)
  $processId = 0
  [void][Win32Window]::GetWindowThreadProcessId($handle, [ref]$processId)
  $processName = ""
  try { $processName = (Get-Process -Id $processId -ErrorAction Stop).ProcessName } catch {}
  $title = $builder.ToString()
  $active = [int]$processId -eq $targetProcessId
  $activated = $active
}
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
[pscustomobject]@{ supported = $true; enabled = $true; active = $active; activated = $activated; showWindow = $showWindowResult; sendInputAlt = $sendInputAltResult; setForeground = $setForegroundResult; autoClick = $false; title = $title; process = $processName; processId = [int]$processId; target = $target; targetProcess = $targetProcessName; targetProcessId = $targetProcessId } | ConvertTo-Json -Compress
"#;
    let output = run_powershell(script, &payload, Duration::from_millis(timeout_ms))?;
    Ok(json_object_output(&output).unwrap_or_else(|| {
        json!({
            "supported": false,
            "enabled": true,
            "active": false,
            "reason": "PowerShell返回无效JSON",
        })
        .to_string()
    }))
}

fn admin_status_json(timeout_ms: u64) -> std::result::Result<String, AppError> {
    let script = r#"
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = New-Object Security.Principal.WindowsPrincipal($identity)
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
[pscustomobject]@{ supported = $true; isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator); user = $identity.Name } | ConvertTo-Json -Compress
"#;
    match run_powershell(script, "", Duration::from_millis(timeout_ms)) {
        Ok(output) if output.trim().is_empty() => Ok("{}".to_string()),
        Ok(output) => Ok(json_object_output(&output).unwrap_or_else(|| {
            json!({
                "supported": false,
                "isAdmin": false,
                "reason": "PowerShell返回无效JSON",
            })
            .to_string()
        })),
        Err(error) => Ok(json!({
            "supported": false,
            "isAdmin": false,
            "reason": error.to_string(),
        })
        .to_string()),
    }
}

fn run_powershell(script: &str, input: &str, timeout: Duration) -> Result<String> {
    let mut child = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("PowerShell执行失败")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .context("PowerShell输入失败")?;
    }
    let started_at = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child.wait_with_output().context("PowerShell执行失败")?;
                if output.status.success() {
                    return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
                }
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                bail!(if stderr.is_empty() {
                    "PowerShell执行失败".to_string()
                } else {
                    stderr
                });
            }
            Ok(None) => {
                if started_at.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("PowerShell执行超时");
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error).context("PowerShell执行失败"),
        }
    }
}

fn json_object_output(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if serde_json::from_str::<Value>(trimmed).is_ok_and(|value| value.is_object()) {
        return Some(trimmed.to_string());
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end < start {
        return None;
    }
    let candidate = &trimmed[start..=end];
    if serde_json::from_str::<Value>(candidate).is_ok_and(|value| value.is_object()) {
        Some(candidate.to_string())
    } else {
        None
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
            | "/history"
            | "/notify"
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
            | "/notify"
            | "/clear-history"
    )
}

fn http_response(
    status: u16,
    content_type: &str,
    body: &str,
    extra_headers: Vec<(String, String)>,
) -> String {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let mut response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        status,
        reason,
        content_type,
        body.as_bytes().len()
    );
    for (key, value) in extra_headers {
        response.push_str(&format!("{}: {}\r\n", key, value));
    }
    response.push_str("\r\n");
    response.push_str(body);
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

fn header_value<'a>(request: &'a Request, key: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(header_key, _)| header_key == key)
        .map(|(_, value)| value.as_str())
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

fn percent_decode(value: &str) -> std::result::Result<String, AppError> {
    let mut output = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex_value(bytes[index + 1]).ok_or_else(|| bad_request("URL编码无效"))?;
                let low = hex_value(bytes[index + 2]).ok_or_else(|| bad_request("URL编码无效"))?;
                output.push(high * 16 + low);
                index += 3;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).map_err(|_| bad_request("URL编码不是UTF-8"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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
