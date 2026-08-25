//! 酷狗音乐公开接口适配器（纯 Rust 内置直连官方接口）。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use md5::compute;
use rand::{Rng, distributions::Alphanumeric};
use reqwest::{Client, StatusCode};
use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::catalog::{
    AccountStatusCache, CatalogError, PlaybackEligibility, ProviderSearchCandidate, SourceAdapter,
    account_status_error_code,
};
use crate::credentials::{CredentialStore, ProviderCredential};
use crate::domain::{ResolverLocator, SearchSpec, Song, SongKey, StreamSource};
use crate::lyrics::{TimedLyrics, parse_lrc_pair};

use super::kugou_crypto::{
    KUGOU_LITE_RSA_PUBLIC_KEY, decode_krc_base64, playlist_aes_decrypt_base64,
    playlist_aes_encrypt_base64, rsa_pkcs1_encrypt_hex,
};

const PROVIDER: &str = "kugou";

/// 酷狗测试版（概念版/lite）Android API 的签名盐值。
const KUGOU_LITE_SIGN_SALT: &str = "LnT6xpN3khm36zse0QzvmgTZ3waWdRSA";
/// `song_url` 的 `key` 参数使用的测试版（概念版/lite）盐值。
const KUGOU_LITE_KEY_SALT: &str = "185672dd44712f60bb1736df5a377e82";
const KUGOU_LITE_APPID: i64 = 3116;
const KUGOU_LITE_CLIENTVER: i64 = 11440;
const KUGOU_STREAM_CLIENTVER: i64 = 11430;
const KUGOU_UA: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";
const KUGOU_KG_RFC: &str = "B9EDA08A64250DEFFBCADDEE00F8F25F";
const KUGOU_DEVICE_FILE: &str = "kugou-device.json";
/// Web 播放接口使用的签名盐和客户端参数。网页登录得到的 `KuGoo.t`
/// 属于 Web 会话，不能送入上面的 Lite/Android 登录协议。
const KUGOU_WEB_SIGN_SALT: &str = "NVPh5oo715z5DIWAeQlhMDsWXXQV4hwt";
const KUGOU_WEB_SRCAPPID: &str = "2919";
const KUGOU_WEB_CLIENTVER: &str = "20000";
const KUGOU_WEB_APPID: &str = "1014";
const KUGOU_WEB_PLATID: &str = "4";
const KUGOU_WEB_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36";

/// 官方端点
const URL_SEARCH: &str = "https://gateway.kugou.com/v3/search/song";
const URL_SONG_STREAM: &str = "https://gateway.kugou.com/v5/url";
const URL_PRIVILEGE: &str = "https://gateway.kugou.com/v2/get_res_privilege/lite";
const URL_SEARCH_LYRIC: &str = "https://lyrics.kugou.com/v1/search";
const URL_DOWNLOAD_LYRIC: &str = "https://lyrics.kugou.com/download";
const URL_WEB_SONGINFO: &str = "https://wwwapi.kugou.com/play/songinfo";
const URL_YOUTH_UNION_VIP: &str = "https://kugouvip.kugou.com/v1/get_union_vip";
const URL_YOUTH_VIP: &str = "https://gateway.kugou.com/youth/v1/ad/play_report";
const URL_YOUTH_VIP_UPGRADE: &str =
    "https://gateway.kugou.com/youth/v1/listen_song/upgrade_vip_reward";

/// 测试版（概念版/lite）广告领取 VIP 的广告位 ID（官方 youth_vip.js 固定值）。
const KUGOU_VIP_AD_ID: u64 = 12_307_537_187;

/// 广告播放时长（毫秒），官方固定 30 秒。
const KUGOU_VIP_AD_PLAY_MS: u64 = 30_000;

/// 酷狗账户/VIP 服务返回的脱敏状态。
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KugouAccountStatus {
    pub logged_in: bool,
    pub user_id: Option<String>,
    pub nickname: Option<String>,
    pub vip: bool,
    pub vip_type: Option<String>,
    pub vip_expire_at_ms: Option<u64>,
    pub listen_report_available: bool,
}

/// 测试版（概念版/lite）VIP 领取结果（广告播放领取）。
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KugouListenReport {
    pub accepted: bool,
    pub vip_days: u32,
    pub message: String,
}

/// get_union_vip 响应解析出的 VIP 状态（测试版/概念版 busi_vip 优先）。
#[derive(Clone, Debug, Default)]
struct UnionVipStatus {
    /// 是否判定为 VIP；`None` 表示响应无法判定。
    active: Option<bool>,
    /// 业务线 VIP 类型（svip/tvip/...），来自 busi_vip。
    product_type: Option<String>,
    /// VIP 到期时间（毫秒），来自 busi_vip 的 vip_end_time。
    expire_at_ms: Option<u64>,
}

#[derive(Clone)]
pub struct KugouAdapter {
    client: Client,
    credentials: CredentialStore,
    legacy_device: Option<KugouDeviceIdentity>,
    /// Production playback routes Web songinfo through the WebView2 helper so
    /// the official SSA challenge can run in a real browser context.
    login_helper_executable: Option<PathBuf>,
    web_request_lock: Arc<tokio::sync::Mutex<()>>,
    /// 账号状态整体缓存（成功 24 小时/失败 15 分钟），防止轮询频繁打账号接口触发风控。
    account_cache: Arc<std::sync::Mutex<AccountStatusCache>>,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct KugouDeviceIdentity {
    guid: String,
    dev: String,
    mac: String,
}

/// Body structs below intentionally follow the property order used by the
/// corresponding KuGouMusicApi JavaScript modules.  Android signatures cover
/// the serialized body bytes, so an equivalent but differently ordered JSON
/// object is not interchangeable on the wire.
#[derive(Serialize)]
struct KugouAdPlayReportBody {
    ad_id: u64,
    play_end: u64,
    play_start: u64,
}

#[derive(Serialize)]
struct KugouPrivilegeResource {
    #[serde(rename = "type")]
    resource_type: &'static str,
    page_id: u8,
    hash: String,
    album_id: String,
}

#[derive(Serialize)]
struct KugouPrivilegeBody {
    appid: i64,
    area_code: u8,
    behavior: &'static str,
    clientver: i64,
    need_hash_offset: u8,
    relate: u8,
    support_verify: u8,
    resource: Vec<KugouPrivilegeResource>,
    qualities: [&'static str; 9],
}

fn serialize_kugou_body<T: Serialize>(body: &T) -> Result<String, CatalogError> {
    serde_json::to_string(body).map_err(|error| CatalogError::InvalidResponse(error.to_string()))
}

/// 账号 VIP 状态缓存有效期（判定成功时）。
const VIP_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 判定失败（接口风控/超时）时的短缓存，尽快重试账号接口。
const VIP_CACHE_FAILED_TTL: Duration = Duration::from_secs(15 * 60);

/// 计算测试版（概念版/lite）Android 请求签名。
///
/// 酷狗 Android 请求把排序后的 `key=value` 参数串、原始请求体和盐值
/// 一起计算 MD5；请求体必须是发送时完全相同的紧凑 JSON 字节序列。
pub fn kugou_android_signature(params: &BTreeMap<String, String>, body: &str) -> String {
    let mut pairs = params.iter().collect::<Vec<_>>();
    pairs.sort_by(|left, right| left.0.cmp(right.0));
    let joined = pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<String>();
    let digest = compute(format!(
        "{KUGOU_LITE_SIGN_SALT}{joined}{body}{KUGOU_LITE_SIGN_SALT}"
    ));
    format!("{digest:x}")
}

/// 计算酷狗 Web 接口签名。
///
/// Web 版本只对查询参数签名：按参数名排序后拼接 `key=value`，不包含
/// `&` 分隔符，也不包含最终的 `signature` 字段。
pub fn kugou_web_signature(params: &BTreeMap<String, String>) -> String {
    let joined = params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<String>();
    let digest = compute(format!(
        "{KUGOU_WEB_SIGN_SALT}{joined}{KUGOU_WEB_SIGN_SALT}"
    ));
    format!("{digest:x}")
}

/// `song_url` 请求的资源 key：MD5(hash + lite salt + appid + mid + userid)。
pub fn kugou_lite_sign_key(hash: &str, mid: &str, userid: &str) -> String {
    let user = if userid.is_empty() { "0" } else { userid };
    let device = if mid.is_empty() { "-" } else { mid };
    let digest = compute(format!(
        "{hash}{KUGOU_LITE_KEY_SALT}{KUGOU_LITE_APPID}{device}{user}"
    ));
    format!("{digest:x}")
}

/// Register the persistent device used by the lite client and obtain a
/// server-issued `dfid`. This is the Rust equivalent of the upstream
/// `/risk/v2/r_register_dev` module that the old sidecar called before URL
/// resolution.
pub fn kugou_register_device(
    token: &str,
    userid: &str,
    cookies: &BTreeMap<String, String>,
) -> Result<String, CatalogError> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct RegistrationPayload<'a> {
        #[serde(rename = "availableRamSize")]
        available_ram_size: u64,
        #[serde(rename = "availableRomSize")]
        available_rom_size: u64,
        // KuGou's register_dev payload preserves the acronym casing here;
        // serde's generic camelCase would emit `availableSdSize`.
        #[serde(rename = "availableSDSize")]
        available_sd_size: u64,
        baseband_ver: &'a str,
        battery_level: u8,
        battery_status: u8,
        brand: &'a str,
        build_serial: &'a str,
        device: &'a str,
        imei: &'a str,
        imsi: &'a str,
        manufacturer: &'a str,
        uuid: &'a str,
        accelerometer: bool,
        accelerometer_value: &'a str,
        gravity: bool,
        gravity_value: &'a str,
        gyroscope: bool,
        gyroscope_value: &'a str,
        light: bool,
        light_value: &'a str,
        magnetic: bool,
        magnetic_value: &'a str,
        orientation: bool,
        orientation_value: &'a str,
        pressure: bool,
        pressure_value: &'a str,
        #[serde(rename = "step_counter")]
        step_counter: bool,
        #[serde(rename = "step_counterValue")]
        step_counter_value: &'a str,
        temperature: bool,
        #[serde(rename = "temperatureValue")]
        temperature_value: &'a str,
    }
    #[derive(serde::Serialize)]
    struct RegistrationProof<'a> {
        aes: &'a str,
        uid: &'a str,
        token: &'a str,
    }

    let guid = cookies
        .get("KUGOU_API_GUID")
        .or_else(|| cookies.get("KugouGUID"))
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("miliastra-device");
    let guid = kugou_normalize_guid(guid);
    let device = RegistrationPayload {
        available_ram_size: 4_983_533_568,
        available_rom_size: 48_114_719,
        available_sd_size: 48_114_717,
        baseband_ver: "",
        battery_level: 100,
        battery_status: 3,
        brand: "Redmi",
        build_serial: "unknown",
        device: "marble",
        imei: &guid,
        imsi: "",
        manufacturer: "Xiaomi",
        uuid: &guid,
        accelerometer: false,
        accelerometer_value: "",
        gravity: false,
        gravity_value: "",
        gyroscope: false,
        gyroscope_value: "",
        light: false,
        light_value: "",
        magnetic: false,
        magnetic_value: "",
        orientation: false,
        orientation_value: "",
        pressure: false,
        pressure_value: "",
        step_counter: false,
        step_counter_value: "",
        temperature: false,
        temperature_value: "",
    };
    let temporary_key = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(6)
        .map(char::from)
        .collect::<String>()
        .to_ascii_lowercase();
    let encrypted_body = playlist_aes_encrypt_base64(
        &serde_json::to_vec(&device)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
        &temporary_key,
    )
    .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
    let proof = RegistrationProof {
        aes: &temporary_key,
        uid: if userid.is_empty() { "0" } else { userid },
        token,
    };
    let proof = rsa_pkcs1_encrypt_hex(
        &serde_json::to_vec(&proof)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
        KUGOU_LITE_RSA_PUBLIC_KEY,
    )
    .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
        .to_string();
    let dfid = cookies
        .get("dfid")
        .map(String::as_str)
        .filter(|value| {
            let value = value.trim();
            !value.is_empty() && !value.eq_ignore_ascii_case("undefined")
        })
        .unwrap_or("-");
    let mid = cookies
        .get("KUGOU_API_MID")
        .or_else(|| cookies.get("mid"))
        .or_else(|| cookies.get("kg_mid"))
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| kugou_calculate_mid(&guid));
    let mut params = BTreeMap::from([
        ("appid".to_owned(), KUGOU_LITE_APPID.to_string()),
        ("clientver".to_owned(), KUGOU_LITE_CLIENTVER.to_string()),
        ("clienttime".to_owned(), now),
        ("dfid".to_owned(), dfid.to_owned()),
        ("mid".to_owned(), mid),
        ("uuid".to_owned(), "-".to_owned()),
        ("part".to_owned(), "1".to_owned()),
        ("platid".to_owned(), "1".to_owned()),
        ("p".to_owned(), proof),
    ]);
    if !token.is_empty() {
        params.insert("token".to_owned(), token.to_owned());
    }
    if !userid.is_empty() {
        params.insert("userid".to_owned(), userid.to_owned());
    }
    let signature = kugou_android_signature(&params, &encrypted_body);
    params.insert("signature".to_owned(), signature);
    let mut cookie_values = cookies.clone();
    cookie_values.insert("token".to_owned(), token.to_owned());
    cookie_values.insert("userid".to_owned(), userid.to_owned());
    cookie_values.insert("dfid".to_owned(), dfid.to_owned());
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(KUGOU_UA)
        .build()
        .map_err(|error| CatalogError::Transient(error.to_string()))?
        .post("https://userservice.kugou.com/risk/v2/r_register_dev")
        .query(&params)
        .headers(reqwest::header::HeaderMap::from_iter([
            (
                reqwest::header::HeaderName::from_static("clienttime"),
                reqwest::header::HeaderValue::from_str(
                    params.get("clienttime").map(String::as_str).unwrap_or("0"),
                )
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            ),
            (
                reqwest::header::HeaderName::from_static("dfid"),
                reqwest::header::HeaderValue::from_str(dfid)
                    .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            ),
            (
                reqwest::header::HeaderName::from_static("mid"),
                reqwest::header::HeaderValue::from_str(
                    params.get("mid").map(String::as_str).unwrap_or("-"),
                )
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            ),
            (
                reqwest::header::HeaderName::from_static("kg-rc"),
                reqwest::header::HeaderValue::from_static("1"),
            ),
            (
                reqwest::header::HeaderName::from_static("kg-thash"),
                reqwest::header::HeaderValue::from_static("5d816a0"),
            ),
            (
                reqwest::header::HeaderName::from_static("kg-rec"),
                reqwest::header::HeaderValue::from_static("1"),
            ),
            (
                reqwest::header::HeaderName::from_static("kg-rf"),
                reqwest::header::HeaderValue::from_static(KUGOU_KG_RFC),
            ),
        ]))
        .header("Cookie", cookie_header(&cookie_values))
        // Axios 1.10 forwards this encrypted string without assigning a
        // content type; the registration endpoint expects that exact shape.
        .body(encrypted_body.clone())
        .send()
        .map_err(classify_request_error_blocking)?;
    let status = response.status();
    let bytes = response
        .bytes()
        .map_err(|error| CatalogError::Transient(error.to_string()))?;
    if !status.is_success() {
        return Err(CatalogError::Unavailable(format!(
            "Kugou device registration returned HTTP {status}"
        )));
    }
    let value = serde_json::from_slice::<Value>(&bytes)
        .or_else(|_| {
            let decrypted = playlist_aes_decrypt_base64(&bytes, &temporary_key)
                .map_err(|error| serde_json::Error::io(std::io::Error::other(error.to_string())))?;
            serde_json::from_slice::<Value>(&decrypted)
        })
        .map_err(|error| {
            CatalogError::InvalidResponse(format!(
                "Kugou device registration response is invalid: {error}"
            ))
        })?;
    classify_business_response(&value)?;
    let data = response_data(&value);
    let registered = data
        .get("dfid")
        .and_then(value_string)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CatalogError::CredentialRejected(
                "Kugou device registration returned no dfid".to_owned(),
            )
        })?;
    Ok(registered)
}

