//! Native Bilibili video-audio catalog adapter.
//!
//! Bilibili search results identify a video rather than a conventional music
//! track.  The adapter uses the stable BV id as the track id, then resolves its
//! current first-page CID and DASH audio URL only when playback starts.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use reqwest::header::{HeaderMap, SET_COOKIE};
use reqwest::{Client, StatusCode};
use rsa::pkcs8::DecodePublicKey;
use rsa::rand_core::OsRng;
use rsa::{Oaep, RsaPublicKey};
use serde_json::Value;
use sha2::Sha256;
use tracing::{info, warn};
use url::Url;

use crate::catalog::{
    CatalogError, CredentialRefreshAdapter, PlaybackEligibility, ProviderSearchCandidate,
    SourceAdapter,
};
use crate::credentials::{CredentialStore, ProviderCredential};
use crate::domain::{ResolverLocator, SearchSpec, Song, SongKey, StreamSource};
use crate::lyrics::TimedLyrics;

const SEARCH_URL: &str = "https://api.bilibili.com/x/web-interface/wbi/search/type";
const PROVIDER: &str = "bilibili";
const NAV_URL: &str = "https://api.bilibili.com/x/web-interface/nav";
const VIEW_URL: &str = "https://api.bilibili.com/x/web-interface/view";
const PLAYURL_URL: &str = "https://api.bilibili.com/x/player/playurl";
const COOKIE_INFO_URL: &str = "https://passport.bilibili.com/x/passport-login/web/cookie/info";
const CORRESPOND_URL: &str = "https://www.bilibili.com/correspond/1/";
const COOKIE_REFRESH_URL: &str =
    "https://passport.bilibili.com/x/passport-login/web/cookie/refresh";
const CONFIRM_REFRESH_URL: &str =
    "https://passport.bilibili.com/x/passport-login/web/confirm/refresh";
const REFERER: &str = "https://www.bilibili.com/";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/134.0.0.0 Safari/537.36";
const CORRESPOND_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDLgd2OAkcGVtoE3ThUREbio0Eg\nUc/prcajMKXvkCKFCWhJYJcLkcM2DKKcSeFpD/j6Boy538YXnR6VhcuUJOhH2x71\nnzPjfdTcqMz7djHum0qSZA0AyCBDABUqCrfNgCiJ00Ra7GmRj+YCK1NJEuewlb40\nJNrRuoEUXpabUzGB8QIDAQAB\n-----END PUBLIC KEY-----";

#[derive(Default)]
struct RefreshState {
    last_check_day: Option<u64>,
    refreshing: bool,
}

#[derive(Clone)]
pub struct BilibiliAdapter {
    client: Client,
    credentials: CredentialStore,
    search_url: Url,
    nav_url: Url,
    view_url: Url,
    playurl_url: Url,
    cookie_info_url: Url,
    correspond_url: Url,
    cookie_refresh_url: Url,
    confirm_refresh_url: Url,
    refresh_state: Arc<Mutex<RefreshState>>,
}

