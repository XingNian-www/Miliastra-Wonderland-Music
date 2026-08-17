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
use url::Url;

use crate::catalog::{
    AccountStatusCache, CatalogError, CredentialRefreshAdapter, PlaybackEligibility,
    ProviderSearchCandidate, SourceAdapter,
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
    account_cache: Arc<Mutex<AccountStatusCache>>,
}

impl BilibiliAdapter {
    const ACCOUNT_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
    const ACCOUNT_CACHE_FAILED_TTL: Duration = Duration::from_secs(15 * 60);

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
            account_cache: Arc::new(Mutex::new(AccountStatusCache::default())),
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

    async fn account_status_cached(
        &self,
        force: bool,
    ) -> Result<Option<crate::catalog::ProviderAccountStatus>, CatalogError> {
        let (refresh_gate, cached) = {
            let guard = self.account_cache.lock().map_err(|_| {
                CatalogError::Transient("Bilibili account cache poisoned".to_owned())
            })?;
            let cached = (!force)
                .then(|| guard.cached(Self::ACCOUNT_CACHE_TTL, Self::ACCOUNT_CACHE_FAILED_TTL))
                .flatten();
            (guard.refresh_gate(), cached)
        };
        if let Some(status) = cached {
            return Ok(Some(status));
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
            let guard = self.account_cache.lock().map_err(|_| {
                CatalogError::Transient("Bilibili account cache poisoned".to_owned())
            })?;
            let cached = (!force || waited_for_refresh)
                .then(|| guard.cached(Self::ACCOUNT_CACHE_TTL, Self::ACCOUNT_CACHE_FAILED_TTL))
                .flatten();
            (guard.generation(), cached)
        };
        if let Some(status) = cached {
            return Ok(Some(status));
        }

        let now = std::time::Instant::now();
        let checked_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let result = match self.credential() {
            Err(_) => Ok(Some(crate::catalog::ProviderAccountStatus {
                provider: PROVIDER.to_owned(),
                logged_in: false,
                checked_at_ms,
                ..crate::catalog::ProviderAccountStatus::default()
            })),
            Ok(credential) => self
                .get_json_with_credential(&credential, &self.nav_url, &[], false, None)
                .await
                .map(|value| {
                    let data = value.get("data").unwrap_or(&value);
                    Some(crate::catalog::ProviderAccountStatus {
                        provider: PROVIDER.to_owned(),
                        logged_in: data
                            .get("isLogin")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        user_id: data
                            .get("mid")
                            .and_then(Value::as_u64)
                            .map(|id| id.to_string()),
                        nickname: data.get("uname").and_then(Value::as_str).map(str::to_owned),
                        vip_known: false,
                        checked_at_ms,
                        ..crate::catalog::ProviderAccountStatus::default()
                    })
                }),
        };
        let mut guard = self
            .account_cache
            .lock()
            .map_err(|_| CatalogError::Transient("Bilibili account cache poisoned".to_owned()))?;
        match result {
            Ok(Some(status)) => {
                guard.store_if_current(generation, status.clone(), now, false);
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                if let Some(stale) = guard.stale_for_current(generation, &error) {
                    guard.store_if_current(generation, stale.clone(), now, true);
                    Ok(Some(stale))
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

    async fn get_json_with_credential(
        &self,
        credential: &ProviderCredential,
        url: &Url,
        query: &[(String, String)],
        persist_cookie_updates: bool,
        expected_revision: Option<u64>,
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
            self.persist_set_cookie_updates(credential, expected_revision, &headers)
                .await?;
        }
        Ok(response)
    }

    async fn get_json(&self, url: &Url, query: &[(String, String)]) -> Result<Value, CatalogError> {
        let snapshot = self
            .credentials
            .snapshot(PROVIDER)
            .map_err(|error| CatalogError::AuthRequired(error.to_string()))?
            .ok_or_else(|| CatalogError::AuthRequired("Bilibili credential_required".to_owned()))?;
        self.get_json_with_credential(
            &snapshot.credential,
            url,
            query,
            true,
            Some(snapshot.revision),
        )
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

    async fn view_json(&self, bvid: &str) -> Result<Value, CatalogError> {
        self.get_json(&self.view_url, &[("bvid".to_owned(), bvid.to_owned())])
            .await
    }

    async fn resolve_json(&self, bvid: &str) -> Result<Value, CatalogError> {
        let view = self.view_json(bvid).await?;
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

    async fn persist_set_cookie_updates(
        &self,
        credential: &ProviderCredential,
        expected_revision: Option<u64>,
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
        let Some(expected_revision) = expected_revision else {
            return Ok(());
        };
        // 凭证落盘(含 fsync/ACL)移到阻塞线程池，并仅在原凭据仍有效时写入。
        let store = self.credentials.clone();
        let refresh_token = refresh_token.clone();
        let saved = tokio::task::spawn_blocking(move || {
            store.save_if_revision(
                "bilibili",
                expected_revision,
                ProviderCredential::Bilibili {
                    cookies: updated,
                    refresh_token,
                },
            )
        })
        .await
        .map_err(|error| CatalogError::Transient(error.to_string()))?
        .map_err(|error| CatalogError::Transient(error.to_string()))?;
        if saved.is_some() {
            self.invalidate_account_status();
        }
        Ok(())
    }

    async fn refresh_cookie(
        &self,
        credential: &ProviderCredential,
    ) -> Result<Option<ProviderCredential>, CatalogError> {
        let ProviderCredential::Bilibili {
            cookies,
            refresh_token,
        } = credential
        else {
            return Err(CatalogError::AuthRequired(
                "Bilibili refresh credential is unavailable".to_owned(),
            ));
        };
        let Some(refresh_token) = refresh_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
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
            return Ok(None);
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
                ("refresh_token", refresh_token),
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
            .unwrap_or_else(|| refresh_token.to_owned());
        let confirm_csrf = first_cookie(&refreshed, &["bili_jct"])
            .filter(|value| !value.is_empty())
            .unwrap_or(csrf.as_str());
        let confirm_response = self
            .client
            .post(self.confirm_refresh_url.clone())
            .form(&[("csrf", confirm_csrf), ("refresh_token", refresh_token)])
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

        Ok(Some(ProviderCredential::Bilibili {
            cookies: refreshed,
            refresh_token: Some(refreshed_refresh_token),
        }))
    }
}

#[async_trait]
impl CredentialRefreshAdapter for BilibiliAdapter {
    async fn refresh_credential(
        &self,
        credential: &ProviderCredential,
    ) -> Result<Option<ProviderCredential>, CatalogError> {
        self.refresh_cookie(credential).await
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
            .get_json_with_credential(candidate, &self.nav_url, &[], false, None)
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
        let keyword = spec.keyword.trim();
        // 游戏聊天 OCR 场景：关键词若是 BV 号（允许 OCR 噪声与混淆字符），
        // 直接按 BV 解析单曲，避免把 BV 号当搜索词返回空结果或无关内容。
        if let Some(bvid) = normalize_bvid(keyword) {
            let response = self.view_json(&bvid).await?;
            return parse_view_candidate(&response)?
                .map(|candidate| vec![candidate])
                .ok_or_else(|| {
                    CatalogError::InvalidResponse(
                        "Bilibili view response contains no valid BV track".to_owned(),
                    )
                });
        }
        let response = self.search_json(keyword, spec.limit).await?;
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

    async fn account_status(
        &self,
    ) -> Result<Option<crate::catalog::ProviderAccountStatus>, CatalogError> {
        self.account_status_cached(false).await
    }

    async fn refresh_account_status(
        &self,
    ) -> Result<Option<crate::catalog::ProviderAccountStatus>, CatalogError> {
        self.account_status_cached(true).await
    }

    fn invalidate_account_status(&self) {
        if let Ok(mut guard) = self.account_cache.lock() {
            guard.invalidate();
        }
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

/// B站 bvid 算法当前生效的官方 58 字符表（base58，排除数字 `0` 与 `I`/`O`/`l`）。
///
/// 真实 BV 号恒为 12 位：固定前缀 `BV1` + 9 位表内字符。
/// 参考 bilibili-API-collect 文档：
/// <https://github.com/SocialSisterYi/bilibili-API-collect/blob/master/docs/misc/bvid_desc.md>
const BVID_TABLE: &str = "FcwAPNKTMug3GV5Lj7EJnHpWsx4tb8haYeviqBz6rkCy12mUSDQX9RdoZf";

fn is_bvid_char(character: char) -> bool {
    BVID_TABLE.contains(character)
}

/// 纠正单个 OCR 误读字符为合法 BV 字符；无法可靠纠正的返回 None。
fn correct_bvid_char(character: char) -> Option<char> {
    if is_bvid_char(character) {
        return Some(character);
    }
    match character {
        // OCR 常把数字 1 识别成大写 I、小写 l、竖线、撇号或反引号；
        // 表内小写 o 与数字 0、大写 O 同形，OCR 常把 o 识别成 0/O；
        // 表内大写 J 常被 OCR 识别成右方括号（半角/全角）。
        'I' | 'l' | '|' | '\'' | '`' => Some('1'),
        'O' | '0' => Some('o'),
        ']' | '】' => Some('J'),
        _ => None,
    }
}

/// 从含 OCR 噪声的文本中容错提取规范化 BV 号（BV1 + 9 位官方字符）。
///
/// 接受任意大小写前缀（bv/Bv/bV/BV），输出规范大写；识别不到返回 None。
/// 窗口滑动扫描：文本中任何「bv + 10 位可纠正字符」的 12 位片段都算命中，
/// 因此分享链接、URL 参数、混入标点/混淆字符的 OCR 结果均可提取。
pub(crate) fn normalize_bvid(input: &str) -> Option<String> {
    let characters = input.chars().collect::<Vec<_>>();
    if characters.len() < 12 {
        return None;
    }
    for start in 0..=characters.len() - 12 {
        if !matches!(characters[start], 'B' | 'b') || !matches!(characters[start + 1], 'V' | 'v') {
            continue;
        }
        let mut corrected = String::with_capacity(12);
        // 前缀强制归一为大写 BV；第三位仍走混淆纠正（I/l/0/|/撇号 → 1）。
        corrected.push('B');
        corrected.push('V');
        let mut valid = true;
        for &character in &characters[start + 2..start + 12] {
            match correct_bvid_char(character) {
                Some(character) => corrected.push(character),
                None => {
                    valid = false;
                    break;
                }
            }
        }
        // 官方格式要求第三位固定为 1（BV1 前缀）。
        if valid && corrected.starts_with("BV1") {
            return Some(corrected);
        }
    }
    None
}

fn is_bvid(value: &str) -> bool {
    value.starts_with("BV1") && value.len() == 12 && value.chars().skip(3).all(is_bvid_char)
}

/// 从 B站 view 接口响应构造单曲候选（BV 号直解析时复用）。
fn parse_view_candidate(response: &Value) -> Result<Option<ProviderSearchCandidate>, CatalogError> {
    let data = response.pointer("/data").ok_or_else(|| {
        CatalogError::InvalidResponse("Bilibili view response is missing data".to_owned())
    })?;
    let Some(bvid) = data
        .get("bvid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|bvid| is_bvid(bvid))
    else {
        return Ok(None);
    };
    let title = data
        .get("title")
        .and_then(Value::as_str)
        .map(strip_html)
        .filter(|title| !title.is_empty());
    let Some(title) = title else {
        return Ok(None);
    };
    let artists = data
        .pointer("/owner/name")
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
        duration_ms: data.get("duration").and_then(parse_duration_ms),
    };
    Ok(Some(ProviderSearchCandidate {
        song,
        eligibility: PlaybackEligibility::Eligible,
    }))
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
    use std::time::{Duration, Instant};

    use super::{
        BilibiliAdapter, PlaybackEligibility, bilibili_dash_audio_url, bilibili_resolver_bvid,
        is_bvid, normalize_bvid, parse_duration_ms, parse_search_candidates,
    };
    use crate::catalog::{CatalogError, ProviderAccountStatus, SourceAdapter};
    use crate::credentials::{CredentialStore, ProviderCredential};
    use crate::domain::{ResolverLocator, SearchSpec, SongKey};
    use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};
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

    #[tokio::test]
    async fn persisted_cookie_rotation_invalidates_account_status_cache() {
        let store = credentials();
        let adapter = BilibiliAdapter::new(store.clone(), Duration::from_secs(2)).unwrap();
        {
            let mut cache = adapter.account_cache.lock().unwrap();
            let generation = cache.generation();
            assert!(cache.store_if_current(
                generation,
                ProviderAccountStatus {
                    provider: "bilibili".to_owned(),
                    logged_in: true,
                    user_id: Some("old-user".to_owned()),
                    ..ProviderAccountStatus::default()
                },
                Instant::now(),
                false,
            ));
        }
        let snapshot = store.snapshot("bilibili").unwrap().unwrap();
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("SESSDATA=rotated-session; Path=/; HttpOnly"),
        );

        adapter
            .persist_set_cookie_updates(&snapshot.credential, Some(snapshot.revision), &headers)
            .await
            .unwrap();

        let cache = adapter.account_cache.lock().unwrap();
        assert!(
            cache
                .cached(Duration::from_secs(1), Duration::from_secs(1))
                .is_none(),
            "newly persisted cookies must not retain the account status from the old session"
        );
        drop(cache);
        assert_eq!(
            store
                .get("bilibili")
                .unwrap()
                .unwrap()
                .cookies()
                .get("SESSDATA")
                .map(String::as_str),
            Some("rotated-session")
        );
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

    #[test]
    fn bvid_matches_the_official_character_table() {
        // 官方示例与真实 BV 号。
        for bvid in [
            "BV1GJ411x7h7",
            "BV1us411d75p",
            "BV1Q5411674a",
            "BV1xx411c7mD",
            "BV1L9Uoa9EUx",
            "BV16x4y1H7M1",
        ] {
            assert!(is_bvid(bvid), "{bvid} 应命中官方字符集");
        }
        // 表外字符 0/I/O/l、长度不符、第三位非 1、非 BV1 前缀一律拒绝。
        for invalid in [
            "BV1GJ411x7h0",
            "BV1GJ411x7hI",
            "BV1GJ411x7hO",
            "BV1GJ411x7hl",
            "BV1GJ411x7h7x",
            "BV1GJ411x7h",
            "BVxGJ411x7h7",
            "bv1GJ411x7h7",
            "av1GJ411x7h7",
        ] {
            assert!(!is_bvid(invalid), "{invalid} 不应命中");
        }
    }

    #[test]
    fn normalize_bvid_extracts_from_ocr_noisy_text() {
        // 分享链接、URL 参数、带大小写变体与混淆字符的 OCR 文本。
        for (input, expected) in [
            ("BV1GJ411x7h7", Some("BV1GJ411x7h7")),
            ("BV1L9Uoa9EUx", Some("BV1L9Uoa9EUx")),
            (
                "https://www.bilibili.com/video/BV1GJ411x7h7?p=2",
                Some("BV1GJ411x7h7"),
            ),
            ("【标题】 BV1GJ411x7h7 超好听", Some("BV1GJ411x7h7")),
            ("bv1GJ411x7h7", Some("BV1GJ411x7h7")),
            // OCR 把数字 1 识别成大写 I / 小写 l / 竖线 / 撇号。
            ("BV1GJ4I1x7h7", Some("BV1GJ411x7h7")),
            ("BV1GJ4l1x7h7", Some("BV1GJ411x7h7")),
            ("BV1GJ41|x7h7", Some("BV1GJ411x7h7")),
            ("BV1GJ41'x7h7", Some("BV1GJ411x7h7")),
            // OCR 把表内小写 o 识别成大写 O 或数字 0。
            ("BV1GJ4O1x7h7", Some("BV1GJ4o1x7h7")),
            ("BV1GJ401x7h7", Some("BV1GJ4o1x7h7")),
            // OCR 把表内大写 J 识别成右方括号（半角/全角）。
            ("BV1G]411x7h7", Some("BV1GJ411x7h7")),
            ("BV1G】411x7h7", Some("BV1GJ411x7h7")),
            // 分隔符被 OCR 吞掉时仍取前 10 位。
            ("BV1GJ411x7h7x后续", Some("BV1GJ411x7h7")),
        ] {
            assert_eq!(normalize_bvid(input).as_deref(), expected, "input={input}");
        }
    }

    #[test]
    fn normalize_bvid_rejects_unrecoverable_input() {
        // 窗口内混入分隔符/中文、长度不足、第三位非 1、无 BV 前缀、空串。
        for input in [
            "",
            "BV1GJ4 1x7h7",
            "BV1GJ4-1x7h7",
            "BV1GJ4啊1x7h7",
            "BV1GJ411x7h",
            "BVxGJ411x7h7",
            "hello world",
        ] {
            assert_eq!(normalize_bvid(input), None, "input={input}");
        }
    }

    #[tokio::test]
    async fn search_resolves_a_bvid_keyword_directly_without_hitting_search_api() {
        let (endpoint, requests) = fixture_server(vec![
            json!({
                "code": 0,
                "data": {
                    "bvid": "BV1GJ411x7h7",
                    "title": "Sample &amp; Title",
                    "owner": {"name": "Uploader"},
                    "duration": "03:21"
                }
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

        // 模拟游戏聊天 OCR 结果：BV 号前带分享文案、数字 1 被识别成大写 I。
        let result = adapter
            .search(&SearchSpec {
                keyword: "这个视频 BV1GJ4I1x7h7 好听".to_owned(),
                sources: Vec::new(),
                limit: 5,
            })
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].song.key.id, "BV1GJ411x7h7");
        assert_eq!(result[0].song.title, "Sample & Title");
        assert_eq!(result[0].song.artists, ["Uploader"]);
        assert_eq!(result[0].song.duration_ms, Some(201_000));
        assert_eq!(result[0].eligibility, PlaybackEligibility::Eligible);
        let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(request.contains("bvid=BV1GJ411x7h7"));
        assert!(!request.contains("search_type"));
    }

    #[tokio::test]
    async fn search_keeps_using_search_api_for_plain_keywords() {
        let (endpoint, requests) =
            fixture_server(vec![json!({"code": 0, "data": {"result": []}}).to_string()]);
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
                keyword: "晴天 周杰伦".to_owned(),
                sources: Vec::new(),
                limit: 5,
            })
            .await
            .unwrap();

        assert!(result.is_empty());
        let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(request.contains("search_type=video"));
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
