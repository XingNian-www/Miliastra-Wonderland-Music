//! 酷狗音乐公开接口适配器（纯 Rust 内置直连官方接口）。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use md5::compute;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use url::Url;

use crate::catalog::{
    AccountStatusCache, CatalogError, PlaybackEligibility, ProviderSearchCandidate, SourceAdapter,
    account_status_error_code,
};
use crate::credentials::{CredentialStore, ProviderCredential};
use crate::domain::{ResolverLocator, SearchSpec, Song, SongKey, StreamSource};
use crate::lyrics::{TimedLyrics, parse_lrc_pair};

const PROVIDER: &str = "kugou";

/// 酷狗 web 版签名盐值。
const KUGOU_WEB_SIGN_SALT: &str = "NVPh5oo715z5DIWAeQlhMDsWXXQV4hwt";
const KUGOU_LITE_APPID: i64 = 3116;
const KUGOU_LITE_CLIENTVER: i64 = 11440;
const KUGOU_UA: &str = "Android15-1070-11083-46-0-DiscoveryDRADProtocol-wifi";

/// 官方端点
const URL_SEARCH: &str = "https://complexsearch.kugou.com/v2/search/song";
const URL_SONG_STREAM: &str = "https://gateway.kugou.com/v5/url";
const URL_PRIVILEGE: &str = "https://gateway.kugou.com/openapi/v1/privilege/lite";
const URL_SEARCH_LYRIC: &str = "http://krcs.kugou.com/search";
const URL_DOWNLOAD_LYRIC: &str = "http://krcs.kugou.com/download";
const URL_TOKEN_REFRESH: &str = "https://login-user.kugou.com/v2/token_refresh";
const URL_USER_DETAIL: &str = "https://gateway.kugou.com/v3/get_my_info";
const URL_YOUTH_UNION_VIP: &str = "https://gateway.kugou.com/youth/v1/union/vip";
const URL_USER_VIP_DETAIL: &str = "https://gateway.kugou.com/youth/v1/vip/detail";
const URL_YOUTH_VIP: &str = "https://gateway.kugou.com/youth/v1/ad/play_report";
const URL_YOUTH_VIP_UPGRADE: &str =
    "https://gateway.kugou.com/youth/v1/listen_song/upgrade_vip_reward";

/// 概念版广告领取 VIP 的广告位 ID（官方 youth_vip.js 固定值）。
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

/// 概念版 VIP 领取结果（广告播放领取）。
#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KugouListenReport {
    pub accepted: bool,
    pub vip_days: u32,
    pub message: String,
}

/// get_union_vip 响应解析出的 VIP 状态（概念版 busi_vip 优先）。
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
    /// 账号状态整体缓存（成功 24 小时/失败 15 分钟），防止轮询频繁打账号接口触发风控。
    account_cache: Arc<std::sync::Mutex<AccountStatusCache>>,
}

/// 账号 VIP 状态缓存有效期（判定成功时）。
const VIP_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 判定失败（接口风控/超时）时的短缓存，尽快重试账号接口。
const VIP_CACHE_FAILED_TTL: Duration = Duration::from_secs(15 * 60);

/// 酷狗 web 版签名：md5(盐值 + 参数按 key 排序后的 k=v 拼接 + 盐值)。
pub fn kugou_web_signature(params: &BTreeMap<String, String>) -> String {
    let mut pairs = params.iter().collect::<Vec<_>>();
    pairs.sort_by(|left, right| left.0.cmp(right.0));
    let joined = pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<String>();
    let digest = compute(format!(
        "{KUGOU_WEB_SIGN_SALT}{joined}{KUGOU_WEB_SIGN_SALT}"
    ));
    format!("{digest:x}")
}

/// 计算酷狗设备 MID：md5(GUID) 的 hex 视为 128 位无符号大整数，转十进制。
pub fn kugou_calculate_mid(guid: &str) -> String {
    let digest = compute(guid.as_bytes());
    let hex = format!("{digest:x}");
    u128::from_str_radix(&hex, 16)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "-".to_owned())
}