impl BilibiliAdapter {
    pub fn new(credentials: CredentialStore, timeout: Duration) -> Result<Self, CatalogError> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        Ok(Self {
            client,
            credentials,
            search_url: Url::parse(SEARCH_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            nav_url: Url::parse(NAV_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            view_url: Url::parse(VIEW_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            playurl_url: Url::parse(PLAYURL_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            cookie_info_url: Url::parse(COOKIE_INFO_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            correspond_url: Url::parse(CORRESPOND_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            cookie_refresh_url: Url::parse(COOKIE_REFRESH_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            confirm_refresh_url: Url::parse(CONFIRM_REFRESH_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            refresh_state: Arc::new(Mutex::new(RefreshState::default())),
        })
    }

    #[cfg(test)]
    pub fn with_endpoints(
        credentials: CredentialStore,
        timeout: Duration,
        search_url: Url,
        view_url: Url,
        playurl_url: Url,
    ) -> Result<Self, CatalogError> {
        let mut adapter = Self::new(credentials, timeout)?;
        adapter.client = Client::builder()
            .timeout(timeout)
            .no_proxy()
            .user_agent(USER_AGENT)
            .build()
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        adapter.search_url = search_url;
        adapter.nav_url = adapter.search_url.clone();
        adapter.view_url = view_url;
        adapter.playurl_url = playurl_url;
        Ok(adapter)
    }

    fn credential(&self) -> Result<ProviderCredential, CatalogError> {
        self.credentials
            .get("bilibili")
            .map_err(|error| CatalogError::AuthRequired(error.to_string()))?
            .ok_or_else(|| CatalogError::AuthRequired("Bilibili credential_required".to_owned()))
    }

    async fn get_json_with_credential(
        &self,
        credential: &ProviderCredential,
        url: &Url,
        query: &[(String, String)],
        persist_cookie_updates: bool,
    ) -> Result<Value, CatalogError> {
        let response = self
            .client
            .get(url.clone())
            .query(query)
            .header("Cookie", cookie_header(credential.cookies()))
            .header("Referer", REFERER)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        let headers = response.headers().clone();
        let response = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        classify_api_response(&response)?;
        if persist_cookie_updates {
            self.persist_set_cookie_updates(credential, &headers)?;
        }
        Ok(response)
    }

    async fn get_json(&self, url: &Url, query: &[(String, String)]) -> Result<Value, CatalogError> {
        self.maybe_refresh_cookie().await;
        let credential = self.credential()?;
        self.get_json_with_credential(&credential, url, query, true)
            .await
    }

    async fn search_json(&self, keyword: &str, limit: usize) -> Result<Value, CatalogError> {
        self.get_json(
            &self.search_url,
            &[
                ("search_type".to_owned(), "video".to_owned()),
                ("keyword".to_owned(), keyword.to_owned()),
                ("page".to_owned(), "1".to_owned()),
                ("page_size".to_owned(), limit.clamp(1, 10).to_string()),
            ],
        )
        .await
    }

    async fn resolve_json(&self, bvid: &str) -> Result<Value, CatalogError> {
        let view = self
            .get_json(&self.view_url, &[("bvid".to_owned(), bvid.to_owned())])
            .await?;
        let cid = view
            .pointer("/data/cid")
            .and_then(as_u64)
            .filter(|cid| *cid > 0)
            .ok_or_else(|| {
                CatalogError::Unavailable("Bilibili video has no playable first page".to_owned())
            })?;
        self.get_json(
            &self.playurl_url,
            &[
                ("bvid".to_owned(), bvid.to_owned()),
                ("cid".to_owned(), cid.to_string()),
                // 16 requests independent DASH streams.  We consume audio only.
                ("fnval".to_owned(), "16".to_owned()),
                ("fnver".to_owned(), "0".to_owned()),
                ("fourk".to_owned(), "0".to_owned()),
                ("platform".to_owned(), "pc".to_owned()),
                ("otype".to_owned(), "json".to_owned()),
            ],
        )
        .await
    }

    fn persist_set_cookie_updates(
        &self,
        credential: &ProviderCredential,
        headers: &HeaderMap,
    ) -> Result<(), CatalogError> {
        let ProviderCredential::Bilibili {
            cookies,
            refresh_token,
        } = credential
        else {
            return Ok(());
        };
        let mut updated = cookies.clone();
        merge_set_cookie_headers(&mut updated, headers);
        if updated == *cookies {
            return Ok(());
        }
        self.credentials
            .save(
                "bilibili",
                ProviderCredential::Bilibili {
                    cookies: updated,
                    refresh_token: refresh_token.clone(),
                },
            )
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        Ok(())
    }

    async fn maybe_refresh_cookie(&self) {
        let has_refresh_token =
            self.credentials
                .get("bilibili")
                .ok()
                .flatten()
                .is_some_and(|credential| {
                    matches!(
                        credential,
                        ProviderCredential::Bilibili {
                            refresh_token: Some(token),
                            ..
                        } if !token.trim().is_empty()
                    )
                });
        if !has_refresh_token {
            return;
        }

        let status = match self.credentials.status(PROVIDER) {
            Ok(status) => status,
            Err(_) => return,
        };
        if !status.refresh_ready
            || status
                .next_refresh_check_at_ms
                .is_some_and(|next| next > now_ms())
        {
            return;
        }
        let today = current_day();
        {
            let Ok(mut state) = self.refresh_state.lock() else {
                return;
            };
            if state.refreshing || state.last_check_day == Some(today) {
                return;
            }
            state.last_check_day = Some(today);
            state.refreshing = true;
        }

        if let Err(error) = self.refresh_cookie().await {
            warn!(error = %error, "Bilibili cookie refresh failed");
        } else {
            info!("Bilibili cookie refresh check completed");
        }

        if let Ok(mut state) = self.refresh_state.lock() {
            state.refreshing = false;
        }
    }

    async fn refresh_cookie(&self) -> Result<bool, CatalogError> {
        let credential = self.credential()?;
        let ProviderCredential::Bilibili {
            cookies,
            refresh_token,
        } = credential
        else {
            return Err(CatalogError::AuthRequired(
                "Bilibili refresh credential is unavailable".to_owned(),
            ));
        };
        let Some(refresh_token) = refresh_token.filter(|value| !value.trim().is_empty()) else {
            return Err(CatalogError::AuthRequired(
                "Bilibili refresh token is missing".to_owned(),
            ));
        };
        let Some(csrf) = first_cookie(&cookies, &["bili_jct"])
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
        else {
            return Err(CatalogError::AuthRequired(
                "Bilibili csrf is missing".to_owned(),
            ));
        };

        let info_response = self
            .client
            .get(self.cookie_info_url.clone())
            .query(&[("csrf", csrf.clone())])
            .header("Cookie", cookie_header(&cookies))
            .header("Referer", REFERER)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&info_response)?;
        let info = info_response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        classify_api_response(&info)?;
        let info_data = info.get("data").and_then(Value::as_object).ok_or_else(|| {
            CatalogError::InvalidResponse("Bilibili cookie info is missing refresh data".to_owned())
        })?;
        if !info_data
            .get("refresh")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(false);
        }
        let timestamp = info_data.get("timestamp").and_then(as_u64).ok_or_else(|| {
            CatalogError::InvalidResponse(
                "Bilibili cookie info is missing refresh timestamp".to_owned(),
            )
        })?;

        let correspond_path = correspond_path(timestamp)?;
        let correspond_url = append_path_segment(&self.correspond_url, &correspond_path)?;
        let correspond_response = self
            .client
            .get(correspond_url)
            .header("Cookie", cookie_header(&cookies))
            .header("Referer", REFERER)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&correspond_response)?;
        let correspond_html = correspond_response
            .text()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        let refresh_csrf = parse_refresh_csrf(&correspond_html).ok_or_else(|| {
            CatalogError::InvalidResponse("Bilibili refresh CSRF was not found".to_owned())
        })?;

        let refresh_response = self
            .client
            .post(self.cookie_refresh_url.clone())
            .form(&[
                ("csrf", csrf.as_str()),
                ("refresh_csrf", refresh_csrf.as_str()),
                ("source", "main_web"),
                ("refresh_token", refresh_token.as_str()),
            ])
            .header("Cookie", cookie_header(&cookies))
            .header("Referer", REFERER)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&refresh_response)?;
        let refresh_headers = refresh_response.headers().clone();
        let refresh_body = refresh_response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        classify_api_response(&refresh_body)?;

        let mut refreshed = cookies.clone();
        merge_set_cookie_headers(&mut refreshed, &refresh_headers);
        let refreshed_refresh_token = refresh_body
            .pointer("/data/refresh_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| refresh_token.clone());
        let confirm_csrf = first_cookie(&refreshed, &["bili_jct"])
            .filter(|value| !value.is_empty())
            .unwrap_or(csrf.as_str());
        let confirm_response = self
            .client
            .post(self.confirm_refresh_url.clone())
            .form(&[
                ("csrf", confirm_csrf),
                ("refresh_token", refresh_token.as_str()),
            ])
            .header("Cookie", cookie_header(&refreshed))
            .header("Referer", REFERER)
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&confirm_response)?;
        let confirm_body = confirm_response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        classify_api_response(&confirm_body)?;

        self.credentials
            .save(
                "bilibili",
                ProviderCredential::Bilibili {
                    cookies: refreshed,
                    refresh_token: Some(refreshed_refresh_token),
                },
            )
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        Ok(true)
    }
}

#[async_trait]
impl CredentialRefreshAdapter for BilibiliAdapter {
    async fn refresh_credential(
        &self,
        _credential: &ProviderCredential,
    ) -> Result<Option<ProviderCredential>, CatalogError> {
        if !self.refresh_cookie().await? {
            return Ok(None);
        }
        self.credential().map(Some)
    }
}

#[async_trait]
impl SourceAdapter for BilibiliAdapter {
    async fn validate_credential(
        &self,
        candidate: &ProviderCredential,
    ) -> Result<(), CatalogError> {
        if candidate.provider() != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "Bilibili candidate provider does not match adapter".to_owned(),
            ));
        }
        let response = self
            .get_json_with_credential(candidate, &self.nav_url, &[], false)
            .await?;
        let data = response
            .get("data")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                CatalogError::InvalidResponse(
                    "Bilibili nav response is missing account data".to_owned(),
                )
            })?;
        if !data
            .get("isLogin")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(CatalogError::CredentialRejected(
                "Bilibili session is not authenticated".to_owned(),
            ));
        }
        if data.get("mid").and_then(as_u64).is_none_or(|mid| mid == 0) {
            return Err(CatalogError::CredentialRejected(
                "Bilibili authenticated session has no user id".to_owned(),
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
                "song key provider does not match Bilibili adapter".to_owned(),
            ));
        }
        let bvid = bilibili_resolver_bvid(key, locator)?;
        let response = self.resolve_json(&bvid).await?;
        let url = bilibili_dash_audio_url(&response)?;
        Ok(StreamSource {
            url,
            // CDN URLs are signed, so only provider-origin headers are needed.
            // Credentials must never be forwarded to a CDN URL.
            headers: BTreeMap::from([
                ("Referer".to_owned(), REFERER.to_owned()),
                ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ]),
            expires_at_epoch_ms: None,
        })
    }

    async fn lyrics(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<Option<TimedLyrics>, CatalogError> {
        if key.source != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "song key provider does not match Bilibili adapter".to_owned(),
            ));
        }
        bilibili_resolver_bvid(key, locator)?;
        Ok(None)
    }
}