fn classify_request_error_blocking(error: reqwest::Error) -> CatalogError {
    if error.is_timeout() {
        CatalogError::TimedOut(error.to_string())
    } else {
        CatalogError::Transient(error.to_string())
    }
}

/// 计算酷狗设备 MID：md5(GUID) 的 hex 视为 128 位无符号大整数，转十进制。
pub fn kugou_calculate_mid(guid: &str) -> String {
    let digest = compute(guid.as_bytes());
    let hex = format!("{digest:x}");
    u128::from_str_radix(&hex, 16)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "-".to_owned())
}

/// Match KuGouMusicApi's device bootstrap: UUIDv4 values are first reduced
/// to their MD5 hex form, while already-normalized device identifiers are
/// kept unchanged. Older sidecars persisted the UUID form, so this helper is
/// also used when loading the shared device file after an upgrade.
pub fn kugou_normalize_guid(guid: &str) -> String {
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
        format!("{:x}", compute(guid.as_bytes()))
    } else {
        guid.to_owned()
    }
}

fn load_legacy_device_identity(credentials: &CredentialStore) -> Option<KugouDeviceIdentity> {
    let path = credentials.auxiliary_path(KUGOU_DEVICE_FILE)?;
    if !path.is_file() {
        return None;
    }
    let loaded = (|| -> Result<KugouDeviceIdentity, String> {
        let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let device = serde_json::from_str::<KugouDeviceIdentity>(&text)
            .map_err(|error| error.to_string())?;
        if device.guid.trim().is_empty()
            || device.dev.trim().is_empty()
            || device.mac.trim().is_empty()
        {
            return Err("device identity contains an empty field".to_owned());
        }
        Ok(KugouDeviceIdentity {
            guid: kugou_normalize_guid(&device.guid),
            dev: device.dev.trim().to_ascii_uppercase(),
            mac: device.mac.trim().to_ascii_uppercase(),
        })
    })();
    match loaded {
        Ok(device) => Some(device),
        Err(error) => {
            log::warn!("failed to load legacy KuGou device identity: {error}");
            None
        }
    }
}

fn insert_cookie_default(cookies: &mut BTreeMap<String, String>, name: &str, value: &str) {
    let missing = cookies.get(name).is_none_or(|current| {
        let current = current.trim();
        current.is_empty() || current == "-" || current.eq_ignore_ascii_case("undefined")
    });
    if missing {
        cookies.insert(name.to_owned(), value.to_owned());
    }
}

impl KugouAdapter {
    pub fn new(credentials: CredentialStore, timeout: Duration) -> Result<Self, CatalogError> {
        Self::new_with_login_helper(credentials, timeout, None)
    }

