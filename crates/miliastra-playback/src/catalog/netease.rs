//! Minimal native NetEase Cloud Music adapter.

use std::collections::BTreeMap;
use std::time::Duration;

use aes::Aes128;
use async_trait::async_trait;
use base64::Engine;
use cbc::Encryptor;
use cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use num_bigint::BigUint;
use rand::{Rng, distributions::Alphanumeric};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use url::Url;

use crate::catalog::{
    CatalogError, PlaybackEligibility, ProviderAccountStatus, ProviderSearchCandidate,
    SourceAdapter,
};
use crate::credentials::{CredentialStore, ProviderCredential};
use crate::domain::{ResolverLocator, SearchSpec, Song, SongKey, StreamSource};
use crate::lyrics::{TimedLyrics, parse_lrc_pair};

const SEARCH_URL: &str = "https://music.163.com/api/search/get";
const PROVIDER: &str = "netease";
const MEDIA_URL: &str = "https://music.163.com/weapi/song/enhance/player/url";
const LYRICS_URL: &str = "https://music.163.com/api/song/lyric";
/// 账号状态接口（weapi）。返回 profile（昵称/ID/VIP 类型）。
const ACCOUNT_URL: &str = "https://music.163.com/weapi/nuser/account/get";
/// 黑胶 VIP 信息接口（weapi）。返回 musicPackage/associator 的 expireTime。
const VIP_INFO_URL: &str = "https://music.163.com/api/music-vip-membership/front/vip/info";

// ===== 网易云 weapi 加密 =====
// 参考 feeluown-netease：AES-128-CBC 双重加密 + RSA 裸模幂（无填充）。
const WEAPI_AES_IV: &[u8; 16] = b"0102030405060708";
const WEAPI_FIRST_KEY: &[u8; 16] = b"0CoJUm6Qyw8W8jud";
const WEAPI_RSA_E: &str = "010001";
const WEAPI_RSA_N: &str = "00e0b509f6259df8642dbc35662901477df22677ec152b5ff68ace615bb7b725152b3ab17a876aea8a5aa76d2e417629ec4ee341f56135fccf695280104e0312ecbda92557c93870114af6c9d05c4f7f0c3685b7a46bee255932575cce10b424d813cfe4875d3e82047b97ddef52741d546b8e289dc6935b3ece0462db0a22b8e7";

/// AES-128-CBC 加密（PKCS7 填充，固定 IV），返回 base64。
fn weapi_aes_encrypt_base64(text: &str, key: &[u8]) -> Result<String, CatalogError> {
    let encryptor = Encryptor::<Aes128>::new_from_slices(key, WEAPI_AES_IV)
        .map_err(|error| CatalogError::InvalidResponse(format!("weapi AES 初始化失败: {error}")))?;
    let mut buffer = text.as_bytes().to_vec();
    let position = buffer.len();
    buffer.resize(position + 16, 0);
    let encrypted = encryptor
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, position)
        .map_err(|error| CatalogError::InvalidResponse(format!("weapi AES 加密失败: {error}")))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(encrypted))
}

/// RSA 裸模幂（无 PKCS#1 填充）：m = 反转后的密钥字节（大端），
/// c = m^e mod n，输出 256 位 hex 补零。
fn weapi_rsa_encrypt(second_key: &str) -> Result<String, CatalogError> {
    let exponent = BigUint::parse_bytes(WEAPI_RSA_E.as_bytes(), 16)
        .ok_or_else(|| CatalogError::InvalidResponse("weapi RSA 指数解析失败".to_owned()))?;
    let modulus = BigUint::parse_bytes(WEAPI_RSA_N.as_bytes(), 16)
        .ok_or_else(|| CatalogError::InvalidResponse("weapi RSA 模数解析失败".to_owned()))?;
    let reversed = second_key.chars().rev().collect::<String>();
    let message = BigUint::from_bytes_be(reversed.as_bytes());
    let encrypted = message.modpow(&exponent, &modulus);
    Ok(format!("{encrypted:0256x}"))
}

/// 构造 weapi 请求体：{params, encSecKey}。
fn weapi_encrypt(data: &Value) -> Result<BTreeMap<String, String>, CatalogError> {
    let text = data.to_string();
    let second_key = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect::<String>()
        .to_ascii_lowercase();
    let first_pass = weapi_aes_encrypt_base64(&text, WEAPI_FIRST_KEY)?;
    let params = weapi_aes_encrypt_base64(&first_pass, second_key.as_bytes())?;
    Ok(BTreeMap::from([
        ("params".to_owned(), params),
        ("encSecKey".to_owned(), weapi_rsa_encrypt(&second_key)?),
    ]))
}

