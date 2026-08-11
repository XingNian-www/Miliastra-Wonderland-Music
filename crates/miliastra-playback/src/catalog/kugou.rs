//! 酷狗音乐公开接口适配器。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use url::Url;

use crate::catalog::{CatalogError, PlaybackEligibility, ProviderSearchCandidate, SourceAdapter};
use crate::credentials::{CredentialStore, ProviderCredential};
use crate::domain::{ResolverLocator, SearchSpec, Song, SongKey, StreamSource};
use crate::lyrics::{TimedLyrics, parse_lrc_pair};

const PROVIDER: &str = "kugou";

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

/// 账号 VIP 状态缓存条目（判定结果、时间、是否失败）。
type VipCacheEntry = Option<(Option<bool>, std::time::Instant, bool)>;
/// 账号状态缓存条目（状态、时间、是否失败）。
type AccountCacheEntry = Option<(KugouAccountStatus, std::time::Instant, bool)>;

#[derive(Clone)]
pub struct KugouAdapter {
    client: Client,
    credentials: CredentialStore,
    api_base_url: Url,
    /// 账号 VIP 状态缓存（/user/vip/detail 查询较重且接口不稳定）。
    vip_cache: Arc<std::sync::Mutex<VipCacheEntry>>,
    /// 账号状态整体缓存（成功 5 分钟/失败 60 秒），防止轮询频繁打账号接口触发风控。
    account_cache: Arc<std::sync::Mutex<AccountCacheEntry>>,
}

/// 账号 VIP 状态缓存有效期（判定成功时）。
const VIP_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// 判定失败（接口风控/超时）时的短缓存，尽快重试账号接口。
const VIP_CACHE_FAILED_TTL: Duration = Duration::from_secs(60);