    pub fn new_with_login_helper(
        credentials: CredentialStore,
        timeout: Duration,
        login_helper_executable: Option<PathBuf>,
    ) -> Result<Self, CatalogError> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent(KUGOU_UA)
            .build()
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        let legacy_device = load_legacy_device_identity(&credentials);
        Ok(Self {
            client,
            credentials,
            legacy_device,
            login_helper_executable: login_helper_executable
                .filter(|path| !path.as_os_str().is_empty()),
            web_request_lock: Arc::new(tokio::sync::Mutex::new(())),
            account_cache: Arc::new(std::sync::Mutex::new(AccountStatusCache::default())),
        })
    }

    fn credential_with_device(&self, credential: &ProviderCredential) -> ProviderCredential {
        let Some(device) = self.legacy_device.as_ref() else {
            return credential.clone();
        };
        let ProviderCredential::Kugou {
            token,
            userid,
            dfid,
            cookies,
        } = credential
        else {
            return credential.clone();
        };
        let mut cookies = cookies.clone();
        insert_cookie_default(&mut cookies, "KUGOU_API_GUID", &device.guid);
        insert_cookie_default(&mut cookies, "KUGOU_API_DFID", dfid);
        insert_cookie_default(
            &mut cookies,
            "KUGOU_API_MID",
            &kugou_calculate_mid(&device.guid),
        );
        insert_cookie_default(&mut cookies, "KUGOU_API_DEV", &device.dev);
        insert_cookie_default(&mut cookies, "KUGOU_API_MAC", &device.mac);
        ProviderCredential::Kugou {
            token: token.clone(),
            userid: userid.clone(),
            dfid: dfid.clone(),
            cookies,
        }
    }

    fn credential_cookie_header(credential: &ProviderCredential) -> String {
        let mut cookies = credential.cookies().clone();
        let ProviderCredential::Kugou { .. } = credential else {
            return cookie_header(&cookies);
        };
        // Lite requests use the separately registered device identity. The
        // browser-only KuGoo cookie belongs to the Web session and must not
        // be mixed into this request family.
        cookies.remove("KuGoo");
        let ProviderCredential::Kugou { token, userid, .. } = credential else {
            unreachable!();
        };
        cookies.insert("token".to_owned(), token.clone());
        cookies.insert("userid".to_owned(), userid.clone());
        cookies.insert("dfid".to_owned(), Self::lite_credential_dfid(credential));
        cookie_header(&cookies)
    }

    fn web_credential_cookie_header(credential: &ProviderCredential) -> String {
        let ProviderCredential::Kugou { token, userid, .. } = credential else {
            return cookie_header(credential.cookies());
        };
        let mut cookies = credential.cookies().clone();
        let ku_goo = cookies
            .remove("KuGoo")
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("t={token}&KugooID={userid}"));

        // Do not leak the synthetic Lite identity into the browser request.
        // The raw KuGoo value and the Web dfid remain paired.
        for name in [
            "KUGOU_API_DFID",
            "KUGOU_API_GUID",
            "KUGOU_API_MID",
            "KUGOU_API_MAC",
            "KUGOU_API_DEV",
        ] {
            cookies.remove(name);
        }
        cookies.insert("KuGoo".to_owned(), ku_goo);
        // Some Web API deployments still read the split aliases even when
        // KuGoo is present. Keep both representations consistent.
        cookies.insert("token".to_owned(), token.clone());
        cookies.insert("userid".to_owned(), userid.clone());
        cookies.insert("dfid".to_owned(), Self::web_credential_dfid(credential));
        cookie_header(&cookies)
    }

    fn credential(&self) -> Result<ProviderCredential, CatalogError> {
        self.credentials
            .get(PROVIDER)
            .map_err(|error| CatalogError::AuthRequired(error.to_string()))?
            .ok_or_else(|| CatalogError::AuthRequired("Kugou credential_required".to_owned()))
            .map(|credential| self.credential_with_device(&credential))
    }

    fn credential_fields(
        credential: &ProviderCredential,
    ) -> Result<(&str, &str, &str), CatalogError> {
        let ProviderCredential::Kugou {
            token,
            userid,
            dfid,
            ..
        } = credential
        else {
            return Err(CatalogError::InvalidResponse(
                "Kugou candidate provider does not match adapter".to_owned(),
            ));
        };
        Ok((token, userid, dfid))
    }

    fn lite_credential_dfid(credential: &ProviderCredential) -> String {
        let ProviderCredential::Kugou { dfid, cookies, .. } = credential else {
            return "-".to_owned();
        };
        cookies
            .get("KUGOU_API_DFID")
            .filter(|value| {
                let value = value.trim();
                !value.is_empty() && !value.eq_ignore_ascii_case("undefined")
            })
            .cloned()
            .unwrap_or_else(|| dfid.clone())
    }

    fn web_credential_dfid(credential: &ProviderCredential) -> String {
        let ProviderCredential::Kugou { dfid, cookies, .. } = credential else {
            return "-".to_owned();
        };
        cookies
            .get("dfid")
            .filter(|value| {
                let value = value.trim();
                !value.is_empty() && !value.eq_ignore_ascii_case("undefined") && value != "-"
            })
            .cloned()
            .unwrap_or_else(|| dfid.clone())
    }

    /// 从登录捕获的 Cookie 中取出 Android 请求所需的设备 MID。
    /// `mid`/`kg_mid` 是服务端下发的十进制 MID；只有在旧凭据没有这两个
    /// 字段时才从 `KugouGUID` 按官方算法派生，不能把 dfid 当作 GUID。
    fn credential_mid(credential: Option<&ProviderCredential>) -> String {
        let Some(credential) = credential else {
            return "-".to_owned();
        };
        let cookies = credential.cookies();
        for name in ["KUGOU_API_MID", "mid", "kg_mid"] {
            if let Some(value) = cookies.get(name).map(String::as_str)
                && !value.trim().is_empty()
                && value != "undefined"
            {
                return value.trim().to_owned();
            }
        }
        cookies
            .get("KUGOU_API_GUID")
            .or_else(|| cookies.get("KugouGUID"))
            .filter(|value| !value.trim().is_empty())
            .map(|value| kugou_calculate_mid(&kugou_normalize_guid(value)))
            .unwrap_or_else(|| "-".to_owned())
    }

    /// Web 播放优先使用登录页面返回的 `mid`/`kg_mid`。持久化的
    /// `KUGOU_API_MID` 是 Lite 设备身份，不能覆盖 Web 会话的 MID。
    fn web_credential_mid(credential: &ProviderCredential) -> String {
        let cookies = credential.cookies();
        ["mid", "kg_mid", "KUGOU_API_MID"]
            .into_iter()
            .find_map(|name| {
                cookies.get(name).map(String::as_str).filter(|value| {
                    let value = value.trim();
                    !value.is_empty() && !value.eq_ignore_ascii_case("undefined")
                })
            })
            .map(str::to_owned)
            .unwrap_or_else(|| "-".to_owned())
    }

    fn device_params(
        credential: Option<&ProviderCredential>,
        appid: i64,
        clientver: i64,
    ) -> BTreeMap<String, String> {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_string();
        let (token, userid) = credential
            .and_then(|cred| Self::credential_fields(cred).ok())
            .map(|(token, userid, _)| (token, userid))
            .unwrap_or(("", ""));
        let dfid = credential
            .map(Self::lite_credential_dfid)
            .unwrap_or_else(|| "-".to_owned());
        let mut params = BTreeMap::from([
            ("appid".to_owned(), appid.to_string()),
            ("clientver".to_owned(), clientver.to_string()),
            ("clienttime".to_owned(), now_secs),
            (
                "dfid".to_owned(),
                if dfid.trim().is_empty() {
                    "-".to_owned()
                } else {
                    dfid
                },
            ),
            ("mid".to_owned(), Self::credential_mid(credential)),
            ("uuid".to_owned(), "-".to_owned()),
        ]);
        if !token.is_empty() {
            params.insert("token".to_owned(), token.to_owned());
        }
        if !userid.is_empty() {
            params.insert("userid".to_owned(), userid.to_owned());
        }
        params
    }

    fn signed_params(
        credential: Option<&ProviderCredential>,
        extra: Vec<(&str, String)>,
        body: &str,
    ) -> BTreeMap<String, String> {
        let mut params = Self::device_params(credential, KUGOU_LITE_APPID, KUGOU_LITE_CLIENTVER);
        for (k, v) in extra {
            params.insert(k.to_owned(), v);
        }
        let signature = kugou_android_signature(&params, body);
        params.insert("signature".to_owned(), signature);
        params
    }

    fn apply_device_headers(
        request: reqwest::RequestBuilder,
        params: &BTreeMap<String, String>,
    ) -> reqwest::RequestBuilder {
        request
            .header("User-Agent", KUGOU_UA)
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
    }

    /// Build the query used by `wwwapi.kugou.com/play/songinfo`.
    fn web_songinfo_params(
        credential: &ProviderCredential,
        selector: Option<(&str, String)>,
    ) -> Result<BTreeMap<String, String>, CatalogError> {
        let (token, userid, _) = Self::credential_fields(credential)?;
        let dfid = Self::web_credential_dfid(credential);
        let mut params = BTreeMap::from([
            ("srcappid".to_owned(), KUGOU_WEB_SRCAPPID.to_owned()),
            ("clientver".to_owned(), KUGOU_WEB_CLIENTVER.to_owned()),
            ("clienttime".to_owned(), epoch_ms().to_string()),
            ("mid".to_owned(), Self::web_credential_mid(credential)),
            ("uuid".to_owned(), Self::web_credential_mid(credential)),
            (
                "dfid".to_owned(),
                if dfid.trim().is_empty() {
                    "-".to_owned()
                } else {
                    dfid
                },
            ),
            ("appid".to_owned(), KUGOU_WEB_APPID.to_owned()),
            ("platid".to_owned(), KUGOU_WEB_PLATID.to_owned()),
            ("token".to_owned(), token.to_owned()),
            ("userid".to_owned(), userid.to_owned()),
        ]);
        if let Some((name, value)) = selector {
            params.insert(name.to_owned(), value);
        }
        Ok(params)
    }

    fn web_request_url(
        endpoint: &str,
        params: &BTreeMap<String, String>,
    ) -> Result<String, CatalogError> {
        let mut url = Url::parse(endpoint)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        {
            let mut query = url.query_pairs_mut();
            for (name, value) in params {
                query.append_pair(name, value);
            }
        }
        Ok(url.into())
    }

    fn web_request_cookies(credential: &ProviderCredential) -> BTreeMap<String, String> {
        let ProviderCredential::Kugou {
            token,
            userid,
            cookies,
            ..
        } = credential
        else {
            return BTreeMap::new();
        };
        let mut result = cookies.clone();
        if !result
            .get("KuGoo")
            .is_some_and(|value| !value.trim().is_empty())
        {
            result.insert("KuGoo".to_owned(), format!("t={token}&KugooID={userid}"));
        }
        if !result
            .get("dfid")
            .is_some_and(|value| !value.trim().is_empty())
        {
            result.insert("dfid".to_owned(), Self::web_credential_dfid(credential));
        }
        result
    }

    async fn web_request_via_helper(
        &self,
        url: String,
        credential: &ProviderCredential,
        executable: &Path,
    ) -> Result<Value, CatalogError> {
        let _request_guard = self.web_request_lock.lock().await;
        let profile = self
            .credentials
            .auxiliary_path(".kugou-web-request-profile")
            .ok_or_else(|| {
                CatalogError::Transient("Kugou WebView2 request profile is unavailable".to_owned())
            })?;
        std::fs::create_dir_all(&profile)
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        let ProviderCredential::Kugou { userid, .. } = credential else {
            return Err(CatalogError::InvalidResponse(
                "Kugou credential provider does not match adapter".to_owned(),
            ));
        };
        let request = serde_json::json!({
            "url": url,
            "userid": userid,
            "mid": Self::web_credential_mid(credential),
            "cookies": Self::web_request_cookies(credential),
        });
        let executable = executable.to_owned();
        let timeout = Duration::from_secs(30);
        Ok(tokio::task::spawn_blocking(move || {
            run_kugou_web_helper(&executable, &profile, timeout, &request)
        })
        .await
        .map_err(|error| CatalogError::Transient(error.to_string()))??)
    }

    /// Send a Web songinfo request without interpreting its business status.
    /// Resolve needs the complete error envelope so it can distinguish a VIP
    /// restriction from an invalid credential.
    async fn web_songinfo_json(
        &self,
        credential: &ProviderCredential,
        selector: Option<(&str, String)>,
    ) -> Result<Value, CatalogError> {
        let mut params = Self::web_songinfo_params(credential, selector)?;
        let signature = kugou_web_signature(&params);
        params.insert("signature".to_owned(), signature);
        if let Some(executable) = self.login_helper_executable.as_deref() {
            let url = Self::web_request_url(URL_WEB_SONGINFO, &params)?;
            return self
                .web_request_via_helper(url, credential, executable)
                .await;
        }
        let response = self
            .client
            .get(URL_WEB_SONGINFO)
            .query(&params)
            .header("User-Agent", KUGOU_WEB_UA)
            .header("Referer", "https://www.kugou.com/")
            .header("Origin", "https://www.kugou.com")
            .header("Cookie", Self::web_credential_cookie_header(credential))
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn validate_web_credential(
        &self,
        credential: &ProviderCredential,
    ) -> Result<(), CatalogError> {
        // Omitting the selector makes the endpoint return err_code=20010 for
        // a valid session, while an expired Web token returns err_code=30020.
        let response = self.web_songinfo_json(credential, None).await?;
        validate_web_credential_response(&response)
    }

    fn web_union_vip_params(
        credential: &ProviderCredential,
    ) -> Result<BTreeMap<String, String>, CatalogError> {
        // The union-VIP endpoint is served on a different Web origin, but it
        // expects the same signed Web session shape as play/songinfo. A bare
        // cookie request returns 20010 and has no membership data.
        let mut params = Self::web_songinfo_params(credential, None)?;
        params.insert("busi_type".to_owned(), "concept".to_owned());
        params.insert("opt_product_types".to_owned(), "dvip,qvip".to_owned());
        params.insert("product_type".to_owned(), "svip".to_owned());
        let signature = kugou_web_signature(&params);
        params.insert("signature".to_owned(), signature);
        Ok(params)
    }

    async fn web_union_vip_json(
        &self,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        let params = Self::web_union_vip_params(credential)?;
        if let Some(executable) = self.login_helper_executable.as_deref() {
            let url = Self::web_request_url(URL_YOUTH_UNION_VIP, &params)?;
            return self
                .web_request_via_helper(url, credential, executable)
                .await;
        }

        // Unit-test and embedding compatibility fallback. The production
        // runtime wires the helper executable, so authenticated VIP lookups
        // use the browser WebView2 session above.
        self.get_json_direct(
            URL_YOUTH_UNION_VIP,
            vec![
                ("busi_type", "concept".to_owned()),
                ("opt_product_types", "dvip,qvip".to_owned()),
                ("product_type", "svip".to_owned()),
            ],
            Some(credential),
        )
        .await
    }

    async fn get_json_direct(
        &self,
        endpoint: &str,
        query: Vec<(&str, String)>,
        credential: Option<&ProviderCredential>,
    ) -> Result<Value, CatalogError> {
        self.get_json_direct_with_headers(endpoint, query, credential, &[])
            .await
    }

    async fn get_json_direct_with_headers(
        &self,
        endpoint: &str,
        query: Vec<(&str, String)>,
        credential: Option<&ProviderCredential>,
        headers: &[(&str, &str)],
    ) -> Result<Value, CatalogError> {
        let signed = Self::signed_params(credential, query, "");
        let mut request =
            Self::apply_device_headers(self.client.get(endpoint).query(&signed), &signed);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }

        if let Some(cred) = credential {
            request = request.header("Cookie", Self::credential_cookie_header(cred));
        }

        let response = request.send().await.map_err(classify_request_error)?;
        classify_status(&response)?;
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        if endpoint != URL_SONG_STREAM {
            classify_business_response(&value)?;
        }
        Ok(value)
    }

    /// Send a signed request whose query is supplied verbatim.  The upstream
    /// lyric-search module opts into `clearDefaultParams`, so its signature
    /// covers only the endpoint-specific fields and not the normal device
    /// defaults.
    async fn get_json_explicit_params(
        &self,
        endpoint: &str,
        query: BTreeMap<String, String>,
        credential: Option<&ProviderCredential>,
    ) -> Result<Value, CatalogError> {
        let mut signed = query;
        let signature = kugou_android_signature(&signed, "");
        signed.insert("signature".to_owned(), signature);
        let device = Self::device_params(credential, KUGOU_LITE_APPID, KUGOU_LITE_CLIENTVER);
        let mut request =
            Self::apply_device_headers(self.client.get(endpoint).query(&signed), &device);
        if let Some(cred) = credential {
            request = request.header("Cookie", Self::credential_cookie_header(cred));
        }
        let response = request.send().await.map_err(classify_request_error)?;
        classify_status(&response)?;
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        classify_business_response(&value)?;
        Ok(value)
    }

    async fn post_json_direct(
        &self,
        endpoint: &str,
        query: Vec<(&str, String)>,
        body: Option<&Value>,
        credential: Option<&ProviderCredential>,
    ) -> Result<(Value, reqwest::header::HeaderMap), CatalogError> {
        self.post_json_direct_with_headers(endpoint, query, body, credential, &[])
            .await
    }

    async fn post_json_direct_with_headers(
        &self,
        endpoint: &str,
        query: Vec<(&str, String)>,
        body: Option<&Value>,
        credential: Option<&ProviderCredential>,
        headers: &[(&str, &str)],
    ) -> Result<(Value, reqwest::header::HeaderMap), CatalogError> {
        let body_text = body
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        self.post_json_text_direct_with_headers(
            endpoint,
            query,
            body_text.as_deref(),
            credential,
            headers,
        )
        .await
    }

    /// Send a JSON body whose serialized bytes are supplied by the caller.
    ///
    /// KuGou's Android signature includes the raw body text.  Keeping this
    /// path separate from `serde_json::Value` lets protocol-specific callers
    /// preserve the insertion order emitted by the upstream JavaScript
    /// `JSON.stringify` implementation.
    async fn post_json_text_direct_with_headers(
        &self,
        endpoint: &str,
        query: Vec<(&str, String)>,
        body_text: Option<&str>,
        credential: Option<&ProviderCredential>,
        headers: &[(&str, &str)],
    ) -> Result<(Value, reqwest::header::HeaderMap), CatalogError> {
        let signed = Self::signed_params(credential, query, body_text.unwrap_or(""));
        let mut request =
            Self::apply_device_headers(self.client.post(endpoint).query(&signed), &signed);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }

        if let Some(cred) = credential {
            request = request.header("Cookie", Self::credential_cookie_header(cred));
        }
        if let Some(body_text) = body_text {
            request = request
                .header("Content-Type", "application/json")
                .body(body_text.to_owned());
        }

        let response = request.send().await.map_err(classify_request_error)?;
        classify_status(&response)?;
        let headers = response.headers().clone();
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        classify_business_response(&value)?;
        Ok((value, headers))
    }

    async fn search_json(
        &self,
        keyword: &str,
        limit: usize,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        self.get_json_direct_with_headers(
            URL_SEARCH,
            vec![
                ("albumhide", "0".to_owned()),
                ("iscorrection", "1".to_owned()),
                ("keyword", keyword.to_owned()),
                ("nocollect", "0".to_owned()),
                ("page", "1".to_owned()),
                ("pagesize", limit.clamp(1, 30).to_string()),
                ("platform", "AndroidFilter".to_owned()),
            ],
            Some(credential),
            &[("x-router", "complexsearch.kugou.com")],
        )
        .await
    }

    async fn user_detail_json(
        &self,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        self.web_songinfo_json(credential, None).await
    }

    /// 测试版（概念版/lite）广告播放领取 VIP：上报一次完整广告播放（约 30 秒）。
    pub async fn claim_vip(&self) -> Result<KugouListenReport, CatalogError> {
        let credential = self.credential()?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let body = serialize_kugou_body(&KugouAdPlayReportBody {
            ad_id: KUGOU_VIP_AD_ID,
            play_end: now_ms,
            play_start: now_ms.saturating_sub(KUGOU_VIP_AD_PLAY_MS),
        })?;
        let (value, _) = self
            .post_json_text_direct_with_headers(
                URL_YOUTH_VIP,
                Vec::new(),
                Some(&body),
                Some(&credential),
                &[],
            )
            .await?;
        let data = response_data(&value);
        let code = value
            .get("code")
            .and_then(value_u64)
            .or_else(|| data.get("code").and_then(value_u64));
        let vip_days = data
            .get("vip_days")
            .or_else(|| data.get("vipDays"))
            .and_then(value_u64)
            .unwrap_or(0) as u32;
        Ok(KugouListenReport {
            accepted: code == Some(0),
            vip_days,
            message: value
                .get("message")
                .or_else(|| data.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("酷狗已接受广告播放上报")
                .to_owned(),
        })
    }

    /// 测试版（概念版/lite）升级 VIP：听歌奖励升级。
    pub async fn upgrade_vip(&self) -> Result<KugouListenReport, CatalogError> {
        let credential = self.credential()?;
        let (_, userid, _) = Self::credential_fields(&credential)?;
        let (value, _) = self
            .post_json_direct(
                URL_YOUTH_VIP_UPGRADE,
                vec![("kugouid", userid.to_owned()), ("ad_type", "1".to_owned())],
                None,
                Some(&credential),
            )
            .await?;
        let data = response_data(&value);
        let code = value
            .get("code")
            .and_then(value_u64)
            .or_else(|| data.get("code").and_then(value_u64));
        let vip_days = data
            .get("vip_days")
            .or_else(|| data.get("vipDays"))
            .and_then(value_u64)
            .unwrap_or(0) as u32;
        Ok(KugouListenReport {
            accepted: code == Some(0),
            vip_days,
            message: value
                .get("message")
                .or_else(|| data.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("酷狗已接受升级请求")
                .to_owned(),
        })
    }

    /// 刷新酷狗 token，并在成功后返回新凭据。
    pub async fn refresh_token(
        &self,
        credential: &ProviderCredential,
    ) -> Result<ProviderCredential, CatalogError> {
        let credential = self.credential_with_device(credential);
        // Web QR login returns a Web session token. KuGou has no compatible
        // Lite `login_by_token` refresh path for that session, so a refresh is
        // an authenticated Web validation and the still-valid snapshot is
        // returned unchanged.
        self.validate_web_credential(&credential).await?;
        Ok(credential)
    }

    /// 查询账号 VIP 状态。展示与播放共用同一份乐观缓存。
    async fn account_vip_status(&self) -> Option<bool> {
        self.account_status_cached(false)
            .await
            .ok()
            .filter(|status| status.vip_known)
            .map(|status| status.vip)
    }

    /// 解析 get_union_vip 系列响应中的 VIP 判定字段（is_vip / vip_type / vip）。
    fn vip_flags_from_union(data: &Value) -> Option<bool> {
        let is_vip = data.get("is_vip").and_then(value_u64);
        let vip_type = data
            .get("vip_type")
            .and_then(value_u64)
            .or_else(|| data.get("vip").and_then(value_u64));
        match (is_vip, vip_type) {
            (Some(value), _) => Some(value != 0),
            (None, Some(value)) => Some(value != 0),
            (None, None) => None,
        }
    }

    fn union_vip_status(data: &Value) -> UnionVipStatus {
        let mut status = UnionVipStatus::default();
        let candidate_data = [
            Some(data),
            data.get("data"),
            data.get("data").and_then(|data| data.get("data")),
        ];
        for candidate in candidate_data.into_iter().flatten() {
            let Some(list) = candidate
                .get("busi_vip")
                .and_then(Value::as_array)
                .filter(|list| !list.is_empty())
            else {
                continue;
            };
            for entry in list {
                if entry.get("is_vip").and_then(value_u64).unwrap_or(0) == 0 {
                    continue;
                }
                status.active = Some(true);
                if status.product_type.is_none() {
                    status.product_type = entry
                        .get("product_type")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                if status.expire_at_ms.is_none() {
                    status.expire_at_ms = entry
                        .get("vip_end_time")
                        .and_then(Value::as_str)
                        .and_then(parse_kugou_datetime_ms);
                }
            }
            if status.active.is_none() {
                status.active = Some(false);
            }
            return status;
        }
        status.active = candidate_data
            .into_iter()
            .flatten()
            .find_map(Self::vip_flags_from_union);
        status
    }

    pub async fn account_status(&self) -> Result<KugouAccountStatus, CatalogError> {
        let status = self.account_status_cached(false).await?;
        Ok(KugouAccountStatus {
            logged_in: status.logged_in,
            user_id: status.user_id,
            nickname: status.nickname,
            vip: status.vip,
            vip_type: status.vip_type,
            vip_expire_at_ms: status.vip_expire_at_ms,
            listen_report_available: status.logged_in,
        })
    }

    async fn account_status_cached(
        &self,
        force: bool,
    ) -> Result<crate::catalog::ProviderAccountStatus, CatalogError> {
        let (refresh_gate, cached) = {
            let guard = self
                .account_cache
                .lock()
                .map_err(|_| CatalogError::Transient("Kugou account cache poisoned".to_owned()))?;
            let cached = (!force)
                .then(|| guard.cached(VIP_CACHE_TTL, VIP_CACHE_FAILED_TTL))
                .flatten();
            (guard.refresh_gate(), cached)
        };
        if let Some(status) = cached {
            return Ok(status);
        }

        let mut waited_for_refresh = false;
        let _status_refresh = match refresh_gate.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                waited_for_refresh = true;
                refresh_gate.lock().await
            }
        };
        let (generation, cached) = {
            let guard = self
                .account_cache
                .lock()
                .map_err(|_| CatalogError::Transient("Kugou account cache poisoned".to_owned()))?;
            let cached = (!force || waited_for_refresh)
                .then(|| guard.cached(VIP_CACHE_TTL, VIP_CACHE_FAILED_TTL))
                .flatten();
            (guard.generation(), cached)
        };
        if let Some(status) = cached {
            return Ok(status);
        }

        let now = std::time::Instant::now();
        let checked_at_ms = epoch_ms();
        let result = self.query_account_status().await;
        let mut guard = self
            .account_cache
            .lock()
            .map_err(|_| CatalogError::Transient("Kugou account cache poisoned".to_owned()))?;
        match result {
            Ok(status) => {
                guard.store_if_current(generation, status.clone(), now, status.stale);
                Ok(status)
            }
            Err(error) => {
                if let Some(stale) = guard.stale_for_current(generation, &error) {
                    guard.store_if_current(generation, stale.clone(), now, true);
                    Ok(stale)
                } else if let Some(failed) =
                    guard.failed_for_current(generation, PROVIDER, checked_at_ms, &error)
                {
                    guard.store_if_current(generation, failed, now, true);
                    Err(error)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn query_account_status(
        &self,
    ) -> Result<crate::catalog::ProviderAccountStatus, CatalogError> {
        let checked_at_ms = epoch_ms();
        let credential = self.credential()?;
        let detail = self
            .user_detail_json(&credential)
            .await
            .and_then(|value| validate_web_credential_response(&value).map(|_| value));
        let vip = self.web_union_vip_json(&credential).await;

        match (detail, vip) {
            (Ok(detail), Ok(vip)) => {
                let status = Self::account_status_from_responses(
                    &credential,
                    Some(&detail),
                    Some(&vip),
                    checked_at_ms,
                );
                Ok(Self::degraded_when_vip_unknown(status, None))
            }
            (Ok(detail), Err(error)) => {
                let status = Self::account_status_from_responses(
                    &credential,
                    Some(&detail),
                    None,
                    checked_at_ms,
                );
                Ok(Self::degraded_when_vip_unknown(status, Some(&error)))
            }
            (Err(_), Ok(vip)) => {
                let status = Self::account_status_from_responses(
                    &credential,
                    None,
                    Some(&vip),
                    checked_at_ms,
                );
                Ok(Self::degraded_when_vip_unknown(status, None))
            }
            (Err(_), Err(error)) => Err(error),
        }
    }

    fn account_status_from_responses(
        credential: &ProviderCredential,
        detail: Option<&Value>,
        vip: Option<&Value>,
        checked_at_ms: u64,
    ) -> crate::catalog::ProviderAccountStatus {
        let credential_userid = match credential {
            ProviderCredential::Kugou { userid, .. } => Some(userid.clone()),
            _ => None,
        };
        let detail_data = detail.map(response_data);
        let vip_data = vip.map(response_data);
        let union_status = vip_data.map(Self::union_vip_status).unwrap_or_default();
        let vip_type = union_status
            .product_type
            .or_else(|| {
                vip_data
                    .and_then(|data| data.get("vip_type"))
                    .and_then(value_u64)
                    .map(|value| value.to_string())
            })
            .or_else(|| match credential {
                ProviderCredential::Kugou { cookies, .. } => cookies
                    .get("vip_type")
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(|value| value.to_string()),
                _ => None,
            });
        let detail_status_code = detail_data.and_then(|data| {
            data.get("error_code")
                .or_else(|| data.get("errorCode"))
                .or_else(|| data.get("err_code"))
                .and_then(value_u64)
        });
        let detail_rejected = detail.is_some_and(kugou_web_credential_is_rejected);
        crate::catalog::ProviderAccountStatus {
            provider: PROVIDER.to_owned(),
            logged_in: !detail_rejected
                && detail_status_code.is_none_or(|code| code == 0 || code == 20010),
            user_id: detail_data
                .and_then(|data| data.get("userid"))
                .and_then(value_string)
                .or(credential_userid),
            nickname: detail_data
                .and_then(|data| data.get("nickname"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            vip_known: union_status.active.is_some(),
            vip: union_status.active.unwrap_or(false),
            vip_type,
            vip_expire_at_ms: union_status.expire_at_ms.or_else(|| {
                vip_data
                    .and_then(|data| data.get("expire_time"))
                    .and_then(value_u64)
                    .map(|value| {
                        if value < 10_000_000_000 {
                            value * 1000
                        } else {
                            value
                        }
                    })
            }),
            login_method: None,
            checked_at_ms,
            stale: false,
            last_error: None,
        }
    }

    fn degraded_when_vip_unknown(
        mut status: crate::catalog::ProviderAccountStatus,
        error: Option<&CatalogError>,
    ) -> crate::catalog::ProviderAccountStatus {
        if !status.vip_known {
            status.stale = true;
            status.last_error = Some(
                error
                    .map(account_status_error_code)
                    .unwrap_or("provider_invalid_response")
                    .to_owned(),
            );
        }
        status
    }

    async fn lyrics_json(
        &self,
        hash: &str,
        album_audio_id: &str,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        let result = self
            .get_json_explicit_params(
                URL_SEARCH_LYRIC,
                BTreeMap::from([
                    ("album_audio_id".to_owned(), album_audio_id.to_owned()),
                    ("appid".to_owned(), KUGOU_LITE_APPID.to_string()),
                    ("clientver".to_owned(), KUGOU_LITE_CLIENTVER.to_string()),
                    ("duration".to_owned(), "0".to_owned()),
                    ("hash".to_owned(), hash.to_owned()),
                    ("keyword".to_owned(), String::new()),
                    ("lrctxt".to_owned(), "1".to_owned()),
                    ("man".to_owned(), "no".to_owned()),
                ]),
                Some(credential),
            )
            .await?;
        let result_data = response_data(&result);
        let Some(candidate) = result_data
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
        else {
            return Ok(Value::Null);
        };
        let Some(id) = candidate.get("id").and_then(value_string) else {
            return Ok(Value::Null);
        };
        let Some(accesskey) = candidate.get("accesskey").and_then(Value::as_str) else {
            return Ok(Value::Null);
        };
        self.get_json_direct(
            URL_DOWNLOAD_LYRIC,
            vec![
                ("id", id),
                ("accesskey", accesskey.to_owned()),
                ("ver", "1".to_owned()),
                ("client", "android".to_owned()),
                ("fmt", "lrc".to_owned()),
                ("charset", "utf8".to_owned()),
            ],
            Some(credential),
        )
        .await
    }
}

#[async_trait]
impl crate::catalog::KugouAccountAdapter for KugouAdapter {
    async fn refresh_token(
        &self,
        credential: &ProviderCredential,
    ) -> Result<ProviderCredential, CatalogError> {
        self.refresh_token(credential).await
    }
    async fn account_status(&self) -> Result<KugouAccountStatus, CatalogError> {
        self.account_status().await
    }
    async fn claim_vip(&self) -> Result<KugouListenReport, CatalogError> {
        self.claim_vip().await
    }
    async fn upgrade_vip(&self) -> Result<KugouListenReport, CatalogError> {
        self.upgrade_vip().await
    }
}

#[async_trait]
impl SourceAdapter for KugouAdapter {
    async fn validate_credential(
        &self,
        candidate: &ProviderCredential,
    ) -> Result<(), CatalogError> {
        let candidate = self.credential_with_device(candidate);
        self.validate_web_credential(&candidate).await
    }

    async fn search(
        &self,
        spec: &SearchSpec,
    ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
        let credential = self.credential()?;
        let response = self
            .search_json(spec.keyword.trim(), spec.limit, &credential)
            .await?;
        let mut candidates = parse_search_candidates(&response)?;
        candidates.truncate(spec.limit);
        Ok(candidates)
    }

    async fn resolve(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<StreamSource, CatalogError> {
        if key.source != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "song key provider does not match Kugou adapter".to_owned(),
            ));
        }
        let (hash, _album_id, _album_audio_id) = resolver_parts(key, locator)?;
        let credential = self.credential()?;
        let hash_lower = hash.to_ascii_lowercase();
        // The QR login returns a Web token. Resolve through the same Web
        // endpoint used by the browser: the first request translates the hash
        // to `encode_album_audio_id`, and the second returns the CDN URL.
        let first = self
            .web_songinfo_json(&credential, Some(("hash", hash_lower)))
            .await?;
        let url = if let Some(url) = extract_stream_url(&first) {
            url.to_owned()
        } else {
            let encoded_id = response_data(&first)
                .get("encode_album_audio_id")
                .and_then(value_string)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| classify_kugou_resolve_failure(&first))?;
            let second = self
                .web_songinfo_json(&credential, Some(("encode_album_audio_id", encoded_id)))
                .await?;
            extract_stream_url(&second)
                .map(str::to_owned)
                .ok_or_else(|| classify_kugou_resolve_failure(&second))?
        };
        let url =
            Url::parse(&url).map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(CatalogError::InvalidResponse(
                "Kugou returned a non-HTTP media URL".to_owned(),
            ));
        }
        Ok(StreamSource {
            url,
            headers: BTreeMap::from([("Referer".to_owned(), "https://www.kugou.com/".to_owned())]),
            expires_at_epoch_ms: None,
        })
    }

    async fn probe_eligibility(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<PlaybackEligibility, CatalogError> {
        if key.source != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "song key provider does not match Kugou adapter".to_owned(),
            ));
        }
        // The Lite privilege endpoint rejects WebView2 credentials. The Web
        // songinfo flow is authoritative for this session and also exercises
        // the exact URL that playback will use.
        let _ = self.resolve(key, locator).await?;
        Ok(PlaybackEligibility::Eligible)
    }

    async fn lyrics(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<Option<TimedLyrics>, CatalogError> {
        if key.source != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "song key provider does not match Kugou adapter".to_owned(),
            ));
        }
        let (hash, _, album_audio_id) = resolver_parts(key, locator)?;
        let credential = self.credential()?;
        let response = self
            .lyrics_json(&hash, &album_audio_id, &credential)
            .await?;
        if response.is_null() {
            return Ok(None);
        }
        let data = response_data(&response);
        let content = response
            .get("decodeContent")
            .or_else(|| response.get("content"))
            .or_else(|| data.get("decodeContent"))
            .or_else(|| data.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let lyric = decode_lrc_content(content)?;
        let translation = response
            .get("decodeTrcContent")
            .or_else(|| response.get("trcContent"))
            .or_else(|| data.get("decodeTrcContent"))
            .or_else(|| data.get("trcContent"))
            .and_then(Value::as_str)
            .map(decode_lrc_content)
            .transpose()?;
        parse_lrc_pair(&lyric, translation.as_deref())
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn account_status(
        &self,
    ) -> Result<Option<crate::catalog::ProviderAccountStatus>, CatalogError> {
        self.account_status_cached(false).await.map(Some)
    }

    async fn refresh_account_status(
        &self,
    ) -> Result<Option<crate::catalog::ProviderAccountStatus>, CatalogError> {
        self.account_status_cached(true).await.map(Some)
    }

    fn invalidate_account_status(&self) {
        if let Ok(mut guard) = self.account_cache.lock() {
            guard.invalidate();
        }
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn classify_kugou_resolve_failure(response: &Value) -> CatalogError {
    let data = response_data(response);
    let vip = data
        .get("vip")
        .or_else(|| data.get("is_vip"))
        .or_else(|| response.get("vip"))
        .and_then(value_u64);
    if vip == Some(1) {
        return CatalogError::VipRequired("Kugou track requires VIP membership".to_owned());
    }
    if let Err(error) = classify_business_response(response) {
        return error;
    }
    CatalogError::Unavailable("Kugou returned no playable stream".to_owned())
}

fn extract_stream_url(response: &Value) -> Option<&str> {
    let data = response_data(response);
    [data, response].into_iter().find_map(|value| {
        value
            .get("url")
            .and_then(first_stream_url)
            .or_else(|| value.get("backupUrl").and_then(first_stream_url))
            .or_else(|| value.get("backup_url").and_then(first_stream_url))
            .or_else(|| value.get("play_url").and_then(first_stream_url))
            .or_else(|| value.get("play_backup_url").and_then(first_stream_url))
    })
}

fn first_stream_url(value: &Value) -> Option<&str> {
    match value {
        Value::Array(items) => items
            .iter()
            .find_map(|item| item.as_str().filter(|url| !url.is_empty())),
        Value::String(text) => (!text.is_empty()).then_some(text.as_str()),
        _ => None,
    }
}

fn parse_search_candidates(response: &Value) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
    let data = response_data(response);
    let songs = data
        .get("lists")
        .or_else(|| data.get("info"))
        .or_else(|| data.pointer("/data/lists"))
        .or_else(|| data.pointer("/data/info"))
        .and_then(Value::as_array)
        .ok_or_else(|| CatalogError::InvalidResponse("Kugou search list is missing".to_owned()))?;
    Ok(songs
        .iter()
        .filter_map(|raw| {
            let hash = ["FileHash", "file_hash", "hash"]
                .iter()
                .find_map(|field| raw.get(*field).and_then(value_string))?;
            let hash = hash.trim();
            let title = [
                "OriSongName",
                "SongName",
                "songname",
                "FileName",
                "filename",
            ]
            .iter()
            .find_map(|field| raw.get(*field).and_then(value_string))?;
            let title = title.trim();
            if hash.is_empty()
                || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                || title.is_empty()
            {
                return None;
            }
            let album_id = kugou_search_identifier(
                ["AlbumID", "AlbumId", "album_id", "albumid"]
                    .iter()
                    .find_map(|field| raw.get(*field)),
            )?;
            let album_audio_id = kugou_search_identifier(
                [
                    "MixSongID",
                    "MixSongId",
                    "mixsongid",
                    "AlbumAudioID",
                    "album_audio_id",
                ]
                .iter()
                .find_map(|field| raw.get(*field)),
            )?;
            let artists = kugou_artists(raw);
            let duration_ms = ["Duration", "duration", "DurationMS", "duration_ms"]
                .iter()
                .find_map(|field| raw.get(*field))
                .and_then(value_u64)
                .map(|value| {
                    if value < 10_000 {
                        value.saturating_mul(1000)
                    } else {
                        value
                    }
                });
            let song = Song {
                key: SongKey::new(PROVIDER, hash).ok()?,
                resolver_locator: ResolverLocator::new(format!(
                    "kugou:{hash}:{album_id}:{album_audio_id}"
                ))
                .ok(),
                title: title.to_owned(),
                artists,
                album: raw
                    .get("AlbumName")
                    .or_else(|| raw.get("Album"))
                    .or_else(|| raw.get("album_name"))
                    .or_else(|| raw.get("albumname"))
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .map(str::to_owned),
                duration_ms,
            };
            Some(ProviderSearchCandidate {
                song,
                eligibility: if kugou_candidate_is_vip(raw) {
                    PlaybackEligibility::VipRequired
                } else {
                    PlaybackEligibility::Unknown
                },
            })
        })
        .collect::<Vec<_>>())
}

fn kugou_artists(raw: &Value) -> Vec<String> {
    let Some(value) = ["SingerName", "singername", "Singer", "singer", "artists"]
        .iter()
        .find_map(|field| raw.get(*field))
    else {
        return Vec::new();
    };
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .or_else(|| item.get("name").and_then(Value::as_str).map(str::to_owned))
            })
            .flat_map(|text| {
                text.split(['、', '/', '&'])
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect(),
        Value::Object(object) => object
            .get("name")
            .and_then(Value::as_str)
            .map(split_kugou_artists)
            .unwrap_or_default(),
        _ => value.as_str().map(split_kugou_artists).unwrap_or_default(),
    }
}

