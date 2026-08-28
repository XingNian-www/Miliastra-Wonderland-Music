use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use percent_encoding::percent_decode_str;
use reqwest::blocking::Client;
use serde_json::{Value, json};
use url::Url;

#[derive(Clone, Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("WebView2 Evergreen Runtime is missing")]
    RuntimeMissing,
    #[error("WebView2 login timed out before required cookies were captured")]
    Timeout,
    #[error("WebView2 login window was closed before credentials were captured")]
    Cancelled,
    #[error("WebView2 COM error: {0}")]
    Com(String),
    #[error("WebView2 I/O error: {0}")]
    Io(String),
}

/// A bounded request handed to the WebView2-only KuGou bridge.  Keeping this
/// as a separate input type prevents request details from being encoded in the
/// helper command line or mixed into the interactive login state machine.
#[derive(Clone, Debug)]
pub struct KugouRequestInput {
    pub url: String,
    pub userid: String,
    pub mid: String,
    pub cookies: BTreeMap<String, String>,
}

pub fn capture(
    provider: &str,
    profile: &Path,
    timeout: Duration,
    publish_qr_code: &mut dyn FnMut(&str) -> Result<(), CaptureError>,
) -> Result<BTreeMap<String, String>, CaptureError> {
    platform::capture(provider, profile, timeout, publish_qr_code)
}

pub fn request_kugou_json(
    profile: &Path,
    timeout: Duration,
    url: &str,
    userid: &str,
    mid: &str,
    cookies: &BTreeMap<String, String>,
) -> Result<Value, CaptureError> {
    let request_url = kugou_request_url(url)?;
    let mut filtered_cookies = cookies
        .iter()
        .filter(|(name, value)| {
            !value.trim().is_empty()
                && matches!(
                    name.as_str(),
                    "KuGoo" | "dfid" | "kg_dfid" | "kg_mid" | "mid" | "KugouGUID" | "t1"
                )
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    // The official KgUser/getBaseInfo scripts read these browser-cookie
    // aliases, while captured credentials commonly expose the same values as
    // `dfid`/`mid`.
    if let Some(value) = filtered_cookies.get("dfid").cloned() {
        filtered_cookies
            .entry("kg_dfid".to_owned())
            .or_insert(value);
    }
    if let Some(value) = filtered_cookies.get("mid").cloned() {
        filtered_cookies.entry("kg_mid".to_owned()).or_insert(value);
    }
    let input = KugouRequestInput {
        url: request_url,
        userid: if userid.trim().is_empty() {
            "0".to_owned()
        } else {
            userid.to_owned()
        },
        mid: if mid.trim().is_empty() {
            "-".to_owned()
        } else {
            mid.to_owned()
        },
        cookies: filtered_cookies,
    };
    if input.cookies.is_empty() {
        return Err(CaptureError::Io("酷狗 Web 请求没有可用 Cookie".to_owned()));
    }
    platform::request_kugou_json(profile, timeout, input)
}

fn validate_kugou_request_url(url: &str) -> Result<Url, CaptureError> {
    let parsed =
        Url::parse(url).map_err(|_| CaptureError::Io("酷狗 Web 请求 URL 无效".to_owned()))?;
    let request_allowed = matches!(
        (parsed.host_str(), parsed.path()),
        (
            Some("wwwapi.kugou.com" | "wwwapiretry.kugou.com"),
            "/play/songinfo"
        ) | (Some("kugouvip.kugou.com"), "/v1/get_union_vip")
    );
    if parsed.scheme() != "https" || !request_allowed {
        return Err(CaptureError::Io(
            "酷狗 Web 请求 URL 不在允许范围内".to_owned(),
        ));
    }
    Ok(parsed)
}

fn kugou_request_url(url: &str) -> Result<String, CaptureError> {
    let mut parsed = validate_kugou_request_url(url)?;
    if parsed.host_str() == Some("wwwapi.kugou.com") {
        parsed
            .set_host(Some("wwwapiretry.kugou.com"))
            .map_err(|_| CaptureError::Io("酷狗 Web 请求地址转换失败".to_owned()))?;
    }
    Ok(parsed.into())
}

/// Build the page-side request. The retry host is the normal browser path for
/// a request that the primary host has marked with an SSA challenge. A request
/// without a challenge is returned immediately; the page is not turned into a
/// verification dialog merely because the endpoint returned a business error.
fn kugou_request_script(input: &KugouRequestInput) -> Result<String, CaptureError> {
    let url = serde_json::to_string(&input.url)
        .map_err(|error| CaptureError::Io(format!("酷狗请求 URL 编码失败: {error}")))?;
    let userid = serde_json::to_string(&input.userid)
        .map_err(|error| CaptureError::Io(format!("酷狗请求用户标识编码失败: {error}")))?;
    let mid = serde_json::to_string(&input.mid)
        .map_err(|error| CaptureError::Io(format!("酷狗请求 MID 编码失败: {error}")))?;
    Ok(format!(
        r#"(() => {{
          const initialRequestUrl = {url};
          let requestUrl = initialRequestUrl;
          const requestUserId = {userid};
          const requestMid = {mid};
          const verifyScript = 'https://staticssl.kugou.com/verify/static/js/verify.min.js?20200707';
          const jqueryScript = 'https://staticssl.kugou.com/common/js/min/jquery-2.1.4.min.js';
          const state = {{ challengeCount: 0 }};
          const postResult = (value) => {{
            try {{
              if (window.chrome && window.chrome.webview) {{
                window.chrome.webview.postMessage({{
                  bridge: '__miliastra_login_helper__',
                  kind: 'kugou_request_result',
                  value
                }});
              }}
            }} catch (_) {{}}
          }};
          const loadScript = (src) => new Promise((resolve, reject) => {{
            const existing = [...document.scripts].find((node) => node.src.indexOf(src) === 0);
            if (existing) {{
              if (window.verifyObject && typeof window.verifyObject.init === 'function') resolve();
              else {{ existing.addEventListener('load', resolve, {{once: true}}); existing.addEventListener('error', reject, {{once: true}}); }}
              return;
            }}
            const node = document.createElement('script');
            node.src = src;
            node.onload = () => resolve();
            node.onerror = () => reject(new Error('verify script load failed'));
            (document.head || document.documentElement).appendChild(node);
          }});
          const cookieNames = () => [...new Set(document.cookie.split(';')
            .map((part) => part.split('=')[0].trim()).filter(Boolean))].sort();
          const safeSummary = (result) => {{
            const body = result && result.body && typeof result.body === 'object'
              ? result.body : {{}};
            const headers = result && result.headers;
            const headerPresent = (name) => !!(headers && headers.get(name));
            const numberField = (names) => {{
              for (const name of names) {{
                if (body[name] !== undefined && body[name] !== null) return String(body[name]);
              }}
              return '';
            }};
            return {{
              origin: String(location.origin || ''),
              requestHost: (() => {{ try {{ return new URL(requestUrl).hostname; }} catch (_) {{ return ''; }} }})(),
              httpStatus: Number(result && result.httpStatus || 0),
              status: numberField(['status']),
              code: numberField(['code']),
              errCode: numberField(['err_code', 'error_code', 'errorCode']),
              bodyKeys: Object.keys(body).sort().slice(0, 32),
              ssaCodePresent: headerPresent('SSA-CODE'),
              ssaHmidPresent: headerPresent('SSA-HMID'),
              cookieNames: cookieNames()
            }};
          }};
          const fetchJson = async () => {{
            const controller = new AbortController();
            const timer = setTimeout(() => controller.abort(), 10000);
            try {{
              const response = await fetch(requestUrl, {{
                method: 'GET',
                credentials: 'include',
                headers: {{ 'Accept': 'application/json, text/plain, */*' }},
                signal: controller.signal
              }});
              const body = await response.json();
              return {{ body, headers: response.headers, httpStatus: response.status }};
            }} finally {{ clearTimeout(timer); }}
          }};
          const challengeFields = (result) => {{
            const body = result && result.body ? result.body : {{}};
            const data = body && body.data && typeof body.data === 'object' ? body.data : {{}};
            const headers = result && result.headers;
            const pick = (names) => {{
              for (const source of [data, body]) {{
                for (const name of names) {{
                  if (source && source[name]) return source[name];
                }}
              }}
              return '';
            }};
            return {{
              eventid: (pick(['SSA-CODE', 'ssa_code', 'ssaCode', 'eventid']) ||
                (headers && headers.get('SSA-CODE')) || ''),
              hmid: (pick(['SSA-HMID', 'ssa_hmid', 'ssaHmid', 'hmid']) ||
                (headers && headers.get('SSA-HMID')) || requestMid)
            }};
          }};
          const isChallenge = (result, fields) => {{
            const body = result && result.body ? result.body : {{}};
            const code = Number(body.err_code ?? body.error_code ?? body.errorCode ?? 0);
            return (code === 30020 || code === 20028) && !!fields.eventid;
          }};
          const runChallenge = (fields) => new Promise((resolve, reject) => {{
            const callbackName = '__miliastra_kugou_verify_' + Date.now() + '_' + Math.random().toString(16).slice(2);
            window[callbackName] = (value) => {{
              try {{
                delete window[callbackName];
                if (value && Number(value.status) === 1) resolve();
                else reject(new Error('verification rejected'));
              }} catch (error) {{ reject(error); }}
            }};
            const invoke = () => {{
              try {{
                window.verifyObject.init(1014, fields.hmid || requestMid,
                  requestUserId || '0', fields.eventid, callbackName, true);
              }} catch (error) {{ reject(error); }}
            }};
            if (window.verifyObject && typeof window.verifyObject.init === 'function') invoke();
            else loadScript(jqueryScript).catch(() => {{}}).then(() => loadScript(verifyScript)).then(invoke).catch(reject);
          }});
          const run = async () => {{
            for (;;) {{
              const result = await fetchJson();
              const fields = challengeFields(result);
              const diagnostics = safeSummary(result);
              if (!isChallenge(result, fields)) return {{ ok: true, body: result.body, diagnostics }};
              if (state.challengeCount++ >= 2) return {{ ok: false, message: 'challenge limit exceeded', diagnostics }};
              try {{
                await runChallenge(fields);
              }} catch (error) {{
                return {{
                  ok: false,
                  retry: true,
                  message: String(error && error.message || error),
                  diagnostics
                }};
              }}
              // KgUser.search switches from the primary API host to the
              // retry host after antiBrush succeeds. songinfo follows the
              // same browser-side contract.
              try {{
                const retry = new URL(requestUrl);
                if (retry.hostname === 'wwwapi.kugou.com') {{
                  retry.hostname = 'wwwapiretry.kugou.com';
                  requestUrl = retry.toString();
                }}
              }} catch (_) {{}}
            }}
          }};
          run().then(postResult).catch((error) => postResult({{
            ok: false,
            retry: true,
            message: String(error && error.message || error),
            diagnostics: {{
              origin: String(location.origin || ''),
              requestHost: (() => {{ try {{ return new URL(requestUrl).hostname; }} catch (_) {{ return ''; }} }})(),
              httpStatus: 0,
              status: '',
              code: '',
              errCode: '',
              bodyKeys: [],
              ssaCodePresent: false,
              ssaHmidPresent: false,
              cookieNames: cookieNames()
            }}
          }}));
          // ExecuteScript does not await a returned Promise. The actual
          // result is delivered through WebMessageReceived above.
          return true;
        }})()"#,
        url = url,
        userid = userid,
        mid = mid,
    ))
}

fn log_kugou_request_diagnostics(value: &Value) {
    if std::env::var_os("MILIASTRA_LOGIN_HELPER_DIAGNOSTICS").is_none() {
        return;
    }
    let Some(summary) = value.get("diagnostics") else {
        eprintln!("[kugou-web-request] browser response had no diagnostics");
        return;
    };
    let fields = [
        ("origin", summary.get("origin")),
        ("requestHost", summary.get("requestHost")),
        ("httpStatus", summary.get("httpStatus")),
        ("status", summary.get("status")),
        ("code", summary.get("code")),
        ("errCode", summary.get("errCode")),
        ("ssaCodePresent", summary.get("ssaCodePresent")),
        ("ssaHmidPresent", summary.get("ssaHmidPresent")),
        ("bodyKeys", summary.get("bodyKeys")),
        ("cookieNames", summary.get("cookieNames")),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| format!("{name}={value}")))
    .collect::<Vec<_>>()
    .join(" ");
    eprintln!("[kugou-web-request] browser {fields}");
}

fn log_kugou_script_result_shape(raw: &str, parsed: Option<&Value>) {
    if std::env::var_os("MILIASTRA_LOGIN_HELPER_DIAGNOSTICS").is_none() {
        return;
    }
    let outer = serde_json::from_str::<Value>(raw).ok();
    let outer_shape = outer
        .as_ref()
        .map(|value| match value {
            Value::Null => "null".to_owned(),
            Value::Bool(_) => "bool".to_owned(),
            Value::Number(_) => "number".to_owned(),
            Value::String(value) => format!("string(len={})", value.len()),
            Value::Array(value) => format!("array(len={})", value.len()),
            Value::Object(value) => format!("object(keys={})", value.len()),
        })
        .unwrap_or_else(|| "invalid-json".to_owned());
    let parsed_shape = parsed
        .map(|value| match value {
            Value::Null => "null".to_owned(),
            Value::Bool(_) => "bool".to_owned(),
            Value::Number(_) => "number".to_owned(),
            Value::String(value) => format!("string(len={})", value.len()),
            Value::Array(value) => format!("array(len={})", value.len()),
            Value::Object(value) => format!("object(keys={})", value.len()),
        })
        .unwrap_or_else(|| "none".to_owned());
    eprintln!(
        "[kugou-web-request] ExecuteScript result raw_len={} outer={} parsed={}",
        raw.len(),
        outer_shape,
        parsed_shape
    );
}

fn login_url(provider: &str) -> Option<&'static str> {
    match provider {
        // The OAuth document owns the ptlogin iframe and redirects the top
        // level document to wx_redirect.html after a successful scan. Opening
        // the iframe's xlogin document directly loses that parent callback.
        "qqmusic" => Some(
            "https://graph.qq.com/oauth2.0/authorize?response_type=code&client_id=100497308&redirect_uri=https%3A%2F%2Fy.qq.com%2Fportal%2Fwx_redirect.html%3Flogin_type%3D1%26surl%3Dhttps%253A%252F%252Fy.qq.com%252Fn%252Fryqq_v2%252Fprofile&state=state&display=pc&scope=get_user_info%2Cget_app_friends",
        ),
        "netease" => Some("https://music.163.com/"),
        "bilibili" => Some("https://www.bilibili.com/"),
        // Official web login page. It renders the QR as a data URL and sets
        // the same KuGoo cookie after a scan.
        "kugou" => Some(
            "https://login-user.kugou.com/login/?appid=1014&ref=https%3A%2F%2Fwww.kugou.com%2Freg%2Fweb%2F&redirect_uri=https%3A%2F%2Fstaticssl.kugou.com%2Fcommon%2Fhtml%2Flogin%2Fregok.html",
        ),
        _ => None,
    }
}

const WEB_QR_PROBE_INTERVAL: Duration = Duration::from_millis(250);
const WEB_QR_SCRIPT_TIMEOUT: Duration = Duration::from_secs(3);
const WEB_QR_FETCH_TIMEOUT: Duration = Duration::from_secs(8);
const WEB_QR_SCRIPT_INSTALL_FALLBACK: Duration = Duration::from_millis(750);
const WEB_QR_REPEAT_FETCH_INTERVAL: Duration = Duration::from_secs(2);

