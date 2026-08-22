use std::collections::BTreeMap;
use std::path::Path;
use std::thread;
use std::time::Duration;

use md5::{Digest, Md5};
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
        // 酷狗登录入口在首页右上角。
        "kugou" => Some("https://www.kugou.com/"),
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
        "bilibili" => has_any_nonempty(cookies, &["SESSDATA"]),
        "kugou" => cookies.get("KuGoo").is_some_and(|value| {
            !value.is_empty() && value.contains("t=") && value.contains("KugooID=")
        }),
        _ => false,
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
/// ac_time_value），cookie 是否下发不稳定。登录完成后最多探测这么多次
/// 读取它（约 6 秒），拿不到则按基础 cookie 判定正常完成。
const BILIBILI_JS_PROBE_ATTEMPTS: u32 = 12;
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

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
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
        &["openid"],
        &["psrf_qqopenid", "openid", "wxopenid"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["wxopenid"],
        &["wxopenid", "psrf_qqopenid", "openid"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["access_token"],
        &["psrf_qqaccess_token", "access_token", "wxaccess_token"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["wxaccess_token"],
        &["wxaccess_token", "psrf_qqaccess_token", "access_token"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["refresh_token"],
        &["psrf_qqrefresh_token", "refresh_token", "wxrefresh_token"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["wxrefresh_token"],
        &["wxrefresh_token", "psrf_qqrefresh_token", "refresh_token"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["refresh_key"],
        &["psrf_qqrefresh_key", "refresh_key"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["musickey", "qqmusic_key", "qm_keyst", "lqm_keyst"],
        &["qqmusic_key", "qm_keyst", "lqm_keyst"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["musicid", "str_musicid"],
        &["uin", "wxuin"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["unionid"],
        &["psrf_qqunionid", "unionid", "wxunionid"],
    );
    merge_qq_response_alias(
        cookies,
        data,
        &["wxunionid"],
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

/// 酷狗概念版扫码登录凭据获取（不依赖 sidecar，直连酷狗官方接口）。
///
/// 流程：v2/qrcode 拿 key → WebView2 显示 H5 二维码页 → 用户用酷狗 App
/// 扫码 → v2/get_userinfo_qrcode 轮询到 status=4 返回 token/userid。
const KUGOU_WEB_SIGN_SALT: &str = "NVPh5oo715z5DIWAeQlhMDsWXXQV4hwt";
const KUGOU_LITE_APPID: i64 = 3116;
const KUGOU_LITE_CLIENTVER: i64 = 11440;
const KUGOU_SRCAPPID: i64 = 2919;
const KUGOU_QR_KEY_URL: &str = "https://login-user.kugou.com/v2/qrcode";
const KUGOU_QR_CHECK_URL: &str = "https://login-user.kugou.com/v2/get_userinfo_qrcode";
const KUGOU_QR_POLL_INTERVAL: Duration = Duration::from_secs(2);
const KUGOU_QR_POLL_ATTEMPTS: u32 = 90;
const KUGOU_UA: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";
const KUGOU_KG_RFC: &str = "B9EDA08A64250DEFFBCADDEE00F8F25F";

/// 酷狗扫码轮询结果：`Ok(Some((token, userid, cookies)))` 表示登录成功。
type KugouQrPollResult =
    Result<Option<(String, String, BTreeMap<String, String>)>, KugouQrPollError>;

#[derive(Debug, PartialEq, Eq)]
enum KugouQrPollError {
    /// 请求层故障可以短暂重试；不保留原始错误，避免意外传播签名参数或二维码 key。
    Transient,
    /// 二维码状态或响应结构已经明确失败，重试不会恢复。
    Terminal(String),
}

impl KugouQrPollError {
    fn into_user_message(self) -> String {
        match self {
            Self::Transient => "酷狗二维码接口请求失败，请重试".to_owned(),
            Self::Terminal(message) => message,
        }
    }
}

/// 酷狗 web 版签名：md5(盐值 + 参数按 key 排序后的 k=v 拼接 + 盐值)。
fn kugou_web_signature(params: &BTreeMap<String, String>) -> String {
    let mut pairs = params.iter().collect::<Vec<_>>();
    pairs.sort_by(|left, right| left.0.cmp(right.0));
    let joined = pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<String>();
    let mut hasher = Md5::new();
    hasher.update(format!(
        "{KUGOU_WEB_SIGN_SALT}{joined}{KUGOU_WEB_SIGN_SALT}"
    ));
    format!("{:x}", hasher.finalize())
}

/// 计算酷狗设备 MID：md5(GUID) 的 hex 视为 128 位无符号大整数，转十进制。
fn kugou_calculate_mid(guid: &str) -> String {
    let guid = kugou_normalize_guid(guid);
    let mut hasher = Md5::new();
    hasher.update(guid.as_bytes());
    let hex = format!("{:x}", hasher.finalize());
    u128::from_str_radix(&hex, 16)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "-".to_owned())
}

fn kugou_normalize_guid(guid: &str) -> String {
    let guid = guid.trim();
    let bytes = guid.as_bytes();
    let is_uuid_v4 = bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes[14].eq_ignore_ascii_case(&b'4')
        && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 8 | 13 | 18 | 23) || byte.is_ascii_hexdigit());
    if is_uuid_v4 {
        let mut hasher = Md5::new();
        hasher.update(guid.as_bytes());
        format!("{:x}", hasher.finalize())
    } else {
        guid.to_owned()
    }
}

/// 扫码登录请求的默认参数。mid 必须与 sidecar 设备一致
/// （主程序通过 KUGOU_API_GUID 环境变量传入），否则扫码 token
/// 绑定匿名设备，概念版 API 会拒绝（error_code 20018）。
fn kugou_default_params() -> BTreeMap<String, String> {
    let mid = std::env::var("KUGOU_API_MID").unwrap_or_else(|_| {
        std::env::var("KUGOU_API_GUID")
            .ok()
            .map(|guid| kugou_calculate_mid(&guid))
            .unwrap_or_else(|| "-".to_owned())
    });
    BTreeMap::from([
        ("dfid".to_owned(), "-".to_owned()),
        ("mid".to_owned(), mid),
        ("uuid".to_owned(), "-".to_owned()),
        ("appid".to_owned(), KUGOU_LITE_APPID.to_string()),
        ("clientver".to_owned(), KUGOU_LITE_CLIENTVER.to_string()),
        (
            "clienttime".to_owned(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0)
                .to_string(),
        ),
    ])
}

fn kugou_web_get(
    url: &str,
    params: &BTreeMap<String, String>,
) -> Result<serde_json::Value, String> {
    kugou_web_get_with_cookies(url, params).map(|(value, _)| value)
}

/// 酷狗 web 接口请求，额外返回响应 Set-Cookie 的键值（续期字段如 t1 只经
/// Set-Cookie 下发，不返回它们会导致概念版 token 刷新被官方拒绝）。
fn kugou_web_get_with_cookies(
    url: &str,
    params: &BTreeMap<String, String>,
) -> Result<(serde_json::Value, BTreeMap<String, String>), String> {
    let mut signed = params.clone();
    let signature = kugou_web_signature(params);
    signed.insert("signature".to_owned(), signature);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(url)
        .query(&signed)
        .header("User-Agent", KUGOU_UA)
        // Keep the login request headers aligned with the shared KuGou
        // request layer; the QR endpoints also use these device headers for
        // risk control even though their signature is the web variant.
        .header(
            "dfid",
            params.get("dfid").map(String::as_str).unwrap_or("-"),
        )
        .header(
            "clienttime",
            params.get("clienttime").map(String::as_str).unwrap_or("0"),
        )
        .header("mid", params.get("mid").map(String::as_str).unwrap_or("-"))
        .header("kg-rc", "1")
        .header("kg-thash", "5d816a0")
        .header("kg-rec", "1")
        .header("kg-rf", KUGOU_KG_RFC)
        .send()
        .map_err(|error| format!("酷狗登录接口请求失败: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("酷狗登录接口返回 HTTP {}", response.status()));
    }
    let mut cookies = BTreeMap::new();
    for set_cookie in response.headers().get_all(reqwest::header::SET_COOKIE) {
        if let Ok(header) = set_cookie.to_str()
            && let Some((name, rest)) = header.split_once('=')
        {
            let name = name.trim();
            let value = rest.split(';').next().unwrap_or("").trim();
            if !name.is_empty() && !value.is_empty() {
                cookies.insert(name.to_owned(), value.to_owned());
            }
        }
    }
    let value = response
        .json::<serde_json::Value>()
        .map_err(|error| format!("酷狗登录接口响应无效: {error}"))?;
    if value
        .get("status")
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|status| status == 0)
        || value
            .get("error_code")
            .and_then(serde_json::Value::as_i64)
            .is_some_and(|code| code != 0)
    {
        return Err("酷狗登录接口拒绝了请求".to_owned());
    }
    Ok((value, cookies))
}

/// 获取扫码登录 key 与二维码图片（base64 data URL）。
fn kugou_qr_key() -> Result<(String, String), String> {
    let mut params = kugou_default_params();
    params.insert("appid".to_owned(), "1001".to_owned());
    params.insert("type".to_owned(), "1".to_owned());
    params.insert("plat".to_owned(), "4".to_owned());
    params.insert(
        "qrcode_txt".to_owned(),
        format!("https://h5.kugou.com/apps/loginQRCode/html/index.html?appid={KUGOU_LITE_APPID}&"),
    );
    params.insert("srcappid".to_owned(), KUGOU_SRCAPPID.to_string());
    let value = kugou_web_get(KUGOU_QR_KEY_URL, &params)?;
    let key = value
        .get("data")
        .and_then(|data| data.get("qrcode"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| "酷狗二维码 key 获取失败".to_owned())?;
    let image = value
        .get("data")
        .and_then(|data| data.get("qrcode_img"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();
    Ok((key, image))
}

/// 构建二维码展示页 HTML（直接内嵌 base64 图片，避免 H5 页 UA 跳转）。
fn kugou_qr_page(image: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>酷狗登录</title></head>\
         <body style=\"margin:0;background:#f5f5f5;display:flex;flex-direction:column;align-items:center;justify-content:center;height:100vh;font-family:sans-serif\">\
         <div style=\"background:#fff;padding:24px;border-radius:12px;box-shadow:0 2px 12px rgba(0,0,0,.08)\">\
         <img src=\"{image}\" style=\"width:min(70vw,520px);height:auto;display:block\"></div>\
         <p style=\"color:#666;margin-top:18px;font-size:14px\">请使用酷狗 App 扫码登录</p>\
         </body></html>"
    )
}

fn parse_kugou_qr_poll_response(
    value: &Value,
    set_cookies: BTreeMap<String, String>,
) -> KugouQrPollResult {
    let status = value
        .pointer("/data/status")
        .and_then(value_as_i64)
        .ok_or_else(|| {
            KugouQrPollError::Terminal("酷狗二维码状态响应缺少有效 status".to_owned())
        })?;
    match status {
        4 => {
            let token = value
                .pointer("/data/token")
                .map(value_to_string)
                .filter(|token| !token.is_empty())
                .ok_or_else(|| KugouQrPollError::Terminal("酷狗扫码登录未返回 token".to_owned()))?;
            // userid 可能是数字或字符串。
            let userid = value
                .pointer("/data/userid")
                .map(value_to_string)
                .filter(|userid| !userid.is_empty())
                .ok_or_else(|| {
                    KugouQrPollError::Terminal("酷狗扫码登录未返回 userid".to_owned())
                })?;
            Ok(Some((token, userid, set_cookies)))
        }
        0 => Err(KugouQrPollError::Terminal(
            "酷狗二维码已过期，请重新登录".to_owned(),
        )),
        1 | 2 => Ok(None),
        _ => Err(KugouQrPollError::Terminal(format!(
            "酷狗二维码接口返回未知状态 {status}"
        ))),
    }
}

/// 轮询扫码状态。cookies 为官方 Set-Cookie 下发的续期字段（t1 等）。
fn kugou_qr_poll(key: &str) -> KugouQrPollResult {
    let mut params = kugou_default_params();
    params.insert("plat".to_owned(), "4".to_owned());
    params.insert("srcappid".to_owned(), KUGOU_SRCAPPID.to_string());
    params.insert("qrcode".to_owned(), key.to_owned());
    params.insert(
        "dev".to_owned(),
        std::env::var("KUGOU_API_DEV")
            .map(|value| value.trim().to_ascii_uppercase())
            .unwrap_or_else(|_| "-".to_owned()),
    );
    let (value, set_cookies) = kugou_web_get_with_cookies(KUGOU_QR_CHECK_URL, &params)
        .map_err(|_| KugouQrPollError::Transient)?;
    parse_kugou_qr_poll_response(&value, set_cookies)
}

/// 后台轮询线程：直到登录成功、连续失败或超时。
/// 酷狗登录接口偶发连接重置，网络类错误连续出现多次才放弃。
fn kugou_qr_poll_loop(
    key: &str,
    sender: std::sync::mpsc::Sender<KugouQrPollResult>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut consecutive_errors = 0;
    for _ in 0..KUGOU_QR_POLL_ATTEMPTS {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        match kugou_qr_poll(key) {
            Ok(Some(triple)) => {
                let _ = sender.send(Ok(Some(triple)));
                return;
            }
            Ok(None) => consecutive_errors = 0,
            Err(KugouQrPollError::Transient) => {
                // Do not echo the provider's raw error: it may contain the
                // signed query, qrcode key, or a full request URL.
                eprintln!(
                    "[kugou-qr] poll attempt failed (consecutive={})",
                    consecutive_errors + 1
                );
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    let _ = sender.send(Err(KugouQrPollError::Terminal(
                        "酷狗二维码接口连续失败，请重试".to_owned(),
                    )));
                    return;
                }
            }
            Err(error @ KugouQrPollError::Terminal(_)) => {
                let _ = sender.send(Err(error));
                return;
            }
        }
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        thread::sleep(KUGOU_QR_POLL_INTERVAL);
    }
    if !cancel.load(std::sync::atomic::Ordering::Acquire) {
        let _ = sender.send(Err(KugouQrPollError::Terminal(
            "酷狗扫码等待超时".to_owned(),
        )));
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
    let seen_at = required_seen_at.get_or_insert(now);
    now.duration_since(*seen_at) >= QQ_REFRESH_COOKIE_GRACE_PERIOD
}

#[cfg(test)]
mod kugou_qr_tests {
    use std::collections::BTreeMap;

    use super::{
        KugouQrPollError, kugou_qr_key, kugou_qr_poll, kugou_web_signature,
        parse_kugou_qr_poll_response, value_as_i64,
    };
    use serde_json::json;

    #[test]
    fn kugou_qr_signature_matches_upstream_web_vector() {
        let params = BTreeMap::from([
            ("dfid".to_owned(), "-".to_owned()),
            ("mid".to_owned(), "123456789".to_owned()),
            ("uuid".to_owned(), "-".to_owned()),
            ("appid".to_owned(), "1001".to_owned()),
            ("clientver".to_owned(), "11440".to_owned()),
            ("clienttime".to_owned(), "1700000000".to_owned()),
            ("type".to_owned(), "1".to_owned()),
            ("plat".to_owned(), "4".to_owned()),
            (
                "qrcode_txt".to_owned(),
                "https://h5.kugou.com/apps/loginQRCode/html/index.html?appid=3116&".to_owned(),
            ),
            ("srcappid".to_owned(), "2919".to_owned()),
        ]);
        assert_eq!(
            kugou_web_signature(&params),
            "da09e77f63350c605f82f9009eb5c827"
        );
    }

    #[test]
    fn kugou_qr_status_accepts_numeric_and_string_wire_values() {
        assert_eq!(value_as_i64(&json!(4)), Some(4));
        assert_eq!(value_as_i64(&json!("4")), Some(4));
        assert_eq!(value_as_i64(&json!(" malformed ")), None);
    }

    #[test]
    fn kugou_qr_statuses_distinguish_waiting_success_and_terminal_failures() {
        for status in [json!(1), json!("2")] {
            assert_eq!(
                parse_kugou_qr_poll_response(&json!({"data": {"status": status}}), BTreeMap::new(),),
                Ok(None)
            );
        }

        let success = parse_kugou_qr_poll_response(
            &json!({"data": {"status": 4, "token": "token", "userid": 42}}),
            BTreeMap::from([("t1".to_owned(), "refresh".to_owned())]),
        )
        .expect("status 4 should succeed")
        .expect("status 4 should include credentials");
        assert_eq!(success.0, "token");
        assert_eq!(success.1, "42");
        assert_eq!(success.2.get("t1").map(String::as_str), Some("refresh"));

        assert!(matches!(
            parse_kugou_qr_poll_response(
                &json!({"data": {"status": 0}}),
                BTreeMap::new(),
            ),
            Err(KugouQrPollError::Terminal(message)) if message.contains("过期")
        ));
        assert!(matches!(
            parse_kugou_qr_poll_response(
                &json!({"data": {"status": 3}}),
                BTreeMap::new(),
            ),
            Err(KugouQrPollError::Terminal(message)) if message.contains("未知状态 3")
        ));
        assert!(matches!(
            parse_kugou_qr_poll_response(&json!({"data": {}}), BTreeMap::new()),
            Err(KugouQrPollError::Terminal(message)) if message.contains("status")
        ));
    }

    #[test]
    #[ignore = "需要公网访问酷狗登录接口"]
    fn kugou_qr_key_and_poll_round_trip() {
        let (key, image) = kugou_qr_key().expect("获取二维码 key");
        assert!(!key.is_empty());
        assert!(image.starts_with("data:image"));
        let result = kugou_qr_poll(&key);
        eprintln!("poll 结果: {result:?}");
    }
}

#[cfg(windows)]
#[allow(unsafe_op_in_unsafe_fn)]
mod platform {
    use super::{CaptureError, KugouQrPollResult, allowed_cookie_names, login_url};
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
    const IID_SCRIPT_HANDLER: GUID = GUID {
        data1: 0x3d8b2a74,
        data2: 0x9f1e,
        data3: 0x4c6a,
        data4: [0xa5, 0x0b, 0x7c, 0x2e, 0x1d, 0x9f, 0x08, 0x53],
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
        js_probe_inflight: bool,
        bilibili_js_probe_attempts: u32,
        kugou_qr_rx: Option<mpsc::Receiver<KugouQrPollResult>>,
        kugou_qr_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    struct CaptureContext {
        provider: String,
        url: Vec<u16>,
        html: Option<Vec<u16>>,
        hwnd: HWND,
        state: Mutex<CaptureState>,
    }

    impl Drop for CaptureContext {
        fn drop(&mut self) {
            if let Ok(state) = self.state.lock()
                && let Some(cancel) = state.kugou_qr_cancel.as_ref()
            {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
        }
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
                html: None,
                hwnd,
                state: Mutex::new(CaptureState::default()),
            })
        }

        fn new_with_html(provider: &str, hwnd: HWND, html: &str) -> Result<Self, CaptureError> {
            Ok(Self {
                provider: provider.to_owned(),
                url: Vec::new(),
                html: Some(wide(html)),
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

        fn poll_kugou_qr(self: &Rc<Self>) {
            if self.provider != "kugou" {
                return;
            }
            let result = {
                let state = self.state.lock().expect("capture state mutex poisoned");
                if state.outcome.is_some() {
                    return;
                }
                let Some(receiver) = state.kugou_qr_rx.as_ref() else {
                    return;
                };
                match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => Some(Err(
                        super::KugouQrPollError::Terminal("酷狗二维码轮询线程已断开".to_owned()),
                    )),
                }
            };
            match result {
                Some(Ok(Some((token, userid, set_cookies)))) => {
                    let mut cookies = BTreeMap::new();
                    cookies.insert("KuGoo".to_owned(), format!("t={token}&KugooID={userid}"));
                    // 保留官方下发的设备/续期字段；播放请求需要与登录时相同的
                    // mid/dfid，token 刷新还依赖 t1 和 lite 设备字段。
                    for (name, value) in set_cookies {
                        if matches!(
                            name.as_str(),
                            "dfid"
                                | "KugouGUID"
                                | "KUGOU_API_GUID"
                                | "KUGOU_API_MID"
                                | "KUGOU_API_MAC"
                                | "KUGOU_API_DEV"
                                | "kg_mid"
                                | "mid"
                                | "t1"
                                | "vip_type"
                                | "vip_token"
                        ) {
                            cookies.insert(name, value);
                        }
                    }
                    let mut state = self.state.lock().expect("capture state mutex poisoned");
                    state.outcome = Some(Ok(cookies));
                }
                Some(Ok(None)) => {}
                Some(Err(error)) => {
                    let mut state = self.state.lock().expect("capture state mutex poisoned");
                    state.outcome = Some(Err(CaptureError::Io(error.into_user_message())));
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
                state.next_cookie_poll = Some(Instant::now());
                return;
            }
            let probe_index = {
                let state = self.state.lock().expect("capture state mutex poisoned");
                state.bilibili_js_probe_attempts
            };
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
            // 前 5 次探测交替刷新首页（触发前端写入 refresh_token）与读取。
            let should_navigate = !on_bilibili || (probe_index <= 5 && probe_index % 2 == 1);
            if should_navigate {
                let _ = unsafe { navigate(webview, url.as_ptr()) };
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                state.js_probe_inflight = false;
                state.next_cookie_poll = Some(Instant::now());
                return;
            }
            // ICoreWebView2::ExecuteScript 位于 vtable 槽位 29
            // （0 get_Settings 起第 26 个方法，加 IUnknown 三个槽位）。
            let handler = Box::into_raw(Box::new(ScriptHandler {
                vtbl: &SCRIPT_HANDLER_VTABLE,
                refs: AtomicU32::new(1),
                context: Rc::clone(self),
            })) as *mut c_void;
            let execute_script: unsafe extern "system" fn(
                *mut c_void,
                PCWSTR,
                *mut c_void,
            ) -> HRESULT = unsafe { vtable_fn(webview, 29) };
            let script = wide(
                "(localStorage.getItem('ac_time_value')||sessionStorage.getItem('ac_time_value'))",
            );
            let result = unsafe { execute_script(webview, script.as_ptr(), handler) };
            unsafe { release_com(handler) };
            if result < 0 {
                eprintln!("[js-probe] ExecuteScript failed: {result:#x}");
                let mut state = self.state.lock().expect("capture state mutex poisoned");
                state.js_probe_inflight = false;
                state.next_cookie_poll = Some(Instant::now());
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

    #[repr(C)]
    struct ScriptHandler {
        vtbl: &'static ScriptHandlerVtable,
        refs: AtomicU32,
        context: Rc<CaptureContext>,
    }

    type ScriptInvoke = unsafe extern "system" fn(*mut c_void, HRESULT, *const u16) -> HRESULT;

    #[repr(C)]
    struct ScriptHandlerVtable {
        query_interface: HandlerQueryInterface,
        add_ref: HandlerAddRef,
        release: HandlerRelease,
        invoke: ScriptInvoke,
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
    static SCRIPT_HANDLER_VTABLE: ScriptHandlerVtable = ScriptHandlerVtable {
        query_interface: script_query_interface,
        add_ref: script_add_ref,
        release: script_release,
        invoke: script_invoke,
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

    unsafe extern "system" fn script_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        query_interface(this, riid, out, &IID_SCRIPT_HANDLER, script_add_ref)
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

    unsafe extern "system" fn script_add_ref(this: *mut c_void) -> u32 {
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

    unsafe extern "system" fn script_release(this: *mut c_void) -> u32 {
        release_handler::<ScriptHandler>(this)
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

        // 内嵌 HTML（酷狗二维码页）走 NavigateToString，其余走 Navigate。
        // ICoreWebView2::NavigateToString 位于 vtable 槽位 6。
        let hr = if let Some(html) = &context.html {
            let navigate_to_string: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT =
                vtable_fn(webview, 6);
            navigate_to_string(webview, html.as_ptr())
        } else {
            let navigate: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT =
                vtable_fn(webview, 5);
            navigate(webview, context.url.as_ptr())
        };
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
        if context.provider == "qqmusic" {
            super::merge_qq_cookie_updates(&mut state.latest_cookies, &exchange_cookies);
            super::normalize_qq_cookie_aliases(&mut state.latest_cookies);
        } else {
            state.latest_cookies.extend(exchange_cookies);
        }
        let cookies = state.latest_cookies.clone();
        let now = Instant::now();
        let exchange_pending = state.exchange_rx.is_some();
        let complete = !exchange_pending
            && super::should_complete_capture(
                &context.provider,
                &cookies,
                &mut state.required_cookies_seen_at,
                now,
            );
        // B 站完成前先尝试从 localStorage/sessionStorage 探测 refresh_token：
        // 未拿到且次数未耗尽时禁止完成，持续轮询（ExecuteScript 异步回调
        // 可能晚于下一次 GetCookies，提前完成会错过稍后写入的值）。
        let probing = complete
            && context.provider == "bilibili"
            && !cookies.contains_key("ac_time_value")
            && state.bilibili_js_probe_attempts < super::BILIBILI_JS_PROBE_ATTEMPTS;
        if probing {
            if !state.js_probe_inflight {
                state.bilibili_js_probe_attempts += 1;
                state.js_probe_inflight = true;
                let webview = state.webview;
                drop(state);
                context.start_bilibili_js_probe(webview);
            } else {
                state.next_cookie_poll = Some(now + super::COOKIE_POLL_INTERVAL);
            }
            return S_OK;
        }
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
        let context = (&*(this as *mut ScriptHandler)).context.clone();
        let mut state = context.state.lock().expect("capture state mutex poisoned");
        state.js_probe_inflight = false;
        state.next_cookie_poll = Some(Instant::now());
        if result >= 0 && !json_result.is_null() {
            let text = unsafe { read_wide(json_result) };
            // ExecuteScript 返回 JSON 编码：null 或带引号的字符串。
            if let Ok(Some(value)) = serde_json::from_str::<Option<String>>(&text)
                && !value.is_empty()
            {
                state
                    .latest_cookies
                    .insert("ac_time_value".to_owned(), value);
                state.outcome = Some(Ok(state.latest_cookies.clone()));
            }
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
        // 酷狗走概念版扫码登录：获取二维码 key 与图片，窗口内直接显示
        // 二维码（内嵌 HTML，避免 H5 页对桌面 UA 跳转），后台轮询扫码状态；
        // 其余平台使用固定的登录页。
        let (context, qr_receiver, qr_cancel) = if provider == "kugou" {
            let (key, image) = super::kugou_qr_key().map_err(CaptureError::Io)?;
            let (sender, receiver) = mpsc::channel();
            let poll_key = key.clone();
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let poll_cancel = std::sync::Arc::clone(&cancel);
            thread::spawn(move || super::kugou_qr_poll_loop(&poll_key, sender, poll_cancel));
            let html = super::kugou_qr_page(&image);
            let context = match CaptureContext::new_with_html(provider, window.hwnd, &html) {
                Ok(context) => Rc::new(context),
                Err(error) => {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                    return Err(error);
                }
            };
            (context, Some(receiver), Some(cancel))
        } else {
            let context = Rc::new(CaptureContext::new(provider, window.hwnd)?);
            (context, None, None)
        };
        if let Some(receiver) = qr_receiver {
            let mut state = context.state.lock().expect("capture state mutex poisoned");
            state.kugou_qr_rx = Some(receiver);
            state.kugou_qr_cancel = qr_cancel;
        }
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
            context.poll_kugou_qr();
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
        QQ_REFRESH_COOKIE_GRACE_PERIOD, allowed_cookie_names, finish_qq_login_exchange,
        has_qq_refresh_cookies, has_required_cookies, login_url, merge_qq_cookie_updates,
        merge_qq_login_response_fields, normalize_qq_cookie_aliases, parse_qq_login_callback,
        qq_login_response_code, qq_login_response_data, should_complete_capture,
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

        let incomplete_refresh = BTreeMap::from([
            ("psrf_qqopenid".to_owned(), "open-id".to_owned()),
            (
                "psrf_qqrefresh_token".to_owned(),
                "refresh-token".to_owned(),
            ),
        ]);
        assert!(!has_qq_refresh_cookies(&incomplete_refresh));
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
    fn bilibili_capture_waits_for_refresh_token_after_sessdata() {
        let basic = BTreeMap::from([("SESSDATA".to_owned(), "session".to_owned())]);
        let start = Instant::now();
        let mut required_seen_at = None;

        assert!(!should_complete_capture(
            "bilibili",
            &basic,
            &mut required_seen_at,
            start,
        ));
        assert_eq!(required_seen_at, Some(start));
    }

    #[test]
    fn bilibili_capture_finishes_when_refresh_token_arrives() {
        let refreshable = BTreeMap::from([
            ("SESSDATA".to_owned(), "session".to_owned()),
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
    fn bilibili_capture_falls_back_to_basic_cookies_after_grace_period() {
        let basic = BTreeMap::from([("SESSDATA".to_owned(), "session".to_owned())]);
        let start = Instant::now();
        let mut required_seen_at = Some(start);

        assert!(should_complete_capture(
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