fn split_kugou_artists(value: &str) -> Vec<String> {
    value
        .split(['、', '/', '&'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn kugou_candidate_is_vip(raw: &Value) -> bool {
    let trans = ["TransParam", "trans_param", "transParam"]
        .iter()
        .find_map(|field| raw.get(*field))
        .and_then(parse_kugou_trans_param);
    for value in [Some(raw), trans.as_ref()].into_iter().flatten() {
        if ["display_vip", "displayVip", "vip", "is_vip", "isVip", "VIP"]
            .iter()
            .find_map(|field| value.get(*field).and_then(value_u64))
            .is_some_and(|flag| flag != 0)
        {
            return true;
        }
        if ["pay_type", "payType", "PayType"]
            .iter()
            .find_map(|field| value.get(*field).and_then(value_u64))
            .is_some_and(|pay_type| matches!(pay_type, 1 | 3))
        {
            return true;
        }
    }
    false
}

fn parse_kugou_trans_param(value: &Value) -> Option<Value> {
    match value {
        Value::Object(_) => Some(value.clone()),
        Value::String(text) => serde_json::from_str(text).ok(),
        _ => None,
    }
}

fn resolver_parts(
    key: &SongKey,
    locator: Option<&ResolverLocator>,
) -> Result<(String, String, String), CatalogError> {
    let (hash, album_id, album_audio_id) = match locator {
        Some(locator) => {
            let mut parts = locator.as_str().split(':');
            let source = parts.next();
            let hash = parts.next();
            let album_id = parts.next();
            let album_audio_id = parts.next();
            if source != Some(PROVIDER) || parts.next().is_some() {
                return Err(CatalogError::InvalidResolverLocator(
                    "Kugou locator format is invalid".to_owned(),
                ));
            }
            (
                hash.ok_or_else(|| {
                    CatalogError::InvalidResolverLocator("Kugou locator hash is missing".to_owned())
                })?,
                album_id.ok_or_else(|| {
                    CatalogError::InvalidResolverLocator(
                        "Kugou locator album id is missing".to_owned(),
                    )
                })?,
                album_audio_id.ok_or_else(|| {
                    CatalogError::InvalidResolverLocator(
                        "Kugou locator album audio id is missing".to_owned(),
                    )
                })?,
            )
        }
        None => (key.id.as_str(), "0", "0"),
    };
    if hash.is_empty()
        || !hash.eq_ignore_ascii_case(&key.id)
        || !hash.chars().all(|character| character.is_ascii_hexdigit())
    {
        return Err(CatalogError::InvalidResolverLocator(
            "Kugou locator hash is invalid".to_owned(),
        ));
    }
    let album_id = kugou_locator_identifier(album_id, "album id")?;
    let album_audio_id = kugou_locator_identifier(album_audio_id, "album audio id")?;
    Ok((hash.to_owned(), album_id, album_audio_id))
}

fn kugou_locator_identifier(value: &str, field: &str) -> Result<String, CatalogError> {
    let value = value.trim();
    if value.is_empty() {
        return Ok("0".to_owned());
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(CatalogError::InvalidResolverLocator(format!(
            "Kugou locator {field} is invalid"
        )));
    }
    Ok(value.to_owned())
}

fn cookie_header(cookies: &BTreeMap<String, String>) -> String {
    cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Invoke the WebView2-only helper with a bounded JSON-over-stdio protocol.
/// The helper owns the browser session and SSA verification; this function
/// intentionally does not inspect or reproduce the challenge protocol.
fn run_kugou_web_helper(
    executable: &Path,
    profile: &Path,
    timeout: Duration,
    request: &Value,
) -> Result<Value, CatalogError> {
    let input = serde_json::to_vec(request)
        .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
    let mut child = Command::new(executable)
        .arg("kugou")
        .arg("--kugou-web-request")
        .arg("--profile")
        .arg(profile)
        .arg("--timeout-seconds")
        .arg(timeout.as_secs().max(1).to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The helper must never forward browser diagnostics or request data
        // into the main process log.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| CatalogError::Transient(error.to_string()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&input)
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
    }

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CatalogError::TimedOut(
                    "Kugou WebView2 request timed out".to_owned(),
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CatalogError::Transient(error.to_string()));
            }
        }
    }

    let mut output = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(2 * 1024 * 1024)
            .read_to_end(&mut output)
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
    }
    let line = output
        .split(|byte| *byte == b'\n')
        .find(|line| !line.iter().all(u8::is_ascii_whitespace))
        .ok_or_else(|| {
            CatalogError::Transient("Kugou WebView2 helper returned no result".to_owned())
        })?;
    let envelope = serde_json::from_slice::<Value>(line).map_err(|_| {
        CatalogError::InvalidResponse("Kugou WebView2 helper result is invalid".to_owned())
    })?;
    if envelope.get("status").and_then(Value::as_str) == Some("success") {
        let response = envelope.get("response").cloned().ok_or_else(|| {
            CatalogError::InvalidResponse("Kugou WebView2 helper result has no response".to_owned())
        });
        if let Ok(response) = &response {
            log_kugou_web_vip_response_shape(request, response);
        }
        return response;
    }
    Err(CatalogError::Transient(
        "Kugou WebView2 request was not completed".to_owned(),
    ))
}