/// Install before the first navigation so QR images and Bilibili's browser
/// refresh token can be reported through the top-level WebView2 host bridge.
/// It only observes Bilibili's documented login response and exact storage
/// keys; it never reads document cookies or arbitrary page storage.
fn web_qr_bridge_script() -> &'static str {
    r#"(() => {
      const bridge = '__miliastra_login_helper__';
      const isTop = window === window.top;
      const post = (payload) => {
        const message = Object.assign({bridge, frameUrl: String(location.href)}, payload);
        try {
          if (isTop && window.chrome && window.chrome.webview) {
            window.chrome.webview.postMessage(message);
          } else if (window.top) {
            window.top.postMessage(message, '*');
          }
        } catch (_) {}
      };

      // A child frame cannot call the host bridge reliably on every Runtime.
      // Forward its bounded QR/login payload to the injected top-level bridge.
      if (isTop) {
        window.addEventListener('message', (event) => {
          const data = event && event.data;
          if (data && data.bridge === bridge &&
              (data.kind === 'qr' || data.kind === 'qq_callback' ||
               data.kind === 'bilibili_refresh')) {
            post({kind: data.kind, value: String(data.value || ''),
              frameUrl: String(data.frameUrl || '')});
          }
        }, true);
        window.addEventListener('message', (event) => {
          if (typeof (event && event.data) === 'string' &&
              event.data.indexOf('qclogin_success') >= 0) {
            post({kind: 'qq_event', value: event.data});
          }
        }, true);
      }

      const bilibiliHost = () => {
        const host = String(location.hostname || '').toLowerCase();
        return host === 'bilibili.com' || host.endsWith('.bilibili.com');
      };
      let lastBilibiliRefresh = '';
      const emitBilibiliRefresh = (value) => {
        value = String(value || '').trim();
        if (!value || value.length > 1024 || value === lastBilibiliRefresh) return;
        lastBilibiliRefresh = value;
        post({kind: 'bilibili_refresh', value});
      };
      const refreshFromLoginPayload = (payload) => {
        if (!payload || typeof payload !== 'object') return '';
        const sources = [payload, payload.data, payload.data && payload.data.data,
          payload.result];
        for (const source of sources) {
          if (!source || typeof source !== 'object') continue;
          for (const name of ['refresh_token', 'refreshToken', 'ac_time_value']) {
            const value = source[name];
            if (typeof value === 'string' && value.trim()) return value;
          }
        }
        return '';
      };
      const parseBilibiliLoginPayload = (text) => {
        const normalized = String(text || '').trim().replace(/^\)\]\}',?\s*/, '');
        if (!normalized) return null;
        try { return JSON.parse(normalized); } catch (_) {}
        const wrapped = normalized.match(/^[^(]+\(([\s\S]*)\)\s*;?\s*$/);
        if (!wrapped) return null;
        try { return JSON.parse(wrapped[1]); } catch (_) { return null; }
      };
      const isBilibiliLoginEndpoint = (raw) => {
        try {
          const url = new URL(String(raw || ''), location.href);
          const host = String(url.hostname || '').toLowerCase();
          if (host !== 'bilibili.com' && !host.endsWith('.bilibili.com')) return false;
          return url.pathname === '/x/passport-login/web/qrcode/poll' ||
            url.pathname === '/x/passport-login/web/cookie/refresh';
        } catch (_) {
          return false;
        }
      };
      const inspectBilibiliLoginResponse = (url, text) => {
        if (!isBilibiliLoginEndpoint(url) || !text || text.length > 65536) return;
        const payload = parseBilibiliLoginPayload(text);
        if (payload) emitBilibiliRefresh(refreshFromLoginPayload(payload));
      };
      const inspectBilibiliStorage = () => {
        if (!bilibiliHost()) return;
        for (const store of [window.localStorage, window.sessionStorage]) {
          try {
            for (const name of ['ac_time_value', 'refresh_token', 'refreshToken']) {
              const value = store.getItem(name);
              if (value) {
                emitBilibiliRefresh(value);
                return;
              }
            }
          } catch (_) {}
        }
      };
      const installBilibiliRefreshCapture = () => {
        if (!bilibiliHost()) return;
        const fetchMarker = '__miliastraBilibiliRefreshCapture';
        try {
          const currentFetch = window.fetch;
          if (typeof currentFetch === 'function' && !currentFetch[fetchMarker]) {
            const wrappedFetch = function(input) {
              const result = currentFetch.apply(this, arguments);
              try {
                const url = input && input.url ? input.url : input;
                if (isBilibiliLoginEndpoint(url)) {
                  Promise.resolve(result).then((response) => {
                    if (!response || typeof response.clone !== 'function') return '';
                    return response.clone().text();
                  }).then((text) => inspectBilibiliLoginResponse(url, text)).catch(() => {});
                }
              } catch (_) {}
              return result;
            };
            try { Object.defineProperty(wrappedFetch, fetchMarker, {value: true}); }
            catch (_) { wrappedFetch[fetchMarker] = true; }
            window.fetch = wrappedFetch;
          }
          const xhrPrototype = window.XMLHttpRequest && window.XMLHttpRequest.prototype;
          if (xhrPrototype && !xhrPrototype.__miliastraBilibiliXhrCapture) {
            const xhrUrls = new WeakMap();
            const originalOpen = xhrPrototype.open;
            const originalSend = xhrPrototype.send;
            xhrPrototype.__miliastraBilibiliXhrCapture = true;
            xhrPrototype.open = function(method, url) {
              xhrUrls.set(this, String(url || ''));
              return originalOpen.apply(this, arguments);
            };
            xhrPrototype.send = function() {
              const url = xhrUrls.get(this);
              if (isBilibiliLoginEndpoint(url) && !this.__miliastraBilibiliRefreshListener) {
                this.__miliastraBilibiliRefreshListener = true;
                this.addEventListener('loadend', () => {
                  try {
                    const text = typeof this.responseText === 'string'
                      ? this.responseText
                      : (this.response && typeof this.response === 'object'
                        ? JSON.stringify(this.response) : '');
                    inspectBilibiliLoginResponse(url, text);
                  } catch (_) {}
                }, {once: true});
              }
              return originalSend.apply(this, arguments);
            };
          }
        } catch (_) {}
        inspectBilibiliStorage();
      };
      installBilibiliRefreshCapture();

      const visible = (node) => {
        if (!node) return false;
        const style = getComputedStyle(node);
        const rect = node.getBoundingClientRect();
        return style.display !== 'none' && style.visibility !== 'hidden' &&
          style.opacity !== '0' && rect.width >= 80 && rect.height >= 80;
      };
      const clickable = (node) => {
        if (!node) return false;
        const style = getComputedStyle(node);
        const rect = node.getBoundingClientRect();
        return style.display !== 'none' && style.visibility !== 'hidden' &&
          style.opacity !== '0' && rect.width > 0 && rect.height > 0;
      };

      const bilibiliLogin = () => {
        // Bilibili moved the login entry from an anchor/button to several
        // clickable div variants. Prefer stable classes before text fallback.
        const selectors = [
          '.right-entry__outside.go-login-btn',
          '.header-login-entry',
          'div.login-btn',
          '[class*="go-login-btn"]',
          '[class*="header-login-entry"]',
          '[class*="login-btn"]'
        ];
        for (const selector of selectors) {
          const node = [...document.querySelectorAll(selector)].find((item) =>
            clickable(item) && !item.dataset.miliastraQrClicked);
          if (node) return node;
        }
        return [...document.querySelectorAll('a,button,[role="button"],div')].find((node) =>
          clickable(node) && !node.dataset.miliastraQrClicked &&
          /^(登录|立即登录)$/.test((node.innerText || '').trim()));
      };
      const square = (width, height) => width >= 80 && height >= 80 &&
        Math.max(width, height) / Math.max(1, Math.min(width, height)) <= 1.35;
      const imageSource = (node) => String(
        node.currentSrc || node.src || node.getAttribute('data-src') ||
        node.getAttribute('data-url') || ''
      ).trim();
      let last = '';
      let lastEmittedAt = 0;
      const emit = (value) => {
        value = String(value || '').trim();
        const now = Date.now();
        if (!value || (value === last && now - lastEmittedAt < 2000)) return;
        last = value;
        lastEmittedAt = now;
        post({kind: 'qr', value});
      };
      const probe = () => {
        installBilibiliRefreshCapture();
        if (location.hostname === 'www.bilibili.com' ||
            location.hostname === 'bilibili.com') {
          const login = bilibiliLogin();
          if (login) {
            login.dataset.miliastraQrClicked = '1';
            try { login.click(); } catch (_) {}
          }
        }
        const candidates = [];
        const markedSelectors = [
          'img.qrImg', '#qrlogin_img', 'img[alt*="二维码"]',
          'img[alt*="QR"]', '[class*="qr"] img', '[id*="qr"] img'
        ];
        for (const selector of markedSelectors) {
          for (const node of document.querySelectorAll(selector)) {
            if (!visible(node)) continue;
            const rect = node.getBoundingClientRect();
            const width = node.naturalWidth || rect.width;
            const height = node.naturalHeight || rect.height;
            if (!square(width, height)) continue;
            const source = imageSource(node);
            if (source) candidates.push({score: 240, value: source});
          }
        }
        for (const node of document.querySelectorAll('canvas')) {
          if (!visible(node)) continue;
          const rect = node.getBoundingClientRect();
          const width = node.width || rect.width;
          const height = node.height || rect.height;
          if (!square(width, height)) continue;
          try {
            const value = node.toDataURL('image/png');
            if (value.startsWith('data:image/png;base64,')) {
              candidates.push({score: 180, value});
            }
          } catch (_) {}
        }
        for (const node of document.querySelectorAll('img')) {
          if (!visible(node)) continue;
          const rect = node.getBoundingClientRect();
          const width = node.naturalWidth || rect.width;
          const height = node.naturalHeight || rect.height;
          if (!square(width, height)) continue;
          const source = imageSource(node);
          if (!source || source.startsWith('data:image/svg')) continue;
          const marker = [node.id, node.className, node.alt, node.title,
            node.getAttribute('aria-label')].join(' ').toLowerCase();
          const marked = /qr|qrcode|qrlogin|二维码|登录二维码/.test(marker);
          const inline = source.startsWith('data:image/png') ||
            source.startsWith('data:image/jpeg');
          if (!marked && !inline) continue;
          candidates.push({score: marked ? (inline ? 220 : 200) : 120, value: source});
        }
        candidates.sort((left, right) => right.score - left.score);
        if (candidates.length) emit(candidates[0].value);
        if (location.hostname === 'graph.qq.com' &&
            new URLSearchParams(location.search).has('code')) {
          post({kind: 'qq_callback', value: String(location.href)});
        }
      };
      probe();
      try {
        new MutationObserver(probe).observe(document.documentElement || document,
          {subtree: true, childList: true, attributes: true});
      } catch (_) {}
      setInterval(probe, 500);
    })()"#
}

/// Extract a QR image from the provider page currently displayed in WebView2.
/// NetEase needs its login link clicked first; QQ and KuGou expose an image on
/// their direct official login pages. The result is a data URL or an image URL
/// that the native side validates before publishing to the HTTP UI.
fn web_qr_probe_script() -> &'static str {
    r#"(() => {
      const visible = (node) => {
        if (!node) return false;
        const style = getComputedStyle(node);
        const rect = node.getBoundingClientRect();
        return style.display !== 'none' && style.visibility !== 'hidden' &&
          style.opacity !== '0' && rect.width >= 80 && rect.height >= 80;
      };
      const clickable = (node) => {
        if (!node) return false;
        const style = getComputedStyle(node);
        const rect = node.getBoundingClientRect();
        return style.display !== 'none' && style.visibility !== 'hidden' &&
          style.opacity !== '0' && rect.width > 0 && rect.height > 0;
      };
      const bilibiliLogin = () => {
        const selectors = [
          '.right-entry__outside.go-login-btn',
          '.header-login-entry',
          'div.login-btn',
          '[class*="go-login-btn"]',
          '[class*="header-login-entry"]',
          '[class*="login-btn"]'
        ];
        for (const selector of selectors) {
          const node = [...document.querySelectorAll(selector)].find((item) =>
            clickable(item) && !item.dataset.miliastraQrClicked);
          if (node) return node;
        }
        return [...document.querySelectorAll('a,button,[role="button"],div')].find((node) =>
          clickable(node) && !node.dataset.miliastraQrClicked &&
          /^(登录|立即登录)$/.test((node.innerText || '').trim()));
      };
      const square = (width, height) => width >= 80 && height >= 80 &&
        Math.max(width, height) / Math.max(1, Math.min(width, height)) <= 1.35;
      const wait = () => ({kind: 'wait'});
      const imageSource = (node) => String(
        node.currentSrc || node.src || node.getAttribute('data-src') ||
        node.getAttribute('data-url') || ''
      ).trim();

      // NetEase keeps the QR dialog behind the login link on its home page.
      if (location.hostname === 'music.163.com' &&
          ![...document.querySelectorAll('[role="dialog"]')].some(visible)) {
        const login = document.querySelector('[data-action="login"]') ||
          [...document.querySelectorAll('a,button,[role="button"]')].find((node) =>
            clickable(node) && /^(登录|立即登录)$/.test((node.innerText || '').trim()));
        if (login && !login.dataset.miliastraQrClicked) {
          login.dataset.miliastraQrClicked = '1';
          login.click();
          return wait();
        }
      }
      if ((location.hostname === 'www.bilibili.com' ||
           location.hostname === 'bilibili.com') &&
           ![...document.querySelectorAll('img[alt*="二维码"], img[alt*="QR"]')].some(visible)) {
        const login = bilibiliLogin();
        if (login && !login.dataset.miliastraQrClicked) {
          login.dataset.miliastraQrClicked = '1';
          login.click();
          return wait();
        }
      }

      const candidates = [];
      for (const node of document.querySelectorAll('canvas')) {
        if (!visible(node)) continue;
        const rect = node.getBoundingClientRect();
        const width = node.width || rect.width;
        const height = node.height || rect.height;
        if (!square(width, height)) continue;
        try {
          const data = node.toDataURL('image/png');
          if (data.startsWith('data:image/png;base64,')) {
            candidates.push({score: 90, value: data});
          }
        } catch (_) {}
      }
      for (const node of document.querySelectorAll('img')) {
        if (!visible(node)) continue;
        const rect = node.getBoundingClientRect();
        const width = node.naturalWidth || rect.width;
        const height = node.naturalHeight || rect.height;
        if (!square(width, height)) continue;
        const source = imageSource(node);
        if (!source || source.startsWith('data:image/svg')) continue;
        const marker = [node.id, node.className, node.alt, node.title,
          node.getAttribute('aria-label')].join(' ').toLowerCase();
        const marked = /qr|qrcode|qrlogin|二维码|登录二维码/.test(marker);
        const inline = source.startsWith('data:image/png') || source.startsWith('data:image/jpeg');
        // Do not mistake a square album/advertisement image for a QR. The
        // known provider QR images are either explicitly marked or inline
        // data URLs; unmarked remote images are ignored.
        if (!marked && !inline) continue;
        let score = marked ? 120 : 20;
        if (inline) score += 20;
        candidates.push({score, value: source});
      }
      candidates.sort((left, right) => right.score - left.score);
      return candidates.length ? {kind: 'image', value: candidates[0].value} : wait();
    })()"#
}

fn web_qr_host_allowed(provider: &str, host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let suffixes: &[&str] = match provider {
        "qqmusic" => &["qq.com"],
        "netease" => &["163.com", "126.net"],
        "kugou" => &["kugou.com"],
        "bilibili" => &["bilibili.com", "hdslb.com"],
        _ => &[],
    };
    suffixes
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

fn web_qr_url_allowed(provider: &str, url: &Url) -> bool {
    matches!(url.scheme(), "https" | "http")
        && url
            .host_str()
            .is_some_and(|host| web_qr_host_allowed(provider, host))
}

fn web_qr_message_source_allowed(provider: &str, source: &str, frame_url: &str) -> bool {
    [source, frame_url].into_iter().any(|raw| {
        Url::parse(raw).ok().is_some_and(|url| {
            matches!(url.scheme(), "https" | "http")
                && url
                    .host_str()
                    .is_some_and(|host| web_qr_host_allowed(provider, host))
        })
    })
}

fn bilibili_login_endpoint_url(raw: &str) -> bool {
    Url::parse(raw).ok().is_some_and(|url| {
        url.host_str()
            .is_some_and(|host| web_qr_host_allowed("bilibili", host))
            && matches!(
                url.path(),
                "/x/passport-login/web/qrcode/poll" | "/x/passport-login/web/cookie/refresh"
            )
    })
}

/// QQ's `ptqrshow` response is stateful: a second request creates a different
/// QR ticket. Read only the response that WebView2 actually rendered.
fn qq_web_qr_response_url(raw: &str) -> bool {
    Url::parse(raw).ok().is_some_and(|url| {
        web_qr_url_allowed("qqmusic", &url) && matches!(url.path(), "/ptqrshow" | "/ssl/ptqrshow")
    })
}

fn parse_bilibili_login_payload(text: &str) -> Option<Value> {
    let text = text.trim().trim_start_matches('\u{feff}');
    let text = text
        .strip_prefix(")]}',")
        .or_else(|| text.strip_prefix(")]}'\n"))
        .unwrap_or(text)
        .trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str(text) {
        return Some(value);
    }
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    (close > open).then(|| serde_json::from_str(&text[open + 1..close]).ok())?
}

fn bilibili_refresh_token_from_payload(payload: &Value) -> Option<String> {
    fn visit(value: &Value, depth: u8) -> Option<String> {
        if depth > 3 {
            return None;
        }
        let object = value.as_object()?;
        for name in ["refresh_token", "refreshToken", "ac_time_value", "token"] {
            if let Some(token) = object
                .get(name)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|token| valid_bilibili_refresh_token(token))
            {
                return Some(token.to_owned());
            }
        }
        for name in ["data", "result"] {
            if let Some(child) = object.get(name)
                && let Some(token) = visit(child, depth + 1)
            {
                return Some(token);
            }
        }
        None
    }

    visit(payload, 0)
}

fn bilibili_login_payload_is_success(payload: &Value) -> bool {
    payload
        .get("data")
        .and_then(Value::as_object)
        .and_then(|data| data.get("code"))
        .is_some_and(|code| code.as_i64() == Some(0) || code.as_str() == Some("0"))
}