#[derive(Clone)]
pub struct NeteaseAdapter {
    client: Client,
    credentials: CredentialStore,
    search_url: Url,
    media_url: Url,
    lyrics_url: Url,
    account_url: Url,
    vip_url: Url,
    /// 账号状态缓存（成功 5 分钟/失败 60 秒），避免轮询频繁打账号接口。
    account_cache: std::sync::Arc<
        std::sync::Mutex<
            Option<(
                crate::catalog::ProviderAccountStatus,
                std::time::Instant,
                bool,
            )>,
        >,
    >,
}

/// 网易账号状态缓存有效期（查询成功时）。
const ACCOUNT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// 查询失败（未登录/风控/超时）时的短缓存。
const ACCOUNT_CACHE_FAILED_TTL: Duration = Duration::from_secs(60);

impl NeteaseAdapter {
    pub fn new(credentials: CredentialStore, timeout: Duration) -> Result<Self, CatalogError> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent("miliastra-wonderland-music/0.1")
            .build()
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        Ok(Self {
            client,
            credentials,
            search_url: Url::parse(SEARCH_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            media_url: Url::parse(MEDIA_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            lyrics_url: Url::parse(LYRICS_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            account_url: Url::parse(ACCOUNT_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            vip_url: Url::parse(VIP_INFO_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            account_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
        })
    }

    #[cfg(test)]
    pub fn with_lyrics_endpoint(mut self, lyrics_url: Url) -> Self {
        self.lyrics_url = lyrics_url;
        self
    }

    fn credential(&self) -> Result<ProviderCredential, CatalogError> {
        self.credentials
            .get("netease")
            .map_err(|error| CatalogError::AuthRequired(error.to_string()))?
            .ok_or_else(|| CatalogError::AuthRequired("NetEase credential_required".to_owned()))
    }

    async fn search_json_with_credential(
        &self,
        credential: &ProviderCredential,
        keyword: &str,
        limit: usize,
    ) -> Result<Value, CatalogError> {
        let response = self
            .client
            .get(self.search_url.clone())
            .query(&[
                ("s", keyword),
                ("type", "1"),
                ("offset", "0"),
                ("total", "true"),
                ("limit", &limit.min(10).to_string()),
            ])
            .header("Cookie", cookie_header(credential.cookies()))
            .header("Referer", "https://music.163.com/")
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn search_json(&self, keyword: &str, limit: usize) -> Result<Value, CatalogError> {
        let credential = self.credential()?;
        self.search_json_with_credential(&credential, keyword, limit)
            .await
    }

    async fn resolve_json(&self, id: &str) -> Result<Value, CatalogError> {
        let credential = self.credential()?;
        let params = serde_json::json!({
            "ids": format!("[{id}]"),
            "br": 320000,
            "csrf_token": credential
                .cookies()
                .get("__csrf")
                .cloned()
                .unwrap_or_default(),
        });
        let payload = weapi_encrypt(&params)?;
        let response = self
            .client
            .post(self.media_url.clone())
            .form(&payload)
            .header("Cookie", cookie_header(credential.cookies()))
            .header("Referer", "https://music.163.com/")
            // 服务端风控：缺 x-os: web 时返回的播放 URL 无效（下载 403）。
            .header("x-os", "web")
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn lyrics_json(&self, id: &str) -> Result<Value, CatalogError> {
        let credential = self.credential()?;
        let response = self
            .client
            .get(self.lyrics_url.clone())
            .query(&[("id", id), ("lv", "-1"), ("kv", "1"), ("tv", "-1")])
            .header("Cookie", cookie_header(credential.cookies()))
            .header("Referer", "https://music.163.com/")
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    /// 查询账号状态（weapi /nuser/account/get）。未登录返回 logged_in=false。
    async fn account_status_uncached(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        let Ok(credential) = self.credential() else {
            return Ok(Some(ProviderAccountStatus {
                logged_in: false,
                ..ProviderAccountStatus::default()
            }));
        };
        let payload = weapi_encrypt(&serde_json::json!({}))?;
        let response = self
            .client
            .post(self.account_url.clone())
            .form(&payload)
            .header("Cookie", cookie_header(credential.cookies()))
            .header("Referer", "https://music.163.com/")
            .header("x-os", "web")
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        let code = value.get("code").and_then(Value::as_i64);
        if code.is_some_and(|code| code == 301 || code == 400) {
            return Ok(Some(ProviderAccountStatus {
                logged_in: false,
                ..ProviderAccountStatus::default()
            }));
        }
        // 已登录但响应缺少 profile（风控/限流瞬时响应）：视为查询失败，
        // 走失败缓存（60 秒后重试），避免把空状态当成功缓存 5 分钟。
        let Some(profile) = value.get("profile").filter(|v| v.is_object()) else {
            return Err(CatalogError::InvalidResponse(
                "NetEase account response is missing profile".to_owned(),
            ));
        };
        let vip_type = profile
            .get("vipType")
            .or_else(|| profile.get("newVipType"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        // 黑胶 VIP 到期时间（weapi /api/music-vip-membership/front/vip/info）。
        // 查询失败不阻塞账号状态，到期时间保持未知。
        let vip_expire_at_ms = self
            .vip_expire_at_ms(&credential, profile.get("userId"))
            .await;
        Ok(Some(ProviderAccountStatus {
            logged_in: true,
            user_id: profile
                .get("userId")
                .and_then(Value::as_i64)
                .map(|id| id.to_string()),
            nickname: profile
                .get("nickname")
                .and_then(Value::as_str)
                .map(str::to_owned),
            vip: vip_type != 0,
            vip_type: Some(vip_type.to_string()),
            vip_expire_at_ms,
            login_method: None,
        }))
    }

    /// 查询黑胶 VIP 到期时间（musicPackage/associator 的 expireTime）。
    async fn vip_expire_at_ms(
        &self,
        credential: &ProviderCredential,
        user_id: Option<&Value>,
    ) -> Option<u64> {
        let user_id = user_id.and_then(Value::as_i64)?;
        let payload = weapi_encrypt(&serde_json::json!({ "userId": user_id.to_string() })).ok()?;
        let response = self
            .client
            .post(self.vip_url.clone())
            .form(&payload)
            .header("Cookie", cookie_header(credential.cookies()))
            .header("Referer", "https://music.163.com/")
            .header("x-os", "web")
            .send()
            .await
            .ok()?;
        let value = response.json::<Value>().await.ok()?;
        value
            .get("data")
            .and_then(|data| data.get("musicPackage").or_else(|| data.get("associator")))
            .and_then(|package| package.get("expireTime"))
            .and_then(Value::as_i64)
            .filter(|expire| *expire > 0)
            .map(|expire| expire as u64)
    }
}

#[async_trait]
impl SourceAdapter for NeteaseAdapter {
    async fn validate_credential(
        &self,
        candidate: &ProviderCredential,
    ) -> Result<(), CatalogError> {
        if candidate.provider() != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "NetEase candidate provider does not match adapter".to_owned(),
            ));
        }
        let response = self
            .search_json_with_credential(candidate, "validation", 1)
            .await?;
        if !response.is_object() {
            return Err(CatalogError::InvalidResponse(
                "NetEase credential validation response is not an object".to_owned(),
            ));
        }
        Ok(())
    }

    async fn search(
        &self,
        spec: &SearchSpec,
    ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
        let response = self.search_json(spec.keyword.trim(), spec.limit).await?;
        parse_search_candidates(&response)
    }

    async fn resolve(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<StreamSource, CatalogError> {
        if key.source != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "song key provider does not match NetEase adapter".to_owned(),
            ));
        }
        let id = netease_resolver_id(key, locator)?;
        let response = self.resolve_json(&id).await?;
        if response
            .get("code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code == 301 || code == 401)
        {
            return Err(CatalogError::AuthRequired(
                "NetEase account requires login or refreshed credentials".to_owned(),
            ));
        }
        let (url, expiry_seconds) = match netease_media_url(&response) {
            Ok(media) => media,
            Err(error) => return Err(classify_netease_resolve_failure(&response, error)),
        };
        Ok(StreamSource {
            url,
            // The response URL carries its own signed token; never forward
            // account cookies to a provider-selected CDN.
            headers: BTreeMap::from([("Referer".to_owned(), "https://music.163.com/".to_owned())]),
            expires_at_epoch_ms: expiry_seconds.and_then(|seconds| {
                std::time::SystemTime::now()
                    .checked_add(Duration::from_secs(seconds))
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as u64)
            }),
        })
    }

    async fn lyrics(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<Option<TimedLyrics>, CatalogError> {
        if key.source != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "song key provider does not match NetEase adapter".to_owned(),
            ));
        }
        let id = netease_resolver_id(key, locator)?;
        let response = self.lyrics_json(&id).await?;
        if response
            .get("code")
            .and_then(as_i64)
            .is_some_and(|code| code == 301 || code == 401)
        {
            return Err(CatalogError::AuthRequired(
                "NetEase lyric request requires login".to_owned(),
            ));
        }
        let primary = response
            .pointer("/lrc/lyric")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let translation = response
            .pointer("/tlyric/lyric")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty());
        parse_lrc_pair(primary, translation)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        let now = std::time::Instant::now();
        if let Ok(guard) = self.account_cache.lock()
            && let Some((status, cached_at, failed)) = guard.as_ref()
            && cached_at.elapsed()
                < if *failed {
                    ACCOUNT_CACHE_FAILED_TTL
                } else {
                    ACCOUNT_CACHE_TTL
                }
        {
            return Ok(Some(status.clone()));
        }
        let result = self.account_status_uncached().await;
        if let Ok(mut guard) = self.account_cache.lock() {
            match &result {
                Ok(Some(status)) => *guard = Some((status.clone(), now, false)),
                // 失败（风控/超时）时保留上次成功值并标记失败，60 秒后重试。
                _ => {
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
}

/// 网易播放 URL 获取失败时的分类：仅返回试听流（freeTrialInfo）视为版权受限；
/// 其余保持原错误（探测层按初始标注确认不可用）。
fn classify_netease_resolve_failure(response: &Value, error: CatalogError) -> CatalogError {
    let preview_only = response
        .pointer("/data/0")
        .and_then(|item| item.get("freeTrialInfo"))
        .is_some_and(|value| !value.is_null());
    if preview_only {
        return CatalogError::NoCopyright("NetEase track only offers a preview stream".to_owned());
    }
    match error {
        CatalogError::Unavailable(message) => {
            CatalogError::NoCopyright(format!("NetEase track has no copyright: {message}"))
        }
        other => other,
    }
}

fn netease_resolver_id(
    key: &SongKey,
    locator: Option<&ResolverLocator>,
) -> Result<String, CatalogError> {
    let id = match locator {
        Some(locator) => locator
            .as_str()
            .strip_prefix("netease:v1:")
            .ok_or_else(|| {
                CatalogError::InvalidResolverLocator(
                    "NetEase locator has an unsupported version".to_owned(),
                )
            })?,
        None => &key.id,
    };
    if id.is_empty() || !id.chars().all(|character| character.is_ascii_digit()) {
        return Err(CatalogError::InvalidResolverLocator(
            "NetEase locator must contain a numeric song id".to_owned(),
        ));
    }
    if id != key.id {
        return Err(CatalogError::ResolverLocatorTrackMismatch {
            requested_id: key.id.clone(),
            locator_id: id.to_owned(),
        });
    }
    Ok(id.to_owned())
}

fn netease_media_url(response: &Value) -> Result<(Url, Option<u64>), CatalogError> {
    let item = response.pointer("/data/0").ok_or_else(|| {
        CatalogError::InvalidResponse("NetEase media response is missing data".to_owned())
    })?;
    if item
        .get("freeTrialInfo")
        .is_some_and(|value| !value.is_null())
    {
        return Err(CatalogError::Unavailable(
            "NetEase only returned a preview stream".to_owned(),
        ));
    }
    let media_url = item
        .get("url")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            CatalogError::Unavailable("NetEase returned no playable URL for this track".to_owned())
        })?;
    let url =
        Url::parse(media_url).map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(CatalogError::InvalidResponse(
            "NetEase returned a non-HTTP media URL".to_owned(),
        ));
    }
    Ok((url, item.get("expi").and_then(Value::as_u64)))
}

pub(crate) fn parse_search_candidates(
    response: &Value,
) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
    let songs = response
        .pointer("/result/songs")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CatalogError::InvalidResponse("NetEase search list is missing".to_owned())
        })?;
    let mut candidates = Vec::new();
    for raw in songs.iter().take(10) {
        let Some(id) = raw.get("id").and_then(as_i64).filter(|id| *id > 0) else {
            continue;
        };
        let title = raw
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if title.is_empty() {
            continue;
        }
        let artists = raw
            .get("artists")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("name").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let album = raw
            .pointer("/album/name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let duration_ms = raw
            .get("duration")
            .and_then(Value::as_u64)
            .or_else(|| raw.get("dt").and_then(Value::as_u64));
        let song = Song {
            key: SongKey::new("netease", id.to_string())
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            resolver_locator: ResolverLocator::new(format!("netease:v1:{id}")).ok(),
            title,
            artists,
            album,
            duration_ms,
        };
        let eligibility = netease_eligibility(raw);
        if eligibility == PlaybackEligibility::Ineligible {
            continue;
        }
        candidates.push(ProviderSearchCandidate { song, eligibility });
        if candidates.len() == 5 {
            break;
        }
    }
    Ok(candidates)
}