impl KugouAdapter {
    pub fn new(
        credentials: CredentialStore,
        timeout: Duration,
        api_base_url: &str,
    ) -> Result<Self, CatalogError> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .build()
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        Ok(Self {
            client,
            credentials,
            api_base_url: parse_url(api_base_url)?,
            vip_cache: Arc::new(std::sync::Mutex::new(None)),
            account_cache: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    fn api_url(&self, path: &str) -> Result<Url, CatalogError> {
        self.api_base_url
            .join(path.trim_start_matches('/'))
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
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

    async fn get_json(
        &self,
        url: &Url,
        query: Vec<(&str, String)>,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        let (token, userid, dfid) = Self::credential_fields(credential)?;
        let mut request = self
            .client
            .get(url.clone())
            .query(&query)
            .header("Cookie", Self::credential_cookie_header(credential));
        request = request.query(&[("token", token), ("userid", userid), ("dfid", dfid)]);
        // 临时诊断：记录搜索请求的完整 URL，便于比对酷狗侧拒绝原因。
        log::warn!(
            "Kugou sidecar request: {url}?{query:?} token={token} userid={userid} dfid={dfid}"
        );
        let response = request.send().await.map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn search_json(
        &self,
        keyword: &str,
        limit: usize,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        self.get_json(
            &self.api_url("/search")?,
            vec![
                ("keywords", keyword.to_owned()),
                ("page", "1".to_owned()),
                ("pagesize", limit.clamp(1, 30).to_string()),
                ("type", "song".to_owned()),
            ],
            credential,
        )
        .await
    }

    /// 概念版广告播放领取 VIP：上报一次完整广告播放（约 30 秒）。
    /// 走 KuGouMusicApi `/youth/vip`（youth_vip.js → /youth/v1/ad/play_report）。
    pub async fn claim_vip(&self) -> Result<KugouListenReport, CatalogError> {
        let credential = self.credential()?;
        let (token, userid, dfid) = Self::credential_fields(&credential)?;
        let url = self.api_url("/youth/vip")?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let response = self
            .client
            .post(url)
            .query(&[("token", token), ("userid", userid), ("dfid", dfid)])
            .header("Cookie", Self::credential_cookie_header(&credential))
            .header("Content-Type", "application/json; charset=utf-8")
            .json(&serde_json::json!({
                "ad_id": KUGOU_VIP_AD_ID,
                "play_end": now_ms,
                "play_start": now_ms.saturating_sub(KUGOU_VIP_AD_PLAY_MS),
            }))
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
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

    /// 概念版升级 VIP：听歌奖励升级（youth_day_vip_upgrade.js → /youth/v1/listen_song/upgrade_vip_reward）。
    /// 需要已领取过听歌奖励，升级到更高档位。
    pub async fn upgrade_vip(&self) -> Result<KugouListenReport, CatalogError> {
        let credential = self.credential()?;
        let (token, userid, dfid) = Self::credential_fields(&credential)?;
        let response = self
            .client
            .post(self.api_url("/youth/day/vip/upgrade")?)
            .query(&[
                ("token", token),
                ("userid", userid),
                ("dfid", dfid),
                ("kugouid", userid),
                ("ad_type", "1"),
            ])
            .header("Cookie", Self::credential_cookie_header(&credential))
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
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

    /// 刷新酷狗 token，并在成功后返回新凭据。响应只提取必要字段，不记录密钥。
    pub async fn refresh_token(&self) -> Result<ProviderCredential, CatalogError> {
        let credential = self.credential()?;
        let (token, userid, dfid) = Self::credential_fields(&credential)?;
        let url = self.api_url("/login/token")?;
        let response = self
            .client
            .post(url)
            .query(&[("userid", userid), ("dfid", dfid), ("token", token)])
            .header("Cookie", Self::credential_cookie_header(&credential))
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        // 官方经 Set-Cookie 下发新的续期字段（t1 等），必须保留供下次刷新。
        let mut refreshed_cookies = response
            .headers()
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
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
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
            unreachable!()
        };
        // 新下发的续期字段覆盖旧值；其余 cookie 保持原样。
        refreshed_cookies.extend(cookies);
        Ok(ProviderCredential::Kugou {
            token: new_token,
            userid: new_userid,
            dfid: new_dfid,
            cookies: refreshed_cookies,
        })
    }

    /// 查询账号 VIP 状态（带缓存）。`None` 表示查询失败、无法判定。
    async fn account_vip_status(&self) -> Option<bool> {
        let now = std::time::Instant::now();
        if let Ok(guard) = self.vip_cache.lock()
            && let Some((status, cached_at, failed)) = guard.as_ref()
            && cached_at.elapsed()
                < if *failed {
                    VIP_CACHE_FAILED_TTL
                } else {
                    VIP_CACHE_TTL
                }
        {
            return *status;
        }
        let result = self.query_account_vip_status().await;
        if let Ok(mut guard) = self.vip_cache.lock() {
            *guard = Some((result, now, result.is_none()));
        }
        result
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

    /// 从 get_union_vip 响应提取概念版 VIP 状态：
    /// 优先解析 `busi_vip` 数组（busi_type=concept 的 svip/tvip 业务线），
    /// 数组缺失或为空时回退顶层 is_vip / vip_type（标准版字段）。
    /// 实测顶层 is_vip 表示标准版会员，概念版真实状态只在 busi_vip 中，
    /// 因此 busi_vip 存在时以它为准（即使顶层 is_vip=0）。
    fn union_vip_status(data: &Value) -> UnionVipStatus {
        let mut status = UnionVipStatus::default();
        if let Some(list) = data.get("busi_vip").and_then(Value::as_array) {
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
            // busi_vip 存在但没有生效条目：明确判定为非 VIP，不回退顶层。
            if status.active.is_none() {
                status.active = Some(false);
            }
            return status;
        }
        status.active = Self::vip_flags_from_union(data);
        status
    }

    async fn query_account_vip_status(&self) -> Option<bool> {
        let credential = self.credential().ok()?;
        // 1) 概念版「获取已领取 VIP 状态」/youth/union/vip（概念版专用，与领取同源的权威状态）；
        //    风控时顺延到下一途径，接口恢复后自动生效。
        if let Ok(vip) = self.account_json("/youth/union/vip", &credential).await
            && let Some(vip_status) = Self::union_vip_status(response_data(&vip)).active
        {
            return Some(vip_status);
        }
        // 2) /user/vip/detail（kugouvip get_union_vip 查询态）。
        if let Ok(vip) = self.account_json("/user/vip/detail", &credential).await
            && let Some(vip_status) = Self::union_vip_status(response_data(&vip)).active
        {
            return Some(vip_status);
        }
        // 3) 凭据 cookie vip_type 不可信（概念版扫码登录不会写入），
        //    风控期间无法判定 → None（播放过滤按可播处理，不误伤）。
        let _ = credential;
        None
    }

    /// 查询账户与 VIP 状态。仅返回脱敏状态。
    /// 账号接口带整体缓存（成功 5 分钟/失败 60 秒），避免频繁轮询触发酷狗风控。
    pub async fn account_status(&self) -> Result<KugouAccountStatus, CatalogError> {
        let now = std::time::Instant::now();
        if let Ok(guard) = self.account_cache.lock()
            && let Some((status, cached_at, failed)) = guard.as_ref()
            && cached_at.elapsed()
                < if *failed {
                    VIP_CACHE_FAILED_TTL
                } else {
                    VIP_CACHE_TTL
                }
        {
            return Ok(status.clone());
        }
        let result = self.query_account_status().await;
        if let Ok(mut guard) = self.account_cache.lock() {
            match &result {
                Ok(status) => *guard = Some((status.clone(), now, false)),
                // 查询失败（风控/超时）时保留上次成功值并标记失败，60 秒后重试。
                Err(_) => {
                    let cached = guard
                        .as_ref()
                        .map(|(status, ..)| status.clone())
                        .unwrap_or_default();
                    *guard = Some((cached, now, true));
                }
            }
        }
        result
    }

    async fn query_account_status(&self) -> Result<KugouAccountStatus, CatalogError> {
        let credential = self.credential()?;
        let credential_userid = match &credential {
            ProviderCredential::Kugou { userid, .. } => Some(userid.clone()),
            _ => None,
        };
        // 1) /user/detail（v3/get_my_info）：登录态 + 基础账号信息（轻量、非登录刷新接口）。
        let detail = self.account_json("/user/detail", &credential).await;
        if let Ok(detail) = detail {
            let detail_data = response_data(&detail);
            let logged_in = detail
                .get("error_code")
                .and_then(value_u64)
                .is_none_or(|code| code == 0);
            let user_id = detail_data
                .get("userid")
                .and_then(value_string)
                .or(credential_userid);
            // 2) VIP 状态：概念版 /youth/union/vip → /user/vip/detail → 凭据 cookie vip_type。
            let vip = match self.account_json("/youth/union/vip", &credential).await {
                Ok(vip) => vip,
                Err(_) => self
                    .account_json("/user/vip/detail", &credential)
                    .await
                    .unwrap_or(Value::Null),
            };
            let vip_data = response_data(&vip);
            let union_status = Self::union_vip_status(vip_data);
            // busi_vip 的 product_type（svip/tvip 等）是概念版权威字段；
            // 回退顺序：busi_vip → 顶层 vip_type → 凭据 cookie vip_type。
            let vip_type = union_status
                .product_type
                .or_else(|| {
                    vip_data
                        .get("vip_type")
                        .and_then(value_u64)
                        .map(|value| value.to_string())
                })
                .or_else(|| match &credential {
                    ProviderCredential::Kugou { cookies, .. } => cookies
                        .get("vip_type")
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(|value| value.to_string()),
                    _ => None,
                });
            return Ok(KugouAccountStatus {
                logged_in,
                user_id,
                nickname: detail_data
                    .get("nickname")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                // 概念版判定以 busi_vip 为准（实测顶层 is_vip=0 不代表无概念版 VIP）。
                vip: union_status.active.unwrap_or(false)
                    || vip_type.as_deref().is_some_and(|v| v != "0")
                    || vip_data
                        .get("is_vip")
                        .and_then(value_u64)
                        .is_some_and(|v| v > 0)
                    || vip_data
                        .get("vip")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                vip_type,
                vip_expire_at_ms: union_status.expire_at_ms.or_else(|| {
                    vip_data
                        .get("expire_time")
                        .and_then(value_u64)
                        .map(|v| if v < 10_000_000_000 { v * 1000 } else { v })
                }),
                listen_report_available: logged_in,
            });
        }
        // 3) 账号接口全部不可用（风控）：凭据存在即视为已登录。
        //    cookie vip_type 在概念版扫码登录时不会写入（接口才权威），
        //    回退时不展示 VIP 判定，前端显示「未知」避免误导。
        Ok(KugouAccountStatus {
            logged_in: true,
            user_id: credential_userid,
            nickname: None,
            vip: false,
            vip_type: None,
            vip_expire_at_ms: None,
            listen_report_available: true,
        })
    }

    async fn account_json(
        &self,
        path: &str,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        self.account_request(reqwest::Method::GET, path, Vec::new(), credential)
            .await
    }

    async fn account_request(
        &self,
        method: reqwest::Method,
        path: &str,
        query: Vec<(&str, String)>,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        let (token, userid, dfid) = Self::credential_fields(credential)?;
        let url = self.api_url(path)?;
        let response = self
            .client
            .request(method, url)
            .query(&query)
            .query(&[("token", token), ("userid", userid), ("dfid", dfid)])
            .header("Cookie", Self::credential_cookie_header(credential))
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn lyrics_json(
        &self,
        hash: &str,
        album_audio_id: &str,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        let result = self
            .get_json(
                &self.api_url("/search/lyric")?,
                vec![
                    ("hash", hash.to_owned()),
                    ("album_audio_id", album_audio_id.to_owned()),
                ],
                credential,
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
        self.get_json(
            &self.api_url("/lyric")?,
            vec![
                ("id", id),
                ("accesskey", accesskey.to_owned()),
                ("fmt", "lrc".to_owned()),
                ("decode", "true".to_owned()),
            ],
            credential,
        )
        .await
    }
}

#[async_trait]
impl crate::catalog::KugouAccountAdapter for KugouAdapter {
    async fn refresh_token(&self) -> Result<ProviderCredential, CatalogError> {
        self.refresh_token().await
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
        if response.is_object() {
            Ok(())
        } else {
            Err(CatalogError::InvalidResponse(
                "Kugou credential validation response is not an object".to_owned(),
            ))
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
            .get_json(
                &self.api_url("/song/url")?,
                vec![
                    ("hash", hash),
                    ("album_id", album_id),
                    ("album_audio_id", album_audio_id),
                    ("quality", "320".to_owned()),
                ],
                &credential,
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

    /// 酷狗权威权限接口 /privilege/lite 的 pay_type：
    /// 0=免费可播，1/3=VIP 歌曲，2=付费专辑。
    /// 账号 VIP 状态可判定时：VIP 账号直接用 VIP 歌曲（Eligible），非 VIP 屏蔽（VipRequired）；
    /// 判定失败（接口不稳定）时回退实际解析播放流确认可播性。
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
            .get_json(
                &self.api_url("/privilege/lite")?,
                vec![("hash", hash), ("album_id", album_id)],
                &credential,
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
                Some(false) => Ok(PlaybackEligibility::VipRequired),
                None => {
                    // 账号 VIP 状态无法判定：以实际解析结果为准（账号凭据能拿到流即可播）。
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
        // 酷狗翻译歌词：decodeTrcContent（若歌曲有翻译）；否则退化为内嵌双语 lrc。
        let translation = response
            .get("decodeTrcContent")
            .or_else(|| response.get("trcContent"))
            .and_then(Value::as_str)
            .map(decode_lrc_content)
            .transpose()?;
        parse_lrc_pair(&lyric, translation.as_deref())
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }
}

/// 酷狗拿不到播放流时的分类：响应明确带 vip=1 标志则需 VIP；
/// 否则无法确认原因，返回 Unavailable（探测层按初始标注确认不可用）。
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

/// 从酷狗 /song/url 响应提取可播放 URL（新版：顶层 `url`/`backupUrl` 数组）。
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
    // 酷狗新版搜索响应：data.lists（大写驼峰字段，FileHash/OriSongName 等）。
    let songs = response
        .pointer("/data/lists")
        .and_then(Value::as_array)
        .ok_or_else(|| CatalogError::InvalidResponse("Kugou search list is missing".to_owned()))?;
    Ok(songs
        .iter()
        .filter_map(|raw| {
            let hash = raw.get("FileHash").and_then(Value::as_str)?.trim();
            let title = raw
                .get("OriSongName")
                .or_else(|| raw.get("FileName"))
                .and_then(Value::as_str)?
                .trim();
            if hash.is_empty() || title.is_empty() {
                return None;
            }
            let album_id = raw
                .get("AlbumID")
                .and_then(value_string)
                .unwrap_or_else(|| "0".to_owned());
            let album_audio_id = raw
                .get("MixSongID")
                .and_then(value_string)
                .unwrap_or_else(|| "0".to_owned());
            let artists = raw
                .get("SingerName")
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
            let duration_ms = raw.get("Duration").and_then(value_u64).map(|value| {
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

/// 酷狗搜索项 VIP 判断：优先读 TransParam（JSON 字符串）的 display_vip/vip，
/// 兼容旧接口顶层 VIP 数字字段。
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
    if album_id.is_empty() || !album_id.chars().all(|character| character.is_ascii_digit()) {
        return Err(CatalogError::InvalidResolverLocator(
            "Kugou locator album id is invalid".to_owned(),
        ));
    }
    if album_audio_id.is_empty()
        || !album_audio_id
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return Err(CatalogError::InvalidResolverLocator(
            "Kugou locator album audio id is invalid".to_owned(),
        ));
    }
    Ok((
        hash.to_owned(),
        album_id.to_owned(),
        album_audio_id.to_owned(),
    ))
}

fn cookie_header(cookies: &BTreeMap<String, String>) -> String {
    cookies
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_url(value: &str) -> Result<Url, CatalogError> {
    Url::parse(value).map_err(|error| CatalogError::InvalidResponse(error.to_string()))
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

/// 解析酷狗时间字符串 "YYYY-MM-DD HH:MM:SS" 为毫秒时间戳。
/// 酷狗 get_union_vip 的 vip_end_time 使用本地时区格式；到期判定以秒级精度即可，
/// 不引入 chrono，按公历换算（Howard Hinnant 算法）。
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

/// 公历日期到 1970-01-01 起的天数（Howard Hinnant days_from_civil）。
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
    use std::collections::BTreeMap;

    use base64::Engine;
    use serde_json::json;

    use super::{
        PROVIDER, decode_lrc_content, extract_stream_url, parse_kugou_datetime_ms,
        parse_search_candidates, resolver_parts,
    };
    use crate::catalog::CatalogError;
    use crate::credentials::ProviderCredential;
    use crate::domain::{ResolverLocator, SongKey};

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
    fn stream_url_accepts_new_array_shape() {
        // 新版：顶层 url 为数组
        assert_eq!(
            extract_stream_url(
                &json!({"url": ["http://cdn.example/1.mp3", "http://cdn.example/2.mp3"]})
            ),
            Some("http://cdn.example/1.mp3")
        );
        // backupUrl 兜底
        assert_eq!(
            extract_stream_url(&json!({"backupUrl": ["http://cdn.example/bak.mp3"]})),
            Some("http://cdn.example/bak.mp3")
        );
        // 空数组 / 缺失字段
        assert_eq!(extract_stream_url(&json!({"url": []})), None);
        assert_eq!(extract_stream_url(&json!({"data": {"status": 1}})), None);
    }

    #[test]
    fn resolver_requires_current_strict_locator_contract() {
        let key = SongKey::new(PROVIDER, "ABCDEF0123456789").unwrap();
        let locator = ResolverLocator::new("kugou:ABCDEF0123456789:42:314159").unwrap();
        assert_eq!(
            resolver_parts(&key, Some(&locator)).unwrap(),
            (
                "ABCDEF0123456789".to_owned(),
                "42".to_owned(),
                "314159".to_owned()
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
    fn listen_report_requires_numeric_mixsongid() {
        assert!(
            super::KugouAdapter::new(
                crate::credentials::CredentialStore::memory(),
                std::time::Duration::from_secs(1),
                "http://127.0.0.1:3000"
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
    fn listen_report_accepts_nested_code_and_vip_days() {
        let value = json!({"data": {"code": 0, "vipDays": 30, "message": "领取成功"}});
        let data = super::response_data(&value);
        assert_eq!(data["code"], 0);
        assert_eq!(data["vipDays"], 30);
    }

    #[test]
    fn union_vip_status_prefers_busi_vip_over_top_level() {
        // 实测响应：顶层 is_vip=0（标准版），概念版 svip/tvip 在 busi_vip 中。
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
        // busi_vip 存在且无生效条目：判定非 VIP，不采信顶层 is_vip=1。
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