fn log_bilibili_login_payload_shape(payload: &Value) {
    if std::env::var_os("MILIASTRA_LOGIN_HELPER_DIAGNOSTICS").is_none() {
        return;
    }
    let top_code = payload
        .get("code")
        .map(Value::to_string)
        .unwrap_or_else(|| "missing".to_owned());
    let data = payload.get("data").and_then(Value::as_object);
    let data_code = data
        .and_then(|object| object.get("code"))
        .map(Value::to_string)
        .unwrap_or_else(|| "missing".to_owned());
    let data_keys = data
        .map(|object| {
            object
                .keys()
                .take(32)
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    eprintln!(
        "[bilibili-capture] login response shape top_code={top_code} data_code={data_code} data_keys={data_keys}"
    );
}

fn validate_web_qr_data_url(value: &str) -> Result<String, String> {
    miliastra_login_protocol::validate_qr_image_data_url(value)
        .map_err(|_| "网页登录二维码图片格式无效".to_owned())?;
    Ok(value.to_owned())
}

fn valid_bilibili_refresh_token(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= BILIBILI_REFRESH_TOKEN_MAX_LEN
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn log_bilibili_capture_diagnostics(detail: &str) {
    if std::env::var_os("MILIASTRA_LOGIN_HELPER_DIAGNOSTICS").is_some() {
        eprintln!("[bilibili-capture] {detail}");
    }
}

fn log_qq_qr_capture_diagnostics(detail: &str) {
    if std::env::var_os("MILIASTRA_LOGIN_HELPER_DIAGNOSTICS").is_some() {
        eprintln!("[qq-qr-capture] {detail}");
    }
}

fn parse_web_qr_probe_result(raw: &str) -> Option<(String, String)> {
    let value = parse_execute_script_value(raw)?;
    let object = value.as_object()?;
    let kind = object.get("kind")?.as_str()?.to_owned();
    let value = object.get("value")?.as_str()?.to_owned();
    (kind == "image" && !value.trim().is_empty()).then_some((kind, value))
}

// WebView2 may return the script result as either a JSON value or a quoted
// JSON string containing that value, depending on the runtime version.
fn parse_execute_script_value(raw: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    match value {
        Value::String(encoded) => serde_json::from_str(&encoded).ok(),
        value => Some(value),
    }
}

fn web_qr_image_data_url_from_bytes(bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() || bytes.len() > miliastra_login_protocol::MAX_QR_IMAGE_BYTES {
        return Err("网页登录二维码图片过大".to_owned());
    }
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else {
        return Err("网页登录二维码图片格式无效".to_owned());
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    validate_web_qr_data_url(&format!("data:{mime};base64,{encoded}"))
}

/// Download an image URL discovered in the WebView2 DOM. This is only an
/// image fetch; cookies, redirects and login state remain owned by WebView2.
fn download_web_qr_image(provider: &str, raw_url: &str) -> Result<String, String> {
    let url = Url::parse(raw_url).map_err(|_| "网页登录二维码地址无效".to_owned())?;
    if !web_qr_url_allowed(provider, &url) {
        return Err("网页登录二维码地址不受支持".to_owned());
    }
    let referer = match provider {
        "qqmusic" => "https://xui.ptlogin2.qq.com/",
        "netease" => "https://music.163.com/",
        "kugou" => "https://login-user.kugou.com/",
        _ => "https://music.163.com/",
    };
    let redirect_provider = provider.to_owned();
    let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 8 {
            attempt.stop()
        } else if web_qr_url_allowed(&redirect_provider, attempt.url()) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    let client = Client::builder()
        .timeout(WEB_QR_FETCH_TIMEOUT)
        .redirect(redirect_policy)
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/124 Safari/537.36",
        )
        .build()
        .map_err(|_| "网页登录二维码网络客户端初始化失败".to_owned())?;
    let mut response = client
        .get(url)
        .header("Referer", referer)
        .header(
            "Accept",
            "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
        )
        .send()
        .map_err(|_| "网页登录二维码图片请求失败".to_owned())?
        .error_for_status()
        .map_err(|_| "网页登录二维码图片请求失败".to_owned())?;
    if !web_qr_url_allowed(provider, response.url()) {
        return Err("网页登录二维码地址不受支持".to_owned());
    }
    if response
        .content_length()
        .is_some_and(|size| size > miliastra_login_protocol::MAX_QR_IMAGE_BYTES as u64)
    {
        return Err("网页登录二维码图片过大".to_owned());
    }
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(miliastra_login_protocol::MAX_QR_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "网页登录二维码图片读取失败".to_owned())?;
    if bytes.is_empty() || bytes.len() > miliastra_login_protocol::MAX_QR_IMAGE_BYTES {
        return Err("网页登录二维码图片过大".to_owned());
    }
    web_qr_image_data_url_from_bytes(&bytes)
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
            "unionid",
        ],
        "netease" => &["MUSIC_U", "__csrf"],
        // 酷狗网页登录凭据集中在单个 KuGoo cookie（值形如
        // t=token&KugooID=userid&ct=...），其余为辅助标识 cookie。
        "kugou" => &[
            "KuGoo",
            "dfid",
            "KugouGUID",
            "KUGOU_API_GUID",
            "KUGOU_API_MID",
            "KUGOU_API_MAC",
            "KUGOU_API_DEV",
            "kg_mid",
            "mid",
            "t1",
            "vip_type",
            "vip_token",
        ],
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
            has_any_nonempty(cookies, &["uin", "wxuin"])
                && has_any_nonempty(cookies, &["qqmusic_key", "qm_keyst", "lqm_keyst"])
        }
        "netease" => has_any_nonempty(cookies, &["MUSIC_U"]),
        // Bilibili assigns an anonymous SESSDATA-like session before QR
        // login completes. DedeUserID is only present after the account
        // session has been established, so requiring both prevents the
        // refresh-token probe from timing out while the user is still
        // looking at the QR code.
        "bilibili" => {
            has_any_nonempty(cookies, &["SESSDATA"]) && has_any_nonempty(cookies, &["DedeUserID"])
        }
        "kugou" => {
            let web_cookie = cookies.get("KuGoo").is_some_and(|value| {
                let value = decode_kugou_cookie_value(value);
                !value.is_empty() && value.contains("t=") && value.contains("KugooID=")
            });
            web_cookie
        }
        _ => false,
    }
}

fn decode_kugou_cookie_value(value: &str) -> String {
    if value.as_bytes().contains(&b'%') {
        percent_decode_str(value).decode_utf8_lossy().into_owned()
    } else {
        value.to_owned()
    }
}

fn has_any_nonempty(cookies: &BTreeMap<String, String>, aliases: &[&str]) -> bool {
    aliases.iter().any(|alias| {
        cookies
            .get(*alias)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

const COOKIE_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// B 站将 refresh_token 存在页面 localStorage/sessionStorage（键名
/// ac_time_value），cookie 是否下发不稳定。登录完成后保持一段受限探测，
/// 但绝不把缺少 refresh_token 的基础 Cookie 当成完整登录结果。
const BILIBILI_JS_PROBE_ATTEMPTS: u32 = 24;
const BILIBILI_JS_PROBE_INTERVAL: Duration = Duration::from_millis(750);
const BILIBILI_REFRESH_TOKEN_MAX_LEN: usize = 1024;
const QQ_REFRESH_COOKIE_GRACE_PERIOD: Duration = Duration::from_secs(3);
const QQ_LOGIN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);
const QQ_WEB_REFRESH_URL: &str = "https://c.y.qq.com/base/fcgi-bin/login_get_musickey.fcg";

fn has_qq_refresh_cookies(cookies: &BTreeMap<String, String>) -> bool {
    let web_ready = has_any_nonempty(cookies, &["wxopenid"])
        && has_any_nonempty(cookies, &["wxrefresh_token"])
        && has_any_nonempty(cookies, &["qqmusic_key", "qm_keyst", "lqm_keyst"])
        && has_any_nonempty(cookies, &["uin", "wxuin"]);
    if web_ready {
        return true;
    }
    let wechat_key = qq_music_key_is_wechat(cookies);
    has_any_nonempty(cookies, &["uin", "wxuin"])
        && has_any_nonempty(cookies, &["qqmusic_key", "qm_keyst", "lqm_keyst"])
        && has_any_nonempty(cookies, &["psrf_qqopenid", "openid"])
        && (wechat_key || has_any_nonempty(cookies, &["psrf_qqaccess_token", "access_token"]))
        // QQ OAuth 网页登录通常只下发 refresh_token，不保证下发 refresh_key；
        // refresh_key 由移动端续期响应补发，任一存在即可视为可刷新。
        && (has_any_nonempty(cookies, &["psrf_qqrefresh_token", "refresh_token"])
            || has_any_nonempty(cookies, &["psrf_qqrefresh_key", "refresh_key"]))
}

fn qq_music_key_is_wechat(cookies: &BTreeMap<String, String>) -> bool {
    ["qqmusic_key", "qm_keyst", "lqm_keyst"]
        .iter()
        .find_map(|name| cookies.get(*name).filter(|value| !value.trim().is_empty()))
        .is_some_and(|value| {
            value
                .trim()
                .as_bytes()
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"W_X"))
        })
}

fn first_nonempty_qq_cookie<'a>(
    cookies: &'a BTreeMap<String, String>,
    aliases: &[&str],
) -> Option<&'a str> {
    aliases.iter().find_map(|alias| {
        cookies
            .get(*alias)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
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
    // The direct QQ QR page uses the official `graph.qq.com/login_jump`
    // callback and may omit the wrapper's `login_type=1` query parameter.
    // It is unambiguously the QQ OAuth flow, so infer type 1 only for that
    // callback host/path; never infer it from arbitrary page URLs.
    let login_type = login_type.or_else(|| {
        (url.host_str() == Some("graph.qq.com") && url.path().ends_with("/oauth2.0/login_jump"))
            .then(|| "1".to_owned())
    })?;
    Some((login_type, code?))
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
            "comm": {
                "tmeLoginType": 2,
                "g_tk": 5381,
                "platform": "yqq",
                "ct": 24,
                "cv": 0
            },
            "req": {
                "module": "QQConnectLogin.LoginServer",
                "method": "QQLogin",
                "param": {"code": code}
            }
        })),
        "2" => Some(json!({
            "comm": {
                "tmeAppID": "qqmusic",
                "tmeLoginType": 1,
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
    body.as_object()?
        .iter()
        .find_map(|(key, value)| {
            qq_login_envelope_key(key)
                .then(|| value.get("data"))
                .flatten()
                .and_then(Value::as_object)
        })
        .or_else(|| body.get("data").and_then(Value::as_object))
}

fn qq_login_response_code(body: &Value) -> Option<String> {
    for key in ["req", "req_0", "req_1", "req1"] {
        if let Some(value) = body.get(key) {
            for field in ["code", "ret", "retcode"] {
                if let Some(code) = value.get(field) {
                    return Some(value_to_string(code));
                }
            }
        }
    }
    if let Some(code) = body.as_object()?.iter().find_map(|(key, value)| {
        qq_login_envelope_key(key)
            .then(|| {
                ["code", "ret", "retcode"]
                    .iter()
                    .find_map(|field| value.get(*field))
            })
            .flatten()
    }) {
        return Some(value_to_string(code));
    }
    ["code", "ret", "retcode"]
        .iter()
        .find_map(|field| {
            body.get(*field)
                .or_else(|| body.pointer(&format!("/data/{field}")))
        })
        .map(value_to_string)
}

fn qq_login_payload_data(body: &Value) -> Option<&serde_json::Map<String, Value>> {
    qq_login_response_data(body).or_else(|| body.as_object())
}

fn qq_response_string(data: &serde_json::Map<String, Value>, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        data.get(*field)
            .map(value_to_string)
            .filter(|value| !value.trim().is_empty())
    })
}

fn merge_qq_response_alias(
    fields: &mut BTreeMap<String, String>,
    data: &serde_json::Map<String, Value>,
    sources: &[&str],
    aliases: &[&str],
) {
    let Some(value) = qq_response_string(data, sources) else {
        return;
    };
    replace_qq_cookie_alias(fields, aliases, &value);
}

fn qq_login_envelope_key(key: &str) -> bool {
    if matches!(key, "req" | "search") {
        return true;
    }
    let suffix = key.strip_prefix("req_").or_else(|| key.strip_prefix("req"));
    suffix.is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn value_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::trim)
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
        .unwrap_or_default()
}

fn replace_qq_cookie_alias(cookies: &mut BTreeMap<String, String>, aliases: &[&str], value: &str) {
    if aliases.is_empty() || value.is_empty() {
        return;
    }
    let existing = aliases
        .iter()
        .copied()
        .filter(|alias| cookies.contains_key(*alias))
        .collect::<Vec<_>>();
    if existing.is_empty() {
        cookies.insert(aliases[0].to_owned(), value.to_owned());
    } else {
        for alias in existing {
            cookies.insert(alias.to_owned(), value.to_owned());
        }
    }
}