pub(crate) fn parse_search_candidates(
    response: &Value,
) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
    let items = response
        .pointer("/data/result")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CatalogError::InvalidResponse("Bilibili search list is missing".to_owned())
        })?;
    let mut candidates = Vec::new();
    for raw in items {
        let Some(bvid) = raw.get("bvid").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if !is_bvid(bvid) {
            continue;
        }
        let title = raw
            .get("title")
            .and_then(Value::as_str)
            .map(strip_html)
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let artists = raw
            .get("author")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|artist| !artist.is_empty())
            .map(|artist| vec![artist.to_owned()])
            .unwrap_or_default();
        let song = Song {
            key: SongKey::new("bilibili", bvid)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            resolver_locator: ResolverLocator::new(format!("bilibili:v1:{bvid}")).ok(),
            title,
            artists,
            album: None,
            duration_ms: raw.get("duration").and_then(parse_duration_ms),
        };
        // B站搜索无需验证可用性：视频流默认可播，直接标 Eligible。
        candidates.push(ProviderSearchCandidate {
            song,
            eligibility: PlaybackEligibility::Eligible,
        });
        if candidates.len() == 5 {
            break;
        }
    }
    Ok(candidates)
}

fn bilibili_resolver_bvid(
    key: &SongKey,
    locator: Option<&ResolverLocator>,
) -> Result<String, CatalogError> {
    let bvid = match locator {
        Some(locator) => locator
            .as_str()
            .strip_prefix("bilibili:v1:")
            .ok_or_else(|| {
                CatalogError::InvalidResolverLocator(
                    "Bilibili locator has an unsupported version".to_owned(),
                )
            })?,
        None => &key.id,
    };
    if !is_bvid(bvid) {
        return Err(CatalogError::InvalidResolverLocator(
            "Bilibili locator must contain a BV id".to_owned(),
        ));
    }
    if bvid != key.id {
        return Err(CatalogError::ResolverLocatorTrackMismatch {
            requested_id: key.id.clone(),
            locator_id: bvid.to_owned(),
        });
    }
    Ok(bvid.to_owned())
}

