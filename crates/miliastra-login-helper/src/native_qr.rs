//! Native QR login flows for providers whose web pages do not expose a stable
//! browser callback.  The worker owns all provider secrets and only emits a
//! validated image or a final, user-safe credential map to the WebView2 loop.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use image::{DynamicImage, ImageFormat, Luma};
use qrcode::QrCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE, USER_AGENT};
use rumqttc::Transport;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::mqttbytes::v5::{
    ConnectProperties, Packet, PublishProperties, SubscribeProperties,
};
use rumqttc::v5::{Client as MqttClient, Event as MqttEvent, MqttOptions};
use serde_json::{Value, json};

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_QR_ROUNDS: u8 = 3;
const QQ_CREATE_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const QQ_MQTT_HOST: &str = "wss://mu.y.qq.com/ws/handshake";
const BILIBILI_QR_URL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/generate";
const BILIBILI_POLL_URL: &str = "https://passport.bilibili.com/x/passport-login/web/qrcode/poll";
const BILIBILI_SPI_URL: &str = "https://api.bilibili.com/x/frontend/finger/spi";
const NETEASE_QR_URL: &str = "https://music.163.com/api/login/qrcode/unikey?type=1";
const NETEASE_POLL_URL: &str = "https://music.163.com/api/login/qrcode/client/login";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// Events are intentionally small: QR frames are sent to the parent process,
/// while status text stays local and never contains a provider token.
#[derive(Debug)]
pub(crate) enum NativeQrEvent {
    Qr(String),
    Success(BTreeMap<String, String>),
    Error(String),
}

pub(crate) type NativeQrSender = Sender<NativeQrEvent>;

pub(crate) fn page(provider: &str, image: &str) -> String {
    let label = match provider {
        "qqmusic" => "QQ 音乐",
        "netease" => "网易云音乐",
        "bilibili" => "哔哩哔哩",
        _ => "音乐平台",
    };
    let image = escape_html_attribute(image);
    let label = escape_html_attribute(label);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{label}登录</title></head>\
         <body style=\"margin:0;background:#f5f6f8;display:flex;flex-direction:column;align-items:center;justify-content:center;height:100vh;font-family:Segoe UI,sans-serif\">\
         <div style=\"background:#fff;padding:24px;border-radius:12px;box-shadow:0 2px 12px rgba(0,0,0,.08);text-align:center\">\
         <img src=\"{image}\" alt=\"{label}登录二维码\" style=\"width:min(70vw,420px);height:auto;display:block\">\
         <p style=\"color:#666;margin:18px 0 0;font-size:14px\">请使用 {label} App 扫码登录</p>\
         </div></body></html>"
    )
}

fn escape_html_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Create the first QR image and start the provider-specific polling worker.
/// The returned image is emitted by the caller before WebView2 is initialized,
/// so the HTTP UI can display it even while the embedded window is starting.
pub(crate) fn start(
    provider: &str,
    timeout: Duration,
    sender: NativeQrSender,
    cancel: Arc<AtomicBool>,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;
    match provider {
        "qqmusic" => {
            let (id, image) = qq_create_qr_until(Some(deadline))?;
            spawn_worker(
                "qqmusic",
                deadline,
                sender,
                cancel,
                move |sender, cancel| qq_poll_loop(id, deadline, sender, cancel),
            )?;
            Ok(image)
        }
        "netease" => {
            let (key, image) = netease_create_qr_until(Some(deadline))?;
            spawn_worker(
                "netease",
                deadline,
                sender,
                cancel,
                move |sender, cancel| netease_poll_loop(key, deadline, sender, cancel),
            )?;
            Ok(image)
        }
        "bilibili" => {
            let (key, image) = bilibili_create_qr_until(Some(deadline))?;
            spawn_worker(
                "bilibili",
                deadline,
                sender,
                cancel,
                move |sender, cancel| bilibili_poll_loop(key, deadline, sender, cancel),
            )?;
            Ok(image)
        }
        _ => Err("unsupported native QR provider".to_owned()),
    }
}

fn spawn_worker<F>(
    provider: &'static str,
    _deadline: Instant,
    sender: NativeQrSender,
    cancel: Arc<AtomicBool>,
    worker: F,
) -> Result<(), String>
where
    F: FnOnce(NativeQrSender, Arc<AtomicBool>) + Send + 'static,
{
    thread::Builder::new()
        .name(format!("{provider}-qr"))
        .spawn(move || worker(sender, cancel))
        .map(|_| ())
        .map_err(|_| "二维码轮询线程启动失败".to_owned())
}

fn http_client_with_timeout(timeout: Duration) -> Result<Client, String> {
    Client::builder()
        .timeout(timeout)
        .user_agent(UA)
        .build()
        .map_err(|_| "登录二维码网络客户端初始化失败".to_owned())
}

fn http_client() -> Result<Client, String> {
    http_client_with_timeout(HTTP_TIMEOUT)
}

fn http_timeout_until(deadline: Instant) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("登录二维码请求已超时".to_owned());
    }
    Ok(remaining.min(HTTP_TIMEOUT).max(Duration::from_millis(1)))
}

fn http_client_until(deadline: Instant) -> Result<Client, String> {
    http_client_with_timeout(http_timeout_until(deadline)?)
}

fn response_json(response: Response) -> Result<(HeaderMap, Value), String> {
    let response = response
        .error_for_status()
        .map_err(|_| "登录二维码接口请求失败".to_owned())?;
    let headers = response.headers().clone();
    let body = response
        .json::<Value>()
        .map_err(|_| "登录二维码接口响应格式无效".to_owned())?;
    Ok((headers, body))
}

fn validate_image(image: &str) -> Result<String, String> {
    miliastra_login_protocol::validate_qr_image_data_url(image)
        .map_err(|_| "登录二维码图片格式无效".to_owned())?;
    Ok(image.to_owned())
}

fn data_url_from_qr(content: &str) -> Result<String, String> {
    if content.trim().is_empty() {
        return Err("二维码内容为空".to_owned());
    }
    let code = QrCode::new(content.as_bytes()).map_err(|_| "二维码编码失败".to_owned())?;
    let image = code
        .render::<Luma<u8>>()
        .quiet_zone(true)
        .min_dimensions(360, 360)
        .max_dimensions(720, 720)
        .build();
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .map_err(|_| "二维码图片编码失败".to_owned())?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes.into_inner());
    validate_image(&format!("data:image/png;base64,{encoded}"))
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default()
}

fn sleep_or_cancel(cancel: &AtomicBool, duration: Duration, deadline: Instant) -> bool {
    let end = Instant::now() + duration;
    while Instant::now() < end {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(100));
    }
    true
}

fn emit_error(sender: &NativeQrSender, message: &str) {
    let _ = sender.send(NativeQrEvent::Error(message.to_owned()));
}

fn emit_success(sender: &NativeQrSender, cookies: BTreeMap<String, String>) {
    let _ = sender.send(NativeQrEvent::Success(cookies));
}

