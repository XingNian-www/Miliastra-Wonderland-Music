use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("WebView2 Evergreen Runtime is missing")]
    RuntimeMissing,
    #[error("WebView2 login timed out before required cookies were captured")]
    Timeout,
    #[error("WebView2 COM error: {0}")]
    Com(String),
    #[error("WebView2 I/O error: {0}")]
    Io(String),
}

pub fn capture(
    provider: &str,
    profile: &Path,
    timeout: Duration,
) -> Result<BTreeMap<String, String>, CaptureError> {
    platform::capture(provider, profile, timeout)
}

fn login_url(provider: &str) -> Option<&'static str> {
    match provider {
        "qqmusic" => Some("https://y.qq.com/portal/profile.html"),
        "netease" => Some("https://music.163.com/"),
        "bilibili" => Some("https://www.bilibili.com/"),
        "kugou" => Some("https://www.kugou.com/login/"),
        _ => None,
    }
}

fn allowed_cookie_names(provider: &str) -> &'static [&'static str] {
    match provider {
        "qqmusic" => &[
            "uin",
            "wxuin",
            "p_uin",
            "p_luin",
            "luin",
            "p_skey",
            "p_lskey",
            "skey",
            "lskey",
            "qqmusic_key",
            "qm_keyst",
            "lqm_keyst",
            "login_type",
            "wxopenid",
            "wxaccess_token",
            "wxrefresh_token",
            "wxunionid",
            "psrf_qqopenid",
            "openid",
            "psrf_qqaccess_token",
            "access_token",
            "psrf_qqrefresh_token",
            "refresh_token",
            "psrf_qqrefresh_key",
            "refresh_key",
            "psrf_qqunionid",
        ],
        "netease" => &["MUSIC_U", "__csrf"],
        "kugou" => &["token", "userid", "dfid", "KugouGUID", "kg_mid", "mid"],
        "bilibili" => &[
            "SESSDATA",
            "bili_jct",
            "DedeUserID",
            "DedeUserID__ckMd5",
            "sid",
            "buvid3",
            "buvid4",
            "b_nut",
            "_uuid",
            "b_lsid",
            // 捕获后会从普通 Cookie 集合移除并写入独立 refreshToken 字段。
            "ac_time_value",
        ],
        _ => &[],
    }
}

fn has_required_cookies(provider: &str, cookies: &BTreeMap<String, String>) -> bool {
    match provider {
        "qqmusic" => {
            (cookies.contains_key("uin") || cookies.contains_key("wxuin"))
                && (cookies.contains_key("qqmusic_key") || cookies.contains_key("qm_keyst"))
        }
        "netease" => cookies.contains_key("MUSIC_U"),
        "bilibili" => cookies.contains_key("SESSDATA"),
        "kugou" => ["token", "userid"].iter().all(|name| {
            cookies
                .get(*name)
                .is_some_and(|value| !value.trim().is_empty())
        }),
        _ => false,
    }
}

const COOKIE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const QQ_REFRESH_COOKIE_GRACE_PERIOD: Duration = Duration::from_secs(3);
const QQ_LOGIN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);
const QQ_WEB_REFRESH_URL: &str = "https://c.y.qq.com/base/fcgi-bin/login_get_musickey.fcg";

fn has_qq_refresh_cookies(cookies: &BTreeMap<String, String>) -> bool {
    [
        ["psrf_qqopenid", "openid"].as_slice(),
        ["psrf_qqaccess_token", "access_token"].as_slice(),
        ["psrf_qqrefresh_token", "refresh_token"].as_slice(),
        ["psrf_qqrefresh_key", "refresh_key"].as_slice(),
    ]
    .iter()
    .all(|aliases| aliases.iter().any(|name| cookies.contains_key(*name)))
}

fn parse_qq_login_callback(raw_url: &str) -> Option<(String, String)> {
    let url = url::Url::parse(raw_url).ok()?;
    let mut login_type = None;
    let mut code = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "login_type" if !value.is_empty() => login_type = Some(value.into_owned()),
            "code" if !value.is_empty() => code = Some(value.into_owned()),
            _ => {}
        }
    }
    Some((login_type?, code?))
}

fn remember_qq_login_callback(callback: &mut Option<(String, String)>, raw_url: &str) {
    let Some(parsed) = parse_qq_login_callback(raw_url) else {
        return;
    };
    if callback.as_ref() != Some(&parsed) {
        *callback = Some(parsed);
    }
}

fn qq_login_exchange_payload(login_type: &str, code: &str) -> Option<Value> {
    match login_type {
        "1" => Some(json!({
            "comm": {"g_tk": 5381, "platform": "yqq", "ct": 24, "cv": 0},
            "req": {
                "module": "QQConnectLogin.LoginServer",
                "method": "QQLogin",
                "param": {"code": code}
            }
        })),
        "2" => Some(json!({
            "comm": {
                "tmeAppID": "qqmusic",
                "tmeLoginType": "1",
                "g_tk": 5381,
                "platform": "yqq",
                "ct": 24,
                "cv": 0
            },
            "req": {
                "module": "music.login.LoginServer",
                "method": "Login",
                "param": {"strAppid": "wx48db31d50e334801", "code": code}
            }
        })),
        _ => None,
    }
}

fn qq_login_response_data(body: &Value) -> Option<&serde_json::Map<String, Value>> {
    for key in ["req", "req_0", "req_1", "req1"] {
        if let Some(data) = body.get(key).and_then(|value| value.get("data"))
            && let Some(data) = data.as_object()
        {
            return Some(data);
        }
    }
    None
}

fn qq_login_response_code(body: &Value) -> Option<String> {
    for key in ["req", "req_0", "req_1", "req1"] {
        if let Some(code) = body.get(key).and_then(|value| value.get("code")) {
            return Some(value_to_string(code));
        }
    }
    body.get("code").map(value_to_string)
}

fn value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
        .unwrap_or_default()
}

fn merge_qq_login_response_fields(cookies: &mut BTreeMap<String, String>, body: &Value) {
    let Some(data) = qq_login_response_data(body).or_else(|| body.as_object()) else {
        return;
    };
    for (source, target) in [
        ("openid", "psrf_qqopenid"),
        ("access_token", "psrf_qqaccess_token"),
        ("refresh_token", "psrf_qqrefresh_token"),
        ("refresh_key", "psrf_qqrefresh_key"),
        ("musickey", "qqmusic_key"),
        ("musicid", "uin"),
    ] {
        let value = data.get(source).map(value_to_string).unwrap_or_default();
        if !value.is_empty() {
            cookies.insert(target.to_owned(), value);
        }
    }
}