fn log_kugou_web_vip_response_shape(request: &Value, response: &Value) {
    if std::env::var_os("MILIASTRA_KUGOU_WEB_DIAGNOSTICS").is_none() {
        return;
    }
    let is_union_vip = request
        .get("url")
        .and_then(Value::as_str)
        .and_then(|url| Url::parse(url).ok())
        .is_some_and(|url| {
            url.host_str() == Some("kugouvip.kugou.com") && url.path() == "/v1/get_union_vip"
        });
    if !is_union_vip {
        return;
    }

    let data = response.get("data");
    let nested_data = data.and_then(|value| value.get("data"));
    let candidates = [Some(response), data, nested_data];
    let busi_vip = candidates.into_iter().flatten().find_map(|value| {
        value
            .get("busi_vip")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
    });
    let first_busi_vip = busi_vip.and_then(|items| items.first());
    let keys = |value: Option<&Value>| {
        value
            .and_then(Value::as_object)
            .map(|object| {
                let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
                keys.sort_unstable();
                keys.join(",")
            })
            .unwrap_or_default()
    };
    let vip_flag = candidates
        .into_iter()
        .flatten()
        .find_map(|value| value.get("is_vip").and_then(value_u64));
    let first_vip_flag = first_busi_vip.and_then(|value| value.get("is_vip").and_then(value_u64));
    let status = response.get("status").and_then(value_u64);
    let error_code = response
        .get("error_code")
        .or_else(|| response.get("errorCode"))
        .or_else(|| response.get("err_code"))
        .and_then(value_u64);
    let error_message_len = data
        .and_then(|value| value.get("errmsg"))
        .and_then(Value::as_str)
        .map(str::len);
    eprintln!(
        "[kugou-web-vip] status={status:?} error_code={error_code:?} errmsg_len={error_message_len:?} top_keys={} data_keys={} nested_data_keys={} busi_vip_count={} is_vip={:?} first_busi_is_vip={:?} first_busi_keys={}",
        keys(Some(response)),
        keys(data),
        keys(nested_data),
        busi_vip.map_or(0, |items| items.len()),
        vip_flag,
        first_vip_flag,
        keys(first_busi_vip),
    );
}