fn parse_set_cookie_headers(headers: &HeaderMap, allowed: &[&str]) -> BTreeMap<String, String> {
    let mut cookies = BTreeMap::new();
    for header in headers.get_all(SET_COOKIE).iter() {
        let Ok(text) = header.to_str() else {
            continue;
        };
        let Some((name, value)) = text.split(';').next().unwrap_or_default().split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if let Some(canonical) = allowed
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(name))
            && !value.is_empty()
        {
            cookies.insert((*canonical).to_owned(), value.to_owned());
        }
    }
    cookies
}

fn merge_cookie_header(cookies: &mut BTreeMap<String, String>, value: &str, allowed: &[&str]) {
    for part in value.split(';') {
        let Some((name, raw)) = part.trim().split_once('=') else {
            continue;
        };
        let name = name.trim();
        let raw = raw.trim();
        if let Some(canonical) = allowed
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(name))
            && !raw.is_empty()
        {
            cookies.insert((*canonical).to_owned(), raw.to_owned());
        }
    }
}

fn merge_netease_cookie_value(
    cookies: &mut BTreeMap<String, String>,
    value: &Value,
    allowed: &[&str],
) {
    match value {
        Value::String(value) => merge_cookie_header(cookies, value, allowed),
        Value::Object(fields) => {
            for name in allowed {
                let field = fields
                    .iter()
                    .find_map(|(field, value)| field.eq_ignore_ascii_case(name).then_some(value));
                if let Some(value) = field.and_then(value_from_cookie)
                    && !value.is_empty()
                {
                    cookies.insert((*name).to_owned(), value);
                }
            }
            let name = fields
                .iter()
                .find_map(|(field, value)| field.eq_ignore_ascii_case("name").then_some(value))
                .and_then(Value::as_str)
                .and_then(|name| {
                    allowed
                        .iter()
                        .find(|candidate| candidate.eq_ignore_ascii_case(name))
                        .copied()
                });
            let cookie_value = ["value", "val"].iter().find_map(|key| {
                fields
                    .iter()
                    .find_map(|(field, value)| field.eq_ignore_ascii_case(key).then_some(value))
                    .and_then(value_from_cookie)
            });
            if let Some(name) = name
                && let Some(value) = cookie_value.filter(|value| !value.is_empty())
            {
                cookies.insert(name.to_owned(), value);
            }
        }
        Value::Array(values) => {
            for value in values {
                merge_netease_cookie_value(cookies, value, allowed);
            }
        }
        _ => {}
    }
}

fn merge_netease_cookie_sources(
    cookies: &mut BTreeMap<String, String>,
    value: &Value,
    allowed: &[&str],
) {
    merge_netease_cookie_value(cookies, value, allowed);
    let Some(fields) = value.as_object() else {
        return;
    };
    // Providers have returned both a flat cookie field and nested `data` /
    // `result` envelopes over time.  Walk only envelope keys so unrelated
    // response objects cannot be mistaken for cookies.
    for key in ["cookie", "cookies", "data", "result", "payload", "body"] {
        if let Some(nested) = fields
            .iter()
            .find_map(|(field, value)| field.eq_ignore_ascii_case(key).then_some(value))
        {
            merge_netease_cookie_sources(cookies, nested, allowed);
        }
    }
}

#[cfg(test)]
fn bilibili_create_qr() -> Result<(String, String), String> {
    bilibili_create_qr_until(None)
}

fn bilibili_create_qr_until(deadline: Option<Instant>) -> Result<(String, String), String> {
    let client = match deadline {
        Some(deadline) => http_client_until(deadline)?,
        None => http_client()?,
    };
    let response = client
        .get(BILIBILI_QR_URL)
        .header("Referer", "https://www.bilibili.com/")
        .send()
        .map_err(|_| "哔哩哔哩二维码接口请求失败".to_owned())?;
    let (_, body) = response_json(response)?;
    if value_i64(body.get("code")) != Some(0) {
        return Err("哔哩哔哩二维码接口拒绝请求".to_owned());
    }
    let data = body
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| "哔哩哔哩二维码响应缺少 data".to_owned())?;
    let key = data
        .get("qrcode_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "哔哩哔哩二维码 key 缺失".to_owned())?
        .to_owned();
    let content = data
        .get("url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "哔哩哔哩二维码内容缺失".to_owned())?;
    Ok((key, data_url_from_qr(content)?))
}

fn bilibili_poll_loop(
    mut key: String,
    deadline: Instant,
    sender: NativeQrSender,
    cancel: Arc<AtomicBool>,
) {
    let client = match http_client_until(deadline) {
        Ok(client) => client,
        Err(error) => {
            emit_error(&sender, &error);
            return;
        }
    };
    let mut rounds = 0u8;
    let mut failures = 0u8;
    loop {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            return;
        }
        let response = client
            .get(BILIBILI_POLL_URL)
            .query(&[("qrcode_key", key.as_str())])
            .header("Referer", "https://www.bilibili.com/")
            .send();
        let (headers, body) = match response {
            Ok(response) => match response_json(response) {
                Ok(value) => {
                    failures = 0;
                    value
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    if failures >= 4 {
                        emit_error(&sender, &error);
                        return;
                    }
                    if !sleep_or_cancel(&cancel, POLL_INTERVAL, deadline) {
                        return;
                    }
                    continue;
                }
            },
            Err(_) => {
                failures = failures.saturating_add(1);
                if failures >= 4 {
                    emit_error(&sender, "哔哩哔哩二维码网络请求连续失败，请重试");
                    return;
                }
                if !sleep_or_cancel(&cancel, POLL_INTERVAL, deadline) {
                    return;
                }
                continue;
            }
        };
        match parse_bilibili_status(&body, headers) {
            BilibiliStatus::Waiting | BilibiliStatus::Scanned => {}
            BilibiliStatus::Success(cookies) => {
                // The QR poll response does not reliably include the device
                // cookies required by Bilibili's later API calls.  Fill them
                // from the public SPI endpoint before handing credentials to
                // the parent; a temporary SPI failure must not discard a
                // valid SESSDATA login.
                let cookies = enrich_bilibili_device_cookies(cookies, deadline);
                emit_success(&sender, cookies);
                return;
            }
            BilibiliStatus::Expired => {
                rounds = rounds.saturating_add(1);
                if rounds >= MAX_QR_ROUNDS {
                    emit_error(&sender, "哔哩哔哩二维码已过期，请重新发起登录");
                    return;
                }
                match bilibili_create_qr_until(Some(deadline)) {
                    Ok((new_key, image)) => {
                        key = new_key;
                        let _ = sender.send(NativeQrEvent::Qr(image));
                    }
                    Err(error) => {
                        emit_error(&sender, &error);
                        return;
                    }
                }
            }
            BilibiliStatus::Error(message) => {
                emit_error(&sender, &message);
                return;
            }
        }
        if !sleep_or_cancel(&cancel, POLL_INTERVAL, deadline) {
            return;
        }
    }
}