fn merge_qq_set_cookie_headers(
    cookies: &mut BTreeMap<String, String>,
    headers: &reqwest::header::HeaderMap,
) {
    for header in headers.get_all("set-cookie").iter() {
        let Ok(header) = header.to_str() else {
            continue;
        };
        let name_value = header.split(';').next().unwrap_or_default();
        let Some((name, value)) = name_value.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if allowed_cookie_names("qqmusic").contains(&name) && !value.is_empty() {
            cookies.insert(name.to_owned(), value.to_owned());
        }
    }
}

fn cookie_header(cookies: &BTreeMap<String, String>) -> String {
    cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn exchange_qq_login_code(
    login_type: &str,
    code: &str,
    cookies: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let payload = qq_login_exchange_payload(login_type, code)
        .ok_or_else(|| format!("unsupported QQ login type: {login_type}"))?;
    let cookie_header = cookie_header(cookies);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let callback_result = runtime.block_on(async move {
        let response = reqwest::Client::builder()
            .timeout(QQ_LOGIN_EXCHANGE_TIMEOUT)
            .user_agent("QQMusic")
            .build()
            .map_err(|error| error.to_string())?
            .post("https://u.y.qq.com/cgi-bin/musicu.fcg")
            .header("Content-Type", "application/json")
            .header("Cookie", cookie_header)
            .json(&payload)
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?;
        let headers = response.headers().clone();
        let body = response
            .json::<Value>()
            .await
            .map_err(|error| error.to_string())?;
        Ok::<_, String>((headers, body))
    });
    let mut fields = BTreeMap::new();
    let callback_error = match callback_result {
        Ok((response_headers, response)) => {
            merge_qq_set_cookie_headers(&mut fields, &response_headers);
            merge_qq_login_response_fields(&mut fields, &response);
            qq_login_response_code(&response)
                .filter(|code| code != "0" && !code.is_empty())
                .map(|code| format!("QQ Music web login returned code {code}"))
        }
        Err(error) => Some(error),
    };
    if login_type == "2"
        && let Ok(web_fields) = refresh_qq_web_session(cookies)
    {
        fields.extend(web_fields);
    }
    if fields.is_empty()
        && let Some(error) = callback_error
    {
        return Err(error);
    }
    Ok(fields)
}

fn refresh_qq_web_session(
    cookies: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let Some(open_id) = cookies.get("wxopenid").filter(|value| !value.is_empty()) else {
        return Err("QQ web session has no wxopenid".to_owned());
    };
    let Some(refresh_token) = cookies
        .get("wxrefresh_token")
        .filter(|value| !value.is_empty())
    else {
        return Err("QQ web session has no wxrefresh_token".to_owned());
    };
    let Some(music_key) = cookies
        .get("qqmusic_key")
        .or_else(|| cookies.get("qm_keyst"))
        .filter(|value| !value.is_empty())
    else {
        return Err("QQ web session has no music key".to_owned());
    };
    let Some(music_uin) = cookies
        .get("wxuin")
        .or_else(|| cookies.get("uin"))
        .map(|value| value.trim_start_matches('o'))
        .filter(|value| !value.is_empty())
    else {
        return Err("QQ web session has no music uin".to_owned());
    };
    let mut endpoint = url::Url::parse(QQ_WEB_REFRESH_URL).map_err(|error| error.to_string())?;
    endpoint
        .query_pairs_mut()
        .append_pair("from", "1")
        .append_pair("force_access", "1")
        .append_pair("wxopenid", open_id)
        .append_pair("wxrefresh_token", refresh_token)
        .append_pair("musickey", music_key)
        .append_pair("musicuin", music_uin)
        .append_pair("get_access_token", "1")
        .append_pair("ct", "1001")
        .append_pair("format", "json")
        .append_pair("inCharset", "utf-8")
        .append_pair("outCharset", "utf-8");
    let endpoint = endpoint.to_string();
    let cookie_header = cookie_header(cookies);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let response = runtime.block_on(async move {
        reqwest::Client::builder()
            .timeout(QQ_LOGIN_EXCHANGE_TIMEOUT)
            .user_agent("QQMusic")
            .build()
            .map_err(|error| error.to_string())?
            .get(endpoint)
            .header("Cookie", cookie_header)
            .header("Referer", "https://y.qq.com/")
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?
            .json::<Value>()
            .await
            .map_err(|error| error.to_string())
    })?;
    if value_to_string(response.get("code").unwrap_or(&Value::Null)) != "0" {
        return Err("QQ web session refresh was rejected".to_owned());
    }
    let mut fields = BTreeMap::new();
    for (source, target) in [
        ("wxaccess_token", "wxaccess_token"),
        ("musickey", "qqmusic_key"),
        ("musicuin", "wxuin"),
    ] {
        if let Some(value) = response.get(source).map(value_to_string)
            && !value.is_empty()
        {
            fields.insert(target.to_owned(), value);
        }
    }
    if fields.contains_key("wxaccess_token") {
        Ok(fields)
    } else {
        Err("QQ web session refresh returned no access token".to_owned())
    }
}

/// QQ writes its refresh cookies after the basic login cookies. Keep polling
/// briefly so a successful login captures the complete refreshable credential.
fn should_complete_capture(
    provider: &str,
    cookies: &BTreeMap<String, String>,
    required_seen_at: &mut Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    if !has_required_cookies(provider, cookies) {
        *required_seen_at = None;
        return false;
    }
    if provider != "qqmusic" || has_qq_refresh_cookies(cookies) {
        return true;
    }
    let seen_at = required_seen_at.get_or_insert(now);
    now.duration_since(*seen_at) >= QQ_REFRESH_COOKIE_GRACE_PERIOD
}

#[cfg(windows)]
#[allow(unsafe_op_in_unsafe_fn)]
mod platform {
    use super::{CaptureError, allowed_cookie_names, login_url};
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, c_void};
    use std::mem::{size_of, transmute_copy};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        E_INVALIDARG, E_NOINTERFACE, HWND, LPARAM, RECT, S_FALSE, S_OK, WPARAM,
    };
    use windows_sys::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
        DispatchMessageW, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, MSG, PM_REMOVE,
        PeekMessageW, PostQuitMessage, RegisterClassW, SetWindowLongPtrW, ShowWindow,
        TranslateMessage, WM_CLOSE, WM_DESTROY, WM_SIZE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
    };
    use windows_sys::core::{GUID, HRESULT, PCWSTR};

    unsafe extern "system" {
        fn CreateCoreWebView2EnvironmentWithOptions(
            browser_executable_folder: PCWSTR,
            user_data_folder: PCWSTR,
            environment_options: *mut c_void,
            environment_created_handler: *mut c_void,
        ) -> HRESULT;
    }

    const IID_IUNKNOWN: GUID = GUID {
        data1: 0x00000000,
        data2: 0x0000,
        data3: 0x0000,
        data4: [0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
    };
    const IID_WEBVIEW2: GUID = GUID {
        data1: 0x9e8f0cf8,
        data2: 0xe670,
        data3: 0x4b5e,
        data4: [0xb2, 0xbc, 0x73, 0xe0, 0x61, 0xe3, 0x18, 0x4c],
    };
    const IID_ENVIRONMENT_HANDLER: GUID = GUID {
        data1: 0x4e8a3389,
        data2: 0xc9d8,
        data3: 0x4bd2,
        data4: [0xb6, 0xb5, 0x12, 0x4f, 0xee, 0x6c, 0xc1, 0x4d],
    };
    const IID_CONTROLLER_HANDLER: GUID = GUID {
        data1: 0x6c4819f3,
        data2: 0xc9b7,
        data3: 0x4260,
        data4: [0x81, 0x27, 0xc9, 0xf5, 0xbd, 0xe7, 0xf6, 0x8c],
    };
    const IID_COOKIES_HANDLER: GUID = GUID {
        data1: 0x5a4f5069,
        data2: 0x5c15,
        data3: 0x47c3,
        data4: [0x86, 0x46, 0xf4, 0xde, 0x1c, 0x11, 0x66, 0x70],
    };
    const IID_NAVIGATION_STARTING_HANDLER: GUID = GUID {
        data1: 0x9adbe429,
        data2: 0xf36d,
        data3: 0x432b,
        data4: [0x9d, 0xdc, 0xf8, 0x88, 0x1f, 0xbd, 0x76, 0xe3],
    };

    type HandlerQueryInterface =
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
    type HandlerAddRef = unsafe extern "system" fn(*mut c_void) -> u32;
    type HandlerRelease = unsafe extern "system" fn(*mut c_void) -> u32;
    type HandlerInvoke = unsafe extern "system" fn(*mut c_void, HRESULT, *mut c_void) -> HRESULT;
    type CookieCapture = (BTreeMap<String, String>, BTreeMap<String, String>);

    #[repr(C)]
    struct HandlerVtable {
        query_interface: HandlerQueryInterface,
        add_ref: HandlerAddRef,
        release: HandlerRelease,
        invoke: HandlerInvoke,
    }

    #[derive(Default)]
    struct CaptureState {
        environment: *mut c_void,
        controller: *mut c_void,
        webview: *mut c_void,
        cookie_manager: *mut c_void,
        cookie_request_inflight: bool,
        next_cookie_poll: Option<Instant>,
        required_cookies_seen_at: Option<Instant>,
        browser_cookies: BTreeMap<String, String>,
        latest_cookies: BTreeMap<String, String>,
        login_callback: Option<(String, String)>,
        exchange_attempted: bool,
        exchange_started_at: Option<Instant>,
        exchange_rx: Option<mpsc::Receiver<Result<BTreeMap<String, String>, String>>>,
        exchange_cookies: BTreeMap<String, String>,
        outcome: Option<Result<BTreeMap<String, String>, CaptureError>>,
    }

    struct CaptureContext {
        provider: String,
        url: Vec<u16>,
        hwnd: HWND,
        state: Mutex<CaptureState>,
    }

    impl CaptureContext {
        fn new(provider: &str, hwnd: HWND) -> Result<Self, CaptureError> {
            let url = login_url(provider).ok_or_else(|| {
                CaptureError::Com(format!("unsupported provider login page: {provider}"))
            })?;
            Ok(Self {
                provider: provider.to_owned(),
                url: wide(url),
                hwnd,
                state: Mutex::new(CaptureState::default()),
            })
        }

        fn finish_error(&self, error: CaptureError) {
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            if state.outcome.is_none() {
                state.outcome = Some(Err(error));
            }
            state.cookie_request_inflight = false;
        }

        fn take_outcome(&self) -> Option<Result<BTreeMap<String, String>, CaptureError>> {
            self.state
                .lock()
                .expect("capture state mutex poisoned")
                .outcome
                .take()
        }

        fn resize_webview_to_client_area(&self) {
            let controller = {
                let state = self.state.lock().expect("capture state mutex poisoned");
                if state.controller.is_null() {
                    return;
                }
                state.controller
            };
            let mut bounds = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if unsafe { GetClientRect(self.hwnd, &mut bounds) } == 0 {
                return;
            }
            let put_bounds: unsafe extern "system" fn(*mut c_void, *const RECT) -> HRESULT =
                unsafe { vtable_fn(controller, 6) };
            let _ = unsafe { put_bounds(controller, &bounds) };
        }

        fn poll_navigation(self: &Rc<Self>) {
            if self.provider != "qqmusic" {
                return;
            }
            let webview = {
                let state = self.state.lock().expect("capture state mutex poisoned");
                if state.webview.is_null() || state.outcome.is_some() {
                    return;
                }
                state.webview
            };
            let get_source: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT =
                unsafe { vtable_fn(webview, 4) };
            let mut source = null_mut();
            let result = unsafe { get_source(webview, &mut source) };
            if result < 0 || source.is_null() {
                return;
            }
            let source_text = unsafe { read_wide(source) };
            unsafe { CoTaskMemFree(source as *const c_void) };
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            let previous = state.login_callback.clone();
            super::remember_qq_login_callback(&mut state.login_callback, &source_text);
            if state.login_callback != previous {
                state.exchange_attempted = false;
            }
        }

        fn remember_navigation_url(&self, raw_url: &str) {
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            let previous = state.login_callback.clone();
            super::remember_qq_login_callback(&mut state.login_callback, raw_url);
            if state.login_callback != previous {
                state.exchange_attempted = false;
            }
        }

        fn start_login_exchange(self: &Rc<Self>) {
            let (login_type, code, cookies, sender) = {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                if self.provider != "qqmusic"
                    || state.outcome.is_some()
                    || state.exchange_attempted
                    || state.exchange_rx.is_some()
                    || state.browser_cookies.is_empty()
                {
                    return;
                }
                let Some((login_type, code)) = state.login_callback.clone() else {
                    return;
                };
                let (sender, receiver) = mpsc::channel();
                state.exchange_attempted = true;
                state.exchange_started_at = Some(Instant::now());
                state.exchange_rx = Some(receiver);
                (login_type, code, state.browser_cookies.clone(), sender)
            };
            thread::spawn(move || {
                let result = super::exchange_qq_login_code(&login_type, &code, &cookies);
                let _ = sender.send(result);
            });
        }

        fn poll_login_exchange(self: &Rc<Self>) {
            let result = {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                let Some(receiver) = state.exchange_rx.as_ref() else {
                    return;
                };
                match receiver.try_recv() {
                    Ok(result) => {
                        state.exchange_rx = None;
                        state.exchange_started_at = None;
                        Some(result)
                    }
                    Err(mpsc::TryRecvError::Empty) => {
                        if state.exchange_started_at.is_some_and(|started| {
                            started.elapsed() >= super::QQ_LOGIN_EXCHANGE_TIMEOUT
                        }) {
                            state.exchange_rx = None;
                            state.exchange_started_at = None;
                        }
                        None
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        state.exchange_rx = None;
                        state.exchange_started_at = None;
                        None
                    }
                }
            };
            match result {
                Some(Ok(fields)) => {
                    let mut state = self.state.lock().expect("capture state mutex poisoned");
                    state.exchange_cookies.extend(fields);
                    state.next_cookie_poll = Some(Instant::now());
                }
                Some(Err(error)) => {
                    let mut state = self.state.lock().expect("capture state mutex poisoned");
                    // The callback code is single-use and QQ may consume it in
                    // the page before this best-effort side request completes.
                    // The browser session itself remains a valid candidate, so
                    // keep capturing cookies instead of discarding the login.
                    state.exchange_attempted = true;
                    state.next_cookie_poll = Some(Instant::now());
                    let _ = error;
                }
                None => {}
            }
        }

        fn poll_cookies(self: &Rc<Self>) {
            let manager = {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                if state.cookie_manager.is_null()
                    || state.cookie_request_inflight
                    || state
                        .next_cookie_poll
                        .is_some_and(|deadline| Instant::now() < deadline)
                    || state.outcome.is_some()
                {
                    return;
                }
                state.cookie_request_inflight = true;
                state.cookie_manager
            };

            let handler = Box::into_raw(Box::new(CookiesHandler {
                vtbl: &COOKIES_HANDLER_VTABLE,
                refs: AtomicU32::new(1),
                context: Rc::clone(self),
            })) as *mut c_void;
            let get_cookies: unsafe extern "system" fn(
                *mut c_void,
                PCWSTR,
                *mut c_void,
            ) -> HRESULT = unsafe { vtable_fn(manager, 5) };
            // The disposable profile belongs to this one provider login. Read
            // all profile cookies, then persist only the provider allowlist,
            // so login flows that set session cookies on callback subdomains
            // are still captured without broadening the credential boundary.
            let result = unsafe { get_cookies(manager, null(), handler) };
            unsafe { release_com(handler) };
            if result < 0 {
                self.finish_error(com_error("ICoreWebView2CookieManager::GetCookies", result));
            }
        }
    }

    #[repr(C)]
    struct EnvironmentHandler {
        vtbl: &'static HandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
    }

    #[repr(C)]
    struct ControllerHandler {
        vtbl: &'static HandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
    }

    #[repr(C)]
    struct CookiesHandler {
        vtbl: &'static HandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
    }

    #[repr(C)]
    struct NavigationStartingHandler {
        vtbl: &'static NavigationStartingHandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
    }

    type NavigationStartingInvoke =
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT;

    #[repr(C)]
    struct NavigationStartingHandlerVtable {
        query_interface: HandlerQueryInterface,
        add_ref: HandlerAddRef,
        release: HandlerRelease,
        invoke: NavigationStartingInvoke,
    }

    static ENVIRONMENT_HANDLER_VTABLE: HandlerVtable = HandlerVtable {
        query_interface: environment_query_interface,
        add_ref: environment_add_ref,
        release: environment_release,
        invoke: environment_invoke,
    };
    static CONTROLLER_HANDLER_VTABLE: HandlerVtable = HandlerVtable {
        query_interface: controller_query_interface,
        add_ref: controller_add_ref,
        release: controller_release,
        invoke: controller_invoke,
    };
    static COOKIES_HANDLER_VTABLE: HandlerVtable = HandlerVtable {
        query_interface: cookies_query_interface,
        add_ref: cookies_add_ref,
        release: cookies_release,
        invoke: cookies_invoke,
    };
    static NAVIGATION_STARTING_HANDLER_VTABLE: NavigationStartingHandlerVtable =
        NavigationStartingHandlerVtable {
            query_interface: navigation_starting_query_interface,
            add_ref: navigation_starting_add_ref,
            release: navigation_starting_release,
            invoke: navigation_starting_invoke,
        };

    unsafe extern "system" fn environment_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(
            this,
            riid,
            out,
            &IID_ENVIRONMENT_HANDLER,
            environment_add_ref,
        )
    }

    unsafe extern "system" fn controller_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(this, riid, out, &IID_CONTROLLER_HANDLER, controller_add_ref)
    }

    unsafe extern "system" fn cookies_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(this, riid, out, &IID_COOKIES_HANDLER, cookies_add_ref)
    }

    unsafe extern "system" fn navigation_starting_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(
            this,
            riid,
            out,
            &IID_NAVIGATION_STARTING_HANDLER,
            navigation_starting_add_ref,
        )
    }

    unsafe fn query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
        interface_iid: &GUID,
        add_ref: HandlerAddRef,
    ) -> HRESULT {
        if riid.is_null() || out.is_null() {
            return E_INVALIDARG;
        }
        if guid_equal(&*riid, &IID_IUNKNOWN) || guid_equal(&*riid, interface_iid) {
            *out = this;
            add_ref(this);
            S_OK
        } else {
            *out = null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn environment_add_ref(this: *mut c_void) -> u32 {
        handler_refs(this).fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn controller_add_ref(this: *mut c_void) -> u32 {
        handler_refs(this).fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn cookies_add_ref(this: *mut c_void) -> u32 {
        handler_refs(this).fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn navigation_starting_add_ref(this: *mut c_void) -> u32 {
        handler_refs(this).fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn environment_release(this: *mut c_void) -> u32 {
        release_handler::<EnvironmentHandler>(this)
    }

    unsafe extern "system" fn controller_release(this: *mut c_void) -> u32 {
        release_handler::<ControllerHandler>(this)
    }

    unsafe extern "system" fn cookies_release(this: *mut c_void) -> u32 {
        release_handler::<CookiesHandler>(this)
    }

    unsafe extern "system" fn navigation_starting_release(this: *mut c_void) -> u32 {
        release_handler::<NavigationStartingHandler>(this)
    }

    unsafe fn handler_refs(this: *mut c_void) -> &'static AtomicU32 {
        &*(((this as *mut u8).add(size_of::<*const HandlerVtable>())) as *const AtomicU32)
    }

    unsafe fn release_handler<T>(this: *mut c_void) -> u32 {
        let refs = handler_refs(this);
        if refs.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            drop(Box::from_raw(this as *mut T));
            0
        } else {
            refs.load(Ordering::Relaxed)
        }
    }

    unsafe extern "system" fn environment_invoke(
        this: *mut c_void,
        result: HRESULT,
        environment: *mut c_void,
    ) -> HRESULT {
        let context = (&*(this as *mut EnvironmentHandler)).context.clone();
        if result < 0 || environment.is_null() {
            context.finish_error(if result < 0 {
                com_error("CreateCoreWebView2Environment", result)
            } else {
                CaptureError::Com(
                    "CreateCoreWebView2Environment returned a null environment".to_owned(),
                )
            });
            return S_OK;
        }
        let add_ref: unsafe extern "system" fn(*mut c_void) -> u32 = vtable_fn(environment, 1);
        add_ref(environment);
        context
            .state
            .lock()
            .expect("capture state mutex poisoned")
            .environment = environment;
        let handler = Box::into_raw(Box::new(ControllerHandler {
            vtbl: &CONTROLLER_HANDLER_VTABLE,
            refs: AtomicU32::new(1),
            context: Rc::clone(&context),
        })) as *mut c_void;
        let create_controller: unsafe extern "system" fn(
            *mut c_void,
            HWND,
            *mut c_void,
        ) -> HRESULT = vtable_fn(environment, 3);
        let hr = create_controller(environment, context.hwnd, handler);
        release_com(handler);
        if hr < 0 {
            context.finish_error(com_error("CreateCoreWebView2Controller", hr));
        }
        S_OK
    }

    unsafe extern "system" fn controller_invoke(
        this: *mut c_void,
        result: HRESULT,
        controller: *mut c_void,
    ) -> HRESULT {
        let context = (&*(this as *mut ControllerHandler)).context.clone();
        if result < 0 || controller.is_null() {
            context.finish_error(if result < 0 {
                com_error("CreateCoreWebView2Controller", result)
            } else {
                CaptureError::Com(
                    "CreateCoreWebView2Controller returned a null controller".to_owned(),
                )
            });
            return S_OK;
        }

        let add_ref: unsafe extern "system" fn(*mut c_void) -> u32 = vtable_fn(controller, 1);
        add_ref(controller);
        {
            let mut state = context.state.lock().expect("capture state mutex poisoned");
            state.controller = controller;
        }

        let get_core: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT =
            vtable_fn(controller, 25);
        let mut webview = null_mut();
        let hr = get_core(controller, &mut webview);
        if hr < 0 || webview.is_null() {
            context.finish_error(if hr < 0 {
                com_error("ICoreWebView2Controller::get_CoreWebView2", hr)
            } else {
                CaptureError::Com("get_CoreWebView2 returned a null WebView".to_owned())
            });
            return S_OK;
        }

        let query: unsafe extern "system" fn(
            *mut c_void,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT = vtable_fn(webview, 0);
        let mut webview2 = null_mut();
        let hr = query(webview, &IID_WEBVIEW2, &mut webview2);
        if hr < 0 || webview2.is_null() {
            release_com(webview);
            context.finish_error(if hr < 0 {
                com_error("QueryInterface(ICoreWebView2_2)", hr)
            } else {
                CaptureError::Com("ICoreWebView2_2 is unavailable".to_owned())
            });
            return S_OK;
        }

        // ICoreWebView2_2 inherits the full ICoreWebView2 vtable. The SDK header
        // places get_CookieManager at slot 66 in that combined vtable.
        let get_manager: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT =
            vtable_fn(webview2, 66);
        let mut cookie_manager = null_mut();
        let hr = get_manager(webview2, &mut cookie_manager);
        release_com(webview2);
        if hr < 0 || cookie_manager.is_null() {
            release_com(webview);
            context.finish_error(if hr < 0 {
                com_error("ICoreWebView2_2::get_CookieManager", hr)
            } else {
                CaptureError::Com("get_CookieManager returned a null manager".to_owned())
            });
            return S_OK;
        }

        let navigation_handler = Box::into_raw(Box::new(NavigationStartingHandler {
            vtbl: &NAVIGATION_STARTING_HANDLER_VTABLE,
            refs: AtomicU32::new(1),
            context: Rc::clone(&context),
        })) as *mut c_void;
        let add_navigation_starting: unsafe extern "system" fn(
            *mut c_void,
            *mut c_void,
            *mut i64,
        ) -> HRESULT = vtable_fn(webview, 7);
        let mut navigation_token = 0i64;
        let hr = add_navigation_starting(webview, navigation_handler, &mut navigation_token);
        release_com(navigation_handler);
        if hr < 0 {
            release_com(cookie_manager);
            release_com(webview);
            context.finish_error(com_error("ICoreWebView2::add_NavigationStarting", hr));
            return S_OK;
        }

        let navigate: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT =
            vtable_fn(webview, 5);
        let hr = navigate(webview, context.url.as_ptr());
        if hr < 0 {
            release_com(cookie_manager);
            release_com(webview);
            context.finish_error(com_error("ICoreWebView2::Navigate", hr));
            return S_OK;
        }

        let put_visible: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT =
            vtable_fn(controller, 4);
        let _ = put_visible(controller, 1);

        let mut state = context.state.lock().expect("capture state mutex poisoned");
        state.controller = controller;
        state.webview = webview;
        state.cookie_manager = cookie_manager;
        state.next_cookie_poll = Some(Instant::now());
        drop(state);
        context.resize_webview_to_client_area();
        S_OK
    }

    unsafe extern "system" fn cookies_invoke(
        this: *mut c_void,
        result: HRESULT,
        cookie_list: *mut c_void,
    ) -> HRESULT {
        let context = (&*(this as *mut CookiesHandler)).context.clone();
        if result < 0 || cookie_list.is_null() {
            context.finish_error(if result < 0 {
                com_error("ICoreWebView2CookieManager::GetCookies", result)
            } else {
                CaptureError::Com("GetCookies returned a null cookie list".to_owned())
            });
            return S_OK;
        }

        let (browser_cookies, cookies) = match read_cookie_list(cookie_list, &context.provider) {
            Ok(cookies) => cookies,
            Err(error) => {
                context.finish_error(error);
                return S_OK;
            }
        };
        let mut state = context.state.lock().expect("capture state mutex poisoned");
        state.cookie_request_inflight = false;
        state.browser_cookies = browser_cookies;
        state.latest_cookies = cookies.clone();
        let exchange_cookies = state.exchange_cookies.clone();
        state.latest_cookies.extend(exchange_cookies);
        let cookies = state.latest_cookies.clone();
        let now = Instant::now();
        let exchange_pending = state.exchange_rx.is_some();
        if !exchange_pending
            && super::should_complete_capture(
                &context.provider,
                &cookies,
                &mut state.required_cookies_seen_at,
                now,
            )
        {
            state.outcome = Some(Ok(cookies));
        } else {
            state.next_cookie_poll = Some(now + super::COOKIE_POLL_INTERVAL);
        }
        drop(state);
        context.start_login_exchange();
        S_OK
    }

    unsafe extern "system" fn navigation_starting_invoke(
        this: *mut c_void,
        _sender: *mut c_void,
        args: *mut c_void,
    ) -> HRESULT {
        let context = (&*(this as *mut NavigationStartingHandler)).context.clone();
        if args.is_null() {
            return S_OK;
        }
        let get_uri: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT =
            vtable_fn(args, 3);
        let mut uri = null_mut();
        let result = get_uri(args, &mut uri);
        if result >= 0 && !uri.is_null() {
            let raw_url = read_wide(uri);
            CoTaskMemFree(uri as *const c_void);
            context.remember_navigation_url(&raw_url);
        }
        S_OK
    }

    unsafe fn read_cookie_list(
        cookie_list: *mut c_void,
        provider: &str,
    ) -> Result<CookieCapture, CaptureError> {
        let get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT =
            vtable_fn(cookie_list, 3);
        let mut count = 0u32;
        let hr = get_count(cookie_list, &mut count);
        if hr < 0 {
            return Err(com_error("ICoreWebView2CookieList::get_Count", hr));
        }
        let allowed = allowed_cookie_names(provider);
        let mut browser_cookies = BTreeMap::new();
        let mut cookies = BTreeMap::new();
        for index in 0..count {
            let get_value: unsafe extern "system" fn(
                *mut c_void,
                u32,
                *mut *mut c_void,
            ) -> HRESULT = vtable_fn(cookie_list, 4);
            let mut cookie = null_mut();
            let hr = get_value(cookie_list, index, &mut cookie);
            if hr < 0 || cookie.is_null() {
                if hr < 0 {
                    return Err(com_error("ICoreWebView2CookieList::GetValueAtIndex", hr));
                }
                continue;
            }
            let get_name: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT =
                vtable_fn(cookie, 3);
            let get_value_text: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT =
                vtable_fn(cookie, 4);
            let mut name = null_mut();
            let mut value = null_mut();
            let name_hr = get_name(cookie, &mut name);
            let value_hr = get_value_text(cookie, &mut value);
            let name_text = if name_hr >= 0 {
                let text = read_wide(name);
                CoTaskMemFree(name as *const c_void);
                text
            } else {
                String::new()
            };
            let value_text = if value_hr >= 0 {
                let text = read_wide(value);
                CoTaskMemFree(value as *const c_void);
                text
            } else {
                String::new()
            };
            release_com(cookie);
            if name_hr < 0 {
                return Err(com_error("ICoreWebView2Cookie::get_Name", name_hr));
            }
            if value_hr < 0 {
                return Err(com_error("ICoreWebView2Cookie::get_Value", value_hr));
            }
            if !value_text.is_empty() {
                browser_cookies.insert(name_text.clone(), value_text.clone());
                if allowed.contains(&name_text.as_str()) {
                    cookies.insert(name_text, value_text);
                }
            }
        }
        Ok((browser_cookies, cookies))
    }

    unsafe fn read_wide(value: *const u16) -> String {
        if value.is_null() {
            return String::new();
        }
        let mut length = 0usize;
        while *value.add(length) != 0 {
            length += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(value, length))
    }

    unsafe fn vtable_fn<F: Copy>(this: *mut c_void, index: usize) -> F {
        let table = *(this as *const *const *const c_void);
        let entry = *table.add(index);
        transmute_copy(&entry)
    }

    unsafe fn release_com(this: *mut c_void) {
        if !this.is_null() {
            let release: unsafe extern "system" fn(*mut c_void) -> u32 = vtable_fn(this, 2);
            release(this);
        }
    }

    fn guid_equal(left: &GUID, right: &GUID) -> bool {
        left.data1 == right.data1
            && left.data2 == right.data2
            && left.data3 == right.data3
            && left.data4 == right.data4
    }

    fn com_error(operation: &str, result: HRESULT) -> CaptureError {
        CaptureError::Com(format!(
            "{operation} failed with HRESULT 0x{:08X}",
            result as u32
        ))
    }

    struct HostWindow {
        hwnd: HWND,
    }

    impl HostWindow {
        fn create() -> Result<Self, CaptureError> {
            let class_name = wide("MiliastraLoginHelperWindow");
            let title = wide("Miliastra Music Login");
            let instance = unsafe { GetModuleHandleW(null()) };
            let window_class = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: null_mut(),
                hCursor: null_mut(),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class_name.as_ptr(),
            };
            let _ = unsafe { RegisterClassW(&window_class) };
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    title.as_ptr(),
                    WS_OVERLAPPEDWINDOW,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    1200,
                    900,
                    null_mut(),
                    null_mut(),
                    instance,
                    null(),
                )
            };
            if hwnd.is_null() {
                return Err(CaptureError::Io("CreateWindowExW failed".to_owned()));
            }
            unsafe { ShowWindow(hwnd, 5) };
            Ok(Self { hwnd })
        }

        fn attach_context(&self, context: &Rc<CaptureContext>) {
            unsafe { SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, Rc::as_ptr(context) as isize) };
        }

        fn clear_context(&self) {
            unsafe { SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0) };
        }
    }

    impl Drop for HostWindow {
        fn drop(&mut self) {
            unsafe { DestroyWindow(self.hwnd) };
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> isize {
        match message {
            WM_SIZE => {
                let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const CaptureContext;
                if !context.is_null() {
                    (*context).resize_webview_to_client_area();
                }
                0
            }
            WM_CLOSE => {
                DestroyWindow(hwnd);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    struct ComApartment;

    impl ComApartment {
        fn initialize() -> Result<Self, CaptureError> {
            let result = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) };
            if result == S_OK || result == S_FALSE {
                Ok(Self)
            } else {
                Err(com_error("CoInitializeEx", result))
            }
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    pub fn capture(
        provider: &str,
        profile: &Path,
        timeout: Duration,
    ) -> Result<BTreeMap<String, String>, CaptureError> {
        set_process_dpi_awareness();
        let _runtime = runtime_executable().ok_or(CaptureError::RuntimeMissing)?;
        let _com = ComApartment::initialize()?;
        let window = HostWindow::create()?;
        let context = Rc::new(CaptureContext::new(provider, window.hwnd)?);
        window.attach_context(&context);
        let environment_handler = Box::into_raw(Box::new(EnvironmentHandler {
            vtbl: &ENVIRONMENT_HANDLER_VTABLE,
            refs: AtomicU32::new(1),
            context: Rc::clone(&context),
        })) as *mut c_void;
        let user_data = wide(&profile.to_string_lossy());
        let result = unsafe {
            CreateCoreWebView2EnvironmentWithOptions(
                null(),
                user_data.as_ptr(),
                null_mut(),
                environment_handler,
            )
        };
        unsafe { release_com(environment_handler) };
        if result < 0 {
            context.finish_error(com_error(
                "CreateCoreWebView2EnvironmentWithOptions",
                result,
            ));
        }

        let deadline = Instant::now() + timeout;
        let outcome = loop {
            pump_messages(&context);
            context.poll_navigation();
            context.poll_login_exchange();
            context.poll_cookies();
            context.start_login_exchange();
            if let Some(outcome) = context.take_outcome() {
                break outcome;
            }
            if Instant::now() >= deadline {
                break Err(CaptureError::Timeout);
            }
            thread::sleep(Duration::from_millis(20));
        };
        release_context_interfaces(&context);
        window.clear_context();
        outcome
    }

    fn set_process_dpi_awareness() {
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }

    fn pump_messages(context: &CaptureContext) {
        let mut message = MSG {
            hwnd: null_mut(),
            message: 0,
            wParam: 0,
            lParam: 0,
            time: 0,
            pt: windows_sys::Win32::Foundation::POINT { x: 0, y: 0 },
        };
        while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if message.message == 0x0012 {
                context.finish_error(CaptureError::Com("login window was closed".to_owned()));
                break;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn release_context_interfaces(context: &CaptureContext) {
        let (environment, cookie_manager, webview, controller) = {
            let mut state = context.state.lock().expect("capture state mutex poisoned");
            let interfaces = (
                state.environment,
                state.cookie_manager,
                state.webview,
                state.controller,
            );
            state.environment = null_mut();
            state.cookie_manager = null_mut();
            state.webview = null_mut();
            state.controller = null_mut();
            interfaces
        };
        unsafe {
            release_com(environment);
            release_com(cookie_manager);
            release_com(webview);
            release_com(controller);
        }
    }

    fn runtime_executable() -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if let Ok(path) = std::env::var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER") {
            let path = PathBuf::from(path);
            candidates.push(if path.is_dir() {
                path.join("msedgewebview2.exe")
            } else {
                path
            });
        }
        for variable in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Ok(root) = std::env::var(variable) {
                candidates.extend(versioned_runtime_paths(Path::new(&root)));
            }
        }
        for root in [
            Path::new(r"C:\Program Files (x86)"),
            Path::new(r"C:\Program Files"),
        ] {
            candidates.extend(versioned_runtime_paths(root));
        }
        candidates.into_iter().find(|path| path.is_file())
    }

    fn versioned_runtime_paths(root: &Path) -> Vec<PathBuf> {
        let base = root.join(r"Microsoft\EdgeWebView\Application");
        let Ok(entries) = std::fs::read_dir(base) else {
            return Vec::new();
        };
        let mut versions = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path().join("msedgewebview2.exe"))
            .collect::<Vec<_>>();
        versions.sort();
        versions.reverse();
        versions
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
}

#[cfg(not(windows))]
mod platform {
    use super::CaptureError;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::Duration;

    pub fn capture(
        _provider: &str,
        _profile: &Path,
        _timeout: Duration,
    ) -> Result<BTreeMap<String, String>, CaptureError> {
        Err(CaptureError::RuntimeMissing)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        QQ_REFRESH_COOKIE_GRACE_PERIOD, allowed_cookie_names, has_qq_refresh_cookies,
        has_required_cookies, login_url, merge_qq_login_response_fields, parse_qq_login_callback,
        should_complete_capture,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    #[test]
    fn login_pages_are_provider_specific() {
        assert_eq!(
            login_url("qqmusic"),
            Some("https://y.qq.com/portal/profile.html")
        );
        assert_eq!(login_url("netease"), Some("https://music.163.com/"));
        assert_eq!(login_url("bilibili"), Some("https://www.bilibili.com/"));
        assert_eq!(login_url("kuwo"), None);
    }

    #[test]
    fn cookie_filter_requires_provider_credentials() {
        let qq = BTreeMap::from([
            ("uin".to_owned(), "1".to_owned()),
            ("qqmusic_key".to_owned(), "key".to_owned()),
        ]);
        assert!(has_required_cookies("qqmusic", &qq));
        assert!(!has_qq_refresh_cookies(&qq));
        assert!(!has_required_cookies("netease", &qq));
        let bilibili = BTreeMap::from([("SESSDATA".to_owned(), "session".to_owned())]);
        assert!(has_required_cookies("bilibili", &bilibili));
        assert!(allowed_cookie_names("netease").contains(&"MUSIC_U"));
    }

    #[test]
    fn qq_capture_waits_for_refresh_cookies_after_basic_login() {
        let basic = BTreeMap::from([
            ("uin".to_owned(), "1".to_owned()),
            ("qqmusic_key".to_owned(), "key".to_owned()),
        ]);
        let start = Instant::now();
        let mut required_seen_at = None;

        assert!(!should_complete_capture(
            "qqmusic",
            &basic,
            &mut required_seen_at,
            start,
        ));
        assert_eq!(required_seen_at, Some(start));
        assert!(!should_complete_capture(
            "qqmusic",
            &basic,
            &mut required_seen_at,
            start + QQ_REFRESH_COOKIE_GRACE_PERIOD - Duration::from_millis(1),
        ));
    }

    #[test]
    fn qq_capture_finishes_when_refresh_cookies_arrive() {
        let complete = BTreeMap::from([
            ("uin".to_owned(), "1".to_owned()),
            ("qqmusic_key".to_owned(), "key".to_owned()),
            ("psrf_qqopenid".to_owned(), "open-id".to_owned()),
            ("psrf_qqaccess_token".to_owned(), "access-token".to_owned()),
            (
                "psrf_qqrefresh_token".to_owned(),
                "refresh-token".to_owned(),
            ),
            ("psrf_qqrefresh_key".to_owned(), "refresh-key".to_owned()),
        ]);
        let mut required_seen_at = Some(Instant::now());

        assert!(has_qq_refresh_cookies(&complete));
        assert!(should_complete_capture(
            "qqmusic",
            &complete,
            &mut required_seen_at,
            Instant::now(),
        ));
    }

    #[test]
    fn qq_capture_falls_back_to_basic_cookies_after_grace_period() {
        let basic = BTreeMap::from([
            ("uin".to_owned(), "1".to_owned()),
            ("qqmusic_key".to_owned(), "key".to_owned()),
        ]);
        let start = Instant::now();
        let mut required_seen_at = Some(start);

        assert!(should_complete_capture(
            "qqmusic",
            &basic,
            &mut required_seen_at,
            start + QQ_REFRESH_COOKIE_GRACE_PERIOD,
        ));
    }

    #[test]
    fn netease_capture_still_finishes_immediately() {
        let netease = BTreeMap::from([("MUSIC_U".to_owned(), "music-u".to_owned())]);
        let mut required_seen_at = None;

        assert!(should_complete_capture(
            "netease",
            &netease,
            &mut required_seen_at,
            Instant::now(),
        ));
        assert_eq!(required_seen_at, None);
    }

    #[test]
    fn bilibili_capture_finishes_when_sessdata_is_available() {
        let bilibili = BTreeMap::from([("SESSDATA".to_owned(), "session".to_owned())]);
        let mut required_seen_at = None;

        assert!(should_complete_capture(
            "bilibili",
            &bilibili,
            &mut required_seen_at,
            Instant::now(),
        ));
        assert_eq!(required_seen_at, None);
    }

    #[test]
    fn qq_login_callback_extracts_code_and_login_type() {
        assert_eq!(
            parse_qq_login_callback(
                "https://y.qq.com/portal/profile.html?login_type=2&code=abc%20123"
            ),
            Some(("2".to_owned(), "abc 123".to_owned()))
        );
        assert_eq!(
            parse_qq_login_callback("https://y.qq.com/portal/profile.html?code=abc"),
            None
        );
    }

    #[test]
    fn navigation_callback_is_recorded_before_source_redirects() {
        let mut callback = None;
        super::remember_qq_login_callback(
            &mut callback,
            "https://y.qq.com/portal/profile.html?login_type=2&code=transient-code",
        );
        assert_eq!(
            callback,
            Some(("2".to_owned(), "transient-code".to_owned()))
        );
    }

    #[test]
    fn qq_login_response_fields_are_merged_into_refresh_cookie_names() {
        let mut cookies = BTreeMap::from([
            ("uin".to_owned(), "123".to_owned()),
            ("qqmusic_key".to_owned(), "old-key".to_owned()),
        ]);
        merge_qq_login_response_fields(
            &mut cookies,
            &json!({
                "musicid": 123,
                "musickey": "new-key",
                "openid": "open-id",
                "access_token": "access-token",
                "refresh_token": "refresh-token",
                "refresh_key": "refresh-key"
            }),
        );
        assert_eq!(cookies.get("qqmusic_key"), Some(&"new-key".to_owned()));
        assert_eq!(
            cookies.get("psrf_qqrefresh_token"),
            Some(&"refresh-token".to_owned())
        );
        assert_eq!(
            cookies.get("psrf_qqrefresh_key"),
            Some(&"refresh-key".to_owned())
        );
    }

    #[test]
    fn qq_login_exchange_payloads_match_the_two_web_login_modes() {
        let qq = super::qq_login_exchange_payload("1", "callback-code").unwrap();
        assert_eq!(qq["req"]["module"], "QQConnectLogin.LoginServer");
        assert_eq!(qq["req"]["method"], "QQLogin");
        assert_eq!(qq["req"]["param"]["code"], "callback-code");

        let wechat = super::qq_login_exchange_payload("2", "callback-code").unwrap();
        assert_eq!(wechat["req"]["module"], "music.login.LoginServer");
        assert_eq!(wechat["req"]["param"]["strAppid"], "wx48db31d50e334801");
        assert!(super::qq_login_exchange_payload("3", "callback-code").is_none());
    }

    #[test]
    fn qq_login_exchange_keeps_session_cookies_outside_persisted_allowlist() {
        let cookies = BTreeMap::from([
            ("qqmusic_key".to_owned(), "music-key".to_owned()),
            ("p_skey".to_owned(), "session-key".to_owned()),
        ]);
        let header = super::cookie_header(&cookies);
        assert!(header.contains("qqmusic_key=music-key"));
        assert!(header.contains("p_skey=session-key"));
    }

    #[test]
    fn qq_login_response_set_cookie_headers_are_filtered_and_captured() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(
            "set-cookie",
            reqwest::header::HeaderValue::from_static(
                "psrf_qqrefresh_key=response-key; Path=/; Secure",
            ),
        );
        headers.append(
            "set-cookie",
            reqwest::header::HeaderValue::from_static("unrelated=value; Path=/"),
        );
        let mut cookies = BTreeMap::new();

        super::merge_qq_set_cookie_headers(&mut cookies, &headers);

        assert_eq!(
            cookies.get("psrf_qqrefresh_key"),
            Some(&"response-key".to_owned())
        );
        assert!(!cookies.contains_key("unrelated"));
    }

    #[test]
    fn cookie_allowlists_are_provider_scoped() {
        assert!(allowed_cookie_names("qqmusic").contains(&"qqmusic_key"));
        assert!(!allowed_cookie_names("qqmusic").contains(&"MUSIC_U"));
        assert!(allowed_cookie_names("netease").contains(&"__csrf"));
        assert!(allowed_cookie_names("bilibili").contains(&"SESSDATA"));
        assert!(!allowed_cookie_names("bilibili").contains(&"MUSIC_U"));
        assert!(allowed_cookie_names("unknown").is_empty());
    }
}