fn decode_lrc_content(content: &str) -> Result<String, CatalogError> {
    use base64::Engine;
    if content.trim().is_empty() {
        return Ok(String::new());
    }
    match base64::engine::general_purpose::STANDARD.decode(content) {
        Ok(bytes) if bytes.starts_with(b"krc1") => decode_krc_base64(content)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string())),
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string())),
        Err(_) => Ok(content.to_owned()),
    }
}

fn response_data(value: &Value) -> &Value {
    let mut current = value;
    // A few gateway deployments wrap the normal payload more than once as
    // `{data:{data:...}}`.  Unwrap only bounded, object-shaped protocol
    // envelopes so an ordinary payload field named `data` is not traversed
    // indefinitely.
    for _ in 0..3 {
        let Some(data) = current.get("data") else {
            break;
        };
        if data
            .as_object()
            .is_some_and(|object| object.contains_key("data"))
        {
            current = data;
            continue;
        }
        return data;
    }
    current.get("data").unwrap_or(current)
}

fn first_privilege_resource(value: &Value) -> Option<&Value> {
    fn find(value: &Value, depth: u8) -> Option<&Value> {
        if depth > 8 {
            return None;
        }
        match value {
            Value::Array(items) => items.iter().find_map(|item| {
                (kugou_pay_type(item).is_some())
                    .then_some(item)
                    .or_else(|| find(item, depth + 1))
            }),
            Value::Object(object) => {
                if kugou_pay_type(value).is_some() {
                    return Some(value);
                }
                for key in [
                    "resource",
                    "resources",
                    "list",
                    "lists",
                    "items",
                    "result",
                    "data",
                    "body",
                    "privilege",
                ] {
                    if let Some(found) = object.get(key).and_then(|child| find(child, depth + 1)) {
                        return Some(found);
                    }
                }
                None
            }
            _ => None,
        }
    }
    find(value, 0)
}

fn kugou_pay_type(value: &Value) -> Option<u64> {
    ["pay_type", "payType", "PayType", "paytype"]
        .iter()
        .find_map(|field| value.get(*field).and_then(value_u64))
        .or_else(|| {
            value.get("privilege").and_then(|privilege| {
                ["pay_type", "payType", "PayType", "paytype"]
                    .iter()
                    .find_map(|field| privilege.get(*field).and_then(value_u64))
            })
        })
        .or_else(|| {
            value.get("Privilege").and_then(|privilege| {
                ["pay_type", "payType", "PayType", "paytype"]
                    .iter()
                    .find_map(|field| privilege.get(*field).and_then(value_u64))
            })
        })
}

fn kugou_validation_is_rejected(response: &Value) -> bool {
    let data = response_data(response);
    [response, data].into_iter().any(|value| {
        ["error_code", "errorCode", "err_code", "code"]
            .into_iter()
            .filter_map(|field| value.get(field))
            .filter_map(value_i64)
            .any(|code| code != 0)
            || value
                .get("status")
                .and_then(value_i64)
                .is_some_and(|status| status == 0)
            || value.get("error").is_some_and(kugou_error_value_is_present)
    })
}

fn kugou_web_credential_is_rejected(response: &Value) -> bool {
    let data = response_data(response);
    [response, data].into_iter().any(|value| {
        ["err_code", "error_code", "errorCode", "code"]
            .into_iter()
            .filter_map(|field| value.get(field))
            .filter_map(value_i64)
            .any(|code| matches!(code, 20001 | 20018 | 20019 | 20020 | 30020 | 401 | 403))
    })
}

fn validate_web_credential_response(response: &Value) -> Result<(), CatalogError> {
    if !response.is_object() {
        return Err(CatalogError::InvalidResponse(
            "Kugou Web credential validation response is not an object".to_owned(),
        ));
    }
    if kugou_web_credential_is_rejected(response) {
        return Err(CatalogError::CredentialRejected(format!(
            "Kugou Web credential was rejected (code={})",
            kugou_web_rejection_code(response)
                .map(|code| code.to_string())
                .unwrap_or_else(|| "unknown".to_owned())
        )));
    }
    let status = find_kugou_business_number(response, &["status"]);
    let error_code =
        find_kugou_business_number(response, &["err_code", "error_code", "errorCode", "code"]);
    // With no hash the Web endpoint intentionally answers 20010 (missing
    // required song parameters). That response still proves the token was
    // accepted; 30020 is the explicit invalid-session response.
    if status.is_some_and(|status| status == 0)
        && error_code.is_some_and(|code| code != 20010 && code != 0)
    {
        return Err(CatalogError::InvalidResponse(
            "Kugou Web credential validation returned an unexpected error".to_owned(),
        ));
    }
    Ok(())
}

fn kugou_web_rejection_code(response: &Value) -> Option<i64> {
    let data = response_data(response);
    [response, data].into_iter().find_map(|value| {
        ["err_code", "error_code", "errorCode", "code"]
            .into_iter()
            .find_map(|field| value.get(field).and_then(value_i64))
            .filter(|code| matches!(code, 20001 | 20018 | 20019 | 20020 | 30020 | 401 | 403))
    })
}

fn value_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|value| value.try_into().ok()))
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn kugou_error_value_is_present(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(_) => value_i64(value).is_some_and(|code| code != 0),
        Value::String(value) => {
            let value = value.trim();
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        }
        Value::Array(values) => !values.is_empty(),
        Value::Object(_) | Value::Bool(true) => true,
    }
}

fn parse_kugou_datetime_ms(value: &str) -> Option<u64> {
    let parts = value.split(['-', ' ', ':']).collect::<Vec<_>>();
    if parts.len() != 6 {
        return None;
    }
    let year: u64 = parts[0].parse().ok()?;
    let month: u64 = parts[1].parse().ok()?;
    let day: u64 = parts[2].parse().ok()?;
    let hour: u64 = parts[3].parse().ok()?;
    let minute: u64 = parts[4].parse().ok()?;
    let second: u64 = parts[5].parse().ok()?;
    let days = days_from_civil(year, month, day)?;
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1000)
}

fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = year / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

fn value_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn kugou_search_identifier(value: Option<&Value>) -> Option<String> {
    let Some(value) = value else {
        return Some("0".to_owned());
    };
    if value.is_null() {
        return Some("0".to_owned());
    }
    let value = value_string(value)?;
    let value = value.trim();
    if value.is_empty() {
        return Some("0".to_owned());
    }
    value
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| value.to_owned())
}

fn value_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn classify_status(response: &reqwest::Response) -> Result<(), CatalogError> {
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(CatalogError::CredentialRejected(
            "Kugou rejected credential".to_owned(),
        )),
        StatusCode::TOO_MANY_REQUESTS => Err(CatalogError::RateLimited(
            "Kugou rate limit reached".to_owned(),
        )),
        status if status.is_server_error() => Err(CatalogError::Transient(format!(
            "Kugou returned HTTP {status}"
        ))),
        status if !status.is_success() => Err(CatalogError::Unavailable(format!(
            "Kugou returned HTTP {status}"
        ))),
        _ => Ok(()),
    }
}

fn classify_business_response(value: &Value) -> Result<(), CatalogError> {
    let data = response_data(value);
    let status = find_kugou_business_number(value, &["status"])
        .or_else(|| data.get("status").and_then(value_i64));
    let error_code = find_kugou_business_number(value, &["err_code", "error_code", "errorCode"])
        .or_else(|| data.get("error_code").and_then(value_i64))
        .or_else(|| data.get("errorCode").and_then(value_i64))
        .or_else(|| data.get("err_code").and_then(value_i64));
    let response_code = find_kugou_root_business_number(value, "code");
    if status.is_some_and(|status| status == 0)
        || error_code.is_some_and(|error_code| error_code != 0)
        || response_code.is_some_and(|code| code != 0)
    {
        let code = [error_code, response_code, status]
            .into_iter()
            .flatten()
            .find(|code| *code != 0)
            .unwrap_or_default();
        if matches!(code, 20001 | 20018 | 20019 | 20020 | 30020 | 401 | 403) {
            return Err(CatalogError::CredentialRejected(
                "Kugou rejected the request credentials".to_owned(),
            ));
        }
        return Err(CatalogError::InvalidResponse(
            "Kugou API returned an error response".to_owned(),
        ));
    }
    Ok(())
}

/// Find a protocol status/error field through the small set of envelopes used
/// by KuGou gateways. Song metadata is deliberately not traversed.
fn find_kugou_business_number(value: &Value, fields: &[&str]) -> Option<i64> {
    fn visit(value: &Value, fields: &[&str], depth: u8) -> Option<i64> {
        if depth > 4 {
            return None;
        }
        let object = value.as_object()?;
        if let Some(number) = fields
            .iter()
            .find_map(|field| object.get(*field).and_then(value_i64))
        {
            return Some(number);
        }
        for key in ["data", "result", "body", "response"] {
            if let Some(child) = object
                .get(key)
                .and_then(|child| visit(child, fields, depth + 1))
            {
                return Some(child);
            }
        }
        None
    }
    visit(value, fields, 0)
}