fn enrich_bilibili_device_cookies(
    mut cookies: BTreeMap<String, String>,
    deadline: Instant,
) -> BTreeMap<String, String> {
    if cookies
        .get("buvid3")
        .is_some_and(|value| !value.trim().is_empty())
        && cookies
            .get("buvid4")
            .is_some_and(|value| !value.trim().is_empty())
    {
        return cookies;
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return cookies;
    }
    let Ok(client) = http_client_until(deadline) else {
        return cookies;
    };
    let response = client
        .get(BILIBILI_SPI_URL)
        .header("Referer", "https://www.bilibili.com/")
        .header("Origin", "https://www.bilibili.com")
        .header("Cache-Control", "no-cache")
        .query(&[("_", now_millis().to_string())])
        .send();
    let Ok(response) = response else {
        return cookies;
    };
    let Ok((_, body)) = response_json(response) else {
        return cookies;
    };
    if value_i64(body.get("code")) != Some(0) {
        return cookies;
    }
    merge_bilibili_spi_fields(&mut cookies, &body);
    cookies
}

fn merge_bilibili_spi_fields(cookies: &mut BTreeMap<String, String>, body: &Value) {
    let Some(data) = body.get("data").and_then(Value::as_object) else {
        return;
    };
    for (source, target) in [("b_3", "buvid3"), ("b_4", "buvid4")] {
        if let Some(value) = data
            .get(source)
            .and_then(value_from_cookie)
            .filter(|value| !value.trim().is_empty())
        {
            cookies.insert(target.to_owned(), value);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum BilibiliStatus {
    Waiting,
    Scanned,
    Success(BTreeMap<String, String>),
    Expired,
    Error(String),
}

fn parse_bilibili_status(body: &Value, headers: HeaderMap) -> BilibiliStatus {
    if value_i64(body.get("code")) != Some(0) {
        return BilibiliStatus::Error("哔哩哔哩二维码接口返回错误".to_owned());
    }
    let Some(data) = body.get("data").and_then(Value::as_object) else {
        return BilibiliStatus::Error("哔哩哔哩二维码状态响应缺少 data".to_owned());
    };
    let code = value_i64(data.get("code")).unwrap_or(-1);
    match code {
        86101 => BilibiliStatus::Waiting,
        86090 => BilibiliStatus::Scanned,
        86038 => BilibiliStatus::Expired,
        0 => {
            let mut cookies = parse_set_cookie_headers(
                &headers,
                &[
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
                ],
            );
            if let Some(url) = data.get("url").and_then(Value::as_str)
                && let Ok(url) = url::Url::parse(url)
            {
                for (name, value) in url.query_pairs() {
                    if matches!(
                        name.as_ref(),
                        "SESSDATA"
                            | "bili_jct"
                            | "DedeUserID"
                            | "DedeUserID__ckMd5"
                            | "sid"
                            | "buvid3"
                            | "buvid4"
                            | "b_nut"
                            | "_uuid"
                            | "b_lsid"
                            | "ac_time_value"
                    ) && !value.is_empty()
                    {
                        cookies.insert(name.into_owned(), value.into_owned());
                    }
                }
            }
            if let Some(refresh) = data
                .get("refresh_token")
                .or_else(|| data.get("refreshToken"))
                .and_then(value_from_cookie)
            {
                cookies.insert("ac_time_value".to_owned(), refresh);
            }
            // SESSDATA is the only cookie the existing credential contract
            // requires.  bili_jct is useful for write APIs, but Bilibili does
            // not include it consistently in every successful QR response.
            if cookies
                .get("SESSDATA")
                .is_some_and(|value| !value.trim().is_empty())
            {
                BilibiliStatus::Success(cookies)
            } else {
                BilibiliStatus::Error("哔哩哔哩登录成功但凭据不完整".to_owned())
            }
        }
        _ => BilibiliStatus::Error("哔哩哔哩二维码登录状态未知".to_owned()),
    }
}

#[cfg(test)]
fn netease_create_qr() -> Result<(String, String), String> {
    netease_create_qr_until(None)
}

fn netease_create_qr_until(deadline: Option<Instant>) -> Result<(String, String), String> {
    let client = match deadline {
        Some(deadline) => http_client_until(deadline)?,
        None => http_client()?,
    };
    let cache_buster = now_millis().to_string();
    let response = client
        .post(NETEASE_QR_URL)
        .query(&[("_", cache_buster.as_str())])
        .header("Referer", "https://music.163.com/")
        .header("Cache-Control", "no-cache")
        .header("Pragma", "no-cache")
        .send()
        .map_err(|_| "网易云音乐二维码接口请求失败".to_owned())?;
    let (_, body) = response_json(response)?;
    if value_i64(body.get("code")) != Some(200)
        && value_i64(body.pointer("/data/code")) != Some(200)
    {
        return Err("网易云音乐二维码接口拒绝请求".to_owned());
    }
    let key = body
        .get("unikey")
        .or_else(|| body.pointer("/data/unikey"))
        .or_else(|| body.get("key"))
        .or_else(|| body.pointer("/data/key"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "网易云音乐二维码 key 缺失".to_owned())?
        .to_owned();
    let content = format!("https://music.163.com/login?codekey={key}");
    Ok((key, data_url_from_qr(&content)?))
}

fn netease_poll_loop(
    mut key: String,
    deadline: Instant,
    sender: NativeQrSender,
    cancel: Arc<AtomicBool>,
) {
    let client = match http_client_until(deadline) {
        Ok(client) => client,
        Err(error) => {
            emit_error(&sender, &error);
            return;
        }
    };
    let mut rounds = 0u8;
    let mut failures = 0u8;
    loop {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            return;
        }
        // The endpoint is fronted by a CDN that can replay an older poll
        // result.  A per-request cache key plus explicit no-cache headers are
        // needed to observe the transition from 801/802 to 803 promptly.
        let cache_buster = now_millis().to_string();
        let response = client
            .post(NETEASE_POLL_URL)
            .query(&[
                ("key", key.as_str()),
                ("type", "1"),
                ("_", cache_buster.as_str()),
            ])
            .header("Referer", "https://music.163.com/")
            .header("Cache-Control", "no-cache")
            .header("Pragma", "no-cache")
            .send();
        let (headers, body) = match response {
            Ok(response) => match response_json(response) {
                Ok(value) => {
                    failures = 0;
                    value
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    if failures >= 4 {
                        emit_error(&sender, &error);
                        return;
                    }
                    if !sleep_or_cancel(&cancel, POLL_INTERVAL, deadline) {
                        return;
                    }
                    continue;
                }
            },
            Err(_) => {
                failures = failures.saturating_add(1);
                if failures >= 4 {
                    emit_error(&sender, "网易云音乐二维码网络请求连续失败，请重试");
                    return;
                }
                if !sleep_or_cancel(&cancel, POLL_INTERVAL, deadline) {
                    return;
                }
                continue;
            }
        };
        match parse_netease_status(&body, headers) {
            NeteaseStatus::Waiting | NeteaseStatus::Scanned => {}
            NeteaseStatus::Success(cookies) => {
                emit_success(&sender, cookies);
                return;
            }
            NeteaseStatus::Expired => {
                rounds = rounds.saturating_add(1);
                if rounds >= MAX_QR_ROUNDS {
                    emit_error(&sender, "网易云音乐二维码已过期，请重新发起登录");
                    return;
                }
                match netease_create_qr_until(Some(deadline)) {
                    Ok((new_key, image)) => {
                        key = new_key;
                        let _ = sender.send(NativeQrEvent::Qr(image));
                    }
                    Err(error) => {
                        emit_error(&sender, &error);
                        return;
                    }
                }
            }
            NeteaseStatus::Error(message) => {
                emit_error(&sender, &message);
                return;
            }
        }
        if !sleep_or_cancel(&cancel, POLL_INTERVAL, deadline) {
            return;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum NeteaseStatus {
    Waiting,
    Scanned,
    Success(BTreeMap<String, String>),
    Expired,
    Error(String),
}

fn parse_netease_status(body: &Value, headers: HeaderMap) -> NeteaseStatus {
    let mut cookies = parse_set_cookie_headers(&headers, &["MUSIC_U", "__csrf"]);
    merge_netease_cookie_sources(&mut cookies, body, &["MUSIC_U", "__csrf"]);

    let code = netease_status_code(body);
    match code {
        Some(801) => NeteaseStatus::Waiting,
        Some(802) => NeteaseStatus::Scanned,
        Some(803) => {
            if cookies
                .get("MUSIC_U")
                .is_some_and(|value| !value.trim().is_empty())
            {
                NeteaseStatus::Success(cookies)
            } else {
                NeteaseStatus::Error("网易云音乐登录成功但未返回 MUSIC_U".to_owned())
            }
        }
        Some(800) => NeteaseStatus::Expired,
        Some(0) | Some(200) => {
            if cookies
                .get("MUSIC_U")
                .is_some_and(|value| !value.trim().is_empty())
            {
                NeteaseStatus::Success(cookies)
            } else {
                match netease_status_text(body) {
                    Some(text) => netease_status_from_text(&text).unwrap_or_else(|| {
                        NeteaseStatus::Error("网易云音乐二维码登录状态未知".to_owned())
                    }),
                    None => NeteaseStatus::Error("网易云音乐二维码登录状态未知".to_owned()),
                }
            }
        }
        Some(_) => NeteaseStatus::Error("网易云音乐二维码登录状态未知".to_owned()),
        None => {
            if cookies
                .get("MUSIC_U")
                .is_some_and(|value| !value.trim().is_empty())
            {
                NeteaseStatus::Success(cookies)
            } else {
                netease_status_text(body)
                    .and_then(|text| netease_status_from_text(&text))
                    .unwrap_or_else(|| {
                        NeteaseStatus::Error("网易云音乐二维码登录状态未知".to_owned())
                    })
            }
        }
    }
}

fn netease_status_code(body: &Value) -> Option<i64> {
    fn visit(value: &Value, depth: u8) -> Option<i64> {
        let object = value.as_object()?;
        let own_candidates = ["code", "status", "qrCodeStatus", "qrcodeStatus", "state"]
            .iter()
            .filter_map(|key| {
                object
                    .iter()
                    .find_map(|(field, value)| field.eq_ignore_ascii_case(key).then_some(value))
                    .and_then(|value| value_i64(Some(value)))
            })
            .collect::<Vec<_>>();
        let own = own_candidates
            .iter()
            .copied()
            .find(|code| !matches!(code, 0 | 200))
            .or_else(|| own_candidates.first().copied());
        let nested = if depth < 4 {
            ["data", "result", "response", "payload", "body"]
                .iter()
                .find_map(|key| {
                    object
                        .iter()
                        .find_map(|(field, value)| field.eq_ignore_ascii_case(key).then_some(value))
                        .and_then(|value| visit(value, depth + 1))
                })
        } else {
            None
        };
        // 0/200 are envelope success codes, not QR states.  A nested 801 /
        // 802 / 803 must win over them.
        if nested.is_some() && own.is_none_or(|code| matches!(code, 0 | 200)) {
            return nested;
        }
        if nested.is_some_and(|code| !matches!(code, 0 | 200)) {
            return nested;
        }
        own.or(nested)
    }
    visit(body, 0)
}

fn netease_status_text(body: &Value) -> Option<String> {
    fn visit(value: &Value, depth: u8) -> Option<String> {
        let object = value.as_object()?;
        let own_candidates = [
            "message",
            "msg",
            "statusText",
            "stateText",
            "description",
            "status",
            "state",
        ]
        .iter()
        .filter_map(|key| {
            object
                .iter()
                .find_map(|(field, value)| field.eq_ignore_ascii_case(key).then_some(value))
        })
        .filter_map(value_from_cookie)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>();
        let own = own_candidates
            .iter()
            .find(|text| !is_generic_netease_status_text(text))
            .cloned()
            .or_else(|| own_candidates.first().cloned());
        let nested = if depth < 4 {
            ["data", "result", "response", "payload", "body"]
                .iter()
                .find_map(|key| {
                    object
                        .iter()
                        .find_map(|(field, value)| field.eq_ignore_ascii_case(key).then_some(value))
                        .and_then(|value| visit(value, depth + 1))
                })
        } else {
            None
        };
        if let Some(text) = nested.as_ref() {
            let own_is_envelope = own.as_deref().is_some_and(|text| {
                matches!(
                    text.trim().to_ascii_lowercase().as_str(),
                    "ok" | "success" | "操作成功"
                )
            });
            if own.is_none() || own_is_envelope {
                return Some(text.clone());
            }
        }
        own.or(nested)
    }
    visit(body, 0)
}

fn is_generic_netease_status_text(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "ok" | "success" | "操作成功" | "200"
    )
}

fn netease_status_from_text(text: &str) -> Option<NeteaseStatus> {
    let normalized = text.trim().to_ascii_lowercase();
    if normalized.contains("801")
        || normalized.contains("等待扫码")
        || normalized.contains("wait scan")
        || normalized == "waiting"
        || normalized == "wait"
        || normalized.contains("not scan")
    {
        return Some(NeteaseStatus::Waiting);
    }
    if normalized.contains("802")
        || normalized.contains("扫描成功")
        || normalized.contains("已扫码")
        || normalized.contains("scanned")
        || normalized == "scan"
    {
        return Some(NeteaseStatus::Scanned);
    }
    if normalized.contains("800")
        || normalized.contains("二维码已过期")
        || normalized.contains("二维码不存在")
        || normalized.contains("expired")
    {
        return Some(NeteaseStatus::Expired);
    }
    if normalized.contains("803")
        || normalized.contains("授权登录成功")
        || normalized.contains("登录成功")
        || normalized.contains("login success")
        || normalized == "success"
    {
        return Some(NeteaseStatus::Error(
            "网易云音乐登录成功但未返回 MUSIC_U".to_owned(),
        ));
    }
    None
}

#[cfg(test)]
fn qq_create_qr() -> Result<(String, String), String> {
    qq_create_qr_until(None)
}

fn qq_create_qr_until(deadline: Option<Instant>) -> Result<(String, String), String> {
    let client = match deadline {
        Some(deadline) => http_client_until(deadline)?,
        None => http_client()?,
    };
    let payload = json!({
        "comm": {"ct": 23, "cv": 0, "tmeAppID": "qqmusic"},
        "req_0": {
            "module": "music.login.LoginServer",
            "method": "CreateQRCode",
            "param": {"tmeAppID": "qqmusic", "ct": 11, "cv": 14090008}
        }
    });
    let response = client
        .post(QQ_CREATE_URL)
        .header(USER_AGENT, UA)
        .header("Origin", "https://y.qq.com")
        .header("Referer", "https://y.qq.com/")
        .json(&payload)
        .send()
        .map_err(|_| "QQ 音乐二维码接口请求失败".to_owned())?;
    let (_, body) = response_json(response)?;
    if qq_business_code(&body) != Some(0) {
        return Err("QQ 音乐二维码接口拒绝请求".to_owned());
    }
    let data = body
        .get("req_0")
        .and_then(|value| value.get("data"))
        .and_then(Value::as_object)
        .ok_or_else(|| "QQ 音乐二维码响应缺少 data".to_owned())?;
    let id = data
        .get("qrcodeID")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "QQ 音乐二维码 ID 缺失".to_owned())?
        .to_owned();
    let image = data
        .get("qrcode")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "QQ 音乐二维码图片缺失".to_owned())?;
    Ok((id, validate_image(image)?))
}

fn qq_poll_loop(id: String, deadline: Instant, sender: NativeQrSender, cancel: Arc<AtomicBool>) {
    let mut attempts = 0u8;
    loop {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            return;
        }
        match qq_mqtt_session(&id, deadline, &cancel) {
            Ok(QqMqttOutcome::Success(cookies)) => {
                emit_success(&sender, cookies);
                return;
            }
            Ok(QqMqttOutcome::Expired) => {
                emit_error(&sender, "QQ 音乐二维码已过期，请重新发起登录");
                return;
            }
            Ok(QqMqttOutcome::Canceled) => {
                emit_error(&sender, "QQ 音乐二维码登录已取消");
                return;
            }
            Err(_) => {
                attempts = attempts.saturating_add(1);
                if attempts >= 3 {
                    emit_error(&sender, "QQ 音乐二维码连接失败，请重试");
                    return;
                }
                if !sleep_or_cancel(&cancel, Duration::from_secs(1), deadline) {
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
enum QqMqttOutcome {
    Success(BTreeMap<String, String>),
    Expired,
    Canceled,
}

fn qq_mqtt_session(
    id: &str,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<QqMqttOutcome, String> {
    let client_id = format!("{}{}", now_millis(), 1000 + (now_millis() % 9000));
    let mut options = MqttOptions::new(client_id, QQ_MQTT_HOST, 443);
    options
        .set_transport(Transport::wss_with_default_config())
        .set_keep_alive(Duration::from_secs(45))
        .set_connection_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_secs(8))
                .as_secs()
                .max(1),
        );
    let mut connect = ConnectProperties::new();
    connect.authentication_method = Some("pass".to_owned());
    connect.user_properties = vec![
        ("tmeAppID".to_owned(), "qqmusic".to_owned()),
        ("business".to_owned(), "management".to_owned()),
        ("hashTag".to_owned(), id.to_owned()),
        ("clientTag".to_owned(), "management.user".to_owned()),
        ("userID".to_owned(), id.to_owned()),
    ];
    options.set_connect_properties(connect);
    options.set_request_modifier(|mut request| async move {
        let headers = request.headers_mut();
        headers.insert("Origin", HeaderValue::from_static("https://y.qq.com"));
        headers.insert("Referer", HeaderValue::from_static("https://y.qq.com/"));
        headers.insert("User-Agent", HeaderValue::from_static(UA));
        request
    });
    let (client, mut connection) = MqttClient::new(options, 16);
    let topic = format!("management.qrcode_login/{id}");
    client
        .subscribe_with_properties(
            topic,
            QoS::AtMostOnce,
            SubscribeProperties {
                id: None,
                user_properties: vec![
                    ("authorization".to_owned(), "tmelogin".to_owned()),
                    ("pubsub".to_owned(), "unicast".to_owned()),
                ],
            },
        )
        .map_err(|_| "QQ 音乐二维码订阅失败".to_owned())?;

    loop {
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err("二维码登录已取消或超时".to_owned());
        }
        match connection.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(MqttEvent::Incoming(Packet::Publish(publish)))) => {
                let event_type = publish_event_type(publish.properties.as_ref(), &publish.payload);
                let normalized_event = event_type.as_deref().map(normalize_event_type);
                match normalized_event.as_deref() {
                    Some("scanned") | Some("scan") | Some("qrcodescanned") => continue,
                    Some("canceled") | Some("cancelled") => {
                        return Ok(QqMqttOutcome::Canceled);
                    }
                    Some("timeout") | Some("expired") => return Ok(QqMqttOutcome::Expired),
                    Some("loginfailed") | Some("failed") => {
                        return Err("QQ 音乐扫码登录失败".to_owned());
                    }
                    Some("cookies") | Some("cookie") | Some("success") | Some("loginsuccess") => {
                        let payload: Value = serde_json::from_slice(&publish.payload)
                            .map_err(|_| "QQ 音乐扫码响应格式无效".to_owned())?;
                        return qq_mobile_login(id, &payload, deadline).map(QqMqttOutcome::Success);
                    }
                    _ => {
                        // Some MQTT brokers omit the event property and put
                        // only the cookie object in the JSON payload.
                        if let Ok(payload) = serde_json::from_slice::<Value>(&publish.payload)
                            && qq_cookie_value(
                                &payload,
                                &["qqmusic_uin", "musicid", "musicuin", "uin", "wxuin"],
                            )
                            .is_some()
                            && qq_cookie_value(
                                &payload,
                                &[
                                    "qqmusic_key",
                                    "musickey",
                                    "musicKey",
                                    "key",
                                    "qm_keyst",
                                    "lqm_keyst",
                                ],
                            )
                            .is_some()
                        {
                            return qq_mobile_login(id, &payload, deadline)
                                .map(QqMqttOutcome::Success);
                        }
                    }
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return Err("QQ 音乐二维码连接失败".to_owned()),
            Err(rumqttc::v5::RecvTimeoutError::Timeout) => {}
            Err(_) => return Err("QQ 音乐二维码连接已断开".to_owned()),
        }
    }
}

fn publish_event_type(properties: Option<&PublishProperties>, payload: &[u8]) -> Option<String> {
    if let Some(properties) = properties
        && let Some((_, value)) = properties
            .user_properties
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("type"))
    {
        return Some(value.clone());
    }
    serde_json::from_slice::<Value>(payload)
        .ok()
        .and_then(|value| qq_cookie_value(&value, &["type", "eventType", "event", "status"]))
}

fn qq_mobile_login(
    id: &str,
    payload: &Value,
    deadline: Instant,
) -> Result<BTreeMap<String, String>, String> {
    let uin = qq_cookie_value(
        payload,
        &[
            "qqmusic_uin",
            "musicid",
            "musicuin",
            "musicUin",
            "str_musicid",
            "uin",
            "wxuin",
        ],
    )
    .ok_or_else(|| "QQ 音乐扫码响应缺少账号标识".to_owned())?;
    let token = qq_cookie_value(
        payload,
        &[
            "qqmusic_key",
            "musickey",
            "musicKey",
            "key",
            "qm_keyst",
            "lqm_keyst",
        ],
    )
    .ok_or_else(|| "QQ 音乐扫码响应缺少登录 token".to_owned())?;
    let music_id = uin
        .trim_start_matches('o')
        .parse::<u64>()
        .map_err(|_| "QQ 音乐扫码响应账号标识无效".to_owned())?;
    let client = http_client_until(deadline)?;
    let body = json!({
        "comm": {
            "v": 14090008,
            "ct": 11,
            "cv": 14090008,
            "chid": "2005000982",
            "tmeAppID": "qqmusic",
            "tmeLoginType": 6,
            "format": "json",
            "inCharset": "utf-8",
            "outCharset": "utf-8"
        },
        "req_0": {
            "module": "music.login.LoginServer",
            "method": "Login",
            "param": {"musicid": music_id, "qrCodeID": id, "token": token}
        }
    });
    let response = client
        .post(QQ_CREATE_URL)
        .header("Origin", "https://y.qq.com")
        .header("Referer", "https://y.qq.com/")
        .json(&body)
        .send()
        .map_err(|_| "QQ 音乐登录凭据请求失败".to_owned())?;
    let (headers, body) = response_json(response)?;
    if qq_business_code(&body) != Some(0) {
        return Err("QQ 音乐登录凭据被拒绝".to_owned());
    }
    let mut result = BTreeMap::new();
    // Keep the fields carried by the MQTT event.  In particular, newer mobile
    // clients put `accessToken` in this object while the Login response may
    // omit it; dropping it here made the saved account show accessToken as
    // missing even though the QR session had returned one.
    merge_qq_login_fields(&mut result, payload);
    result.extend(parse_set_cookie_headers(
        &headers,
        &[
            "uin",
            "wxuin",
            "qqmusic_key",
            "qm_keyst",
            "lqm_keyst",
            "openid",
            "access_token",
            "refresh_token",
            "refresh_key",
            "unionid",
        ],
    ));
    merge_qq_login_fields(&mut result, &body);
    if !result.contains_key("uin") && !result.contains_key("wxuin") {
        result.insert("uin".to_owned(), uin);
    }
    if !result.contains_key("qqmusic_key")
        && !result.contains_key("qm_keyst")
        && !result.contains_key("lqm_keyst")
    {
        result.insert("qqmusic_key".to_owned(), token);
    }
    result.insert("login_type".to_owned(), "6".to_owned());
    normalize_qq_aliases(&mut result);
    if !has_any(&result, &["uin", "wxuin"])
        || !has_any(&result, &["qqmusic_key", "qm_keyst", "lqm_keyst"])
    {
        return Err("QQ 音乐登录凭据不完整".to_owned());
    }
    Ok(result)
}

fn qq_cookie_value(cookies: &Value, names: &[&str]) -> Option<String> {
    qq_find_wire_value(cookies, names, 0).and_then(value_from_cookie)
}

fn qq_find_wire_value<'a>(value: &'a Value, names: &[&str], depth: u8) -> Option<&'a Value> {
    if depth >= 8 {
        return None;
    }
    match value {
        Value::Object(fields) => {
            // Cookie-list items use {name/key, value/val}; map-style payloads
            // use the cookie name as the object key.  Support both forms.
            if let Some(cookie_name) = fields
                .iter()
                .find_map(|(key, value)| {
                    (wire_key_matches(key, &["name", "key"]) && value.as_str().is_some())
                        .then_some(value)
                })
                .and_then(Value::as_str)
                && wire_key_matches(cookie_name, names)
            {
                for key in ["value", "val", "content"] {
                    if let Some(value) = fields
                        .iter()
                        .find_map(|(field, value)| wire_key_matches(field, &[key]).then_some(value))
                        .filter(|value| value_from_cookie(value).is_some())
                    {
                        return Some(value);
                    }
                }
            }
            for (key, nested) in fields {
                if wire_key_matches(key, names) {
                    if value_from_cookie(nested).is_some() {
                        return Some(nested);
                    }
                    if depth < 6
                        && let Some(found) = qq_find_wire_value(nested, names, depth + 1)
                    {
                        return Some(found);
                    }
                }
            }
            if depth >= 6 {
                return None;
            }
            // Envelopes are normally named, but walking remaining nested
            // objects keeps compatibility with provider-side wrapper changes.
            for nested in fields.values() {
                if let Some(found) = qq_find_wire_value(nested, names, depth + 1) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|item| qq_find_wire_value(item, names, depth.saturating_add(1))),
        _ => None,
    }
}

fn wire_key_matches(key: &str, aliases: &[&str]) -> bool {
    let normalized = key
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>();
    aliases.iter().any(|alias| {
        normalized
            == alias
                .bytes()
                .filter(u8::is_ascii_alphanumeric)
                .map(|byte| byte.to_ascii_lowercase())
                .collect::<Vec<_>>()
    })
}

fn value_from_cookie(value: &Value) -> Option<String> {
    if let Some(object) = value.as_object() {
        for key in ["value", "val", "content"] {
            if let Some(value) = object.get(key).map(value_string)
                && !value.is_empty()
            {
                return Some(value);
            }
        }
        None
    } else {
        let value = value_string(value);
        (!value.is_empty()).then_some(value)
    }
}

fn merge_qq_login_fields(cookies: &mut BTreeMap<String, String>, body: &Value) {
    for (sources, aliases) in [
        (
            &[
                "musicid",
                "musicuin",
                "musicUin",
                "str_musicid",
                "qqmusic_uin",
                "uin",
                "wxuin",
            ][..],
            &["uin", "wxuin"][..],
        ),
        (
            &[
                "musickey",
                "musicKey",
                "qqmusic_key",
                "qm_keyst",
                "lqm_keyst",
                "key",
            ][..],
            &["qqmusic_key", "qm_keyst", "lqm_keyst"][..],
        ),
        (
            &["openid", "openId", "wxopenid", "wxOpenId"][..],
            &["psrf_qqopenid", "openid", "wxopenid"][..],
        ),
        (
            &[
                "access_token",
                "accessToken",
                "wxaccess_token",
                "wxAccessToken",
                "psrf_qqaccess_token",
            ][..],
            &["psrf_qqaccess_token", "access_token", "wxaccess_token"][..],
        ),
        (
            &[
                "refresh_token",
                "refreshToken",
                "wxrefresh_token",
                "wxRefreshToken",
                "psrf_qqrefresh_token",
            ][..],
            &["psrf_qqrefresh_token", "refresh_token", "wxrefresh_token"][..],
        ),
        (
            &["refresh_key", "refreshKey", "psrf_qqrefresh_key"][..],
            &["psrf_qqrefresh_key", "refresh_key"][..],
        ),
        (
            &["unionid", "unionId", "wxunionid", "wxUnionId"][..],
            &["psrf_qqunionid", "unionid", "wxunionid"][..],
        ),
    ] {
        if let Some(value) = qq_cookie_value(body, sources) {
            for alias in aliases {
                cookies.insert((*alias).to_owned(), value.clone());
            }
        }
    }
}

fn qq_business_code(value: &Value) -> Option<i64> {
    fn visit(value: &Value, depth: u8) -> Option<i64> {
        let object = value.as_object()?;
        let own = ["code", "ret", "retcode"]
            .iter()
            .find_map(|field| {
                object
                    .iter()
                    .find_map(|(key, value)| wire_key_matches(key, &[field]).then_some(value))
            })
            .and_then(|value| value_i64(Some(value)));
        if depth < 6 {
            let mut nested_zero = None;
            for nested in object.values() {
                if let Some(code) = visit(nested, depth + 1) {
                    if code != 0 {
                        return Some(code);
                    }
                    nested_zero = Some(0);
                }
            }
            return own.or(nested_zero);
        }
        own
    }
    visit(value, 0)
}

fn normalize_qq_aliases(cookies: &mut BTreeMap<String, String>) {
    for aliases in [
        &["uin", "wxuin"][..],
        &["qqmusic_key", "qm_keyst", "lqm_keyst"][..],
        &["psrf_qqopenid", "openid", "wxopenid"][..],
        &["psrf_qqaccess_token", "access_token", "wxaccess_token"][..],
        &["psrf_qqrefresh_token", "refresh_token", "wxrefresh_token"][..],
        &["psrf_qqrefresh_key", "refresh_key"][..],
        &["psrf_qqunionid", "unionid", "wxunionid"][..],
    ] {
        let value = aliases
            .iter()
            .find_map(|name| {
                cookies
                    .get(*name)
                    .filter(|value| !value.trim().is_empty())
                    .cloned()
            })
            .or_else(|| {
                cookies.iter().find_map(|(name, value)| {
                    wire_key_matches(name, aliases)
                        .then_some(value)
                        .filter(|value| !value.trim().is_empty())
                        .cloned()
                })
            });
        if let Some(value) = value {
            for alias in aliases.iter() {
                cookies.insert((*alias).to_owned(), value.clone());
            }
        }
    }
}

fn has_any(cookies: &BTreeMap<String, String>, names: &[&str]) -> bool {
    names.iter().any(|name| {
        cookies
            .get(*name)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

fn value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
        .unwrap_or_default()
}

fn value_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_str()?.trim().parse().ok())
    })
}

fn normalize_event_type(value: &str) -> String {
    value
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|byte| byte.to_ascii_lowercase() as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers() -> HeaderMap {
        HeaderMap::new()
    }

    #[test]
    fn generated_qr_is_protocol_valid() {
        let image = data_url_from_qr("https://example.invalid/login?key=test").unwrap();
        assert!(image.starts_with("data:image/png;base64,"));
        assert!(image.len() < 700_000);
    }

    #[test]
    fn bilibili_status_parser_covers_wait_scan_expire() {
        assert_eq!(
            parse_bilibili_status(&json!({"code": 0, "data": {"code": 86101}}), headers()),
            BilibiliStatus::Waiting
        );
        assert_eq!(
            parse_bilibili_status(&json!({"code": 0, "data": {"code": 86090}}), headers()),
            BilibiliStatus::Scanned
        );
        assert_eq!(
            parse_bilibili_status(&json!({"code": 0, "data": {"code": 86038}}), headers()),
            BilibiliStatus::Expired
        );
    }

    #[test]
    fn bilibili_success_accepts_sessdata_without_bili_jct() {
        let mut response_headers = headers();
        response_headers.append(
            SET_COOKIE,
            HeaderValue::from_static("SESSDATA=session-value; Path=/; HttpOnly"),
        );
        assert!(matches!(
            parse_bilibili_status(&json!({"code": 0, "data": {"code": 0}}), response_headers),
            BilibiliStatus::Success(cookies)
                if cookies.get("SESSDATA") == Some(&"session-value".to_owned())
        ));
    }

    #[test]
    fn bilibili_success_accepts_camel_case_refresh_token() {
        let mut response_headers = headers();
        response_headers.append(
            SET_COOKIE,
            HeaderValue::from_static("SESSDATA=session-value; Path=/; HttpOnly"),
        );
        let status = parse_bilibili_status(
            &json!({
                "code": 0,
                "data": {"code": 0, "refreshToken": {"value": "refresh-value"}}
            }),
            response_headers,
        );
        assert!(matches!(
            status,
            BilibiliStatus::Success(cookies)
                if cookies.get("ac_time_value") == Some(&"refresh-value".to_owned())
        ));
    }

    #[test]
    fn netease_status_parser_accepts_cookie_string() {
        let mut body = json!({"code": 803, "cookie": "MUSIC_U=music-user; __csrf=csrf"});
        let result = parse_netease_status(&body, headers());
        assert!(
            matches!(result, NeteaseStatus::Success(cookies) if cookies.get("MUSIC_U") == Some(&"music-user".to_owned()))
        );
        body["code"] = json!(801);
        assert_eq!(
            parse_netease_status(&body, headers()),
            NeteaseStatus::Waiting
        );
    }

    #[test]
    fn netease_status_parser_accepts_cookie_object_and_array() {
        let body = json!({
            "code": 803,
            "cookies": [
                {"name": "MUSIC_U", "value": "music-user"},
                {"name": "__csrf", "value": "csrf"}
            ]
        });
        assert!(matches!(
            parse_netease_status(&body, headers()),
            NeteaseStatus::Success(cookies)
                if cookies.get("MUSIC_U") == Some(&"music-user".to_owned())
        ));

        let body = json!({
            "code": 803,
            "cookie": {"MUSIC_U": "object-user", "__csrf": "object-csrf"}
        });
        assert!(matches!(
            parse_netease_status(&body, headers()),
            NeteaseStatus::Success(cookies)
                if cookies.get("MUSIC_U") == Some(&"object-user".to_owned())
        ));
    }

    #[test]
    fn netease_status_parser_prefers_nested_qr_state_over_envelope_code() {
        let waiting = json!({
            "code": 200,
            "message": "OK",
            "data": {"code": "801", "message": "等待扫码"}
        });
        assert_eq!(
            parse_netease_status(&waiting, headers()),
            NeteaseStatus::Waiting
        );
        assert_eq!(
            parse_netease_status(
                &json!({"code": 200, "status": 801, "message": "OK"}),
                headers()
            ),
            NeteaseStatus::Waiting
        );

        let success = json!({
            "code": 200,
            "data": {
                "code": "803",
                "cookie": {"MUSIC_U": "nested-user", "__csrf": "nested-csrf"}
            }
        });
        assert!(matches!(
            parse_netease_status(&success, headers()),
            NeteaseStatus::Success(cookies)
                if cookies.get("MUSIC_U") == Some(&"nested-user".to_owned())
        ));
    }

    #[test]
    fn netease_status_parser_accepts_message_only_expiration() {
        assert_eq!(
            parse_netease_status(&json!({"message": "二维码已过期，请重新获取"}), headers()),
            NeteaseStatus::Expired
        );
    }

    #[test]
    fn netease_status_parser_canonicalizes_cookie_header_case() {
        let mut response_headers = headers();
        response_headers.append(
            SET_COOKIE,
            HeaderValue::from_static("music_u=header-user; Path=/"),
        );
        assert!(matches!(
            parse_netease_status(&json!({"code": 803}), response_headers),
            NeteaseStatus::Success(cookies)
                if cookies.get("MUSIC_U") == Some(&"header-user".to_owned())
        ));
    }

    #[test]
    fn mqtt_event_type_prefers_user_property_and_falls_back_to_json() {
        let properties = PublishProperties {
            user_properties: vec![("type".to_owned(), "cookies".to_owned())],
            ..PublishProperties::default()
        };
        assert_eq!(
            publish_event_type(Some(&properties), br#"{"type":"scanned"}"#),
            Some("cookies".to_owned())
        );
        assert_eq!(
            publish_event_type(None, br#"{"type":"scanned"}"#),
            Some("scanned".to_owned())
        );
        assert_eq!(normalize_event_type("login_success"), "loginsuccess");
    }

    #[test]
    fn qq_alias_normalization_keeps_refresh_fields_consistent() {
        let mut cookies = BTreeMap::from([
            ("uin".to_owned(), "123".to_owned()),
            ("qqmusic_key".to_owned(), "Q_H_L_key".to_owned()),
        ]);
        normalize_qq_aliases(&mut cookies);
        assert_eq!(cookies.get("wxuin").map(String::as_str), Some("123"));
        assert_eq!(
            cookies.get("qm_keyst").map(String::as_str),
            Some("Q_H_L_key")
        );
    }

    #[test]
    fn qq_business_code_reads_nested_login_error_and_data_aliases() {
        let body = json!({
            "code": 0,
            "req_0": {
                "code": 104400,
                "data": {"musicid": 123, "musickey": "Q_H_L_key", "refresh_key": "rk"}
            }
        });
        assert_eq!(qq_business_code(&body), Some(104400));

        let body = json!({
            "code": 0,
            "req_0": {
                "code": 0,
                "data": {"musicid": 123, "musickey": "Q_H_L_key", "refresh_key": "rk"}
            }
        });
        let mut cookies = BTreeMap::new();
        merge_qq_login_fields(&mut cookies, &body);
        assert_eq!(cookies.get("uin").map(String::as_str), Some("123"));
        assert_eq!(
            cookies.get("qqmusic_key").map(String::as_str),
            Some("Q_H_L_key")
        );
        assert_eq!(
            cookies.get("psrf_qqrefresh_key").map(String::as_str),
            Some("rk")
        );
    }

    #[test]
    fn qq_business_code_requires_an_explicit_success_or_error_code() {
        assert_eq!(
            qq_business_code(&json!({
                "req_0": {"data": {"musicid": 123, "musickey": "key"}}
            })),
            None
        );
        assert_eq!(qq_business_code(&json!({"code": "0"})), Some(0));
    }

    #[test]
    fn qq_login_fields_accept_cookie_style_objects() {
        let body = json!({
            "data": {
                "musicid": {"value": "456"},
                "musickey": {"value": "wrapped-key"},
                "refresh_key": {"value": "wrapped-refresh"}
            }
        });
        let mut cookies = BTreeMap::new();
        merge_qq_login_fields(&mut cookies, &body);
        assert_eq!(cookies.get("uin").map(String::as_str), Some("456"));
        assert_eq!(
            cookies.get("qqmusic_key").map(String::as_str),
            Some("wrapped-key")
        );
        assert_eq!(
            cookies.get("psrf_qqrefresh_key").map(String::as_str),
            Some("wrapped-refresh")
        );
    }

    #[test]
    fn qq_cookie_value_accepts_object_scalar_and_array_wire_forms() {
        let object = json!({
            "qqmusic_uin": {"value": "o123"},
            "qqmusic_key": {"value": "Q_H_L_key"}
        });
        assert_eq!(
            qq_cookie_value(&object, &["qqmusic_uin"]),
            Some("o123".to_owned())
        );

        let scalar = json!({"qqmusic_uin": "123", "qqmusic_key": "Q_H_L_key"});
        assert_eq!(
            qq_cookie_value(&scalar, &["qqmusic_key"]),
            Some("Q_H_L_key".to_owned())
        );

        let array = json!([
            {"name": "qqmusic_uin", "value": "123"},
            {"name": "qqmusic_key", "value": "Q_H_L_key"}
        ]);
        assert_eq!(
            qq_cookie_value(&array, &["qqmusic_key"]),
            Some("Q_H_L_key".to_owned())
        );
    }

    #[test]
    fn qq_wire_aliases_accept_camel_case_and_nested_cookie_envelopes() {
        let payload = json!({
            "event": "cookies",
            "data": {
                "cookies": {
                    "musicUin": "o123",
                    "musicKey": {"value": "Q_H_L_key"},
                    "accessToken": "access-token",
                    "refreshToken": "refresh-token",
                    "refreshKey": "refresh-key"
                }
            }
        });
        assert_eq!(
            qq_cookie_value(&payload, &["uin", "musicid", "musicUin"]),
            Some("o123".to_owned())
        );
        let mut fields = BTreeMap::new();
        merge_qq_login_fields(&mut fields, &payload);
        assert_eq!(fields.get("uin"), Some(&"o123".to_owned()));
        assert_eq!(fields.get("access_token"), Some(&"access-token".to_owned()));
        assert_eq!(
            fields.get("psrf_qqrefresh_key"),
            Some(&"refresh-key".to_owned())
        );
    }

    #[test]
    fn qq_alias_normalization_maps_camel_case_access_token() {
        let mut fields = BTreeMap::from([("accessToken".to_owned(), "token".to_owned())]);
        normalize_qq_aliases(&mut fields);
        assert_eq!(fields.get("access_token"), Some(&"token".to_owned()));
        assert_eq!(fields.get("psrf_qqaccess_token"), Some(&"token".to_owned()));
    }

    #[test]
    fn bilibili_spi_fields_map_public_device_names() {
        let mut cookies = BTreeMap::new();
        merge_bilibili_spi_fields(
            &mut cookies,
            &json!({"code": 0, "data": {"b_3": "buvid-3", "b_4": "buvid-4"}}),
        );
        assert_eq!(cookies.get("buvid3"), Some(&"buvid-3".to_owned()));
        assert_eq!(cookies.get("buvid4"), Some(&"buvid-4".to_owned()));
    }

    #[test]
    #[ignore = "需要公网访问平台登录接口"]
    fn live_qr_endpoints_return_protocol_valid_images() {
        let (_, qq) = qq_create_qr().expect("QQ QR");
        let (_, netease) = netease_create_qr().expect("NetEase QR");
        let (_, bilibili) = bilibili_create_qr().expect("Bilibili QR");
        for image in [qq, netease, bilibili] {
            miliastra_login_protocol::validate_qr_image_data_url(&image).unwrap();
        }
    }

    #[test]
    #[ignore = "需要公网访问 QQ MQTT 接口"]
    fn live_qq_mqtt_handshake_is_bounded() {
        let (id, _) = qq_create_qr().expect("QQ QR");
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(8);
        let _ = qq_mqtt_session(&id, deadline, &cancel);
    }
}