fn bilibili_dash_audio_url(response: &Value) -> Result<Url, CatalogError> {
    let audio = response
        .pointer("/data/dash/audio")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CatalogError::Unavailable("Bilibili returned no DASH audio streams".to_owned())
        })?;
    audio
        .iter()
        .filter_map(|item| {
            let raw_url = item
                .get("base_url")
                .or_else(|| item.get("baseUrl"))
                .and_then(Value::as_str)?;
            let url = Url::parse(raw_url).ok()?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return None;
            }
            Some((
                item.get("bandwidth").and_then(as_u64).unwrap_or_default(),
                url,
            ))
        })
        .max_by_key(|(bandwidth, _)| *bandwidth)
        .map(|(_, url)| url)
        .ok_or_else(|| {
            CatalogError::Unavailable("Bilibili returned no playable DASH audio URL".to_owned())
        })
}

fn parse_duration_ms(value: &Value) -> Option<u64> {
    if let Some(seconds) = as_u64(value) {
        return seconds.checked_mul(1_000);
    }
    let text = value.as_str()?.trim();
    let seconds = text.split(':').try_fold(0_u64, |total, part| {
        let part = part.trim().parse::<u64>().ok()?;
        total.checked_mul(60)?.checked_add(part)
    })?;
    seconds.checked_mul(1_000)
}