/// `code` is used by a few gateway envelopes, but song metadata can also have
/// a field with that name.  Restrict this lookup to the root and one known
/// response wrapper; never recurse through lists or arbitrary nested objects.
fn find_kugou_root_business_number(value: &Value, field: &str) -> Option<i64> {
    let object = value.as_object()?;
    object.get(field).and_then(value_i64).or_else(|| {
        ["data", "result", "body", "response"]
            .iter()
            .find_map(|key| {
                object
                    .get(*key)?
                    .as_object()?
                    .get(field)
                    .and_then(value_i64)
            })
    })
}

fn classify_request_error(error: reqwest::Error) -> CatalogError {
    if error.is_timeout() {
        CatalogError::TimedOut(error.to_string())
    } else {
        CatalogError::Transient(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use serde_json::json;
    use std::collections::BTreeMap;

    use super::{
        KugouAdPlayReportBody, KugouPrivilegeBody, KugouPrivilegeResource,
        classify_business_response, classify_kugou_resolve_failure, decode_lrc_content,
        extract_stream_url, kugou_android_signature, kugou_calculate_mid, kugou_lite_sign_key,
        kugou_normalize_guid, kugou_validation_is_rejected, load_legacy_device_identity,
        parse_kugou_datetime_ms, parse_search_candidates, resolver_parts, serialize_kugou_body,
        validate_web_credential_response,
    };
    use crate::catalog::CatalogError;
    use crate::credentials::ProviderCredential;
    use crate::domain::{ResolverLocator, SongKey};

    #[test]
    fn mid_calculation_is_decimal() {
        let mid = kugou_calculate_mid("test-guid-123");
        assert!(!mid.is_empty());
        assert!(mid.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn uuidv4_guid_is_normalized_once_before_mid_calculation() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let normalized = kugou_normalize_guid(uuid);
        assert_eq!(normalized.len(), 32);
        assert!(normalized.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let expected =
            u128::from_str_radix(&format!("{:x}", md5::compute(normalized.as_bytes())), 16)
                .unwrap()
                .to_string();
        assert_eq!(kugou_calculate_mid(&normalized), expected);
        assert_ne!(kugou_calculate_mid(&normalized), kugou_calculate_mid(uuid));
        assert_eq!(kugou_normalize_guid("device-guid"), "device-guid");
    }

    #[test]
    fn android_signature_sorts_params_and_includes_body() {
        let params = BTreeMap::from([
            ("appid".to_owned(), "3116".to_owned()),
            ("clientver".to_owned(), "11440".to_owned()),
            ("clienttime".to_owned(), "1700000000".to_owned()),
            ("dfid".to_owned(), "dfid".to_owned()),
            ("mid".to_owned(), "123".to_owned()),
            ("uuid".to_owned(), "-".to_owned()),
            ("token".to_owned(), "tok".to_owned()),
            ("userid".to_owned(), "42".to_owned()),
            ("keyword".to_owned(), "海阔天空".to_owned()),
        ]);
        assert_eq!(
            kugou_android_signature(&params, "{}"),
            "6805e7e467eb0f16c702de7388a3431a"
        );
        assert_eq!(
            kugou_lite_sign_key("ABC", "123", "42"),
            "8d3218976947812b23207c437f69dd5f"
        );
    }

    #[test]
    fn playback_signature_matches_upstream_case_sensitive_sorting_vector() {
        let mut params = BTreeMap::from([
            ("album_id".to_owned(), "123456".to_owned()),
            ("area_code".to_owned(), "1".to_owned()),
            (
                "hash".to_owned(),
                "abcdef0123456789abcdef0123456789".to_owned(),
            ),
            ("ssa_flag".to_owned(), "is_fromtrack".to_owned()),
            ("version".to_owned(), "11430".to_owned()),
            ("page_id".to_owned(), "967177915".to_owned()),
            ("quality".to_owned(), "320".to_owned()),
            ("album_audio_id".to_owned(), "987654321".to_owned()),
            ("behavior".to_owned(), "play".to_owned()),
            ("pid".to_owned(), "411".to_owned()),
            ("cmd".to_owned(), "26".to_owned()),
            ("pidversion".to_owned(), "3001".to_owned()),
            ("IsFreePart".to_owned(), "0".to_owned()),
            (
                "ppage_id".to_owned(),
                "356753938,823673182,967485191".to_owned(),
            ),
            ("cdnBackup".to_owned(), "1".to_owned()),
            ("module".to_owned(), String::new()),
            ("clientver".to_owned(), "11430".to_owned()),
            ("dfid".to_owned(), "test-dfid".to_owned()),
            ("mid".to_owned(), "12345678901234567890".to_owned()),
            ("uuid".to_owned(), "-".to_owned()),
            ("appid".to_owned(), "3116".to_owned()),
            ("clienttime".to_owned(), "1700000000".to_owned()),
            ("token".to_owned(), "test-token".to_owned()),
            ("userid".to_owned(), "42".to_owned()),
        ]);
        let key = kugou_lite_sign_key(
            "abcdef0123456789abcdef0123456789",
            "12345678901234567890",
            "42",
        );
        assert_eq!(key, "dbc11897d9041d25df1c41e1902cbf8d");
        params.insert("key".to_owned(), key);
        assert_eq!(
            kugou_android_signature(&params, ""),
            "edf100987bced84a64d8a83bc77097c1"
        );
    }

    #[test]
    fn signed_post_bodies_keep_upstream_json_property_order() {
        assert_eq!(
            serialize_kugou_body(&KugouAdPlayReportBody {
                ad_id: 12_307_537_187,
                play_end: 1_700_000_030_000,
                play_start: 1_700_000_000_000,
            })
            .unwrap(),
            "{\"ad_id\":12307537187,\"play_end\":1700000030000,\"play_start\":1700000000000}"
        );

        let body = serialize_kugou_body(&KugouPrivilegeBody {
            appid: 3116,
            area_code: 1,
            behavior: "play",
            clientver: 11440,
            need_hash_offset: 1,
            relate: 1,
            support_verify: 1,
            resource: vec![KugouPrivilegeResource {
                resource_type: "audio",
                page_id: 0,
                hash: "ABC".to_owned(),
                album_id: "7".to_owned(),
            }],
            qualities: [
                "128",
                "320",
                "flac",
                "high",
                "viper_atmos",
                "viper_tape",
                "viper_clear",
                "super",
                "multitrack",
            ],
        })
        .unwrap();
        assert!(body.starts_with(
            "{\"appid\":3116,\"area_code\":1,\"behavior\":\"play\",\"clientver\":11440,\"need_hash_offset\":1,\"relate\":1,\"support_verify\":1,\"resource\":[{\"type\":\"audio\",\"page_id\":0,\"hash\":\"ABC\",\"album_id\":\"7\"}],\"qualities\":["
        ));
    }

    #[test]
    fn search_preserves_kugou_playback_and_vip_identifiers() {
        let parsed = parse_search_candidates(&json!({"data": {"lists": [{
            "FileHash": "ABCDEF0123456789",
            "OriSongName": "测试歌曲",
            "SingerName": "歌手甲、歌手乙",
            "AlbumID": 42,
            "MixSongID": 314159,
            "AlbumName": "测试专辑",
            "Duration": 180
        }]}}))
        .unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].song.key.id, "ABCDEF0123456789");
        assert_eq!(parsed[0].song.artists, ["歌手甲", "歌手乙"]);
        assert_eq!(parsed[0].song.duration_ms, Some(180_000));
        assert_eq!(
            parsed[0].song.resolver_locator.as_ref().unwrap().as_str(),
            "kugou:ABCDEF0123456789:42:314159"
        );
    }

    #[test]
    fn search_accepts_lists_shape_and_mix_song_id() {
        let parsed = parse_search_candidates(&json!({"data": {"lists": [{
            "FileHash": "0123456789ABCDEF",
            "OriSongName": "另一首歌",
            "SingerName": "歌手",
            "AlbumID": "7",
            "MixSongID": "88",
            "Duration": 180
        }]}}))
        .unwrap();

        assert_eq!(parsed[0].song.duration_ms, Some(180_000));
        assert_eq!(
            parsed[0].song.resolver_locator.as_ref().unwrap().as_str(),
            "kugou:0123456789ABCDEF:7:88"
        );
    }

    #[test]
    fn search_accepts_lowercase_protocol_aliases_and_structured_trans_param() {
        let parsed = parse_search_candidates(&json!({
            "data": {"info": [{
                "hash": "ABCDEF0123456789",
                "songname": "别名歌曲",
                "singername": [{"name": "歌手甲"}, {"name": "歌手乙"}],
                "album_id": "9",
                "album_audio_id": 77,
                "album_name": "别名专辑",
                "duration_ms": 181000,
                "trans_param": {"PayType": 3}
            }]}
        }))
        .unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].song.artists, ["歌手甲", "歌手乙"]);
        assert_eq!(parsed[0].song.album.as_deref(), Some("别名专辑"));
        assert_eq!(parsed[0].song.duration_ms, Some(181_000));
        assert_eq!(
            parsed[0].song.resolver_locator.as_ref().unwrap().as_str(),
            "kugou:ABCDEF0123456789:9:77"
        );
        assert!(matches!(
            parsed[0].eligibility,
            crate::catalog::PlaybackEligibility::VipRequired
        ));
    }

    #[test]
    fn search_drops_candidates_with_malformed_kugou_identifiers() {
        let parsed = parse_search_candidates(&json!({"data": {"lists": [
            {
                "FileHash": "0123456789ABCDEF",
                "OriSongName": "缺省标识歌曲",
                "SingerName": "歌手"
            },
            {
                "FileHash": "0011223344556677",
                "OriSongName": "空白标识歌曲",
                "SingerName": "歌手",
                "AlbumID": "  ",
                "MixSongID": ""
            },
            {
                "FileHash": "ABCDEF0123456789",
                "OriSongName": "坏专辑标识歌曲",
                "SingerName": "歌手",
                "AlbumID": "pending",
                "MixSongID": "88"
            },
            {
                "FileHash": "1122334455667788",
                "OriSongName": "坏音频标识歌曲",
                "SingerName": "歌手",
                "AlbumID": "7",
                "MixSongID": "pending"
            },
            {
                "FileHash": "not-a-kugou-hash",
                "OriSongName": "坏 Hash 歌曲",
                "SingerName": "歌手",
                "AlbumID": "7",
                "MixSongID": "88"
            }
        ]}}))
        .unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed[0].song.resolver_locator.as_ref().unwrap().as_str(),
            "kugou:0123456789ABCDEF:0:0"
        );
        assert_eq!(
            parsed[1].song.resolver_locator.as_ref().unwrap().as_str(),
            "kugou:0011223344556677:0:0"
        );
    }

    #[test]
    fn stream_url_accepts_new_array_shape() {
        assert_eq!(
            extract_stream_url(
                &json!({"url": ["http://cdn.example/1.mp3", "http://cdn.example/2.mp3"]})
            ),
            Some("http://cdn.example/1.mp3")
        );
        assert_eq!(
            extract_stream_url(&json!({"backupUrl": ["http://cdn.example/bak.mp3"]})),
            Some("http://cdn.example/bak.mp3")
        );
        assert_eq!(
            extract_stream_url(&json!({
                "url": [],
                "backupUrl": ["http://cdn.example/fallback.mp3"]
            })),
            Some("http://cdn.example/fallback.mp3")
        );
        assert_eq!(extract_stream_url(&json!({"url": []})), None);
        assert_eq!(extract_stream_url(&json!({"data": {"status": 1}})), None);
    }

    #[test]
    fn resolver_requires_current_strict_locator_contract() {
        let key = SongKey::new("kugou", "ABCDEF0123456789").unwrap();
        let locator = ResolverLocator::new("kugou:ABCDEF0123456789:42:314159").unwrap();
        assert_eq!(
            resolver_parts(&key, Some(&locator)).unwrap(),
            (
                "ABCDEF0123456789".to_owned(),
                "42".to_owned(),
                "314159".to_owned()
            )
        );

        let empty_album = ResolverLocator::new("kugou:ABCDEF0123456789::314159").unwrap();
        assert_eq!(
            resolver_parts(&key, Some(&empty_album)).unwrap(),
            (
                "ABCDEF0123456789".to_owned(),
                "0".to_owned(),
                "314159".to_owned()
            )
        );

        let empty_album_audio = ResolverLocator::new("kugou:ABCDEF0123456789:42:").unwrap();
        assert_eq!(
            resolver_parts(&key, Some(&empty_album_audio)).unwrap(),
            (
                "ABCDEF0123456789".to_owned(),
                "42".to_owned(),
                "0".to_owned()
            )
        );

        let legacy = ResolverLocator::new("kugou:v1:ABCDEF0123456789:42").unwrap();
        assert!(matches!(
            resolver_parts(&key, Some(&legacy)),
            Err(CatalogError::InvalidResolverLocator(_))
        ));
    }

    #[test]
    fn lyrics_decode_base64_and_keep_decoded_text() {
        let lyric = "[00:01.00]测试歌词";
        let encoded = base64::engine::general_purpose::STANDARD.encode(lyric);
        assert_eq!(decode_lrc_content(&encoded).unwrap(), lyric);
        assert_eq!(decode_lrc_content(lyric).unwrap(), lyric);
    }

    #[test]
    fn credential_cookie_header_contains_required_api_cookie_fields() {
        let credential = ProviderCredential::Kugou {
            token: "token-secret".to_owned(),
            userid: "123".to_owned(),
            dfid: "device-id".to_owned(),
            cookies: BTreeMap::from([("mid".to_owned(), "device-mid".to_owned())]),
        };
        let header = super::KugouAdapter::credential_cookie_header(&credential);
        assert!(header.contains("token=token-secret"));
        assert!(header.contains("userid=123"));
        assert!(header.contains("dfid=device-id"));
        assert!(header.contains("mid=device-mid"));
    }

    #[test]
    fn web_and_lite_cookie_headers_keep_their_dfid_sessions_separate() {
        let credential = ProviderCredential::Kugou {
            token: "token-secret".to_owned(),
            userid: "123".to_owned(),
            dfid: "web-dfid-field".to_owned(),
            cookies: BTreeMap::from([
                (
                    "KuGoo".to_owned(),
                    "t=token-secret&KugooID=123&ct=1700000000".to_owned(),
                ),
                ("dfid".to_owned(), "web-dfid-cookie".to_owned()),
                ("KUGOU_API_DFID".to_owned(), "lite-dfid".to_owned()),
                ("mid".to_owned(), "web-mid".to_owned()),
            ]),
        };
        let web = super::KugouAdapter::web_credential_cookie_header(&credential);
        assert!(web.contains("KuGoo=t=token-secret&KugooID=123&ct=1700000000"));
        assert!(web.contains("dfid=web-dfid-cookie"));
        assert!(!web.contains("dfid=lite-dfid"));
        assert!(!web.contains("KUGOU_API_DFID=lite-dfid"));

        let lite = super::KugouAdapter::credential_cookie_header(&credential);
        assert!(lite.contains("dfid=lite-dfid"));
        assert!(lite.contains("token=token-secret"));
        assert!(!lite.contains("KuGoo="));
    }

    #[test]
    fn web_union_vip_params_keep_the_signed_web_session_shape() {
        let credential = ProviderCredential::Kugou {
            token: "token-secret".to_owned(),
            userid: "123".to_owned(),
            dfid: "web-dfid-field".to_owned(),
            cookies: BTreeMap::from([
                ("dfid".to_owned(), "web-dfid-cookie".to_owned()),
                ("mid".to_owned(), "web-mid".to_owned()),
            ]),
        };

        let params = super::KugouAdapter::web_union_vip_params(&credential).unwrap();
        assert_eq!(
            params.get("token").map(String::as_str),
            Some("token-secret")
        );
        assert_eq!(params.get("userid").map(String::as_str), Some("123"));
        assert_eq!(
            params.get("dfid").map(String::as_str),
            Some("web-dfid-cookie")
        );
        assert_eq!(params.get("mid").map(String::as_str), Some("web-mid"));
        assert_eq!(params.get("busi_type").map(String::as_str), Some("concept"));
        assert_eq!(
            params.get("opt_product_types").map(String::as_str),
            Some("dvip,qvip")
        );
        assert_eq!(params.get("product_type").map(String::as_str), Some("svip"));

        let mut unsigned = params.clone();
        let signature = unsigned.remove("signature").expect("Web signature");
        assert_eq!(signature, super::kugou_web_signature(&unsigned));
    }

    #[test]
    fn web_validation_accepts_missing_song_error_but_rejects_expired_session() {
        assert!(
            validate_web_credential_response(&json!({
                "status": 0,
                "err_code": 20010,
                "error": "missing hash"
            }))
            .is_ok()
        );
        assert!(matches!(
            validate_web_credential_response(&json!({
                "status": 0,
                "err_code": 30020,
                "error": "token invalid"
            })),
            Err(CatalogError::CredentialRejected(_))
        ));
    }

    #[test]
    fn credential_mid_prefers_server_cookie_over_dfid() {
        let credential = ProviderCredential::Kugou {
            token: "token".to_owned(),
            userid: "123".to_owned(),
            dfid: "dfid-is-not-mid".to_owned(),
            cookies: BTreeMap::from([
                ("mid".to_owned(), "987654321".to_owned()),
                ("KugouGUID".to_owned(), "guid".to_owned()),
            ]),
        };
        assert_eq!(
            super::KugouAdapter::credential_mid(Some(&credential)),
            "987654321"
        );
    }

    #[test]
    fn credential_mid_falls_back_to_api_guid() {
        let credential = ProviderCredential::Kugou {
            token: "token".to_owned(),
            userid: "123".to_owned(),
            dfid: "dfid".to_owned(),
            cookies: BTreeMap::from([(
                "KUGOU_API_GUID".to_owned(),
                "550e8400-e29b-41d4-a716-446655440000".to_owned(),
            )]),
        };
        let normalized = kugou_normalize_guid(
            credential
                .cookies()
                .get("KUGOU_API_GUID")
                .expect("api guid"),
        );
        assert_eq!(
            super::KugouAdapter::credential_mid(Some(&credential)),
            kugou_calculate_mid(&normalized)
        );
    }

    #[test]
    fn validation_rejects_explicit_kugou_error_envelopes() {
        assert!(!kugou_validation_is_rejected(&json!({
            "code": 0,
            "data": {"lists": []}
        })));
        assert!(kugou_validation_is_rejected(&json!({
            "error_code": 20018,
            "message": "token invalid"
        })));
        assert!(kugou_validation_is_rejected(&json!({
            "data": {"code": "1"}
        })));
        assert!(kugou_validation_is_rejected(&json!({
            "error": "credential expired"
        })));
    }

    #[test]
    fn business_response_rejects_kugou_error_envelopes() {
        assert!(classify_business_response(&json!({"status": 1, "data": {}})).is_ok());
        assert!(matches!(
            classify_business_response(&json!({"status": 0, "error_code": 20018})),
            Err(CatalogError::CredentialRejected(_))
        ));
        assert!(matches!(
            classify_business_response(&json!({"data": {"error_code": 1234}})),
            Err(CatalogError::InvalidResponse(_))
        ));
        assert!(matches!(
            classify_business_response(&json!({"data": {"errorCode": "20018"}})),
            Err(CatalogError::CredentialRejected(_))
        ));
        assert!(matches!(
            classify_business_response(&json!({"code": 20018, "data": {}})),
            Err(CatalogError::CredentialRejected(_))
        ));
        assert!(
            classify_business_response(&json!({
                "data": {"lists": [{"code": 20018}]}
            }))
            .is_ok()
        );
    }

    #[test]
    fn resolve_failure_preserves_vip_and_auth_classification() {
        assert!(matches!(
            classify_kugou_resolve_failure(&json!({"data": {"vip": 1}})),
            CatalogError::VipRequired(_)
        ));
        assert!(matches!(
            classify_kugou_resolve_failure(&json!({
                "status": 0,
                "error_code": 20018
            })),
            CatalogError::CredentialRejected(_)
        ));
        assert!(matches!(
            classify_kugou_resolve_failure(&json!({"data": {}})),
            CatalogError::Unavailable(_)
        ));
    }

    #[test]
    fn legacy_device_identity_is_normalized_without_rewriting_disk_shape() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-kugou-device-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("kugou-device.json");
        let original_guid = "550e8400-e29b-41d4-a716-446655440000";
        std::fs::write(
            &path,
            serde_json::json!({
                "guid": original_guid,
                "dev": "device-id",
                "mac": "aa:bb:cc:dd:ee:ff"
            })
            .to_string(),
        )
        .unwrap();
        let store = crate::credentials::CredentialStore::open(directory.clone()).unwrap();
        let loaded = load_legacy_device_identity(&store).unwrap();
        assert_eq!(loaded.guid, kugou_normalize_guid(original_guid));
        assert_eq!(loaded.dev, "DEVICE-ID");
        assert_eq!(loaded.mac, "AA:BB:CC:DD:EE:FF");
        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains(original_guid));
        drop(store);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn adapter_instantiation_without_base_url() {
        assert!(
            super::KugouAdapter::new(
                crate::credentials::CredentialStore::memory(),
                std::time::Duration::from_secs(1),
            )
            .is_ok()
        );
    }

    #[test]
    fn account_status_reads_sanitized_identity_and_vip_fields() {
        let detail_value = json!({
            "data": {
                "userid": "123",
                "nickname": "测试账号",
                "is_vip": 1
            }
        });
        let vip_value = json!({
            "data": {
                "vip_type": "年费",
                "expire_time": 1_735_689_600u64
            }
        });
        let detail = super::response_data(&detail_value);
        let vip = super::response_data(&vip_value);
        assert_eq!(detail["userid"], "123");
        assert_eq!(detail["nickname"], "测试账号");
        assert_eq!(vip["vip_type"], "年费");
        assert_eq!(vip["expire_time"], 1_735_689_600u64);
    }

    #[test]
    fn vip_result_is_kept_when_kugou_detail_request_failed() {
        let credential = ProviderCredential::Kugou {
            token: "token".to_owned(),
            userid: "123".to_owned(),
            dfid: "device".to_owned(),
            cookies: BTreeMap::new(),
        };
        let vip = json!({
            "data": {
                "busi_vip": [{
                    "is_vip": 1,
                    "product_type": "svip",
                    "vip_end_time": "2026-09-12 02:08:52"
                }]
            }
        });

        let status =
            super::KugouAdapter::account_status_from_responses(&credential, None, Some(&vip), 42);

        assert!(status.logged_in);
        assert_eq!(status.user_id.as_deref(), Some("123"));
        assert!(status.vip_known);
        assert!(status.vip);
        assert_eq!(status.vip_type.as_deref(), Some("svip"));
        assert_eq!(status.checked_at_ms, 42);
    }

    #[test]
    fn listen_report_accepts_nested_code_and_vip_days() {
        let value = json!({"data": {"code": 0, "vipDays": 30, "message": "领取成功"}});
        let data = super::response_data(&value);
        assert_eq!(data["code"], 0);
        assert_eq!(data["vipDays"], 30);
    }

    #[test]
    fn union_vip_status_prefers_busi_vip_over_top_level() {
        let value = json!({
            "is_vip": 0,
            "vip_type": 0,
            "busi_vip": [
                {
                    "is_vip": 1,
                    "is_paid_vip": 1,
                    "product_type": "svip",
                    "vip_end_time": "2026-09-12 02:08:52",
                    "busi_type": "concept"
                },
                {
                    "is_vip": 1,
                    "is_paid_vip": 0,
                    "product_type": "tvip",
                    "vip_end_time": "2026-09-12 01:37:19",
                    "busi_type": "concept"
                }
            ]
        });
        let status = super::KugouAdapter::union_vip_status(&value);
        assert_eq!(status.active, Some(true));
        assert_eq!(status.product_type.as_deref(), Some("svip"));
        assert_eq!(
            status.expire_at_ms,
            Some(parse_kugou_datetime_ms("2026-09-12 02:08:52").unwrap())
        );
    }

    #[test]
    fn union_vip_status_marks_inactive_when_busi_vip_present_but_all_inactive() {
        let value = json!({
            "is_vip": 1,
            "busi_vip": [{"is_vip": 0, "product_type": "svip", "vip_end_time": ""}]
        });
        let status = super::KugouAdapter::union_vip_status(&value);
        assert_eq!(status.active, Some(false));
    }

    #[test]
    fn union_vip_status_falls_back_to_top_level_without_busi_vip() {
        let active = super::KugouAdapter::union_vip_status(&json!({"is_vip": 1}));
        assert_eq!(active.active, Some(true));
        let inactive = super::KugouAdapter::union_vip_status(&json!({"vip_type": 0}));
        assert_eq!(inactive.active, Some(false));
        let unknown = super::KugouAdapter::union_vip_status(&json!({"error": 1}));
        assert_eq!(unknown.active, None);

        let empty_busi_vip = super::KugouAdapter::union_vip_status(&json!({
            "is_vip": 1,
            "busi_vip": []
        }));
        assert_eq!(empty_busi_vip.active, Some(true));
    }

    #[test]
    fn kugou_datetime_is_parsed_to_epoch_ms() {
        assert_eq!(parse_kugou_datetime_ms("1970-01-01 00:00:00"), Some(0));
        assert_eq!(
            parse_kugou_datetime_ms("2026-09-12 02:08:52"),
            Some(1_789_178_932_000)
        );
        assert_eq!(parse_kugou_datetime_ms(""), None);
        assert_eq!(parse_kugou_datetime_ms("2026-09-12"), None);
    }
}