impl KugouAdapter {
    pub fn new(credentials: CredentialStore, timeout: Duration) -> Result<Self, CatalogError> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent(KUGOU_UA)
            .build()
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        Ok(Self {
            client,
            credentials,
            account_cache: Arc::new(std::sync::Mutex::new(AccountStatusCache::default())),
        })
    }

    fn credential_cookie_header(credential: &ProviderCredential) -> String {
        let mut cookies = credential.cookies().clone();
        let ProviderCredential::Kugou {
            token,
            userid,
            dfid,
            ..
        } = credential
        else {
            return cookie_header(&cookies);
        };
        cookies.insert("token".to_owned(), token.clone());
        cookies.insert("userid".to_owned(), userid.clone());
        cookies.insert("dfid".to_owned(), dfid.clone());
        cookie_header(&cookies)
    }

    fn credential(&self) -> Result<ProviderCredential, CatalogError> {
        self.credentials
            .get(PROVIDER)
            .map_err(|error| CatalogError::AuthRequired(error.to_string()))?
            .ok_or_else(|| CatalogError::AuthRequired("Kugou credential_required".to_owned()))
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

    fn signed_params(
        credential: Option<&ProviderCredential>,
        extra: Vec<(&str, String)>,
    ) -> BTreeMap<String, String> {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_string();

        let mut params = BTreeMap::new();
        params.insert("appid".to_owned(), KUGOU_LITE_APPID.to_string());
        params.insert("clientver".to_owned(), KUGOU_LITE_CLIENTVER.to_string());
        params.insert("clienttime".to_owned(), now_secs);

        let (token, userid, dfid) = credential
            .and_then(|cred| Self::credential_fields(cred).ok())
            .unwrap_or(("", "", ""));

        let mid = if !dfid.is_empty() {
            kugou_calculate_mid(dfid)
        } else {
            "-".to_owned()
        };

        params.insert("mid".to_owned(), mid);
        params.insert(
            "dfid".to_owned(),
            if dfid.is_empty() {
                "-".to_owned()
            } else {
                dfid.to_owned()
            },
        );

        if !token.is_empty() {
            params.insert("token".to_owned(), token.to_owned());
        }
        if !userid.is_empty() {
            params.insert("userid".to_owned(), userid.to_owned());
        }

        for (k, v) in extra {
            params.insert(k.to_owned(), v);
        }

        let signature = kugou_web_signature(&params);
        params.insert("signature".to_owned(), signature);
        params
    }

    async fn get_json_direct(
        &self,
        endpoint: &str,
        query: Vec<(&str, String)>,
        credential: Option<&ProviderCredential>,
    ) -> Result<Value, CatalogError> {
        let signed = Self::signed_params(credential, query);
        let mut request = self.client.get(endpoint).query(&signed);

        if let Some(cred) = credential {
            request = request.header("Cookie", Self::credential_cookie_header(cred));
        }

        let response = request.send().await.map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn post_json_direct(
        &self,
        endpoint: &str,
        query: Vec<(&str, String)>,
        body: Option<&Value>,
        credential: Option<&ProviderCredential>,
    ) -> Result<(Value, reqwest::header::HeaderMap), CatalogError> {
        let signed = Self::signed_params(credential, query);
        let mut request = self.client.post(endpoint).query(&signed);

        if let Some(cred) = credential {
            request = request.header("Cookie", Self::credential_cookie_header(cred));
        }
        if let Some(body_json) = body {
            request = request
                .header("Content-Type", "application/json; charset=utf-8")
                .json(body_json);
        }

        let response = request.send().await.map_err(classify_request_error)?;
        classify_status(&response)?;
        let headers = response.headers().clone();
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        Ok((value, headers))
    }

    async fn search_json(
        &self,
        keyword: &str,
        limit: usize,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        self.get_json_direct(
            URL_SEARCH,
            vec![
                ("keyword", keyword.to_owned()),
                ("page", "1".to_owned()),
                ("pagesize", limit.clamp(1, 30).to_string()),
                ("type", "song".to_owned()),
            ],
            Some(credential),
        )
        .await
    }

    /// 概念版广告播放领取 VIP：上报一次完整广告播放（约 30 秒）。
    pub async fn claim_vip(&self) -> Result<KugouListenReport, CatalogError> {
        let credential = self.credential()?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let body = serde_json::json!({
            "ad_id": KUGOU_VIP_AD_ID,
            "play_end": now_ms,
            "play_start": now_ms.saturating_sub(KUGOU_VIP_AD_PLAY_MS),
        });
        let (value, _) = self
            .post_json_direct(URL_YOUTH_VIP, Vec::new(), Some(&body), Some(&credential))
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

    /// 概念版升级 VIP：听歌奖励升级。
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
        let (_token, userid, dfid) = Self::credential_fields(credential)?;
        let (value, headers) = self
            .post_json_direct(URL_TOKEN_REFRESH, Vec::new(), None, Some(credential))
            .await?;

        // 官方经 Set-Cookie 下发新的续期字段（t1 等），必须保留供下次刷新。
        let mut refreshed_cookies = headers
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|header| header.to_str().ok())
            .filter_map(|header| {
                let (name, rest) = header.split_once('=')?;
                let name = name.trim();
                let value = rest.split(';').next()?.trim();
                matches!(name, "t1" | "vip_type" | "vip_token")
                    .then(|| (name.to_owned(), value.to_owned()))
            })
            .collect::<BTreeMap<_, _>>();

        let data = value.get("data").unwrap_or(&value);
        let new_token = data
            .get("token")
            .and_then(value_string)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                CatalogError::AuthRequired("Kugou token refresh was rejected".to_owned())
            })?;
        let new_userid = data
            .get("userid")
            .and_then(value_string)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| userid.to_owned());
        let new_dfid = data
            .get("dfid")
            .and_then(value_string)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| dfid.to_owned());
        let ProviderCredential::Kugou { cookies, .. } = credential else {
            return Err(CatalogError::CredentialRejected(
                "kugou refresh requires a kugou credential".to_owned(),
            ));
        };
        let mut merged_cookies = cookies.clone();
        merged_cookies.extend(refreshed_cookies);
        refreshed_cookies = merged_cookies;
        Ok(ProviderCredential::Kugou {
            token: new_token,
            userid: new_userid,
            dfid: new_dfid,
            cookies: refreshed_cookies,
        })
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
        if let Some(list) = data
            .get("busi_vip")
            .and_then(Value::as_array)
            .filter(|list| !list.is_empty())
        {
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
        status.active = Self::vip_flags_from_union(data);
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
            .get_json_direct(URL_USER_DETAIL, Vec::new(), Some(&credential))
            .await;
        let vip = match self
            .get_json_direct(URL_YOUTH_UNION_VIP, Vec::new(), Some(&credential))
            .await
        {
            Ok(vip) => Ok(vip),
            Err(_) => {
                self.get_json_direct(URL_USER_VIP_DETAIL, Vec::new(), Some(&credential))
                    .await
            }
        };

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
        crate::catalog::ProviderAccountStatus {
            provider: PROVIDER.to_owned(),
            logged_in: detail
                .and_then(|detail| detail.get("error_code"))
                .and_then(value_u64)
                .is_none_or(|code| code == 0),
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
            .get_json_direct(
                URL_SEARCH_LYRIC,
                vec![
                    ("hash", hash.to_owned()),
                    ("album_audio_id", album_audio_id.to_owned()),
                ],
                Some(credential),
            )
            .await?;
        let Some(candidate) = result
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
                ("fmt", "lrc".to_owned()),
                ("decode", "true".to_owned()),
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
        let response = self.search_json("validation", 1, candidate).await?;
        if !response.is_object() {
            Err(CatalogError::InvalidResponse(
                "Kugou credential validation response is not an object".to_owned(),
            ))
        } else if kugou_validation_is_rejected(&response) {
            Err(CatalogError::CredentialRejected(
                "Kugou credential validation was rejected".to_owned(),
            ))
        } else {
            Ok(())
        }
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
        let (hash, album_id, album_audio_id) = resolver_parts(key, locator)?;
        let credential = self.credential()?;
        let response = self
            .get_json_direct(
                URL_SONG_STREAM,
                vec![
                    ("hash", hash),
                    ("album_id", album_id),
                    ("album_audio_id", album_audio_id),
                    ("quality", "320".to_owned()),
                ],
                Some(&credential),
            )
            .await?;
        let url = match extract_stream_url(&response) {
            Some(url) => url,
            None => return Err(classify_kugou_resolve_failure(&response)),
        };
        Ok(StreamSource {
            url: Url::parse(url)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
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
        let (hash, album_id, _) = resolver_parts(key, locator)?;
        let credential = self.credential()?;
        let response = self
            .get_json_direct(
                URL_PRIVILEGE,
                vec![("hash", hash), ("album_id", album_id)],
                Some(&credential),
            )
            .await?;
        let item = response
            .pointer("/data/0")
            .or_else(|| response.pointer("/data"))
            .and_then(|value| {
                value
                    .as_array()
                    .and_then(|items| items.first())
                    .or(Some(value))
            });
        let Some(item) = item else {
            return Err(CatalogError::InvalidResponse(
                "Kugou privilege response has no resource".to_owned(),
            ));
        };
        let pay_type = item.get("pay_type").and_then(value_u64);
        match pay_type {
            Some(0) => Ok(PlaybackEligibility::Eligible),
            Some(2) => Ok(PlaybackEligibility::PaidRequired),
            Some(1 | 3) => match self.account_vip_status().await {
                Some(true) => Ok(PlaybackEligibility::Eligible),
                Some(false) => {
                    let _ = self.resolve(key, locator).await?;
                    Ok(PlaybackEligibility::Eligible)
                }
                None => {
                    let _ = self.resolve(key, locator).await?;
                    Ok(PlaybackEligibility::Eligible)
                }
            },
            _ => Err(CatalogError::InvalidResponse(
                "Kugou privilege response has no pay_type".to_owned(),
            )),
        }
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
        let content = response
            .get("decodeContent")
            .or_else(|| response.get("content"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let lyric = decode_lrc_content(content)?;
        let translation = response
            .get("decodeTrcContent")
            .or_else(|| response.get("trcContent"))
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
    let vip = response
        .pointer("/data/vip")
        .or_else(|| response.pointer("/vip"))
        .and_then(value_u64);
    if vip == Some(1) {
        return CatalogError::VipRequired("Kugou track requires VIP membership".to_owned());
    }
    CatalogError::Unavailable("Kugou returned no playable stream".to_owned())
}

fn extract_stream_url(response: &Value) -> Option<&str> {
    response
        .get("url")
        .or_else(|| response.get("backupUrl"))
        .and_then(|value| match value {
            Value::Array(items) => items
                .iter()
                .find_map(|item| item.as_str().filter(|url| !url.is_empty())),
            Value::String(text) => (!text.is_empty()).then_some(text.as_str()),
            _ => None,
        })
}

fn parse_search_candidates(response: &Value) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
    let songs = response
        .pointer("/data/lists")
        .or_else(|| response.pointer("/data/info"))
        .and_then(Value::as_array)
        .ok_or_else(|| CatalogError::InvalidResponse("Kugou search list is missing".to_owned()))?;
    Ok(songs
        .iter()
        .filter_map(|raw| {
            let hash = raw
                .get("FileHash")
                .or_else(|| raw.get("hash"))
                .and_then(Value::as_str)?
                .trim();
            let title = raw
                .get("OriSongName")
                .or_else(|| raw.get("FileName"))
                .or_else(|| raw.get("songname"))
                .and_then(Value::as_str)?
                .trim();
            if hash.is_empty()
                || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                || title.is_empty()
            {
                return None;
            }
            let album_id =
                kugou_search_identifier(raw.get("AlbumID").or_else(|| raw.get("album_id")))?;
            let album_audio_id =
                kugou_search_identifier(raw.get("MixSongID").or_else(|| raw.get("mixsongid")))?;
            let artists = raw
                .get("SingerName")
                .or_else(|| raw.get("singername"))
                .and_then(Value::as_str)
                .map(|value| {
                    value
                        .split(['、', '/', '&'])
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let duration_ms = raw
                .get("Duration")
                .or_else(|| raw.get("duration"))
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
                    .or_else(|| raw.get("album_name"))
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

fn kugou_candidate_is_vip(raw: &Value) -> bool {
    if let Some(trans) = raw.get("TransParam").and_then(Value::as_str)
        && let Ok(param) = serde_json::from_str::<Value>(trans)
    {
        for field in ["display_vip", "vip"] {
            if let Some(value) = param.get(field).and_then(value_u64) {
                return value != 0;
            }
        }
    }
    raw.get("VIP")
        .or_else(|| raw.get("is_vip"))
        .and_then(value_u64)
        .is_some_and(|value| value != 0)
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

fn decode_lrc_content(content: &str) -> Result<String, CatalogError> {
    use base64::Engine;
    if content.trim().is_empty() {
        return Ok(String::new());
    }
    match base64::engine::general_purpose::STANDARD.decode(content) {
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string())),
        Err(_) => Ok(content.to_owned()),
    }
}

fn response_data(value: &Value) -> &Value {
    value.get("data").unwrap_or(value)
}

fn kugou_validation_is_rejected(response: &Value) -> bool {
    let data = response_data(response);
    [response, data].into_iter().any(|value| {
        ["error_code", "errorCode", "code"]
            .into_iter()
            .filter_map(|field| value.get(field))
            .filter_map(value_i64)
            .any(|code| code != 0)
            || value.get("error").is_some_and(kugou_error_value_is_present)
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
        decode_lrc_content, extract_stream_url, kugou_calculate_mid, kugou_validation_is_rejected,
        kugou_web_signature, parse_kugou_datetime_ms, parse_search_candidates, resolver_parts,
    };
    use crate::catalog::CatalogError;
    use crate::credentials::ProviderCredential;
    use crate::domain::{ResolverLocator, SongKey};

    #[test]
    fn kugou_signature_and_mid_calculation() {
        let mut params = BTreeMap::new();
        params.insert("appid".to_owned(), "3116".to_owned());
        params.insert("clientver".to_owned(), "11440".to_owned());
        let sig = kugou_web_signature(&params);
        assert!(!sig.is_empty());
        assert_eq!(sig.len(), 32);

        let mid = kugou_calculate_mid("test-guid-123");
        assert!(!mid.is_empty());
        assert!(mid.chars().all(|c| c.is_ascii_digit()));
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