fn strip_html(value: &str) -> String {
    let mut plain = String::with_capacity(value.len());
    let mut inside_tag = false;
    for character in value.chars() {
        match character {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => plain.push(character),
            _ => {}
        }
    }
    plain
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_owned()
}

fn is_bvid(value: &str) -> bool {
    value.starts_with("BV")
        && (8..=32).contains(&value.len())
        && value
            .bytes()
            .all(|character| character.is_ascii_alphanumeric())
}

fn as_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn current_day() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() / 86_400)
        .unwrap_or_default()
}

fn first_cookie<'a>(cookies: &'a BTreeMap<String, String>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| cookies.get(*name).map(String::as_str))
}

fn merge_set_cookie_headers(cookies: &mut BTreeMap<String, String>, headers: &HeaderMap) {
    for header in headers.get_all(SET_COOKIE).iter() {
        let Ok(header) = header.to_str() else {
            continue;
        };
        let Some((name, value)) = header.split(';').next().unwrap_or_default().split_once('=')
        else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if is_allowed_bilibili_cookie(name) && !value.is_empty() {
            cookies.insert(name.to_owned(), value.to_owned());
        }
    }
}

fn is_allowed_bilibili_cookie(name: &str) -> bool {
    matches!(
        name,
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
    )
}

fn correspond_path(timestamp: u64) -> Result<String, CatalogError> {
    let public_key = RsaPublicKey::from_public_key_pem(CORRESPOND_PUBLIC_KEY)
        .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
    let encrypted = public_key
        .encrypt(
            &mut OsRng,
            Oaep::new::<Sha256>(),
            format!("refresh_{timestamp}").as_bytes(),
        )
        .map_err(|error| CatalogError::Transient(error.to_string()))?;
    Ok(encrypted.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn append_path_segment(base: &Url, segment: &str) -> Result<Url, CatalogError> {
    let mut url = base.clone();
    let path = format!("{}/{segment}", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

fn parse_refresh_csrf(html: &str) -> Option<String> {
    for marker in ["id=\"1-name\"", "id='1-name'", "id=1-name"] {
        let start = html.find(marker)? + marker.len();
        let content_start = html[start..].find('>')? + start + 1;
        let content_end = html[content_start..].find('<')? + content_start;
        let value = html[content_start..content_end].trim();
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    None
}

fn classify_api_response(response: &Value) -> Result<(), CatalogError> {
    let code = response.get("code").and_then(as_i64).ok_or_else(|| {
        CatalogError::InvalidResponse("Bilibili response is missing code".to_owned())
    })?;
    if code == 0 {
        return Ok(());
    }
    let message = response
        .get("message")
        .or_else(|| response.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("no provider message");
    match code {
        -101 | -111 => Err(CatalogError::CredentialRejected(format!(
            "Bilibili rejected credentials (code {code}): {message}"
        ))),
        -352 | -412 => Err(CatalogError::RateLimited(format!(
            "Bilibili request was blocked (code {code}): {message}"
        ))),
        -404 | 62002 => Err(CatalogError::Unavailable(format!(
            "Bilibili track is unavailable (code {code}): {message}"
        ))),
        _ => Err(CatalogError::InvalidResponse(format!(
            "Bilibili API code {code}: {message}"
        ))),
    }
}

fn classify_status(response: &reqwest::Response) -> Result<(), CatalogError> {
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(CatalogError::CredentialRejected(
            "Bilibili rejected credentials".to_owned(),
        )),
        StatusCode::TOO_MANY_REQUESTS => {
            Err(CatalogError::RateLimited("Bilibili rate limit".to_owned()))
        }
        status if status.is_server_error() => {
            Err(CatalogError::Transient(format!("Bilibili HTTP {status}")))
        }
        status if !status.is_success() => Err(CatalogError::InvalidResponse(format!(
            "Bilibili HTTP {status}"
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
        BilibiliAdapter, PlaybackEligibility, bilibili_dash_audio_url, bilibili_resolver_bvid,
        parse_duration_ms, parse_search_candidates,
    };
    use crate::catalog::{CatalogError, SourceAdapter};
    use crate::credentials::{CredentialStore, ProviderCredential};
    use crate::domain::{ResolverLocator, SearchSpec, SongKey};
    use serde_json::json;
    use url::Url;

    fn fixture_server(responses: Vec<String>) -> (Url, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for body in responses {
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
            }
        });
        (
            Url::parse(&format!("http://{address}/bilibili-fixture")).unwrap(),
            receiver,
        )
    }

    fn credentials() -> CredentialStore {
        let store = CredentialStore::memory();
        store
            .save(
                "bilibili",
                ProviderCredential::Bilibili {
                    cookies: BTreeMap::from([("SESSDATA".to_owned(), "secret".to_owned())]),
                    refresh_token: None,
                },
            )
            .unwrap();
        store
    }

    #[test]
    fn search_normalizes_bilibili_video_metadata_and_preserves_unknown_rights() {
        let parsed = parse_search_candidates(&json!({
            "code": 0,
            "data": {"result": [{
                "bvid": "BV1xx411c7mD",
                "title": "A <em class=\"keyword\">song</em> &amp; more",
                "author": "Uploader",
                "duration": "03:21"
            }]}
        }))
        .unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].song.key.id, "BV1xx411c7mD");
        assert_eq!(parsed[0].song.title, "A song & more");
        assert_eq!(parsed[0].song.artists, ["Uploader"]);
        assert_eq!(parsed[0].song.duration_ms, Some(201_000));
        assert_eq!(parsed[0].eligibility, PlaybackEligibility::Eligible);
        assert_eq!(
            parsed[0].song.resolver_locator.as_ref().unwrap().as_str(),
            "bilibili:v1:BV1xx411c7mD"
        );
    }

    #[test]
    fn search_keeps_at_most_five_valid_bvids() {
        let result = (1..=9)
            .map(|index| {
                json!({
                    "bvid": format!("BV1xx411c7m{index}"),
                    "title": format!("Song {index}"),
                    "duration": 60
                })
            })
            .collect::<Vec<_>>();
        let parsed = parse_search_candidates(&json!({"data": {"result": result}})).unwrap();

        assert_eq!(parsed.len(), 5);
    }

    #[test]
    fn resolver_rejects_mismatched_or_non_bv_locators() {
        let key = SongKey::new("bilibili", "BV1xx411c7mD").unwrap();
        let mismatched = ResolverLocator::new("bilibili:v1:BV1xx411c7mE").unwrap();
        let invalid = ResolverLocator::new("bilibili:v1:av123").unwrap();

        assert!(matches!(
            bilibili_resolver_bvid(&key, Some(&mismatched)),
            Err(CatalogError::ResolverLocatorTrackMismatch { .. })
        ));
        assert!(matches!(
            bilibili_resolver_bvid(&key, Some(&invalid)),
            Err(CatalogError::InvalidResolverLocator(_))
        ));
    }

    #[test]
    fn dash_audio_prefers_the_highest_bandwidth_http_url() {
        let url = bilibili_dash_audio_url(&json!({"data": {"dash": {"audio": [
            {"baseUrl": "https://cdn.example.test/low.m4s", "bandwidth": 64000},
            {"base_url": "https://cdn.example.test/high.m4s", "bandwidth": "192000"},
            {"base_url": "ftp://cdn.example.test/invalid.m4s", "bandwidth": 999999}
        ]}}}))
        .unwrap();

        assert_eq!(url.as_str(), "https://cdn.example.test/high.m4s");
        assert!(matches!(
            bilibili_dash_audio_url(&json!({"data": {"dash": {"audio": []}}})),
            Err(CatalogError::Unavailable(_))
        ));
    }

    #[test]
    fn duration_parser_handles_text_and_numeric_values() {
        assert_eq!(parse_duration_ms(&json!("1:02:03")), Some(3_723_000));
        assert_eq!(parse_duration_ms(&json!(180)), Some(180_000));
        assert_eq!(parse_duration_ms(&json!("bad")), None);
    }

    #[tokio::test]
    async fn bilibili_lyrics_are_explicitly_absent() {
        let adapter =
            BilibiliAdapter::new(CredentialStore::memory(), Duration::from_secs(2)).unwrap();
        let key = SongKey::new("bilibili", "BV1us411d75p").unwrap();
        let locator = ResolverLocator::new("bilibili:v1:BV1us411d75p").unwrap();

        assert!(
            adapter
                .lyrics(&key, Some(&locator))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn resolve_uses_injected_endpoints_and_never_forwards_cookies_to_the_cdn() {
        let (endpoint, requests) = fixture_server(vec![
            json!({"code": 0, "data": {"cid": 42}}).to_string(),
            json!({"code": 0, "data": {"dash": {"audio": [{
                "base_url": "https://cdn.example.test/audio.m4s", "bandwidth": 192000
            }]}}})
            .to_string(),
        ]);
        let adapter = BilibiliAdapter::with_endpoints(
            credentials(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();
        let key = SongKey::new("bilibili", "BV1xx411c7mD").unwrap();

        let source = adapter.resolve(&key, None).await.unwrap();

        assert_eq!(source.url.as_str(), "https://cdn.example.test/audio.m4s");
        assert!(!source.headers.contains_key("Cookie"));
        assert_eq!(
            source.headers.get("Referer").unwrap(),
            "https://www.bilibili.com/"
        );
        assert!(source.headers.contains_key("User-Agent"));
        let view_request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        let playurl_request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(view_request.contains("bvid=BV1xx411c7mD"));
        assert!(playurl_request.contains("cid=42"));
        assert!(
            view_request
                .to_ascii_lowercase()
                .contains("cookie: sessdata=secret")
        );
    }

    #[tokio::test]
    async fn search_uses_injected_endpoint_and_transmits_bilibili_cookie() {
        let (endpoint, requests) = fixture_server(vec![
            json!({
                "code": 0,
                "data": {"result": []}
            })
            .to_string(),
        ]);
        let adapter = BilibiliAdapter::with_endpoints(
            credentials(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();

        let result = adapter
            .search(&SearchSpec {
                keyword: "music".to_owned(),
                sources: Vec::new(),
                limit: 1,
            })
            .await
            .unwrap();

        assert!(result.is_empty());
        let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(request.contains("search_type=video"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("cookie: sessdata=secret")
        );
    }

    #[tokio::test]
    async fn credential_validation_requires_an_authenticated_nav_response() {
        let (endpoint, requests) = fixture_server(vec![
            json!({
                "code": 0,
                "data": {"isLogin": true, "mid": 12345}
            })
            .to_string(),
        ]);
        let adapter = BilibiliAdapter::with_endpoints(
            credentials(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();

        adapter
            .validate_credential(&ProviderCredential::Bilibili {
                cookies: BTreeMap::from([("SESSDATA".to_owned(), "candidate".to_owned())]),
                refresh_token: None,
            })
            .await
            .unwrap();
        assert!(
            requests
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .to_ascii_lowercase()
                .contains("cookie: sessdata=candidate")
        );
    }

    #[tokio::test]
    async fn credential_validation_rejects_anonymous_nav_response() {
        let (endpoint, _requests) = fixture_server(vec![
            json!({"code": 0, "data": {"isLogin": false, "mid": 0}}).to_string(),
        ]);
        let adapter = BilibiliAdapter::with_endpoints(
            credentials(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();

        assert!(matches!(
            adapter
                .validate_credential(&ProviderCredential::Bilibili {
                    cookies: BTreeMap::from([("SESSDATA".to_owned(), "expired".to_owned())]),
                    refresh_token: None,
                })
                .await,
            Err(CatalogError::CredentialRejected(_))
        ));
    }
}