fn normalize_qq_cookie_aliases(cookies: &mut BTreeMap<String, String>) {
    for aliases in [
        ["qqmusic_key", "qm_keyst", "lqm_keyst"].as_slice(),
        ["uin", "wxuin"].as_slice(),
        ["wxopenid", "psrf_qqopenid", "openid"].as_slice(),
        ["wxaccess_token", "psrf_qqaccess_token", "access_token"].as_slice(),
        ["wxrefresh_token", "psrf_qqrefresh_token", "refresh_token"].as_slice(),
        ["psrf_qqrefresh_key", "refresh_key"].as_slice(),
        ["wxunionid", "psrf_qqunionid", "unionid"].as_slice(),
    ] {
        let Some(value) = aliases.iter().find_map(|alias| {
            cookies
                .get(*alias)
                .map(String::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        }) else {
            continue;
        };
        replace_qq_cookie_alias(cookies, aliases, &value);
    }
}

fn merge_qq_cookie_updates(
    cookies: &mut BTreeMap<String, String>,
    updates: &BTreeMap<String, String>,
) {
    cookies.extend(
        updates
            .iter()
            .filter(|(_, value)| !value.trim().is_empty())
            .map(|(name, value)| (name.clone(), value.clone())),
    );
    for aliases in [
        ["qqmusic_key", "qm_keyst", "lqm_keyst"].as_slice(),
        ["uin", "wxuin"].as_slice(),
        ["wxopenid", "psrf_qqopenid", "openid"].as_slice(),
        ["wxaccess_token", "psrf_qqaccess_token", "access_token"].as_slice(),
        ["wxrefresh_token", "psrf_qqrefresh_token", "refresh_token"].as_slice(),
        ["psrf_qqrefresh_key", "refresh_key"].as_slice(),
        ["wxunionid", "psrf_qqunionid", "unionid"].as_slice(),
    ] {
        let Some(value) = aliases.iter().find_map(|alias| {
            updates
                .get(*alias)
                .filter(|value| !value.trim().is_empty())
                .cloned()
        }) else {
            continue;
        };
        replace_qq_cookie_alias(cookies, aliases, &value);
    }
}

fn merge_qq_login_response_fields(cookies: &mut BTreeMap<String, String>, body: &Value) {
    let Some(data) = qq_login_payload_data(body) else {
        return;
    };
    merge_qq_response_alias(
        cookies,
        data,
        &["openid", "openId"],
        &["psrf_qqopenid", "openid", "wxopenid"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["wxopenid", "wxOpenId"],
        &["wxopenid", "psrf_qqopenid", "openid"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["access_token", "accessToken"],
        &["psrf_qqaccess_token", "access_token", "wxaccess_token"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["wxaccess_token", "wxAccessToken"],
        &["wxaccess_token", "psrf_qqaccess_token", "access_token"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["refresh_token", "refreshToken"],
        &["psrf_qqrefresh_token", "refresh_token", "wxrefresh_token"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["wxrefresh_token", "wxRefreshToken"],
        &["wxrefresh_token", "psrf_qqrefresh_token", "refresh_token"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["refresh_key", "refreshKey"],
        &["psrf_qqrefresh_key", "refresh_key"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &[
            "musickey",
            "musicKey",
            "qqmusic_key",
            "qm_keyst",
            "lqm_keyst",
        ],
        &["qqmusic_key", "qm_keyst", "lqm_keyst"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &[
            "musicid",
            "musicuin",
            "musicUin",
            "str_musicid",
            "qqmusic_uin",
        ],
        &["uin", "wxuin"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["unionid", "unionId"],
        &["psrf_qqunionid", "unionid", "wxunionid"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["wxunionid", "wxUnionId"],
        &["wxunionid", "psrf_qqunionid", "unionid"],
    );
    normalize_qq_cookie_aliases(cookies);
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

fn finish_qq_login_exchange<F>(
    login_type: &str,
    cookies: &BTreeMap<String, String>,
    mut fields: BTreeMap<String, String>,
    callback_error: Option<String>,
    refresh_web_session: F,
) -> Result<BTreeMap<String, String>, String>
where
    F: FnOnce(&BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String>,
{
    let mut callback_error = callback_error;
    // 微信网页登录的授权码交换可能已经被页面消费；此时仍可用浏览器会话
    // 刷新出凭据。交换成功且已经返回 access token 时不再额外消耗一次
    // refresh token；只有失败或缺少 token 时才走会话兜底。
    let callback_has_access_token = ["wxaccess_token", "psrf_qqaccess_token", "access_token"]
        .iter()
        .any(|name| {
            fields
                .get(*name)
                .is_some_and(|value| !value.trim().is_empty())
        });
    let callback_has_music_key = ["qqmusic_key", "qm_keyst", "lqm_keyst"].iter().any(|name| {
        fields
            .get(*name)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let callback_has_uin = ["uin", "wxuin"].iter().any(|name| {
        fields
            .get(*name)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let callback_has_openid = ["wxopenid", "psrf_qqopenid", "openid"].iter().any(|name| {
        fields
            .get(*name)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let callback_has_refresh_material = [
        "wxrefresh_token",
        "psrf_qqrefresh_token",
        "refresh_token",
        "psrf_qqrefresh_key",
        "refresh_key",
    ]
    .iter()
    .any(|name| {
        fields
            .get(*name)
            .is_some_and(|value| !value.trim().is_empty())
    });
    if login_type == "2"
        && (callback_error.is_some()
            || !callback_has_access_token
            || !callback_has_music_key
            || !callback_has_uin
            || !callback_has_openid
            || !callback_has_refresh_material)
        && let Ok(web_fields) = refresh_web_session(cookies)
    {
        // A partial callback can contain a new token while the WebView still
        // holds the old key/openid tuple.  Once the browser-session refresh
        // succeeds, its fields are the coherent credential set.  Remove every
        // callback alias from the same credential groups before applying the
        // web result; otherwise a stale `psrf_*` value can survive beside a
        // fresh `wx*` value and the next refresh may combine two sessions.
        for alias in [
            "wxopenid",
            "psrf_qqopenid",
            "openid",
            "wxaccess_token",
            "psrf_qqaccess_token",
            "access_token",
            "wxrefresh_token",
            "psrf_qqrefresh_token",
            "refresh_token",
            "wxrefresh_key",
            "psrf_qqrefresh_key",
            "refresh_key",
            "wxunionid",
            "psrf_qqunionid",
            "unionid",
            "qqmusic_key",
            "qm_keyst",
            "lqm_keyst",
            "uin",
            "wxuin",
        ] {
            fields.remove(alias);
        }
        fields.extend(web_fields);
        normalize_qq_cookie_aliases(&mut fields);
        callback_error = None;
    }
    if let Some(error) = callback_error.take() {
        // A failed OAuth exchange may still return a few fields.  Never merge
        // those untrusted partial values with the browser session: doing so
        // can persist a half credential that fails only after a restart.  The
        // browser cookie snapshot is the trusted fallback for a basic login;
        // the next poll can still capture refresh material that arrives late.
        if !has_required_cookies("qqmusic", cookies) {
            return Err(error);
        }
        fields.clear();
    }
    if !fields.is_empty() {
        // Preserve the WebView flow marker for account-status presentation.
        // The refresh API's numeric `tmeLoginType` is a different contract
        // marker and must not be inferred from this field later.
        fields.insert("login_type".to_owned(), login_type.to_owned());
    }
    Ok(fields)
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
            // 记录接口响应结构（仅字段名，不打印值），用于定位 refresh_key 缺失。
            let data_keys = qq_login_response_data(&response)
                .map(|data| data.keys().cloned().collect::<Vec<_>>().join(","))
                .unwrap_or_else(|| "<no data>".to_owned());
            eprintln!(
                "[qqmusic-oauth] 交换响应 data 字段: {data_keys}; 已合并字段: {}",
                fields.keys().cloned().collect::<Vec<_>>().join(",")
            );
            match qq_login_response_code(&response) {
                Some(code) if code == "0" => None,
                Some(code) if !code.is_empty() => {
                    Some(format!("QQ Music web login returned code {code}"))
                }
                _ => Some("QQ Music web login response has no business code".to_owned()),
            }
        }
        Err(error) => Some(error),
    };
    let fields = finish_qq_login_exchange(
        login_type,
        cookies,
        fields,
        callback_error,
        refresh_qq_web_session,
    )?;
    if login_type == "1" {
        // QQ OAuth 交换只保证 openid/access_token/refresh_token；refresh_key
        // 往往不返回（由移动端续期接口补发）。字段缺失时保留已合并部分并记录诊断。
        let missing = [
            ("uin", ["uin"].as_slice()),
            (
                "musicKey",
                ["qqmusic_key", "qm_keyst", "lqm_keyst"].as_slice(),
            ),
            ("openId", ["psrf_qqopenid", "openid"].as_slice()),
            (
                "accessToken",
                ["psrf_qqaccess_token", "access_token"].as_slice(),
            ),
            (
                "refreshToken/refreshKey",
                [
                    "psrf_qqrefresh_token",
                    "refresh_token",
                    "psrf_qqrefresh_key",
                    "refresh_key",
                ]
                .as_slice(),
            ),
        ]
        .into_iter()
        .filter_map(|(label, aliases)| {
            (!aliases.iter().any(|name| fields.contains_key(*name))).then_some(label)
        })
        .collect::<Vec<_>>();
        if !missing.is_empty() {
            eprintln!(
                "[qqmusic-oauth] QQ OAuth 授权码交换返回字段不完整(保留可用字段): {}",
                missing.join(", ")
            );
        }
    }
    Ok(fields)
}

fn refresh_qq_web_session(
    cookies: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let Some(open_id) = first_nonempty_qq_cookie(cookies, &["wxopenid"]) else {
        return Err("QQ web session has no wxopenid".to_owned());
    };
    let Some(refresh_token) = first_nonempty_qq_cookie(cookies, &["wxrefresh_token"]) else {
        return Err("QQ web session has no wxrefresh_token".to_owned());
    };
    let Some(music_key) =
        first_nonempty_qq_cookie(cookies, &["qqmusic_key", "qm_keyst", "lqm_keyst"])
    else {
        return Err("QQ web session has no music key".to_owned());
    };
    let Some(music_uin) = first_nonempty_qq_cookie(cookies, &["wxuin", "uin"])
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
    let (response_headers, response) = runtime.block_on(async move {
        let response = reqwest::Client::builder()
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
            .map_err(|error| error.to_string())?;
        let headers = response.headers().clone();
        let body = response
            .json::<Value>()
            .await
            .map_err(|error| error.to_string())?;
        Ok::<_, String>((headers, body))
    })?;
    let code = qq_login_response_code(&response).unwrap_or_default();
    if code != "0" {
        return Err("QQ web session refresh was rejected".to_owned());
    }
    let mut fields = BTreeMap::new();
    merge_qq_set_cookie_headers(&mut fields, &response_headers);
    let Some(data) = qq_login_payload_data(&response) else {
        return Err("QQ web session refresh returned no data".to_owned());
    };
    merge_qq_response_alias(
        &mut fields,
        data,
        &["wxaccess_token", "access_token"],
        &["wxaccess_token"],
    );
    merge_qq_response_alias(
        &mut fields,
        data,
        &["musickey", "qqmusic_key", "qm_keyst", "lqm_keyst"],
        &["qqmusic_key", "qm_keyst", "lqm_keyst"],
    );
    merge_qq_response_alias(
        &mut fields,
        data,
        &["musicuin", "musicid", "str_musicid"],
        &["wxuin", "uin"],
    );
    merge_qq_response_alias(
        &mut fields,
        data,
        &["wxrefresh_token", "refresh_token"],
        &["wxrefresh_token"],
    );
    merge_qq_response_alias(&mut fields, data, &["wxopenid", "openid"], &["wxopenid"]);
    merge_qq_response_alias(&mut fields, data, &["wxunionid", "unionid"], &["wxunionid"]);
    normalize_qq_cookie_aliases(&mut fields);
    if let Some(access_token) = ["wxaccess_token", "psrf_qqaccess_token", "access_token"]
        .iter()
        .find_map(|name| fields.get(*name).filter(|value| !value.trim().is_empty()))
    {
        fields.insert("wxaccess_token".to_owned(), access_token.to_owned());
        Ok(fields)
    } else {
        Err("QQ web session refresh returned no access token".to_owned())
    }
}

/// QQ 与 B 站的刷新 cookie 在基础登录 cookie 之后写入。保持短暂轮询，
/// 使成功登录捕获到完整的可刷新凭据。
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
    let refresh_ready = match provider {
        "qqmusic" => has_qq_refresh_cookies(cookies),
        "bilibili" => cookies.contains_key("ac_time_value"),
        _ => true,
    };
    if refresh_ready {
        return true;
    }
    // The Bilibili refresh token is part of the persisted credential, not an
    // optional enhancement. Cookie polling drives a bounded browser probe
    // and reports an explicit capture failure when that probe is exhausted.
    if provider == "bilibili" {
        return false;
    }
    let seen_at = required_seen_at.get_or_insert(now);
    now.duration_since(*seen_at) >= QQ_REFRESH_COOKIE_GRACE_PERIOD
}

#[cfg(windows)]
#[allow(unsafe_op_in_unsafe_fn)]
mod platform {
    use super::{
        CaptureError, KugouRequestInput, allowed_cookie_names, kugou_request_script, login_url,
    };
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

    use serde_json::Value;
    use url::Url;
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
    const IID_SCRIPT_HANDLER: GUID = GUID {
        data1: 0x3d8b2a74,
        data2: 0x9f1e,
        data3: 0x4c6a,
        data4: [0xa5, 0x0b, 0x7c, 0x2e, 0x1d, 0x9f, 0x08, 0x53],
    };
    const IID_WEB_MESSAGE_HANDLER: GUID = GUID {
        data1: 0x57213f19,
        data2: 0x00e6,
        data3: 0x49fa,
        data4: [0x8e, 0x07, 0x89, 0x8e, 0xa0, 0x1e, 0xcb, 0xd2],
    };
    const IID_SCRIPT_INSTALL_HANDLER: GUID = GUID {
        data1: 0xb99369f3,
        data2: 0x9b11,
        data3: 0x47b5,
        data4: [0xbc, 0x6f, 0x8e, 0x78, 0x95, 0xfc, 0xea, 0x17],
    };
    const IID_WEB_RESOURCE_RESPONSE_RECEIVED_HANDLER: GUID = GUID {
        data1: 0x7de9898a,
        data2: 0x24f5,
        data3: 0x40c3,
        data4: [0xa2, 0xde, 0xd4, 0xf4, 0x58, 0xe6, 0x98, 0x28],
    };
    const IID_WEB_RESOURCE_CONTENT_HANDLER: GUID = GUID {
        data1: 0x875738e1,
        data2: 0x9fa2,
        data3: 0x40e3,
        data4: [0x8b, 0x74, 0x2e, 0x89, 0x72, 0xdd, 0x6f, 0xe7],
    };

    type HandlerQueryInterface =
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
    type HandlerAddRef = unsafe extern "system" fn(*mut c_void) -> u32;
    type HandlerRelease = unsafe extern "system" fn(*mut c_void) -> u32;
    type HandlerInvoke = unsafe extern "system" fn(*mut c_void, HRESULT, *mut c_void) -> HRESULT;
    type CookieCapture = (BTreeMap<String, String>, BTreeMap<String, String>);

    #[derive(Clone, Copy)]
    enum ScriptPurpose {
        BilibiliToken,
        WebQr,
        KugouRequest,
    }

    #[derive(Clone, Copy)]
    enum WebResourceResponseKind {
        BilibiliLogin,
        QqQrImage,
    }

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
        js_probe_inflight: bool,
        bilibili_js_probe_attempts: u32,
        bilibili_login_succeeded: bool,
        bilibili_refresh_token: Option<String>,
        web_qr_probe_inflight: bool,
        web_qr_probe_started_at: Option<Instant>,
        web_qr_next_probe: Option<Instant>,
        web_qr_fetch_rx: Option<mpsc::Receiver<Result<String, String>>>,
        web_qr_fetch_source: Option<String>,
        web_qr_last_fetch_at: Option<Instant>,
        web_qr_pending: Option<String>,
        web_qr_published: Option<String>,
        web_qr_failures: u8,
        web_message_token: Option<i64>,
        web_resource_response_token: Option<i64>,
        script_install_started_at: Option<Instant>,
        navigation_started: bool,
        request_script_inflight: bool,
        request_next_attempt: Option<Instant>,
        request_outcome: Option<Result<Value, CaptureError>>,
    }

    struct CaptureContext {
        provider: String,
        url: Vec<u16>,
        hwnd: HWND,
        kugou_request: Option<KugouRequestInput>,
        state: Mutex<CaptureState>,
    }

    impl CaptureContext {
        fn new(provider: &str, hwnd: HWND) -> Result<Self, CaptureError> {
            let url = login_url(provider).ok_or_else(|| {
                CaptureError::Com(format!("unsupported provider login page: {provider}"))
            })?;
            Self::new_with_url(provider, hwnd, url)
        }

        fn new_with_url(provider: &str, hwnd: HWND, url: &str) -> Result<Self, CaptureError> {
            Ok(Self {
                provider: provider.to_owned(),
                url: wide(url),
                hwnd,
                kugou_request: None,
                state: Mutex::new(CaptureState::default()),
            })
        }

        fn new_kugou_request(hwnd: HWND, input: KugouRequestInput) -> Result<Self, CaptureError> {
            // Execute the request from the API origin itself.  The KuGou API
            // response does not consistently grant credentialed CORS from
            // www.kugou.com, while a WebView2 page loaded at the request URL
            // keeps the browser cookie jar and makes the request same-origin.
            let navigation_url = input.url.clone();
            Ok(Self {
                provider: "kugou".to_owned(),
                url: wide(&navigation_url),
                hwnd,
                kugou_request: Some(input),
                state: Mutex::new(CaptureState::default()),
            })
        }

        fn finish_error(&self, error: CaptureError) {
            if let CaptureError::Com(detail) = &error {
                // HRESULTs are safe diagnostics: do not print page URLs or
                // cookie values while still making COM failures actionable.
                eprintln!("[webview2] COM error: {detail}");
            }
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            if state.outcome.is_none() {
                state.outcome = Some(Err(error.clone()));
            }
            if self.kugou_request.is_some() && state.request_outcome.is_none() {
                state.request_outcome = Some(Err(error));
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

        fn take_request_outcome(&self) -> Option<Result<Value, CaptureError>> {
            self.state
                .lock()
                .expect("capture state mutex poisoned")
                .request_outcome
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
            let changed = state.login_callback != previous;
            if changed {
                state.exchange_attempted = false;
            }
        }

        fn remember_navigation_url(&self, raw_url: &str) {
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            let previous = state.login_callback.clone();
            super::remember_qq_login_callback(&mut state.login_callback, raw_url);
            let changed = state.login_callback != previous;
            if changed {
                state.exchange_attempted = false;
            }
        }

        fn accept_bilibili_refresh_token(&self, value: &str, source: &str) {
            let value = value.trim();
            if !super::valid_bilibili_refresh_token(value) {
                super::log_bilibili_capture_diagnostics(&format!(
                    "{source} ignored invalid refresh token length={}",
                    value.len()
                ));
                return;
            }
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            if state.outcome.is_some() {
                return;
            }
            super::log_bilibili_capture_diagnostics(&format!(
                "{source} accepted refresh token length={}",
                value.len()
            ));
            state.bilibili_refresh_token = Some(value.to_owned());
            state
                .latest_cookies
                .insert("ac_time_value".to_owned(), value.to_owned());
            // Re-read the cookie jar so the token is paired with the final
            // browser session before completing the capture.
            state.next_cookie_poll = Some(Instant::now());
        }

        fn note_bilibili_login_response(&self, payload: &Value) {
            if !super::bilibili_login_payload_is_success(payload) {
                return;
            }
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            if !state.bilibili_login_succeeded {
                super::log_bilibili_capture_diagnostics(
                    "Bilibili QR poll reported login success; awaiting refresh token",
                );
            }
            state.bilibili_login_succeeded = true;
            state.next_cookie_poll = Some(Instant::now());
        }

        fn accept_web_qr_image(&self, image: String) {
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            if state.outcome.is_none() {
                state.web_qr_pending = Some(image);
                state.web_qr_failures = 0;
            }
        }

        fn start_initial_navigation(self: &Rc<Self>) {
            let webview = {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                if state.outcome.is_some() || state.navigation_started {
                    return;
                }
                state.navigation_started = true;
                state.webview
            };
            if webview.is_null() {
                self.finish_error(CaptureError::Com(
                    "WebView2 初始化后没有可导航的 WebView".to_owned(),
                ));
                return;
            }
            let navigate: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT =
                unsafe { vtable_fn(webview, 5) };
            let result = unsafe { navigate(webview, self.url.as_ptr()) };
            if result < 0 {
                self.finish_error(com_error("ICoreWebView2::Navigate", result));
            } else if self.kugou_request.is_some() {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                state.request_next_attempt = Some(Instant::now() + Duration::from_millis(900));
            }
        }

        /// Execute the signed songinfo request in the browser origin.  The
        /// page-side script owns the official SSA challenge lifecycle and
        /// returns only the JSON response to the host.
        fn poll_kugou_request(self: &Rc<Self>) {
            let input = match self.kugou_request.as_ref() {
                Some(input) => input,
                None => return,
            };
            let should_start = {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                if state.request_outcome.is_some()
                    || state.request_script_inflight
                    || state.webview.is_null()
                    || !state.navigation_started
                    || state
                        .request_next_attempt
                        .is_some_and(|deadline| Instant::now() < deadline)
                {
                    false
                } else {
                    state.request_script_inflight = true;
                    true
                }
            };
            if !should_start {
                return;
            }
            let webview = {
                let state = self.state.lock().expect("capture state mutex poisoned");
                state.webview
            };
            let handler = Box::into_raw(Box::new(ScriptHandler {
                vtbl: &SCRIPT_HANDLER_VTABLE,
                refs: AtomicU32::new(1),
                context: Rc::clone(self),
                purpose: ScriptPurpose::KugouRequest,
            })) as *mut c_void;
            let execute_script: unsafe extern "system" fn(
                *mut c_void,
                PCWSTR,
                *mut c_void,
            ) -> HRESULT = unsafe { vtable_fn(webview, 29) };
            let script = match kugou_request_script(input) {
                Ok(script) => wide(&script),
                Err(error) => {
                    unsafe { release_com(handler) };
                    self.finish_error(error);
                    return;
                }
            };
            let result = unsafe { execute_script(webview, script.as_ptr(), handler) };
            unsafe { release_com(handler) };
            if result < 0 {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                state.request_script_inflight = false;
                state.request_next_attempt = Some(Instant::now() + Duration::from_millis(250));
            }
        }

        fn ensure_initial_navigation(self: &Rc<Self>) {
            let fallback = {
                let state = self.state.lock().expect("capture state mutex poisoned");
                state.navigation_started
                    || state.outcome.is_some()
                    || !state.script_install_started_at.is_some_and(|started| {
                        started.elapsed() >= super::WEB_QR_SCRIPT_INSTALL_FALLBACK
                    })
            };
            if !fallback {
                // The completion callback is preferred because it guarantees
                // document-created script registration. This watchdog keeps a
                // Runtime callback failure from leaving a permanently white
                // host window.
                self.start_initial_navigation();
            }
        }

        fn handle_web_message(self: &Rc<Self>, source: &str, raw_message: &str) {
            let Some(object) = serde_json::from_str::<Value>(raw_message)
                .ok()
                .and_then(|value| value.as_object().cloned())
            else {
                return;
            };
            if object.get("bridge").and_then(Value::as_str) != Some("__miliastra_login_helper__") {
                return;
            }
            let kind = object
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match kind {
                "kugou_request_result"
                    if self.provider == "kugou"
                        && self.kugou_request.is_some()
                        && super::web_qr_message_source_allowed("kugou", source, "") =>
                {
                    let Some(value) = object.get("value").cloned() else {
                        return;
                    };
                    self.finish_kugou_request_result(value);
                }
                "qq_callback" if self.provider == "qqmusic" => {
                    let Some(callback_url) = object.get("value").and_then(Value::as_str) else {
                        return;
                    };
                    if super::web_qr_message_source_allowed("qqmusic", source, callback_url) {
                        self.remember_navigation_url(callback_url);
                    }
                }
                "qq_event" if self.provider == "qqmusic" => {
                    // The outer OAuth document normally redirects on its own;
                    // force an immediate source check when the iframe reports
                    // qclogin_success so a slow navigation cannot look like a
                    // successful scan followed by a blank window.
                    self.poll_navigation();
                }
                "bilibili_refresh" if self.provider == "bilibili" => {
                    let Some(value) = object.get("value").and_then(Value::as_str) else {
                        return;
                    };
                    let frame_url = object
                        .get("frameUrl")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !super::web_qr_message_source_allowed("bilibili", source, frame_url) {
                        super::log_bilibili_capture_diagnostics(
                            "refresh bridge ignored because its source was outside Bilibili",
                        );
                        return;
                    }
                    self.accept_bilibili_refresh_token(value, "refresh bridge");
                }
                "qr" => {
                    let Some(value) = object.get("value").and_then(Value::as_str) else {
                        return;
                    };
                    let frame_url = object
                        .get("frameUrl")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if !super::web_qr_message_source_allowed(&self.provider, source, frame_url) {
                        return;
                    }
                    if value.starts_with("data:image/") {
                        if let Ok(image) = super::validate_web_qr_data_url(value) {
                            let mut state =
                                self.state.lock().expect("capture state mutex poisoned");
                            if state.outcome.is_none() {
                                state.web_qr_pending = Some(image);
                                state.web_qr_failures = 0;
                            }
                        }
                    } else if let Ok(url) = Url::parse(value)
                        && super::web_qr_url_allowed(&self.provider, &url)
                    {
                        self.start_web_qr_fetch(value.to_owned());
                    }
                }
                _ => {}
            }
        }

        fn finish_kugou_request_result(&self, value: Value) {
            super::log_kugou_request_diagnostics(&value);
            let mut state = self.state.lock().expect("capture state mutex poisoned");
            state.request_script_inflight = false;
            match value {
                value if value.get("retry").and_then(Value::as_bool) == Some(true) => {
                    state.request_next_attempt = Some(Instant::now() + Duration::from_millis(300));
                }
                value if value.get("ok").and_then(Value::as_bool) == Some(true) => {
                    if let Some(body) = value.get("body").cloned() {
                        state.request_outcome = Some(Ok(body));
                    } else {
                        state.request_outcome = Some(Err(CaptureError::Io(
                            "酷狗 Web 请求响应缺少 body".to_owned(),
                        )));
                    }
                }
                value => {
                    let message = value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("酷狗网页风控验证失败");
                    state.request_outcome = Some(Err(CaptureError::Io(message.to_owned())));
                }
            }
        }

        fn start_login_exchange(&self) {
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
                            Some(Err("QQ OAuth 授权码交换超时".to_owned()))
                        } else {
                            None
                        }
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        state.exchange_rx = None;
                        state.exchange_started_at = None;
                        Some(Err("QQ OAuth 授权码交换线程异常结束".to_owned()))
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
                    // 基础 Cookie 仍可用于当前登录，但必须记录 OAuth 交换失败原因，
                    // 避免最终只表现为 refreshKey 缺失而无法定位。
                    eprintln!("[qqmusic-oauth] {error}");
                    state.exchange_attempted = true;
                    state.next_cookie_poll = Some(Instant::now());
                }
                None => {}
            }
        }

        /// Ask the currently loaded provider page for its QR image. The
        /// injected bridge is the primary path; this ExecuteScript probe is a
        /// top-level fallback for runtimes that do not deliver child-frame
        /// messages. All four providers stay inside the same WebView2 flow.
        fn poll_web_qr(self: &Rc<Self>) -> Option<String> {
            if !matches!(
                self.provider.as_str(),
                "qqmusic" | "netease" | "bilibili" | "kugou"
            ) {
                return None;
            }
            let now = Instant::now();
            let mut schedule_probe = false;
            let mut fetched = None;
            {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                if state.outcome.is_some() || state.webview.is_null() {
                    return None;
                }
                if let Some(receiver) = state.web_qr_fetch_rx.as_ref() {
                    match receiver.try_recv() {
                        Ok(result) => {
                            state.web_qr_fetch_rx = None;
                            fetched = Some(result);
                        }
                        Err(mpsc::TryRecvError::Disconnected) => {
                            state.web_qr_fetch_rx = None;
                            fetched = Some(Err("网页登录二维码图片线程已断开".to_owned()));
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                }
                if let Some(result) = fetched.take() {
                    match result {
                        Ok(image) => {
                            state.web_qr_pending = Some(image);
                            state.web_qr_failures = 0;
                        }
                        Err(_) => {
                            state.web_qr_fetch_source = None;
                            state.web_qr_failures = state.web_qr_failures.saturating_add(1);
                            // QR extraction is an auxiliary display path. A
                            // transient image URL/content-type failure must
                            // not abort the credential wait; the DOM bridge
                            // may publish another candidate and cookie polling
                            // must continue until login succeeds or times out.
                            if state.web_qr_failures >= 3 {
                                state.web_qr_failures = 0;
                            }
                            state.web_qr_next_probe = Some(now + super::WEB_QR_PROBE_INTERVAL);
                        }
                    }
                }
                if let Some(image) = state.web_qr_pending.take()
                    && state.web_qr_published.as_ref() != Some(&image)
                {
                    state.web_qr_published = Some(image.clone());
                    return Some(image);
                }
                if state.outcome.is_none() && state.web_qr_fetch_rx.is_none() {
                    if state.web_qr_probe_inflight
                        && state.web_qr_probe_started_at.is_some_and(|started| {
                            started.elapsed() >= super::WEB_QR_SCRIPT_TIMEOUT
                        })
                    {
                        state.web_qr_probe_inflight = false;
                        state.web_qr_probe_started_at = None;
                    }
                    if !state.web_qr_probe_inflight
                        && state
                            .web_qr_next_probe
                            .is_none_or(|deadline| now >= deadline)
                    {
                        state.web_qr_probe_inflight = true;
                        state.web_qr_probe_started_at = Some(now);
                        schedule_probe = true;
                    }
                }
            }
            if schedule_probe {
                self.start_web_qr_probe();
            }
            None
        }

        fn start_web_qr_probe(self: &Rc<Self>) {
            let webview = {
                let state = self.state.lock().expect("capture state mutex poisoned");
                state.webview
            };
            if webview.is_null() {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                state.web_qr_probe_inflight = false;
                state.web_qr_probe_started_at = None;
                state.web_qr_next_probe = Some(Instant::now() + super::WEB_QR_PROBE_INTERVAL);
                return;
            }
            let handler = Box::into_raw(Box::new(ScriptHandler {
                vtbl: &SCRIPT_HANDLER_VTABLE,
                refs: AtomicU32::new(1),
                context: Rc::clone(self),
                purpose: ScriptPurpose::WebQr,
            })) as *mut c_void;
            let execute_script: unsafe extern "system" fn(
                *mut c_void,
                PCWSTR,
                *mut c_void,
            ) -> HRESULT = unsafe { vtable_fn(webview, 29) };
            let script = wide(super::web_qr_probe_script());
            let result = unsafe { execute_script(webview, script.as_ptr(), handler) };
            unsafe { release_com(handler) };
            if result < 0 {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                state.web_qr_probe_inflight = false;
                state.web_qr_probe_started_at = None;
                state.web_qr_next_probe = Some(Instant::now() + super::WEB_QR_PROBE_INTERVAL);
            }
        }

        fn start_web_qr_fetch(self: &Rc<Self>, source: String) {
            // QQ's ptqrshow endpoint creates a fresh ticket for every GET.
            // The WebResourceResponseReceived handler captures the response
            // that the WebView actually rendered, so a second request would
            // publish an unusable QR code.
            if self.provider == "qqmusic" {
                return;
            }
            let (sender, receiver) = mpsc::channel();
            let now = Instant::now();
            {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                let same_source_recently = state.web_qr_fetch_source.as_deref()
                    == Some(source.as_str())
                    && state.web_qr_last_fetch_at.is_some_and(|started| {
                        started.elapsed() < super::WEB_QR_REPEAT_FETCH_INTERVAL
                    });
                if state.web_qr_fetch_rx.is_some() || same_source_recently {
                    state.web_qr_next_probe = Some(Instant::now() + super::WEB_QR_PROBE_INTERVAL);
                    return;
                }
                state.web_qr_fetch_source = Some(source.clone());
                state.web_qr_last_fetch_at = Some(now);
                state.web_qr_fetch_rx = Some(receiver);
            }
            let provider = self.provider.clone();
            thread::spawn(move || {
                let result = super::download_web_qr_image(&provider, &source);
                let _ = sender.send(result);
            });
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
            // Persist only the provider allowlist from the disposable profile cookies.
            let result = unsafe { get_cookies(manager, null(), handler) };
            unsafe { release_com(handler) };
            if result < 0 {
                self.finish_error(com_error("ICoreWebView2CookieManager::GetCookies", result));
            }
        }

        /// B 站把 refresh_token 存在页面 localStorage/sessionStorage（键名
        /// ac_time_value），cookie 里通常拿不到。登录完成后通过 ExecuteScript
        /// 读取：若当前页面不在 bilibili 域则先导航回首页；首页的重新加载会
        /// 触发 B 站前端写入 refresh_token，所以前几次探测交替执行
        /// 导航刷新与 JS 读取，直到拿到值或达到探测次数上限。
        fn start_bilibili_js_probe(self: &Rc<Self>, webview: *mut c_void) {
            if webview.is_null() {
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                state.js_probe_inflight = false;
                state.next_cookie_poll = Some(Instant::now() + super::BILIBILI_JS_PROBE_INTERVAL);
                return;
            }
            let get_source: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT =
                unsafe { vtable_fn(webview, 4) };
            let mut source = null_mut();
            let mut on_bilibili = false;
            if unsafe { get_source(webview, &mut source) } >= 0 && !source.is_null() {
                let source_text = unsafe { read_wide(source) };
                unsafe { CoTaskMemFree(source as *const c_void) };
                on_bilibili = source_text.starts_with("https://www.bilibili.com")
                    || source_text.starts_with("https://bilibili.com");
            }
            let navigate: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT =
                unsafe { vtable_fn(webview, 5) };
            let url = wide("https://www.bilibili.com/");
            // A successful QR scan often leaves the top-level document on
            // passport.bilibili.com. Navigate home once, then wait for its
            // login bootstrap to write ac_time_value before probing again.
            // Repeated immediate reloads used to exhaust every probe before
            // that asynchronous write had a chance to run.
            if !on_bilibili {
                super::log_bilibili_capture_diagnostics(
                    "refresh probe navigating to Bilibili home before storage read",
                );
                let _ = unsafe { navigate(webview, url.as_ptr()) };
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                state.js_probe_inflight = false;
                state.next_cookie_poll = Some(Instant::now() + super::BILIBILI_JS_PROBE_INTERVAL);
                return;
            }
            // ICoreWebView2::ExecuteScript 位于 vtable 槽位 29
            // （0 get_Settings 起第 26 个方法，加 IUnknown 三个槽位）。
            let handler = Box::into_raw(Box::new(ScriptHandler {
                vtbl: &SCRIPT_HANDLER_VTABLE,
                refs: AtomicU32::new(1),
                context: Rc::clone(self),
                purpose: ScriptPurpose::BilibiliToken,
            })) as *mut c_void;
            let execute_script: unsafe extern "system" fn(
                *mut c_void,
                PCWSTR,
                *mut c_void,
            ) -> HRESULT = unsafe { vtable_fn(webview, 29) };
            let script = wide(
                "(localStorage.getItem('ac_time_value')||localStorage.getItem('refresh_token')||localStorage.getItem('refreshToken')||sessionStorage.getItem('ac_time_value')||sessionStorage.getItem('refresh_token')||sessionStorage.getItem('refreshToken'))",
            );
            super::log_bilibili_capture_diagnostics("refresh probe executing storage read");
            let result = unsafe { execute_script(webview, script.as_ptr(), handler) };
            unsafe { release_com(handler) };
            if result < 0 {
                eprintln!("[js-probe] ExecuteScript failed: {result:#x}");
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                state.js_probe_inflight = false;
                state.next_cookie_poll = Some(Instant::now() + super::BILIBILI_JS_PROBE_INTERVAL);
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

    #[repr(C)]
    struct WebMessageHandler {
        vtbl: &'static WebMessageHandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
    }

    #[repr(C)]
    struct ScriptInstallHandler {
        vtbl: &'static ScriptInstallHandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
    }

    #[repr(C)]
    struct WebResourceResponseReceivedHandler {
        vtbl: &'static WebResourceResponseReceivedHandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
    }

    #[repr(C)]
    struct WebResourceContentHandler {
        vtbl: &'static WebResourceContentHandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
        response_kind: WebResourceResponseKind,
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

    type WebMessageInvoke =
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT;

    #[repr(C)]
    struct WebMessageHandlerVtable {
        query_interface: HandlerQueryInterface,
        add_ref: HandlerAddRef,
        release: HandlerRelease,
        invoke: WebMessageInvoke,
    }

    #[repr(C)]
    struct ScriptHandler {
        vtbl: &'static ScriptHandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
        purpose: ScriptPurpose,
    }

    type ScriptInvoke = unsafe extern "system" fn(*mut c_void, HRESULT, *const u16) -> HRESULT;

    #[repr(C)]
    struct ScriptHandlerVtable {
        query_interface: HandlerQueryInterface,
        add_ref: HandlerAddRef,
        release: HandlerRelease,
        invoke: ScriptInvoke,
    }

    type ScriptInstallInvoke =
        unsafe extern "system" fn(*mut c_void, HRESULT, *const u16) -> HRESULT;

    #[repr(C)]
    struct ScriptInstallHandlerVtable {
        query_interface: HandlerQueryInterface,
        add_ref: HandlerAddRef,
        release: HandlerRelease,
        invoke: ScriptInstallInvoke,
    }

    type WebResourceResponseReceivedInvoke =
        unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT;

    #[repr(C)]
    struct WebResourceResponseReceivedHandlerVtable {
        query_interface: HandlerQueryInterface,
        add_ref: HandlerAddRef,
        release: HandlerRelease,
        invoke: WebResourceResponseReceivedInvoke,
    }

    type WebResourceContentInvoke =
        unsafe extern "system" fn(*mut c_void, HRESULT, *mut c_void) -> HRESULT;

    #[repr(C)]
    struct WebResourceContentHandlerVtable {
        query_interface: HandlerQueryInterface,
        add_ref: HandlerAddRef,
        release: HandlerRelease,
        invoke: WebResourceContentInvoke,
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
    static WEB_MESSAGE_HANDLER_VTABLE: WebMessageHandlerVtable = WebMessageHandlerVtable {
        query_interface: web_message_query_interface,
        add_ref: web_message_add_ref,
        release: web_message_release,
        invoke: web_message_invoke,
    };
    static SCRIPT_HANDLER_VTABLE: ScriptHandlerVtable = ScriptHandlerVtable {
        query_interface: script_query_interface,
        add_ref: script_add_ref,
        release: script_release,
        invoke: script_invoke,
    };
    static SCRIPT_INSTALL_HANDLER_VTABLE: ScriptInstallHandlerVtable = ScriptInstallHandlerVtable {
        query_interface: script_install_query_interface,
        add_ref: script_install_add_ref,
        release: script_install_release,
        invoke: script_install_invoke,
    };
    static WEB_RESOURCE_RESPONSE_RECEIVED_HANDLER_VTABLE: WebResourceResponseReceivedHandlerVtable =
        WebResourceResponseReceivedHandlerVtable {
            query_interface: web_resource_response_received_query_interface,
            add_ref: web_resource_response_received_add_ref,
            release: web_resource_response_received_release,
            invoke: web_resource_response_received_invoke,
        };
    static WEB_RESOURCE_CONTENT_HANDLER_VTABLE: WebResourceContentHandlerVtable =
        WebResourceContentHandlerVtable {
            query_interface: web_resource_content_query_interface,
            add_ref: web_resource_content_add_ref,
            release: web_resource_content_release,
            invoke: web_resource_content_invoke,
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

    unsafe extern "system" fn web_message_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(
            this,
            riid,
            out,
            &IID_WEB_MESSAGE_HANDLER,
            web_message_add_ref,
        )
    }

    unsafe extern "system" fn script_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(this, riid, out, &IID_SCRIPT_HANDLER, script_add_ref)
    }

    unsafe extern "system" fn script_install_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(
            this,
            riid,
            out,
            &IID_SCRIPT_INSTALL_HANDLER,
            script_install_add_ref,
        )
    }

    unsafe extern "system" fn web_resource_response_received_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(
            this,
            riid,
            out,
            &IID_WEB_RESOURCE_RESPONSE_RECEIVED_HANDLER,
            web_resource_response_received_add_ref,
        )
    }

    unsafe extern "system" fn web_resource_content_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(
            this,
            riid,
            out,
            &IID_WEB_RESOURCE_CONTENT_HANDLER,
            web_resource_content_add_ref,
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

    unsafe extern "system" fn web_message_add_ref(this: *mut c_void) -> u32 {
        handler_refs(this).fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn script_add_ref(this: *mut c_void) -> u32 {
        handler_refs(this).fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn script_install_add_ref(this: *mut c_void) -> u32 {
        handler_refs(this).fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn web_resource_response_received_add_ref(this: *mut c_void) -> u32 {
        handler_refs(this).fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe extern "system" fn web_resource_content_add_ref(this: *mut c_void) -> u32 {
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

    unsafe extern "system" fn web_message_release(this: *mut c_void) -> u32 {
        release_handler::<WebMessageHandler>(this)
    }

    unsafe extern "system" fn script_release(this: *mut c_void) -> u32 {
        release_handler::<ScriptHandler>(this)
    }

    unsafe extern "system" fn script_install_release(this: *mut c_void) -> u32 {
        release_handler::<ScriptInstallHandler>(this)
    }

    unsafe extern "system" fn web_resource_response_received_release(this: *mut c_void) -> u32 {
        release_handler::<WebResourceResponseReceivedHandler>(this)
    }

    unsafe extern "system" fn web_resource_content_release(this: *mut c_void) -> u32 {
        release_handler::<WebResourceContentHandler>(this)
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
        if hr < 0 || cookie_manager.is_null() {
            release_com(webview2);
            release_com(webview);
            context.finish_error(if hr < 0 {
                com_error("ICoreWebView2_2::get_CookieManager", hr)
            } else {
                CaptureError::Com("get_CookieManager returned a null manager".to_owned())
            });
            return S_OK;
        }

        if let Some(input) = context.kugou_request.as_ref() {
            if let Err(error) = set_kugou_request_cookies(cookie_manager, &input.cookies) {
                release_com(cookie_manager);
                release_com(webview2);
                release_com(webview);
                context.finish_error(error);
                return S_OK;
            }
        }

        // WebMessageReceived is enabled by default, but set it explicitly so
        // the bridge remains correct with enterprise Runtime policies.
        let get_settings: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT =
            vtable_fn(webview, 3);
        let mut settings = null_mut();
        let hr = get_settings(webview, &mut settings);
        if hr < 0 || settings.is_null() {
            release_com(cookie_manager);
            release_com(webview2);
            release_com(webview);
            context.finish_error(if hr < 0 {
                com_error("ICoreWebView2::get_Settings", hr)
            } else {
                CaptureError::Com("get_Settings returned a null settings object".to_owned())
            });
            return S_OK;
        }
        let put_web_message: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT =
            vtable_fn(settings, 6);
        let hr = put_web_message(settings, 1);
        release_com(settings);
        if hr < 0 {
            release_com(cookie_manager);
            release_com(webview2);
            release_com(webview);
            context.finish_error(com_error(
                "ICoreWebView2Settings::put_IsWebMessageEnabled",
                hr,
            ));
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
            release_com(webview2);
            release_com(webview);
            context.finish_error(com_error("ICoreWebView2::add_NavigationStarting", hr));
            return S_OK;
        }

        let web_message_handler = Box::into_raw(Box::new(WebMessageHandler {
            vtbl: &WEB_MESSAGE_HANDLER_VTABLE,
            refs: AtomicU32::new(1),
            context: Rc::clone(&context),
        })) as *mut c_void;
        let add_web_message: unsafe extern "system" fn(
            *mut c_void,
            *mut c_void,
            *mut i64,
        ) -> HRESULT = vtable_fn(webview, 34);
        let mut web_message_token = 0i64;
        let hr = add_web_message(webview, web_message_handler, &mut web_message_token);
        release_com(web_message_handler);
        if hr < 0 {
            release_com(cookie_manager);
            release_com(webview2);
            release_com(webview);
            context.finish_error(com_error("ICoreWebView2::add_WebMessageReceived", hr));
            return S_OK;
        }

        let put_visible: unsafe extern "system" fn(*mut c_void, i32) -> HRESULT =
            vtable_fn(controller, 4);
        let _ = put_visible(controller, 1);

        {
            let mut state = context.state.lock().expect("capture state mutex poisoned");
            state.controller = controller;
            state.webview = webview;
            state.cookie_manager = cookie_manager;
            state.web_message_token = Some(web_message_token);
            state.script_install_started_at = Some(Instant::now());
            state.next_cookie_poll = Some(Instant::now());
        }

        let web_resource_response_token =
            if matches!(context.provider.as_str(), "bilibili" | "qqmusic") {
                let response_handler = Box::into_raw(Box::new(WebResourceResponseReceivedHandler {
                    vtbl: &WEB_RESOURCE_RESPONSE_RECEIVED_HANDLER_VTABLE,
                    refs: AtomicU32::new(1),
                    context: Rc::clone(&context),
                })) as *mut c_void;
                // ICoreWebView2_2::add_WebResourceResponseReceived.
                let add_response: unsafe extern "system" fn(
                    *mut c_void,
                    *mut c_void,
                    *mut i64,
                ) -> HRESULT = vtable_fn(webview2, 61);
                let mut token = 0i64;
                let hr = add_response(webview2, response_handler, &mut token);
                release_com(response_handler);
                if hr < 0 {
                    release_com(webview2);
                    context.finish_error(com_error(
                        "ICoreWebView2_2::add_WebResourceResponseReceived",
                        hr,
                    ));
                    return S_OK;
                }
                Some(token)
            } else {
                None
            };
        {
            let mut state = context.state.lock().expect("capture state mutex poisoned");
            state.web_resource_response_token = web_resource_response_token;
        }
        release_com(webview2);

        // The completion callback is the only place that starts the first
        // navigation. WebView2 otherwise may miss the document-created script
        // on the initial page, especially for QQ's cross-origin iframe.
        let script_handler = Box::into_raw(Box::new(ScriptInstallHandler {
            vtbl: &SCRIPT_INSTALL_HANDLER_VTABLE,
            refs: AtomicU32::new(1),
            context: Rc::clone(&context),
        })) as *mut c_void;
        let add_script: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> HRESULT =
            vtable_fn(webview, 27);
        let script = wide(super::web_qr_bridge_script());
        let hr = add_script(webview, script.as_ptr(), script_handler);
        release_com(script_handler);
        if hr < 0 {
            context.finish_error(com_error(
                "ICoreWebView2::AddScriptToExecuteOnDocumentCreated",
                hr,
            ));
            return S_OK;
        }
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
        if context.provider == "qqmusic" {
            super::merge_qq_cookie_updates(&mut state.latest_cookies, &exchange_cookies);
            super::normalize_qq_cookie_aliases(&mut state.latest_cookies);
        } else {
            state.latest_cookies.extend(exchange_cookies);
        }
        if context.provider == "bilibili"
            && let Some(refresh_token) = state.bilibili_refresh_token.clone()
        {
            // GetCookies returns only browser cookies. Keep the refresh token
            // received from the bounded page bridge across later polls.
            state
                .latest_cookies
                .insert("ac_time_value".to_owned(), refresh_token);
        }
        let cookies = state.latest_cookies.clone();
        let now = Instant::now();
        let exchange_pending = state.exchange_rx.is_some();
        let bilibili_needs_refresh = !exchange_pending
            && context.provider == "bilibili"
            && state.bilibili_login_succeeded
            && super::has_required_cookies("bilibili", &cookies)
            && !cookies.contains_key("ac_time_value");
        if bilibili_needs_refresh {
            super::log_bilibili_capture_diagnostics(&format!(
                "required cookies seen; refresh missing; probe attempt={}",
                state.bilibili_js_probe_attempts
            ));
            if state.bilibili_js_probe_attempts >= super::BILIBILI_JS_PROBE_ATTEMPTS {
                super::log_bilibili_capture_diagnostics("refresh probe limit exhausted");
                state.outcome = Some(Err(CaptureError::Io(
                    "B站登录成功后未能捕获 refreshToken".to_owned(),
                )));
            } else if !state.js_probe_inflight {
                state.bilibili_js_probe_attempts += 1;
                state.js_probe_inflight = true;
                let webview = state.webview;
                drop(state);
                context.start_bilibili_js_probe(webview);
            } else {
                state.next_cookie_poll = Some(now + super::BILIBILI_JS_PROBE_INTERVAL);
            }
            return S_OK;
        }
        let complete = !exchange_pending
            && super::should_complete_capture(
                &context.provider,
                &cookies,
                &mut state.required_cookies_seen_at,
                now,
            );
        if complete {
            state.outcome = Some(Ok(cookies));
        } else {
            state.next_cookie_poll = Some(now + super::COOKIE_POLL_INTERVAL);
        }
        drop(state);
        context.start_login_exchange();
        S_OK
    }

    unsafe extern "system" fn script_invoke(
        this: *mut c_void,
        result: HRESULT,
        json_result: *const u16,
    ) -> HRESULT {
        let handler = &*(this as *mut ScriptHandler);
        let context = handler.context.clone();
        match handler.purpose {
            ScriptPurpose::BilibiliToken => {
                let mut state = context.state.lock().expect("capture state mutex poisoned");
                state.js_probe_inflight = false;
                state.next_cookie_poll = Some(Instant::now() + super::BILIBILI_JS_PROBE_INTERVAL);
                let mut storage_value_len = None;
                let mut accepted = false;
                if result >= 0 && !json_result.is_null() {
                    let text = unsafe { read_wide(json_result) };
                    // ExecuteScript 返回 JSON 编码：null 或带引号的字符串。
                    if let Ok(Some(value)) = serde_json::from_str::<Option<String>>(&text) {
                        storage_value_len = Some(value.len());
                        if super::valid_bilibili_refresh_token(&value) {
                            let value = value.trim().to_owned();
                            accepted = true;
                            state.bilibili_refresh_token = Some(value.clone());
                            state
                                .latest_cookies
                                .insert("ac_time_value".to_owned(), value);
                            if super::has_required_cookies("bilibili", &state.latest_cookies) {
                                state.outcome = Some(Ok(state.latest_cookies.clone()));
                            } else {
                                state.next_cookie_poll = Some(Instant::now());
                            }
                        }
                    }
                }
                super::log_bilibili_capture_diagnostics(&format!(
                    "storage probe callback result={result} value_length={:?} accepted={accepted}",
                    storage_value_len
                ));
            }
            ScriptPurpose::WebQr => {
                let parsed = if result >= 0 && !json_result.is_null() {
                    let text = unsafe { read_wide(json_result) };
                    super::parse_web_qr_probe_result(&text)
                } else {
                    None
                };
                {
                    let mut state = context.state.lock().expect("capture state mutex poisoned");
                    state.web_qr_probe_inflight = false;
                    state.web_qr_probe_started_at = None;
                    state.web_qr_next_probe = Some(Instant::now() + super::WEB_QR_PROBE_INTERVAL);
                }
                let Some((_, source)) = parsed else {
                    return S_OK;
                };
                if source.starts_with("data:image/") {
                    match super::validate_web_qr_data_url(&source) {
                        Ok(image) => {
                            let mut state =
                                context.state.lock().expect("capture state mutex poisoned");
                            state.web_qr_pending = Some(image);
                            state.web_qr_failures = 0;
                        }
                        Err(_) => {
                            let mut state =
                                context.state.lock().expect("capture state mutex poisoned");
                            state.web_qr_failures = state.web_qr_failures.saturating_add(1);
                            if state.web_qr_failures >= 3 {
                                state.web_qr_failures = 0;
                            }
                            state.web_qr_next_probe =
                                Some(Instant::now() + super::WEB_QR_PROBE_INTERVAL);
                        }
                    }
                } else {
                    context.start_web_qr_fetch(source);
                }
            }
            ScriptPurpose::KugouRequest => {
                if result < 0 || json_result.is_null() {
                    let mut state = context.state.lock().expect("capture state mutex poisoned");
                    state.request_script_inflight = false;
                    state.request_next_attempt = Some(Instant::now() + Duration::from_millis(250));
                } else {
                    let text = unsafe { read_wide(json_result) };
                    super::log_kugou_script_result_shape(
                        &text,
                        super::parse_execute_script_value(&text).as_ref(),
                    );
                }
            }
        }
        S_OK
    }

    unsafe extern "system" fn web_resource_response_received_invoke(
        this: *mut c_void,
        _sender: *mut c_void,
        args: *mut c_void,
    ) -> HRESULT {
        if args.is_null() {
            return S_OK;
        }
        let handler = &*(this as *mut WebResourceResponseReceivedHandler);
        if !matches!(handler.context.provider.as_str(), "bilibili" | "qqmusic") {
            return S_OK;
        }
        let context = handler.context.clone();
        let get_request: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT =
            vtable_fn(args, 3);
        let mut request = null_mut();
        if unsafe { get_request(args, &mut request) } < 0 || request.is_null() {
            return S_OK;
        }
        let get_uri: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT =
            vtable_fn(request, 3);
        let mut uri = null_mut();
        let request_url = if unsafe { get_uri(request, &mut uri) } >= 0 && !uri.is_null() {
            let value = unsafe { read_wide(uri) };
            unsafe { CoTaskMemFree(uri as *const c_void) };
            value
        } else {
            String::new()
        };
        unsafe { release_com(request) };
        let response_kind = match handler.context.provider.as_str() {
            "bilibili" if super::bilibili_login_endpoint_url(&request_url) => {
                super::log_bilibili_capture_diagnostics(
                    "WebView2 response received for login endpoint",
                );
                WebResourceResponseKind::BilibiliLogin
            }
            "qqmusic" if super::qq_web_qr_response_url(&request_url) => {
                super::log_qq_qr_capture_diagnostics(
                    "WebView2 response received for rendered QR image",
                );
                WebResourceResponseKind::QqQrImage
            }
            _ => return S_OK,
        };

        let get_response: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT =
            vtable_fn(args, 4);
        let mut response = null_mut();
        if unsafe { get_response(args, &mut response) } < 0 || response.is_null() {
            return S_OK;
        }
        let content_handler = Box::into_raw(Box::new(WebResourceContentHandler {
            vtbl: &WEB_RESOURCE_CONTENT_HANDLER_VTABLE,
            refs: AtomicU32::new(1),
            context,
            response_kind,
        })) as *mut c_void;
        let get_content: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT =
            vtable_fn(response, 6);
        let result = unsafe { get_content(response, content_handler) };
        unsafe { release_com(content_handler) };
        unsafe { release_com(response) };
        if result < 0 {
            match response_kind {
                WebResourceResponseKind::BilibiliLogin => {
                    super::log_bilibili_capture_diagnostics(
                        "WebView2 response content read failed",
                    );
                }
                WebResourceResponseKind::QqQrImage => {
                    super::log_qq_qr_capture_diagnostics("WebView2 QR image content read failed");
                }
            }
        }
        S_OK
    }

    unsafe extern "system" fn web_resource_content_invoke(
        this: *mut c_void,
        result: HRESULT,
        stream: *mut c_void,
    ) -> HRESULT {
        let handler = &*(this as *mut WebResourceContentHandler);
        match handler.response_kind {
            WebResourceResponseKind::BilibiliLogin => {
                if result < 0 || stream.is_null() {
                    super::log_bilibili_capture_diagnostics(
                        "WebView2 login response returned no readable content",
                    );
                    return S_OK;
                }
                let Some(text) = (unsafe { read_response_stream(stream) }) else {
                    super::log_bilibili_capture_diagnostics(
                        "WebView2 login response stream could not be decoded",
                    );
                    return S_OK;
                };
                let Some(payload) = super::parse_bilibili_login_payload(&text) else {
                    super::log_bilibili_capture_diagnostics(
                        "WebView2 login response JSON was invalid",
                    );
                    return S_OK;
                };
                super::log_bilibili_login_payload_shape(&payload);
                handler.context.note_bilibili_login_response(&payload);
                let Some(token) = super::bilibili_refresh_token_from_payload(&payload) else {
                    super::log_bilibili_capture_diagnostics(
                        "WebView2 login response had no refresh token",
                    );
                    return S_OK;
                };
                handler
                    .context
                    .accept_bilibili_refresh_token(&token, "WebView2 response");
            }
            WebResourceResponseKind::QqQrImage => {
                if result < 0 || stream.is_null() {
                    super::log_qq_qr_capture_diagnostics(
                        "WebView2 QR image returned no readable content",
                    );
                    return S_OK;
                }
                let Some(bytes) = (unsafe {
                    read_response_bytes(stream, miliastra_login_protocol::MAX_QR_IMAGE_BYTES)
                }) else {
                    super::log_qq_qr_capture_diagnostics(
                        "WebView2 QR image stream could not be read",
                    );
                    return S_OK;
                };
                let byte_len = bytes.len();
                let Ok(image) = super::web_qr_image_data_url_from_bytes(&bytes) else {
                    super::log_qq_qr_capture_diagnostics(&format!(
                        "WebView2 QR image had an invalid format bytes={byte_len}",
                    ));
                    return S_OK;
                };
                super::log_qq_qr_capture_diagnostics(&format!(
                    "WebView2 QR image accepted bytes={byte_len}",
                ));
                handler.context.accept_web_qr_image(image);
            }
        }
        S_OK
    }

    unsafe extern "system" fn script_install_invoke(
        this: *mut c_void,
        result: HRESULT,
        _script_id: *const u16,
    ) -> HRESULT {
        let context = (&*(this as *mut ScriptInstallHandler)).context.clone();
        if result < 0 {
            context.finish_error(com_error(
                "ICoreWebView2::AddScriptToExecuteOnDocumentCreated",
                result,
            ));
        } else {
            context.start_initial_navigation();
        }
        S_OK
    }

    unsafe extern "system" fn web_message_invoke(
        this: *mut c_void,
        _sender: *mut c_void,
        args: *mut c_void,
    ) -> HRESULT {
        if args.is_null() {
            return S_OK;
        }
        let context = (&*(this as *mut WebMessageHandler)).context.clone();
        let get_source: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT =
            vtable_fn(args, 3);
        let get_message: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT =
            vtable_fn(args, 4);
        let mut source = null_mut();
        let mut message = null_mut();
        let source_hr = get_source(args, &mut source);
        let message_hr = get_message(args, &mut message);
        let source_text = if source_hr >= 0 && !source.is_null() {
            let value = read_wide(source);
            CoTaskMemFree(source as *const c_void);
            value
        } else {
            String::new()
        };
        let message_text = if message_hr >= 0 && !message.is_null() {
            let value = read_wide(message);
            CoTaskMemFree(message as *const c_void);
            value
        } else {
            String::new()
        };
        if source_hr >= 0 && message_hr >= 0 {
            context.handle_web_message(&source_text, &message_text);
        }
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

    /// Seed the WebView2 cookie jar before navigating to the page origin.  The
    /// request bridge deliberately uses browser cookies rather than an HTTP
    /// client cookie header so SameSite, origin and challenge state follow the
    /// normal web flow.
    unsafe fn set_kugou_request_cookies(
        manager: *mut c_void,
        cookies: &BTreeMap<String, String>,
    ) -> Result<(), CaptureError> {
        let create_cookie: unsafe extern "system" fn(
            *mut c_void,
            PCWSTR,
            PCWSTR,
            PCWSTR,
            PCWSTR,
            *mut *mut c_void,
        ) -> HRESULT = vtable_fn(manager, 3);
        let add_or_update: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT =
            vtable_fn(manager, 6);
        let domain = wide(".kugou.com");
        let path = wide("/");
        for (name, value) in cookies {
            let name_wide = wide(name);
            let value_wide = wide(value);
            let mut cookie = null_mut();
            let hr = create_cookie(
                manager,
                name_wide.as_ptr(),
                value_wide.as_ptr(),
                domain.as_ptr(),
                path.as_ptr(),
                &mut cookie,
            );
            if hr < 0 || cookie.is_null() {
                return Err(if hr < 0 {
                    com_error("ICoreWebView2CookieManager::CreateCookie", hr)
                } else {
                    CaptureError::Com("CreateCookie returned a null cookie".to_owned())
                });
            }
            let hr = add_or_update(manager, cookie);
            release_com(cookie);
            if hr < 0 {
                return Err(com_error(
                    "ICoreWebView2CookieManager::AddOrUpdateCookie",
                    hr,
                ));
            }
        }
        Ok(())
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

    unsafe fn read_response_bytes(
        stream: *mut c_void,
        max_response_bytes: usize,
    ) -> Option<Vec<u8>> {
        const CHUNK_SIZE: usize = 4096;
        if stream.is_null() || max_response_bytes == 0 {
            return None;
        }
        let read: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *mut u32) -> HRESULT =
            vtable_fn(stream, 3);
        let mut bytes = Vec::with_capacity(CHUNK_SIZE.min(max_response_bytes));
        let mut chunk = [0u8; CHUNK_SIZE];
        loop {
            let mut count = 0u32;
            let hr = read(
                stream,
                chunk.as_mut_ptr() as *mut c_void,
                CHUNK_SIZE as u32,
                &mut count,
            );
            if hr < 0 {
                return None;
            }
            let count = count as usize;
            if count > CHUNK_SIZE || bytes.len().checked_add(count)? > max_response_bytes {
                return None;
            }
            bytes.extend_from_slice(&chunk[..count]);
            if count == 0 {
                break;
            }
        }
        Some(bytes)
    }

    unsafe fn read_response_stream(stream: *mut c_void) -> Option<String> {
        const MAX_RESPONSE_BYTES: usize = 64 * 1024;
        String::from_utf8(unsafe { read_response_bytes(stream, MAX_RESPONSE_BYTES) }?).ok()
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
        publish_qr_code: &mut dyn FnMut(&str) -> Result<(), CaptureError>,
    ) -> Result<BTreeMap<String, String>, CaptureError> {
        set_process_dpi_awareness();
        // Include QR creation and WebView2 initialization in the same absolute
        // deadline used by the polling loop. This prevents a slow provider
        // endpoint from extending the advertised login timeout.
        let deadline = Instant::now() + timeout;
        let _runtime = runtime_executable().ok_or(CaptureError::RuntimeMissing)?;
        let _com = ComApartment::initialize()?;
        let window = HostWindow::create()?;
        // Every provider uses its official page, DOM bridge, and CookieManager
        // in the same WebView2 profile. No provider-specific native API is
        // started before or alongside this flow.
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

        let outcome = loop {
            pump_messages(&context);
            context.ensure_initial_navigation();
            context.poll_navigation();
            context.poll_login_exchange();
            if let Some(image) = context.poll_web_qr()
                && let Err(error) = publish_qr_code(&image)
            {
                // Convert callback failures into the normal outcome path so
                // COM interfaces and the WebView2 profile are released.
                context.finish_error(error);
            }
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

    pub fn request_kugou_json(
        profile: &Path,
        timeout: Duration,
        input: KugouRequestInput,
    ) -> Result<Value, CaptureError> {
        set_process_dpi_awareness();
        let deadline = Instant::now() + timeout;
        let _runtime = runtime_executable().ok_or(CaptureError::RuntimeMissing)?;
        let _com = ComApartment::initialize()?;
        let window = HostWindow::create()?;
        let context = Rc::new(CaptureContext::new_kugou_request(window.hwnd, input)?);
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

        let outcome = loop {
            pump_messages(&context);
            context.ensure_initial_navigation();
            context.poll_kugou_request();
            if let Some(outcome) = context.take_request_outcome() {
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
                context.finish_error(CaptureError::Cancelled);
                break;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn release_context_interfaces(context: &CaptureContext) {
        let (
            environment,
            cookie_manager,
            webview,
            controller,
            web_message_token,
            web_resource_response_token,
        ) = {
            let mut state = context.state.lock().expect("capture state mutex poisoned");
            let interfaces = (
                state.environment,
                state.cookie_manager,
                state.webview,
                state.controller,
                state.web_message_token,
                state.web_resource_response_token,
            );
            state.environment = null_mut();
            state.cookie_manager = null_mut();
            state.webview = null_mut();
            state.controller = null_mut();
            state.web_message_token = None;
            state.web_resource_response_token = None;
            interfaces
        };
        unsafe {
            if !webview.is_null() {
                if let Some(token) = web_resource_response_token {
                    // remove_WebResourceResponseReceived lives on
                    // ICoreWebView2_2, so reacquire that interface before
                    // releasing the base WebView2 object.
                    let query: unsafe extern "system" fn(
                        *mut c_void,
                        *const GUID,
                        *mut *mut c_void,
                    ) -> HRESULT = vtable_fn(webview, 0);
                    let mut webview2 = null_mut();
                    if query(webview, &IID_WEBVIEW2, &mut webview2) >= 0 && !webview2.is_null() {
                        let remove_response: unsafe extern "system" fn(
                            *mut c_void,
                            i64,
                        )
                            -> HRESULT = vtable_fn(webview2, 62);
                        let _ = remove_response(webview2, token);
                        release_com(webview2);
                    }
                }
                if let Some(token) = web_message_token {
                    let remove_web_message: unsafe extern "system" fn(*mut c_void, i64) -> HRESULT =
                        vtable_fn(webview, 35);
                    let _ = remove_web_message(webview, token);
                }
            }
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

    #[cfg(test)]
    mod tests {
        use super::CaptureContext;

        #[test]
        fn qq_navigation_callback_waits_for_browser_cookies_before_exchange() {
            let context =
                CaptureContext::new_with_url("qqmusic", std::ptr::null_mut(), "about:blank")
                    .expect("build test capture context");

            context.remember_navigation_url(
                "https://y.qq.com/portal/profile.html?login_type=2&code=callback-code",
            );
            {
                let state = context.state.lock().expect("capture state mutex poisoned");
                assert_eq!(
                    state
                        .login_callback
                        .as_ref()
                        .map(|(login_type, code)| (login_type.as_str(), code.as_str())),
                    Some(("2", "callback-code"))
                );
                assert!(!state.exchange_attempted);
                assert!(state.exchange_rx.is_none());
            }

            context.start_login_exchange();

            let state = context.state.lock().expect("capture state mutex poisoned");
            assert!(!state.exchange_attempted);
            assert!(state.exchange_rx.is_none());
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{CaptureError, KugouRequestInput};
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::Duration;

    pub fn capture(
        _provider: &str,
        _profile: &Path,
        _timeout: Duration,
        _publish_qr_code: &mut dyn FnMut(&str) -> Result<(), CaptureError>,
    ) -> Result<BTreeMap<String, String>, CaptureError> {
        Err(CaptureError::RuntimeMissing)
    }

    pub fn request_kugou_json(
        _profile: &Path,
        _timeout: Duration,
        _input: KugouRequestInput,
    ) -> Result<serde_json::Value, CaptureError> {
        Err(CaptureError::RuntimeMissing)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureError, KugouRequestInput, QQ_REFRESH_COOKIE_GRACE_PERIOD, allowed_cookie_names,
        bilibili_login_endpoint_url, bilibili_login_payload_is_success,
        bilibili_refresh_token_from_payload, finish_qq_login_exchange, has_qq_refresh_cookies,
        has_required_cookies, kugou_request_script, login_url, merge_qq_cookie_updates,
        merge_qq_login_response_fields, normalize_qq_cookie_aliases, parse_bilibili_login_payload,
        parse_execute_script_value, parse_qq_login_callback, parse_web_qr_probe_result,
        qq_login_response_code, qq_login_response_data, qq_web_qr_response_url,
        should_complete_capture, valid_bilibili_refresh_token, validate_kugou_request_url,
        web_qr_bridge_script, web_qr_host_allowed, web_qr_image_data_url_from_bytes,
        web_qr_probe_script, web_qr_url_allowed,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    #[test]
    fn login_pages_are_provider_specific() {
        assert_eq!(
            login_url("qqmusic"),
            Some(
                "https://graph.qq.com/oauth2.0/authorize?response_type=code&client_id=100497308&redirect_uri=https%3A%2F%2Fy.qq.com%2Fportal%2Fwx_redirect.html%3Flogin_type%3D1%26surl%3Dhttps%253A%252F%252Fy.qq.com%252Fn%252Fryqq_v2%252Fprofile&state=state&display=pc&scope=get_user_info%2Cget_app_friends",
            )
        );
        assert_eq!(login_url("netease"), Some("https://music.163.com/"));
        assert!(
            login_url("kugou")
                .is_some_and(|url| { url.starts_with("https://login-user.kugou.com/login/") })
        );
        assert_eq!(login_url("bilibili"), Some("https://www.bilibili.com/"));
        assert_eq!(login_url("kuwo"), None);
    }

    #[test]
    fn kugou_request_url_validation_is_https_and_host_scoped() {
        assert!(
            validate_kugou_request_url("https://wwwapi.kugou.com/play/songinfo?signature=abc")
                .is_ok()
        );
        assert!(
            validate_kugou_request_url("https://wwwapiretry.kugou.com/play/songinfo?signature=abc")
                .is_ok()
        );
        assert!(
            validate_kugou_request_url("https://kugouvip.kugou.com/v1/get_union_vip?userid=1")
                .is_ok()
        );
        for url in [
            "http://wwwapi.kugou.com/play/songinfo",
            "https://wwwapi.kugou.com/v1/get_union_vip",
            "https://wwwapi.kugou.com.evil.example/play/songinfo",
            "https://evil.example/play/songinfo",
            "https://kugou.com/play/songinfo",
            "https://kugouvip.kugou.com/v1/other",
        ] {
            assert!(matches!(
                validate_kugou_request_url(url),
                Err(CaptureError::Io(_))
            ));
        }
    }

    #[test]
    fn kugou_request_script_uses_official_challenge_and_retry_flow() {
        let script = kugou_request_script(&KugouRequestInput {
            url: "https://wwwapi.kugou.com/play/songinfo?signature=abc".to_owned(),
            userid: "123".to_owned(),
            mid: "mid-value".to_owned(),
            cookies: BTreeMap::from([("KuGoo".to_owned(), "session".to_owned())]),
        })
        .unwrap();
        assert!(script.contains("verifyObject.init(1014"));
        assert!(script.contains("20028"));
        assert!(script.contains("30020"));
        assert!(script.contains("SSA-CODE"));
        assert!(script.contains("wwwapiretry.kugou.com"));
        assert!(script.contains("credentials: 'include'"));
        assert!(script.contains("kugou_request_result"));
        assert!(script.contains("window.chrome.webview.postMessage"));
        assert!(script.contains("return true;"));
        assert!(!script.contains("session"));
    }

    #[test]
    fn web_qr_probe_parser_accepts_runtime_object_and_quoted_object() {
        let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==";
        let raw = format!(r##"{{"kind":"image","value":"{image}"}}"##);
        assert_eq!(
            parse_web_qr_probe_result(&raw),
            Some(("image".to_owned(), image.to_owned()))
        );
        let quoted = serde_json::to_string(&raw).unwrap();
        assert_eq!(
            parse_web_qr_probe_result(&quoted).map(|(_, value)| value),
            Some(image.to_owned())
        );
        assert_eq!(parse_web_qr_probe_result(r#"{"kind":"wait"}"#), None);
    }

    #[test]
    fn bilibili_login_parser_reads_refresh_token_from_api_data() {
        let payload = parse_bilibili_login_payload(
            r#"{"code":0,"data":{"refresh_token":"refresh-token-test"}}"#,
        )
        .expect("Bilibili JSON response should parse");
        assert_eq!(
            bilibili_refresh_token_from_payload(&payload).as_deref(),
            Some("refresh-token-test")
        );
        assert!(valid_bilibili_refresh_token("refresh-token-test"));
    }

    #[test]
    fn bilibili_login_success_is_distinguished_from_qr_waiting() {
        let waiting =
            parse_bilibili_login_payload(r#"{"code":0,"data":{"code":86101,"message":"未扫码"}}"#)
                .expect("waiting response should parse");
        let success = parse_bilibili_login_payload(
            r#"{"code":0,"data":{"code":0,"refresh_token":"refresh-token-test"}}"#,
        )
        .expect("success response should parse");
        assert!(!bilibili_login_payload_is_success(&waiting));
        assert!(bilibili_login_payload_is_success(&success));
    }

    #[test]
    fn bilibili_login_endpoint_is_scoped_to_official_login_paths() {
        assert!(bilibili_login_endpoint_url(
            "https://passport.bilibili.com/x/passport-login/web/qrcode/poll"
        ));
        assert!(bilibili_login_endpoint_url(
            "https://passport.bilibili.com/x/passport-login/web/cookie/refresh"
        ));
        assert!(!bilibili_login_endpoint_url(
            "https://passport.bilibili.com/x/passport-login/web/qrcode/generate"
        ));
        assert!(!bilibili_login_endpoint_url(
            "https://evil.example/x/passport-login/web/qrcode/poll"
        ));
    }

    #[test]
    fn qq_qr_response_is_scoped_to_official_ptqrshow() {
        assert!(qq_web_qr_response_url(
            "https://xui.ptlogin2.qq.com/ssl/ptqrshow?appid=100497308"
        ));
        assert!(qq_web_qr_response_url(
            "https://ptlogin2.qq.com/ptqrshow?appid=100497308"
        ));
        assert!(!qq_web_qr_response_url(
            "https://xui.ptlogin2.qq.com/ssl/logo.png"
        ));
        assert!(!qq_web_qr_response_url(
            "https://evil.example/ssl/ptqrshow?appid=100497308"
        ));
    }

    #[test]
    fn web_qr_image_response_accepts_only_supported_image_bytes() {
        let png = b"\x89PNG\r\n\x1a\n";
        assert_eq!(
            web_qr_image_data_url_from_bytes(png).as_deref(),
            Ok("data:image/png;base64,iVBORw0KGgo=")
        );
        assert!(web_qr_image_data_url_from_bytes(b"GIF89a").is_err());
    }

    #[test]
    fn execute_script_parser_unwraps_quoted_json_values() {
        let raw = r#"{"ok":true,"body":{"err_code":20010}}"#;
        assert_eq!(
            parse_execute_script_value(raw),
            Some(json!({"ok": true, "body": {"err_code": 20010}}))
        );

        let quoted = serde_json::to_string(raw).unwrap();
        assert_eq!(
            parse_execute_script_value(&quoted),
            parse_execute_script_value(raw)
        );
        assert_eq!(parse_execute_script_value("null"), Some(Value::Null));
        assert_eq!(parse_execute_script_value("not-json"), None);
    }

    #[test]
    fn web_qr_probe_script_contains_only_expected_login_actions() {
        let script = web_qr_probe_script();
        assert!(script.contains("data-action=\"login\""));
        assert!(script.contains("toDataURL('image/png')"));
        assert!(script.contains("qrlogin"));
        assert!(!script.contains("document.cookie"));
    }

    #[test]
    fn web_qr_bridge_captures_only_bounded_bilibili_refresh_sources() {
        let script = web_qr_bridge_script();
        assert!(script.contains("window.top.postMessage"));
        assert!(script.contains("chrome.webview.postMessage"));
        assert!(script.contains("qrlogin_img"));
        assert!(script.contains("qclogin_success"));
        assert!(script.contains("bilibili_refresh"));
        assert!(script.contains("installBilibiliRefreshCapture"));
        assert!(script.contains("parseBilibiliLoginPayload"));
        assert!(script.contains("/x/passport-login/web/qrcode/poll"));
        assert!(script.contains("ac_time_value"));
        assert!(script.contains("window.localStorage"));
        assert!(script.contains("store.getItem(name)"));
        assert!(!script.contains("get_userinfo_qrcode"));
        assert!(!script.contains("kugou_qr_login"));
        assert!(!script.contains("document.cookie"));
    }

    #[test]
    fn web_qr_image_hosts_are_provider_scoped() {
        assert!(web_qr_host_allowed("qqmusic", "xui.ptlogin2.qq.com"));
        assert!(!web_qr_host_allowed("qqmusic", "evil.example.com"));
        assert!(web_qr_host_allowed("netease", "p5.music.126.net"));
        assert!(web_qr_host_allowed("kugou", "login-user.kugou.com"));
        assert!(web_qr_host_allowed("bilibili", "www.bilibili.com"));
        assert!(!web_qr_host_allowed("kugou", "login-user.qq.com"));
        assert!(web_qr_url_allowed(
            "qqmusic",
            &url::Url::parse("https://xui.ptlogin2.qq.com/ssl/ptqrshow").unwrap()
        ));
        assert!(!web_qr_url_allowed(
            "qqmusic",
            &url::Url::parse("https://evil.example/qr.png").unwrap()
        ));
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
        let bilibili_anonymous =
            BTreeMap::from([("SESSDATA".to_owned(), "anonymous-session".to_owned())]);
        assert!(!has_required_cookies("bilibili", &bilibili_anonymous));
        let bilibili = BTreeMap::from([
            ("SESSDATA".to_owned(), "session".to_owned()),
            ("DedeUserID".to_owned(), "42".to_owned()),
        ]);
        assert!(has_required_cookies("bilibili", &bilibili));
        assert!(allowed_cookie_names("netease").contains(&"MUSIC_U"));

        let incomplete_refresh = BTreeMap::from([
            ("psrf_qqopenid".to_owned(), "open-id".to_owned()),
            (
                "psrf_qqrefresh_token".to_owned(),
                "refresh-token".to_owned(),
            ),
        ]);
        assert!(!has_qq_refresh_cookies(&incomplete_refresh));

        let kugou_encoded = BTreeMap::from([(
            "KuGoo".to_owned(),
            "t%3Dtoken%26KugooID%3D123%26ct%3D1700000000".to_owned(),
        )]);
        assert!(has_required_cookies("kugou", &kugou_encoded));
        let kugou_qr = BTreeMap::from([
            ("KUGOU_QR_TOKEN".to_owned(), "lite-token".to_owned()),
            ("KUGOU_QR_USERID".to_owned(), "123".to_owned()),
        ]);
        assert!(!has_required_cookies("kugou", &kugou_qr));
        assert!(!allowed_cookie_names("kugou").contains(&"KUGOU_QR_TOKEN"));
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
    fn qq_capture_finishes_with_refresh_token_only() {
        // QQ OAuth 网页登录通常只下发 refresh_token（无 refresh_key），
        // 此时应视为可刷新并立即完成捕获，无需等待 3 秒宽限；
        // refresh_key 由后续移动端续期响应补发。
        let refreshable = BTreeMap::from([
            ("uin".to_owned(), "1".to_owned()),
            ("qqmusic_key".to_owned(), "key".to_owned()),
            ("psrf_qqopenid".to_owned(), "open-id".to_owned()),
            ("psrf_qqaccess_token".to_owned(), "access-token".to_owned()),
            (
                "psrf_qqrefresh_token".to_owned(),
                "refresh-token".to_owned(),
            ),
        ]);
        let mut required_seen_at = None;

        assert!(has_qq_refresh_cookies(&refreshable));
        assert!(should_complete_capture(
            "qqmusic",
            &refreshable,
            &mut required_seen_at,
            Instant::now(),
        ));
        assert_eq!(required_seen_at, None);

        let wechat = BTreeMap::from([
            ("uin".to_owned(), "1".to_owned()),
            ("qqmusic_key".to_owned(), "W_X_key".to_owned()),
            ("psrf_qqopenid".to_owned(), "open-id".to_owned()),
            (
                "psrf_qqrefresh_token".to_owned(),
                "refresh-token".to_owned(),
            ),
        ]);
        assert!(has_qq_refresh_cookies(&wechat));
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
    fn bilibili_capture_waits_for_refresh_token_after_account_cookies() {
        let basic = BTreeMap::from([
            ("SESSDATA".to_owned(), "session".to_owned()),
            ("DedeUserID".to_owned(), "42".to_owned()),
        ]);
        let start = Instant::now();
        let mut required_seen_at = None;

        assert!(!should_complete_capture(
            "bilibili",
            &basic,
            &mut required_seen_at,
            start,
        ));
        assert_eq!(required_seen_at, None);
    }

    #[test]
    fn bilibili_capture_finishes_when_refresh_token_arrives() {
        let refreshable = BTreeMap::from([
            ("SESSDATA".to_owned(), "session".to_owned()),
            ("DedeUserID".to_owned(), "42".to_owned()),
            ("ac_time_value".to_owned(), "refresh".to_owned()),
        ]);
        let mut required_seen_at = None;

        assert!(should_complete_capture(
            "bilibili",
            &refreshable,
            &mut required_seen_at,
            Instant::now(),
        ));
        assert_eq!(required_seen_at, None);
    }

    #[test]
    fn bilibili_capture_never_falls_back_to_basic_cookies() {
        let basic = BTreeMap::from([
            ("SESSDATA".to_owned(), "session".to_owned()),
            ("DedeUserID".to_owned(), "42".to_owned()),
        ]);
        let start = Instant::now();
        let mut required_seen_at = Some(start);

        assert!(!should_complete_capture(
            "bilibili",
            &basic,
            &mut required_seen_at,
            start + QQ_REFRESH_COOKIE_GRACE_PERIOD,
        ));
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
        assert_eq!(
            parse_qq_login_callback("https://graph.qq.com/oauth2.0/login_jump?code=direct-qr-code"),
            Some(("1".to_owned(), "direct-qr-code".to_owned()))
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
    fn qq_web_session_fallback_precedes_callback_error() {
        let cookies = BTreeMap::from([
            ("wxopenid".to_owned(), "open-id".to_owned()),
            ("wxuin".to_owned(), "42".to_owned()),
            ("qqmusic_key".to_owned(), "Q_H_key".to_owned()),
        ]);
        let fields = finish_qq_login_exchange(
            "2",
            &cookies,
            BTreeMap::new(),
            Some("callback exchange rejected".to_owned()),
            |captured_cookies| {
                assert_eq!(captured_cookies, &cookies);
                Ok(BTreeMap::from([(
                    "wxaccess_token".to_owned(),
                    "web-access-token".to_owned(),
                )]))
            },
        )
        .expect("web session fallback should recover a rejected callback exchange");

        assert_eq!(
            fields.get("wxaccess_token"),
            Some(&"web-access-token".to_owned())
        );
        assert_eq!(fields.get("login_type"), Some(&"2".to_owned()));
    }

    #[test]
    fn qq_failed_callback_does_not_return_partial_fields_without_basic_session() {
        let error = finish_qq_login_exchange(
            "1",
            &BTreeMap::new(),
            BTreeMap::from([("access_token".to_owned(), "partial".to_owned())]),
            Some("callback exchange rejected".to_owned()),
            |_| panic!("QQ login type 1 has no web-session fallback"),
        )
        .unwrap_err();
        assert_eq!(error, "callback exchange rejected");
    }

    #[test]
    fn qq_failed_callback_discards_partial_fields_when_basic_session_is_available() {
        let cookies = BTreeMap::from([
            ("uin".to_owned(), "42".to_owned()),
            ("qqmusic_key".to_owned(), "Q_H_key".to_owned()),
        ]);
        let fields = finish_qq_login_exchange(
            "1",
            &cookies,
            BTreeMap::from([("access_token".to_owned(), "partial".to_owned())]),
            Some("callback exchange rejected".to_owned()),
            |_| panic!("QQ login type 1 has no web-session fallback"),
        )
        .unwrap();
        assert!(fields.is_empty());
    }

    #[test]
    fn qq_login_response_data_accepts_root_data_envelope() {
        let body = json!({"code": 0, "data": {"musickey": "key"}});
        assert_eq!(
            qq_login_response_data(&body)
                .and_then(|data| data.get("musickey"))
                .and_then(serde_json::Value::as_str),
            Some("key")
        );
    }

    #[test]
    fn qq_login_response_code_accepts_legacy_return_code_envelopes() {
        assert_eq!(
            qq_login_response_code(&json!({"req_0": {"retcode": 0}})),
            Some("0".to_owned())
        );
        assert_eq!(
            qq_login_response_code(&json!({"data": {"ret": 1000}})),
            Some("1000".to_owned())
        );
    }

    #[test]
    fn qq_login_response_merges_str_musicid_and_wechat_aliases() {
        let mut cookies = BTreeMap::new();
        merge_qq_login_response_fields(
            &mut cookies,
            &json!({
                "code": 0,
                "data": {
                    "str_musicid": "42",
                    "musickey": "W_X_key",
                    "wxopenid": "wx-open-id",
                    "wxaccess_token": "wx-access-token",
                    "wxrefresh_token": "wx-refresh-token",
                    "wxunionid": "wx-union-id"
                }
            }),
        );
        assert_eq!(cookies.get("uin"), Some(&"42".to_owned()));
        assert_eq!(cookies.get("qqmusic_key"), Some(&"W_X_key".to_owned()));
        assert_eq!(
            cookies.get("wxaccess_token"),
            Some(&"wx-access-token".to_owned())
        );
        assert_eq!(
            cookies.get("wxrefresh_token"),
            Some(&"wx-refresh-token".to_owned())
        );
    }

    #[test]
    fn qq_wechat_exchange_does_not_consume_web_refresh_when_callback_has_token() {
        let cookies = BTreeMap::new();
        let fields = finish_qq_login_exchange(
            "2",
            &cookies,
            BTreeMap::from([
                ("wxaccess_token".to_owned(), "callback-token".to_owned()),
                ("qqmusic_key".to_owned(), "callback-key".to_owned()),
                ("wxuin".to_owned(), "42".to_owned()),
                ("wxopenid".to_owned(), "callback-openid".to_owned()),
                (
                    "wxrefresh_token".to_owned(),
                    "callback-refresh-token".to_owned(),
                ),
            ]),
            None,
            |_| panic!("web fallback must not run after a successful token exchange"),
        )
        .unwrap();
        assert_eq!(
            fields.get("wxaccess_token"),
            Some(&"callback-token".to_owned())
        );
    }

    #[test]
    fn qq_wechat_partial_callback_uses_a_coherent_web_refresh_set() {
        let cookies = BTreeMap::from([
            ("wxopenid".to_owned(), "session-openid".to_owned()),
            ("wxrefresh_token".to_owned(), "session-refresh".to_owned()),
            ("wxuin".to_owned(), "42".to_owned()),
            ("qqmusic_key".to_owned(), "session-key".to_owned()),
        ]);
        let fields = finish_qq_login_exchange(
            "2",
            &cookies,
            BTreeMap::from([
                ("wxaccess_token".to_owned(), "callback-token".to_owned()),
                ("qqmusic_key".to_owned(), "callback-key".to_owned()),
                ("wxuin".to_owned(), "42".to_owned()),
            ]),
            None,
            |_| {
                Ok(BTreeMap::from([
                    ("wxaccess_token".to_owned(), "web-token".to_owned()),
                    ("qqmusic_key".to_owned(), "web-key".to_owned()),
                    ("wxopenid".to_owned(), "web-openid".to_owned()),
                    ("wxrefresh_token".to_owned(), "web-refresh".to_owned()),
                    ("wxuin".to_owned(), "42".to_owned()),
                ]))
            },
        )
        .unwrap();

        assert_eq!(fields.get("wxaccess_token"), Some(&"web-token".to_owned()));
        assert_eq!(fields.get("qqmusic_key"), Some(&"web-key".to_owned()));
        assert_eq!(
            fields.get("wxrefresh_token"),
            Some(&"web-refresh".to_owned())
        );
        assert_eq!(fields.get("login_type"), Some(&"2".to_owned()));
    }

    #[test]
    fn qq_web_refresh_cookies_are_ready_without_grace_delay() {
        let cookies = BTreeMap::from([
            ("wxopenid".to_owned(), "open-id".to_owned()),
            ("wxrefresh_token".to_owned(), "refresh-token".to_owned()),
            ("wxuin".to_owned(), "o42".to_owned()),
            ("qqmusic_key".to_owned(), "Q_H_key".to_owned()),
        ]);
        assert!(has_qq_refresh_cookies(&cookies));
        let now = Instant::now();
        let mut seen = None;
        assert!(should_complete_capture("qqmusic", &cookies, &mut seen, now,));
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
    fn qq_login_response_accepts_camel_case_token_fields() {
        let mut cookies = BTreeMap::new();
        merge_qq_login_response_fields(
            &mut cookies,
            &json!({
                "code": 0,
                "data": {
                    "musicUin": "42",
                    "musicKey": "key",
                    "openId": "open-id",
                    "accessToken": "access-token",
                    "refreshToken": "refresh-token",
                    "refreshKey": "refresh-key",
                    "unionId": "union-id"
                }
            }),
        );
        assert_eq!(cookies.get("uin"), Some(&"42".to_owned()));
        assert_eq!(
            cookies.get("psrf_qqaccess_token"),
            Some(&"access-token".to_owned())
        );
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
    fn qq_login_response_updates_all_existing_cookie_aliases() {
        let mut cookies = BTreeMap::from([
            ("qqmusic_key".to_owned(), "old-key".to_owned()),
            ("qm_keyst".to_owned(), "old-secondary-key".to_owned()),
            ("uin".to_owned(), "1".to_owned()),
            ("wxuin".to_owned(), "o1".to_owned()),
        ]);
        merge_qq_login_response_fields(
            &mut cookies,
            &json!({
                "musicid": 42,
                "musickey": "new-key"
            }),
        );
        assert_eq!(cookies.get("qqmusic_key"), Some(&"new-key".to_owned()));
        assert_eq!(cookies.get("qm_keyst"), Some(&"new-key".to_owned()));
        assert_eq!(cookies.get("uin"), Some(&"42".to_owned()));
        assert_eq!(cookies.get("wxuin"), Some(&"42".to_owned()));

        cookies.insert("qm_keyst".to_owned(), "newer-key".to_owned());
        normalize_qq_cookie_aliases(&mut cookies);
        assert_eq!(cookies.get("qqmusic_key"), Some(&"new-key".to_owned()));
        assert_eq!(cookies.get("qm_keyst"), Some(&"new-key".to_owned()));

        cookies.insert("qqmusic_key".to_owned(), "   ".to_owned());
        cookies.insert("qm_keyst".to_owned(), "secondary-key".to_owned());
        normalize_qq_cookie_aliases(&mut cookies);
        assert_eq!(
            cookies.get("qqmusic_key"),
            Some(&"secondary-key".to_owned())
        );
        assert_eq!(cookies.get("qm_keyst"), Some(&"secondary-key".to_owned()));
    }

    #[test]
    fn qq_exchange_updates_win_over_stale_browser_aliases() {
        let mut cookies = BTreeMap::from([
            ("uin".to_owned(), "old-uin".to_owned()),
            ("wxuin".to_owned(), "old-wxuin".to_owned()),
            ("qqmusic_key".to_owned(), "old-key".to_owned()),
            ("qm_keyst".to_owned(), "old-secondary-key".to_owned()),
        ]);
        let updates = BTreeMap::from([
            ("wxuin".to_owned(), "new-wxuin".to_owned()),
            ("qqmusic_key".to_owned(), "new-key".to_owned()),
        ]);
        merge_qq_cookie_updates(&mut cookies, &updates);
        assert_eq!(cookies.get("uin"), Some(&"new-wxuin".to_owned()));
        assert_eq!(cookies.get("wxuin"), Some(&"new-wxuin".to_owned()));
        assert_eq!(cookies.get("qqmusic_key"), Some(&"new-key".to_owned()));
        assert_eq!(cookies.get("qm_keyst"), Some(&"new-key".to_owned()));
    }

    #[test]
    fn qq_exchange_empty_updates_do_not_erase_existing_cookies() {
        let mut cookies = BTreeMap::from([
            ("qqmusic_key".to_owned(), "existing-key".to_owned()),
            ("qm_keyst".to_owned(), "existing-key".to_owned()),
            ("session_cookie".to_owned(), "existing-session".to_owned()),
        ]);
        let updates = BTreeMap::from([
            ("qqmusic_key".to_owned(), "   ".to_owned()),
            ("qm_keyst".to_owned(), String::new()),
            ("session_cookie".to_owned(), "\t".to_owned()),
        ]);

        merge_qq_cookie_updates(&mut cookies, &updates);

        assert_eq!(
            cookies.get("qqmusic_key").map(String::as_str),
            Some("existing-key")
        );
        assert_eq!(
            cookies.get("qm_keyst").map(String::as_str),
            Some("existing-key")
        );
        assert_eq!(
            cookies.get("session_cookie").map(String::as_str),
            Some("existing-session")
        );
    }

    #[test]
    fn qq_login_exchange_payloads_match_the_two_web_login_modes() {
        let qq = super::qq_login_exchange_payload("1", "callback-code").unwrap();
        assert_eq!(qq["req"]["module"], "QQConnectLogin.LoginServer");
        assert_eq!(qq["req"]["method"], "QQLogin");
        assert_eq!(qq["req"]["param"]["code"], "callback-code");
        assert_eq!(qq["comm"]["tmeLoginType"], 2);

        let wechat = super::qq_login_exchange_payload("2", "callback-code").unwrap();
        assert_eq!(wechat["req"]["module"], "music.login.LoginServer");
        assert_eq!(wechat["req"]["param"]["strAppid"], "wx48db31d50e334801");
        assert_eq!(wechat["comm"]["tmeLoginType"], 1);
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