pub(crate) fn netease_eligibility(raw: &Value) -> PlaybackEligibility {
    if raw
        .get("status")
        .and_then(as_i64)
        .is_some_and(|value| value < 0)
    {
        return PlaybackEligibility::Ineligible;
    }
    if raw
        .pointer("/privilege/st")
        .and_then(as_i64)
        .is_some_and(|value| value < 0)
    {
        return PlaybackEligibility::Ineligible;
    }
    let fee = raw.get("fee").and_then(as_i64);
    let status = raw.get("status").and_then(as_i64);
    let copyright = raw.get("copyrightId").and_then(as_i64);
    match (fee, status, copyright) {
        (Some(0), Some(0), Some(value)) if value > 0 => PlaybackEligibility::Eligible,
        (Some(1), _, _) => PlaybackEligibility::VipRequired,
        _ => PlaybackEligibility::Unknown,
    }
}

fn as_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn cookie_header(cookies: &BTreeMap<String, String>) -> String {
    cookies
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn classify_status(response: &reqwest::Response) -> Result<(), CatalogError> {
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(CatalogError::AuthRequired(
            "provider rejected credentials".to_owned(),
        )),
        StatusCode::TOO_MANY_REQUESTS => {
            Err(CatalogError::RateLimited("NetEase rate limit".to_owned()))
        }
        status if status.is_server_error() => {
            Err(CatalogError::Transient(format!("provider HTTP {status}")))
        }
        status if !status.is_success() => Err(CatalogError::InvalidResponse(format!(
            "provider HTTP {status}"
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{
        NeteaseAdapter, netease_eligibility, netease_media_url, netease_resolver_id,
        parse_search_candidates,
    };
    use crate::catalog::SourceAdapter;
    use crate::catalog::{CatalogError, PlaybackEligibility};
    use crate::credentials::{CredentialStore, ProviderCredential};
    use crate::domain::{ResolverLocator, SongKey};
    use serde_json::json;
    use url::Url;

    fn fixture_server(body: String) -> (Url, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = [0_u8; 16 * 1024];
            let size = stream.read(&mut request).unwrap();
            sender
                .send(String::from_utf8_lossy(&request[..size]).into_owned())
                .unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (
            Url::parse(&format!("http://{address}/song/lyric")).unwrap(),
            receiver,
        )
    }

    fn credentials() -> CredentialStore {
        let store = CredentialStore::memory();
        store
            .save(
                "netease",
                ProviderCredential::Netease {
                    cookies: BTreeMap::from([(String::from("MUSIC_U"), String::from("secret"))]),
                },
            )
            .unwrap();
        store
    }

    #[test]
    fn weapi_encryption_matches_reference_vectors() {
        // 参考实现（feeluown-netease / Node crypto）生成的期望值：
        // text={"ids":"[42]","br":320000,"csrf_token":"token"}，second_key=1234567890abcdef
        let text = r#"{"ids":"[42]","br":320000,"csrf_token":"token"}"#;
        let second_key = "1234567890abcdef";
        let first_pass = super::weapi_aes_encrypt_base64(text, super::WEAPI_FIRST_KEY).unwrap();
        assert_eq!(
            first_pass,
            "KWxUcxtHzWTJ54oXtah+UUsdY6jYhIc8X2U0MjR58DkeUILbKW4A5mZ7klw+Q0nA"
        );
        let params = super::weapi_aes_encrypt_base64(&first_pass, second_key.as_bytes()).unwrap();
        assert_eq!(
            params,
            "47V96V0k548QH7Q38d7j7KspR6S6rWt45b/f0ZvmqP47Ap+vUFNOrRVML7AG8ONES1fZ8jy70gAgzRZYzW4a7cg3KPg4yP9EcMA1eI0Ovyw="
        );
        assert_eq!(
            super::weapi_rsa_encrypt(second_key).unwrap(),
            "bc05a756cb87b1a2674efda9e90a27d641dcbbe2e6b7be3b61193e47315c298183d409578ac80502bd441d82bf1611d6d9238bcaf64794d096417b726373fdd8cf4a9e9d31d198d6d785ffa3d0a2e8c621b4d6c019fabe0ca6f3a815d9f2a4875a0bcab968f331f394566e1162603ed1dd002aa89b47fd8dcbc77d2c6976efed"
        );
    }

    #[test]
    fn netease_resolver_rejects_a_locator_for_another_track() {
        let key = SongKey::new("netease", "42").unwrap();
        let locator = ResolverLocator::new("netease:v1:43").unwrap();

        assert!(matches!(
            netease_resolver_id(&key, Some(&locator)),
            Err(CatalogError::ResolverLocatorTrackMismatch { .. })
        ));
    }

    #[test]
    fn netease_media_url_requires_a_nonempty_http_url() {
        let playable = netease_media_url(
            &json!({"data":[{"url":"https://m8.music.126.net/song.mp3","expi":120}]}),
        )
        .unwrap();
        assert_eq!(playable.0.as_str(), "https://m8.music.126.net/song.mp3");
        assert_eq!(playable.1, Some(120));

        assert!(matches!(
            netease_media_url(&json!({"data":[{"url":""}]})),
            Err(CatalogError::Unavailable(_))
        ));
        assert!(matches!(
            netease_media_url(&json!({"data":[{"url":"ftp://example.test/song.mp3"}]})),
            Err(CatalogError::InvalidResponse(_))
        ));
        assert!(matches!(
            netease_media_url(&json!({"data":[{
                "url":"https://m8.music.126.net/preview.mp3",
                "freeTrialInfo":{"start":0,"end":30}
            }]})),
            Err(CatalogError::Unavailable(_))
        ));
    }

    #[test]
    fn netease_rights_metadata_preserves_unknown_membership_cases() {
        assert_eq!(
            netease_eligibility(&json!({"fee":0,"status":1,"copyrightId":5003})),
            PlaybackEligibility::Unknown
        );
        assert_eq!(
            netease_eligibility(&json!({"fee":0,"status":0,"copyrightId":5003})),
            PlaybackEligibility::Eligible
        );
        assert_eq!(
            netease_eligibility(&json!({"fee":1,"status":0,"copyrightId":5003})),
            PlaybackEligibility::VipRequired
        );
        assert_eq!(
            netease_eligibility(&json!({"fee":"0","status":"0","copyrightId":"5003"})),
            PlaybackEligibility::Eligible
        );
    }

    #[test]
    fn netease_search_keeps_at_most_five_candidates() {
        let songs = (1..=10)
            .map(|id| {
                json!({
                    "id": id,
                    "name": format!("Song {id}"),
                    "artists": [{"name":"Artist"}],
                    "album": {"name":"Album"},
                    "dt": 180000,
                    "fee": 0,
                    "status": 0,
                    "copyrightId": 5003
                })
            })
            .collect::<Vec<_>>();
        let parsed = parse_search_candidates(&json!({"result":{"songs":songs}})).unwrap();
        assert_eq!(parsed.len(), 5);
    }

    #[test]
    fn netease_search_accepts_numeric_string_ids_without_refill_requests() {
        let parsed = parse_search_candidates(&json!({"result":{"songs":[{
            "id":"42", "name":"Song", "artists":[], "fee":"0",
            "status":"0", "copyrightId":"5003"
        }]}}))
        .unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].song.key.id, "42");
        assert_eq!(parsed[0].eligibility, PlaybackEligibility::Eligible);
    }

    #[tokio::test]
    async fn netease_lyrics_uses_the_song_lyric_query_and_prefers_translation() {
        let body = json!({
            "code": 200,
            "lrc": {"lyric": "[00:01.00]original"},
            "tlyric": {"lyric": "[00:01.00]translated"}
        })
        .to_string();
        let (endpoint, request) = fixture_server(body);
        let adapter = NeteaseAdapter::new(credentials(), Duration::from_secs(2))
            .unwrap()
            .with_lyrics_endpoint(endpoint);
        let key = SongKey::new("netease", "42").unwrap();
        let locator = ResolverLocator::new("netease:v1:42").unwrap();

        let lyrics = adapter.lyrics(&key, Some(&locator)).await.unwrap().unwrap();

        assert_eq!(lyrics.line_at_ms(1_000), Some("translated"));
        let request = request.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(request.contains("/song/lyric?id=42&lv=-1&kv=1&tv=-1"));
        assert!(request.to_ascii_lowercase().contains("cookie:"));
    }
}
