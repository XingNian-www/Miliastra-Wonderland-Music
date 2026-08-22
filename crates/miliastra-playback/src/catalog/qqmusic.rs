//! Minimal native QQ Music protocol adapter.
//!
//! The adapter deliberately owns the provider-specific JSON shape and rights
//! flags. The playback core only sees canonical Song values and three-state
//! PlaybackEligibility.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aes::Aes128;
use async_trait::async_trait;
use base64::Engine;
use cbc::Encryptor;
use cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use rand::Rng;
use reqwest::{Client, StatusCode};
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::catalog::{
    AccountStatusCache, CatalogError, CredentialRefreshAdapter, PlaybackEligibility,
    ProviderAccountStatus, ProviderSearchCandidate, SourceAdapter,
};
use crate::credentials::{CredentialStore, ProviderCredential};
use crate::domain::{ResolverLocator, SearchSpec, Song, SongKey, StreamSource};
use crate::lyrics::{TimedLyrics, parse_lrc_pair};

const SEARCH_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const PROVIDER: &str = "qqmusic";
const RESOLVE_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const WEB_REFRESH_URL: &str = "https://c.y.qq.com/base/fcgi-bin/login_get_musickey.fcg";
const LYRICS_URL: &str = "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg";
const LYRICS_NATIVE_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const QQ_MOBILE_VERSION: i64 = 14_090_008;
const QQ_QIMEI_FALLBACK: &str = "6c9d3cd110abca9b16311cee10001e717614";
const QQ_DEVICE_FILE: &str = "qqmusic-device.json";
const QIMEI_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const QIMEI_APP_VERSION: &str = "14.9.0.8";
const QIMEI_SDK_VERSION: &str = "1.2.13.6";
const QIMEI_URL: &str = "https://api.tencentmusic.com/tme/trpc/proxy";
const QIMEI_APP_KEY: &str = "0AND0HD6FE4HY80F";
const QIMEI_SECRET: &str = "ZdJqM15EeO2zWc08";
const QIMEI_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDEIxgwoutfwoJxcGQeedgP7FG9\nqaIuS0qzfR8gWkrkTZKM2iWHn2ajQpBRZjMSoSf6+KJGvar2ORhBfpDXyVtZCKpq\nLQ+FLkpncClKVIrBwv6PHyUvuCb0rIarmgDnzkfQAqVufEtR64iazGDKatvJ9y6B\n9NMbHddGSAUmRTCrHQIDAQAB\n-----END PUBLIC KEY-----";
const PLAY_BITS_MASK: i64 = 0b1110;
const TRY_BIT_MASK: i64 = 1 << 14;

#[derive(Clone)]
pub struct QqMusicAdapter {
    client: Client,
    credentials: CredentialStore,
    search_url: Url,
    resolve_url: Url,
    lyrics_url: Url,
    native_lyrics_url: Url,
    device: QqDevice,
    /// QIMEI is refreshed at most once per 24 hours and shared by concurrent
    /// credential refreshes in the same adapter instance.
    qimei_cache: std::sync::Arc<std::sync::Mutex<Option<QqQimeiCache>>>,
    /// Single-flight gate for a cache miss.  Without this gate concurrent
    /// refreshes could issue several QIMEI requests for the same device.
    qimei_gate: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// 账号 VIP 状态缓存（成功 24 小时/失败 15 分钟），避免轮询频繁打账号接口。
    account_cache: std::sync::Arc<std::sync::Mutex<AccountStatusCache>>,
}

/// Extra query/header values used by a provider request.  Keeping these
/// alongside the request body means an auth retry can replay the exact same
/// native-lyrics request after the credential snapshot is rotated.
#[derive(Clone, Default)]
struct QqRequestOptions {
    query: Vec<(String, String)>,
    referer: Option<String>,
}

/// QQ 账号状态采用 24 小时乐观缓存，避免频繁请求会员接口。
const ACCOUNT_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 上次刷新失败后，旧成功结果继续返回；15 分钟后才允许再次联网重试。
const ACCOUNT_CACHE_FAILED_TTL: Duration = Duration::from_secs(15 * 60);

impl QqMusicAdapter {
    pub fn new(credentials: CredentialStore, timeout: Duration) -> Result<Self, CatalogError> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent("miliastra-wonderland-music/0.1")
            .build()
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        let device = QqDevice::load(credentials.auxiliary_path(QQ_DEVICE_FILE))
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        let qimei_cache = device
            .qimei
            .clone()
            .zip(device.qimei_saved_at)
            .filter(|(qimei, saved_at)| qimei_is_fresh(qimei, *saved_at, epoch_secs()))
            .map(|(value, saved_at)| QqQimeiCache { value, saved_at });
        Ok(Self {
            client,
            credentials,
            device,
            search_url: Url::parse(SEARCH_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            resolve_url: Url::parse(RESOLVE_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            lyrics_url: Url::parse(LYRICS_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            native_lyrics_url: Url::parse(LYRICS_NATIVE_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            qimei_cache: std::sync::Arc::new(std::sync::Mutex::new(qimei_cache)),
            qimei_gate: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            account_cache: std::sync::Arc::new(
                std::sync::Mutex::new(AccountStatusCache::default()),
            ),
        })
    }

    #[cfg(test)]
    pub fn with_endpoints(
        credentials: CredentialStore,
        timeout: Duration,
        search_url: Url,
        resolve_url: Url,
    ) -> Result<Self, CatalogError> {
        let mut adapter = Self::new(credentials, timeout)?;
        adapter.search_url = search_url;
        adapter.resolve_url = resolve_url.clone();
        Ok(adapter)
    }

    #[cfg(test)]
    pub fn with_lyrics_endpoints(mut self, native_lyrics_url: Url, lyrics_url: Url) -> Self {
        self.native_lyrics_url = native_lyrics_url;
        self.lyrics_url = lyrics_url;
        self
    }

    fn credential(&self) -> Result<ProviderCredential, CatalogError> {
        self.credentials
            .get("qqmusic")
            .map_err(|error| CatalogError::AuthRequired(error.to_string()))?
            .ok_or_else(|| CatalogError::AuthRequired("QQ Music credential_required".to_owned()))
    }

    async fn request_json_with_credential(
        &self,
        credential: &ProviderCredential,
        url: &Url,
        body: Value,
    ) -> Result<Value, CatalogError> {
        self.request_json_with_credential_options(
            credential,
            url,
            body,
            &QqRequestOptions::default(),
        )
        .await
    }

    async fn request_json_with_credential_options(
        &self,
        credential: &ProviderCredential,
        url: &Url,
        body: Value,
        options: &QqRequestOptions,
    ) -> Result<Value, CatalogError> {
        let cookie = cookie_header(credential.cookies());
        let mut request = self
            .client
            .get(url.clone())
            .query(&[("data", body.to_string())])
            .header("Cookie", cookie);
        for (name, value) in &options.query {
            request = request.query(&[(name.as_str(), value.as_str())]);
        }
        if let Some(referer) = options.referer.as_deref() {
            request = request.header("Referer", referer);
        }
        let response = request.send().await.map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn request_json(&self, url: &Url, body: Value) -> Result<Value, CatalogError> {
        self.request_json_with_options(url, body, QqRequestOptions::default())
            .await
    }

    async fn request_json_with_options(
        &self,
        url: &Url,
        body: Value,
        options: QqRequestOptions,
    ) -> Result<Value, CatalogError> {
        let snapshot = self
            .credentials
            .snapshot(PROVIDER)
            .map_err(|error| CatalogError::AuthRequired(error.to_string()))?
            .ok_or_else(|| CatalogError::AuthRequired("QQ Music credential_required".to_owned()))?;
        match self
            .request_json_with_credential_options(&snapshot.credential, url, body.clone(), &options)
            .await
        {
            Ok(response) if qq_response_requires_auth_refresh(&response) => {
                self.retry_after_refresh_with_options(
                    &snapshot.credential,
                    snapshot.revision,
                    url,
                    body,
                    options,
                )
                .await
            }
            Ok(response) => {
                if let Some(error) = qq_response_business_error(&response) {
                    Err(error)
                } else {
                    Ok(response)
                }
            }
            Err(CatalogError::AuthRequired(_)) => {
                self.retry_after_refresh_with_options(
                    &snapshot.credential,
                    snapshot.revision,
                    url,
                    body,
                    options,
                )
                .await
            }
            Err(error) => Err(error),
        }
    }

    async fn lyrics_json(&self, song_mid: &str) -> Result<Value, CatalogError> {
        let credential = self.credential()?;
        let response = self
            .client
            .get(self.lyrics_url.clone())
            .query(&[
                ("songmid", song_mid),
                ("format", "json"),
                ("nobase64", "1"),
                ("g_tk", "5381"),
            ])
            .header("Cookie", cookie_header(credential.cookies()))
            .header("Referer", "https://y.qq.com/")
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        let body = response
            .text()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        parse_qq_json(&body)
    }

    async fn native_lyrics_json(&self, song_mid: &str) -> Result<Value, CatalogError> {
        let body = json!({
            "req": {
                "method": "GetPlayLyricInfo",
                "module": "music.musichallSong.PlayLyricInfo",
                "param": {
                    "songMID": song_mid,
                    "trans": 1
                }
            }
        });
        let options = QqRequestOptions {
            query: vec![
                ("format".to_owned(), "json".to_owned()),
                ("inCharset".to_owned(), "utf8".to_owned()),
                ("outCharset".to_owned(), "utf-8".to_owned()),
            ],
            referer: Some("https://y.qq.com/".to_owned()),
        };
        self.request_json_with_options(&self.native_lyrics_url, body, options)
            .await
    }

    async fn lyrics_json_with_translation(&self, song_mid: &str) -> Result<Value, CatalogError> {
        // Native lyrics now use the same one-shot refresh path as search and
        // vkey requests.  Only a successful response with no lyric falls back
        // to the legacy endpoint; auth/rate-limit/provider errors are returned
        // intact instead of being hidden by an unconditional fallback.
        let response = self.native_lyrics_json(song_mid).await?;
        if qq_response_code(&response).is_some_and(|code| code != 0) {
            if let Some(error) = qq_response_business_error(&response) {
                return Err(error);
            }
            return Err(CatalogError::Unavailable(
                "QQ Music native lyric request returned a business error".to_owned(),
            ));
        }
        if qq_response_has_lyrics(&response) {
            Ok(response)
        } else {
            self.lyrics_json(song_mid).await
        }
    }

    async fn retry_after_refresh_with_options(
        &self,
        credential: &ProviderCredential,
        expected_revision: u64,
        url: &Url,
        body: Value,
        options: QqRequestOptions,
    ) -> Result<Value, CatalogError> {
        let Some(_refresh_lease) = self
            .credentials
            .try_acquire_refresh_lease(PROVIDER)
            .map_err(|error| CatalogError::Transient(error.to_string()))?
        else {
            return self
                .retry_with_current_credential_with_options(
                    Some(expected_revision),
                    url,
                    body,
                    options,
                )
                .await;
        };
        let snapshot_is_current = self
            .credentials
            .snapshot(PROVIDER)
            .map_err(|error| CatalogError::Transient(error.to_string()))?
            .is_some_and(|current| {
                current.revision == expected_revision && current.credential == *credential
            });
        if !snapshot_is_current {
            return self
                .retry_with_current_credential_with_options(
                    Some(expected_revision),
                    url,
                    body,
                    options,
                )
                .await;
        }
        let Some(refreshed) = self.refresh_credential(credential).await? else {
            return Err(CatalogError::AuthRequired(
                "QQ Music credentials require relogin".to_owned(),
            ));
        };
        if self
            .persist_refreshed_credential(expected_revision, refreshed.clone())
            .await?
        {
            return self
                .request_with_refreshed_credential_with_options(&refreshed, url, body, options)
                .await;
        }
        self.retry_with_current_credential_with_options(Some(expected_revision), url, body, options)
            .await
    }

    async fn retry_with_current_credential_with_options(
        &self,
        expected_revision: Option<u64>,
        url: &Url,
        body: Value,
        options: QqRequestOptions,
    ) -> Result<Value, CatalogError> {
        let current = self
            .credentials
            .snapshot(PROVIDER)
            .map_err(|error| CatalogError::Transient(error.to_string()))?
            .ok_or_else(|| {
                CatalogError::AuthRequired("QQ Music credentials require relogin".to_owned())
            })?;
        if expected_revision.is_some_and(|expected| current.revision == expected) {
            return Err(CatalogError::Transient(
                "QQ Music credential refresh is already in progress".to_owned(),
            ));
        }
        self.request_with_refreshed_credential_with_options(&current.credential, url, body, options)
            .await
    }

    async fn request_with_refreshed_credential_with_options(
        &self,
        credential: &ProviderCredential,
        url: &Url,
        body: Value,
        options: QqRequestOptions,
    ) -> Result<Value, CatalogError> {
        let response = self
            .request_json_with_credential_options(credential, url, body, &options)
            .await?;
        if let Some(error) = qq_response_business_error(&response) {
            return Err(if matches!(error, CatalogError::AuthRequired(_)) {
                CatalogError::AuthRequired("QQ Music credentials require relogin".to_owned())
            } else {
                error
            });
        }
        Ok(response)
    }

    async fn persist_refreshed_credential(
        &self,
        expected_revision: u64,
        refreshed: ProviderCredential,
    ) -> Result<bool, CatalogError> {
        let store = self.credentials.clone();
        let saved = tokio::task::spawn_blocking(move || {
            store.save_if_revision(PROVIDER, expected_revision, refreshed)
        })
        .await
        .map_err(|error| CatalogError::Transient(error.to_string()))?
        .map_err(|error| CatalogError::Transient(error.to_string()))?;
        if saved.is_some() {
            self.invalidate_account_status();
            return Ok(true);
        }
        Ok(false)
    }

    async fn refresh_credential(
        &self,
        credential: &ProviderCredential,
    ) -> Result<Option<ProviderCredential>, CatalogError> {
        let ProviderCredential::QqMusic { cookies } = credential else {
            return Ok(None);
        };
        // A credential may retain both the WebView session and the mobile
        // OAuth tuple. Prefer the web contract, but do not let a stale web
        // session prevent a valid mobile refresh from running.
        let web_refresh_error = match self.refresh_web_credential(cookies).await {
            Ok(Some(refreshed)) => return Ok(Some(refreshed)),
            Ok(None) => None,
            Err(error) => Some(error),
        };
        let no_mobile_refresh = || web_refresh_error.clone().map_or(Ok(None), Err);
        let Some(uin) = first_cookie(cookies, &["uin", "wxuin"])
            .map(|value| value.trim_start_matches('o').to_owned())
            .filter(|value| !value.is_empty())
        else {
            return no_mobile_refresh();
        };
        let Some(music_key) = first_cookie(cookies, &["qqmusic_key", "qm_keyst", "lqm_keyst"])
        else {
            return no_mobile_refresh();
        };
        let Some(open_id) = first_cookie(cookies, &["psrf_qqopenid", "openid", "wxopenid"]) else {
            return no_mobile_refresh();
        };
        let access_token = first_cookie(
            cookies,
            &["psrf_qqaccess_token", "access_token", "wxaccess_token"],
        );
        // 网页登录（QQ OAuth）通常只下发 refresh_token；refresh_key 由移动端
        // 续期接口在响应中补发。两者任一存在即可发起续期，缺失者传空串。
        let refresh_token = first_cookie(
            cookies,
            &["psrf_qqrefresh_token", "refresh_token", "wxrefresh_token"],
        );
        let refresh_key = first_cookie(cookies, &["psrf_qqrefresh_key", "refresh_key"]);
        if refresh_token.is_none() && refresh_key.is_none() {
            return no_mobile_refresh();
        }
        let login_type = qq_refresh_login_type(music_key);
        if login_type == 2 && access_token.is_none() {
            return no_mobile_refresh();
        }
        let union_id =
            first_cookie(cookies, &["psrf_qqunionid", "unionid", "wxunionid"]).unwrap_or("");
        let music_id = match uin.parse::<u64>() {
            Ok(value) => value,
            Err(_) if login_type == 1 => 0,
            Err(_) => {
                return Err(CatalogError::AuthRequired(
                    "QQ Music account identifier requires relogin".to_owned(),
                ));
            }
        };
        let qimei = self.qimei().await;
        let payload = qq_refresh_payload(
            &uin,
            music_key,
            open_id,
            access_token.unwrap_or(""),
            refresh_token.unwrap_or(""),
            refresh_key.unwrap_or(""),
            union_id,
            music_id,
            login_type,
            &self.device,
            &qimei,
        );
        let response = self
            .client
            .post(self.search_url.clone())
            .header("Content-Type", "application/json")
            .header("User-Agent", "QQMusic")
            .json(&payload)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        let response_headers = response.headers().clone();
        let response = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        let Some(code) = qq_response_code(&response) else {
            return Err(CatalogError::InvalidResponse(
                "QQ Music refresh response has no business code".to_owned(),
            ));
        };
        if code != 0 {
            return Err(classify_qq_refresh_code(code));
        }
        let Some(data) = qq_response_payload_object(&response) else {
            return Err(CatalogError::InvalidResponse(
                "QQ Music refresh returned no credential data".to_owned(),
            ));
        };
        let Some(new_music_key) =
            qq_object_string(data, &["musickey", "qqmusic_key", "qm_keyst", "lqm_keyst"])
        else {
            return Err(CatalogError::InvalidResponse(
                "QQ Music refresh returned no music key".to_owned(),
            ));
        };
        let mut refreshed = cookies.clone();
        // Some mobile responses rotate refresh material through Set-Cookie
        // instead of (or in addition to) the JSON data object.  Merge those
        // fields before applying the JSON aliases so either wire form is
        // persisted for the next refresh.
        merge_qq_web_set_cookie_headers(&mut refreshed, &response_headers);
        replace_cookie_alias(
            &mut refreshed,
            &["qqmusic_key", "qm_keyst", "lqm_keyst"],
            &new_music_key,
        );
        replace_cookie_alias_from_values(
            &mut refreshed,
            &["uin", "wxuin"],
            data,
            &["musicid", "str_musicid"],
        );
        replace_cookie_alias_from_value(
            &mut refreshed,
            &["psrf_qqopenid", "openid"],
            data,
            "openid",
        );
        replace_cookie_alias_from_value(
            &mut refreshed,
            &["psrf_qqaccess_token", "access_token"],
            data,
            "access_token",
        );
        replace_cookie_alias_from_value(
            &mut refreshed,
            &["psrf_qqrefresh_token", "refresh_token"],
            data,
            "refresh_token",
        );
        replace_cookie_alias_from_value(
            &mut refreshed,
            &["psrf_qqrefresh_key", "refresh_key"],
            data,
            "refresh_key",
        );
        replace_cookie_alias_from_value(
            &mut refreshed,
            &["psrf_qqunionid", "unionid"],
            data,
            "unionid",
        );
        Ok(Some(ProviderCredential::QqMusic { cookies: refreshed }))
    }

    /// QQ Music's current web login stores a WeChat refresh session rather
    /// than the mobile `refresh_key` tuple. The web frontend renews it through
    /// this endpoint before requesting provider data; mirror that contract so
    /// a WebView login remains refreshable without exposing a second login UI.
    async fn refresh_web_credential(
        &self,
        cookies: &BTreeMap<String, String>,
    ) -> Result<Option<ProviderCredential>, CatalogError> {
        let Some(open_id) = first_cookie(cookies, &["wxopenid"]) else {
            return Ok(None);
        };
        let Some(refresh_token) = first_cookie(cookies, &["wxrefresh_token"]) else {
            return Ok(None);
        };
        let Some(music_key) = first_cookie(cookies, &["qqmusic_key", "qm_keyst", "lqm_keyst"])
        else {
            return Ok(None);
        };
        let Some(music_uin) = first_cookie(cookies, &["wxuin", "uin"])
            .map(|value| value.trim_start_matches('o'))
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let response = self
            .client
            .get(WEB_REFRESH_URL)
            .query(&[
                ("from", "1"),
                ("force_access", "1"),
                ("wxopenid", open_id),
                ("wxrefresh_token", refresh_token),
                ("musickey", music_key),
                ("musicuin", music_uin),
                ("get_access_token", "1"),
                ("ct", "1001"),
                ("format", "json"),
                ("inCharset", "utf-8"),
                ("outCharset", "utf-8"),
            ])
            .header("Cookie", cookie_header(cookies))
            .header("Referer", "https://y.qq.com/")
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        let response_headers = response.headers().clone();
        let body = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        let Some(code) = qq_response_code(&body) else {
            return Err(CatalogError::InvalidResponse(
                "QQ Music web refresh response has no business code".to_owned(),
            ));
        };
        if code != 0 {
            return Err(classify_qq_refresh_code(code));
        }
        let Some(data) = qq_response_payload_object(&body) else {
            return Err(CatalogError::InvalidResponse(
                "QQ Music web refresh returned no credential data".to_owned(),
            ));
        };
        let response_access_token = qq_object_string(data, &["wxaccess_token", "access_token"]);
        let mut refreshed = cookies.clone();
        merge_qq_web_set_cookie_headers(&mut refreshed, &response_headers);
        let Some(access_token) = response_access_token
            .or_else(|| first_cookie(&refreshed, &["wxaccess_token"]).map(str::to_owned))
            .filter(|value| !value.trim().is_empty())
        else {
            return Err(CatalogError::InvalidResponse(
                "QQ Music web refresh returned no access token".to_owned(),
            ));
        };
        if let Some(new_music_key) =
            qq_object_string(data, &["musickey", "qqmusic_key", "qm_keyst", "lqm_keyst"])
        {
            replace_cookie_alias(
                &mut refreshed,
                &["qqmusic_key", "qm_keyst", "lqm_keyst"],
                &new_music_key,
            );
        }
        replace_cookie_alias_from_values(
            &mut refreshed,
            &["uin", "wxuin"],
            data,
            &["musicuin", "musicid", "str_musicid"],
        );
        replace_cookie_alias_from_values(
            &mut refreshed,
            &["wxaccess_token", "psrf_qqaccess_token", "access_token"],
            data,
            &["wxaccess_token", "access_token"],
        );
        replace_cookie_alias_from_values(
            &mut refreshed,
            &["wxrefresh_token"],
            data,
            &["wxrefresh_token", "refresh_token"],
        );
        replace_cookie_alias_from_values(
            &mut refreshed,
            &["wxopenid"],
            data,
            &["wxopenid", "openid"],
        );
        replace_cookie_alias_from_values(
            &mut refreshed,
            &["wxunionid"],
            data,
            &["wxunionid", "unionid"],
        );
        if refreshed
            .get("wxaccess_token")
            .or_else(|| refreshed.get("psrf_qqaccess_token"))
            .or_else(|| refreshed.get("access_token"))
            .is_none_or(|value| value != &access_token)
        {
            return Err(CatalogError::InvalidResponse(
                "QQ Music web refresh returned inconsistent access token aliases".to_owned(),
            ));
        }
        Ok(Some(ProviderCredential::QqMusic { cookies: refreshed }))
    }

    async fn qimei(&self) -> QqQimei {
        let now = epoch_secs();
        if let Some(qimei) = self.fresh_qimei(now) {
            return qimei;
        }

        // Serialize only cache misses.  Re-check after taking the gate because
        // another caller may have completed the network request while this
        // task was waiting.
        let _gate = self.qimei_gate.lock().await;
        let now = epoch_secs();
        if let Some(qimei) = self.fresh_qimei(now) {
            return qimei;
        }

        let Ok(payload) = qimei_payload(&self.device) else {
            return QqQimei {
                q16: String::new(),
                q36: QQ_QIMEI_FALLBACK.to_owned(),
            };
        };
        let result = match self
            .client
            .post(QIMEI_URL)
            .header("method", "GetQimei")
            .header("service", "trpc.tme_datasvr.qimeiproxy.QimeiProxy")
            .header("appid", "qimei_qq_android")
            .header("User-Agent", "QQMusic")
            .header("timestamp", payload.timestamp.to_string())
            .header(
                "sign",
                md5_hex(&format!(
                    "qimei_qq_androidpzAuCmaFAaFaHrdakPjLIEqKrGnSOOvH{}",
                    payload.timestamp
                )),
            )
            .json(&payload.body)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response
                .json::<Value>()
                .await
                .ok()
                .and_then(|body| parse_qimei_response(&body)),
            _ => None,
        };
        if let Some(result) = result {
            let saved_at = epoch_secs();
            if let Ok(mut cache) = self.qimei_cache.lock() {
                *cache = Some(QqQimeiCache {
                    value: result.clone(),
                    saved_at,
                });
            }
            let mut device = self.device.clone();
            device.qimei = Some(result.clone());
            device.qimei_saved_at = Some(saved_at);
            let _ = device.save();
            return result;
        }
        QqQimei {
            q16: String::new(),
            q36: QQ_QIMEI_FALLBACK.to_owned(),
        }
    }

    fn fresh_qimei(&self, now: u64) -> Option<QqQimei> {
        if let Ok(cache) = self.qimei_cache.lock()
            && let Some(cache) = cache.as_ref()
            && qimei_is_fresh(&cache.value, cache.saved_at, now)
        {
            return Some(cache.value.clone());
        }
        // A device file written by an older build may contain QIMEI values
        // without a timestamp. Treat those values as expired; a fresh request
        // prevents a permanently reused identifier after the 24-hour contract.
        if let (Some(qimei), Some(saved_at)) =
            (self.device.qimei.clone(), self.device.qimei_saved_at)
            && qimei_is_fresh(&qimei, saved_at, now)
        {
            if let Ok(mut cache) = self.qimei_cache.lock() {
                *cache = Some(QqQimeiCache {
                    value: qimei.clone(),
                    saved_at,
                });
            }
            return Some(qimei);
        }
        None
    }

    fn search_payload(keyword: &str, limit: usize) -> Value {
        json!({
            "comm": {
                "ct": 24,
                "cv": 4747474,
                "format": "json",
                "inCharset": "utf-8",
                "outCharset": "utf-8",
                "notice": 0,
                "platform": "yqq.json",
                "needNewCode": 1
            },
            "search": {
                "module": "music.search.SearchCgiService",
                "method": "DoSearchForQQMusicDesktop",
                "param": {
                    "query": keyword,
                    "page_num": 1,
                    "num_per_page": limit.min(10),
                    "search_type": 0
                }
            }
        })
    }

    fn resolve_payload(song_mid: &str, media_mid: &str, uin: &str) -> Value {
        json!({
            "req_0": {
                "module": "vkey.GetVkeyServer",
                "method": "CgiGetVkey",
                "param": {
                    "guid": "1000000000",
                    "songmid": [song_mid],
                    "songtype": [0],
                    // M500 is QQ Music's standard-quality MP3. An explicit
                    // filename is required for the vkey response to have purl.
                    "filename": [qq_filename(media_mid, "M500", "mp3")],
                    "uin": uin,
                    "loginflag": 1,
                    "platform": "20"
                }
            },
            "comm": {
                "uin": uin,
                "format": "json",
                "ct": 24,
                "cv": 4747474
            }
        })
    }

    async fn account_status_cached(
        &self,
        force: bool,
    ) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        let (refresh_gate, cached) = {
            let guard = self.account_cache.lock().map_err(|_| {
                CatalogError::Transient("QQ Music account cache poisoned".to_owned())
            })?;
            let cached = (!force)
                .then(|| guard.cached(ACCOUNT_CACHE_TTL, ACCOUNT_CACHE_FAILED_TTL))
                .flatten();
            (guard.refresh_gate(), cached)
        };
        if let Some(status) = cached {
            return Ok(Some(status));
        }

        // A normal cache miss joins the in-flight provider query, then reads the
        // freshly written cache. A caller that explicitly requested refresh still
        // performs a query unless it had to wait for another refresh already in flight.
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
                CatalogError::Transient("QQ Music account cache poisoned".to_owned())
            })?;
            let cached = (!force || waited_for_refresh)
                .then(|| guard.cached(ACCOUNT_CACHE_TTL, ACCOUNT_CACHE_FAILED_TTL))
                .flatten();
            (guard.generation(), cached)
        };
        if let Some(status) = cached {
            return Ok(Some(status));
        }

        let now = std::time::Instant::now();
        let checked_at_ms = epoch_ms();
        let result = self.query_account_status().await;
        let mut guard = self
            .account_cache
            .lock()
            .map_err(|_| CatalogError::Transient("QQ Music account cache poisoned".to_owned()))?;
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

    async fn query_account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        let checked_at_ms = epoch_ms();
        let Ok(credential) = self.credential() else {
            return Ok(Some(ProviderAccountStatus {
                provider: PROVIDER.to_owned(),
                logged_in: false,
                checked_at_ms,
                ..ProviderAccountStatus::default()
            }));
        };
        let cookies = credential.cookies();
        let configured = ["qqmusic_key", "qm_keyst", "lqm_keyst", "uin", "wxuin"]
            .iter()
            .any(|name| cookies.get(*name).is_some_and(|value| !value.is_empty()));
        let user_id = first_cookie(cookies, &["uin", "wxuin"])
            .map(str::to_owned)
            .filter(|value| value != "0" && !value.is_empty());
        // login_type：1=QQ 登录，2=微信授权登录（y.qq.com 微信扫码入口）。
        let login_method = match cookies.get("login_type").map(String::as_str) {
            Some("1") => Some("qq".to_owned()),
            Some("2") => Some("wechat".to_owned()),
            _ if qq_refresh_login_type(
                first_cookie(cookies, &["qqmusic_key", "qm_keyst", "lqm_keyst"])
                    .unwrap_or_default(),
            ) == 1 =>
            {
                Some("wechat".to_owned())
            }
            _ => first_cookie(cookies, &["uin", "wxuin"])
                .filter(|value| *value != "0")
                .map(|_| "qq".to_owned()),
        };
        if !configured {
            return Ok(Some(ProviderAccountStatus {
                provider: PROVIDER.to_owned(),
                logged_in: false,
                user_id,
                login_method,
                checked_at_ms,
                ..ProviderAccountStatus::default()
            }));
        }
        let uin = first_cookie(cookies, &["uin", "wxuin"]).unwrap_or("0");
        let body = serde_json::json!({
            "comm": {"ct": 24, "cv": 0, "uin": uin},
            "req_0": {
                "module": "VipLogin.VipLoginInter",
                "method": "vip_login_base",
                "param": {}
            }
        });
        let response = self
            .client
            .post(self.search_url.clone())
            .header("Cookie", cookie_header(cookies))
            .header("Referer", "https://y.qq.com/")
            .json(&body)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        let value = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        if let Some(error) = qq_response_business_error(&value) {
            return Err(match error {
                CatalogError::AuthRequired(_) => CatalogError::CredentialRejected(
                    "QQ Music vip_login_base rejected credentials".to_owned(),
                ),
                other => other,
            });
        }
        if qq_validation_is_rejected(&value) {
            return Err(CatalogError::InvalidResponse(
                "QQ Music vip_login_base returned an unrecognized error code".to_owned(),
            ));
        }
        let Some(data) = qq_response_data(&value).filter(|value| value.is_object()) else {
            // 登录态失效或接口拒绝：视为查询失败，走失败缓存快速重试。
            return Err(CatalogError::InvalidResponse(
                "QQ Music vip_login_base returned no data".to_owned(),
            ));
        };
        let identity = data.get("identity").unwrap_or(data);
        let vip = qq_account_is_vip(identity, data);
        let vip_type = vip_vip_type_label(identity, data);
        let vip_expire_at_ms = vip
            .then(|| qq_vip_expire_at_ms(identity, checked_at_ms))
            .flatten();
        Ok(Some(ProviderAccountStatus {
            provider: PROVIDER.to_owned(),
            logged_in: true,
            user_id,
            nickname: None,
            vip_known: true,
            vip,
            vip_type: vip.then_some(vip_type).or_else(|| Some("非VIP".to_owned())),
            vip_expire_at_ms,
            login_method,
            checked_at_ms,
            stale: false,
            last_error: None,
        }))
    }
}

#[async_trait]
impl CredentialRefreshAdapter for QqMusicAdapter {
    async fn refresh_credential(
        &self,
        credential: &ProviderCredential,
    ) -> Result<Option<ProviderCredential>, CatalogError> {
        QqMusicAdapter::refresh_credential(self, credential).await
    }
}

#[async_trait]
impl SourceAdapter for QqMusicAdapter {
    async fn validate_credential(
        &self,
        candidate: &ProviderCredential,
    ) -> Result<(), CatalogError> {
        if candidate.provider() != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "QQ Music candidate provider does not match adapter".to_owned(),
            ));
        }
        let response = self
            .request_json_with_credential(
                candidate,
                &self.search_url,
                Self::search_payload("validation", 1),
            )
            .await?;
        if !response.is_object() {
            return Err(CatalogError::InvalidResponse(
                "QQ Music credential validation response is not an object".to_owned(),
            ));
        }
        if let Some(error) = qq_response_business_error(&response) {
            return Err(match error {
                CatalogError::AuthRequired(_) => CatalogError::CredentialRejected(
                    "QQ Music credential validation was rejected".to_owned(),
                ),
                other => other,
            });
        }
        if qq_validation_is_rejected(&response) {
            return Err(CatalogError::InvalidResponse(
                "QQ Music credential validation returned an unrecognized error code".to_owned(),
            ));
        }
        Ok(())
    }

    async fn search(
        &self,
        spec: &SearchSpec,
    ) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
        let response = self
            .request_json(
                &self.search_url,
                Self::search_payload(spec.keyword.trim(), spec.limit),
            )
            .await?;
        parse_search_candidates(&response)
    }

    async fn resolve(
        &self,
        key: &SongKey,
        locator: Option<&ResolverLocator>,
    ) -> Result<StreamSource, CatalogError> {
        if key.source != PROVIDER {
            return Err(CatalogError::InvalidResponse(
                "song key provider does not match QQ Music adapter".to_owned(),
            ));
        }
        let (song_mid, media_mid) = qq_resolver_ids(key, locator)?;
        let credential = self.credential()?;
        let cookies = credential.cookies();
        let uin = first_cookie(cookies, &["uin", "wxuin"])
            .map(|value| value.trim_start_matches('o'))
            .unwrap_or("0");
        let response = self
            .request_json(
                &self.resolve_url,
                Self::resolve_payload(&song_mid, &media_mid, uin),
            )
            .await?;
        let url = match qq_stream_url(&response) {
            Ok(url) => url,
            Err(error) => return Err(classify_qq_resolve_failure(&response, error)),
        };
        Ok(StreamSource {
            url,
            // The vkey is embedded in purl. Account cookies must not be sent
            // to an arbitrary CDN host returned by the provider.
            headers: BTreeMap::from([("Referer".to_owned(), "https://y.qq.com/".to_owned())]),
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
                "song key provider does not match QQ Music adapter".to_owned(),
            ));
        }
        let (song_mid, _) = qq_resolver_ids(key, locator)?;
        let response = self.lyrics_json_with_translation(&song_mid).await?;
        let code = qq_response_code(&response).unwrap_or_default();
        if code != 0 {
            if let Some(error) = qq_response_business_error(&response) {
                return Err(if matches!(error, CatalogError::AuthRequired(_)) {
                    CatalogError::AuthRequired("QQ Music lyric request requires relogin".to_owned())
                } else {
                    error
                });
            }
            return Err(CatalogError::Unavailable(
                "QQ Music returned no lyrics for this track".to_owned(),
            ));
        }
        let primary = lyric_value(&response, "lyric")
            .map(decode_qq_lyric)
            .unwrap_or_default();
        let translation = lyric_value(&response, "trans")
            .map(decode_qq_lyric)
            .and_then(|value| strip_untranslated_placeholder_lines(&value));
        parse_lrc_pair(&primary, translation.as_deref())
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    /// QQ 音乐账号状态：登录态 + VIP（绿钻/SVIP）查询。
    /// 使用 musicu.fcg `VipLogin.VipLoginInter/vip_login_base`（无签名即可用）。
    async fn account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        self.account_status_cached(false).await
    }

    async fn refresh_account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
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
    let songs = response
        .pointer("/search/data/body/song/list")
        .or_else(|| response.pointer("/search/data/body/songlist/list"))
        .or_else(|| qq_response_data(response).and_then(|data| data.pointer("/body/song/list")))
        .or_else(|| qq_response_data(response).and_then(|data| data.pointer("/body/songlist/list")))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CatalogError::InvalidResponse("QQ Music search list is missing".to_owned())
        })?;
    let mut candidates = Vec::new();
    for raw in songs.iter().take(10) {
        let Some(mid) = raw
            .get("mid")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                raw.get("id")
                    .and_then(Value::as_i64)
                    .map(|value| value.to_string())
            })
        else {
            continue;
        };
        let title = raw
            .get("title")
            .or_else(|| raw.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if title.is_empty() {
            continue;
        }
        let artists = raw
            .get("singer")
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
            .get("interval")
            .and_then(Value::as_u64)
            .map(|seconds| seconds.saturating_mul(1000));
        let media_mid = raw
            .pointer("/file/media_mid")
            .or_else(|| raw.get("media_mid"))
            .and_then(Value::as_str)
            .filter(|value| is_qq_mid(value))
            .unwrap_or(&mid);
        let song = Song {
            key: SongKey::new("qqmusic", mid.clone())
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            resolver_locator: ResolverLocator::new(format!("qqmusic:v2:{mid}:{media_mid}")).ok(),
            title,
            artists,
            album,
            duration_ms,
        };
        let eligibility = qq_eligibility(raw);
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

fn qq_filename(media_mid: &str, prefix: &str, extension: &str) -> String {
    format!("{prefix}{media_mid}.{extension}")
}

/// QQ 播放 URL 获取失败时的分类：先排除登录失效（响应带 auth 错误码），
/// 再查 midurlinfo 的 vip=1 表示需要 VIP；否则视为版权/音源受限。
fn classify_qq_resolve_failure(response: &Value, error: CatalogError) -> CatalogError {
    if qq_response_requires_auth_refresh(response) {
        return CatalogError::AuthRequired(
            "QQ Music credential expired; login is required".to_owned(),
        );
    }
    let vip = qq_media_info(response)
        .and_then(|item| item.get("vip"))
        .and_then(as_i64);
    if vip == Some(1) {
        return CatalogError::VipRequired("QQ Music track requires VIP membership".to_owned());
    }
    match error {
        CatalogError::Unavailable(message) => {
            CatalogError::NoCopyright(format!("QQ Music track has no copyright: {message}"))
        }
        other => other,
    }
}

pub(crate) fn qq_eligibility(raw: &Value) -> PlaybackEligibility {
    let switch = raw.pointer("/action/switch").and_then(as_i64);
    let play_bits_off = switch.is_some_and(|value| value & PLAY_BITS_MASK == 0);
    if play_bits_off && switch.is_some_and(|value| value & TRY_BIT_MASK == 0) {
        return PlaybackEligibility::NoCopyright;
    }
    if raw.pointer("/pay/pay_play").and_then(as_i64) == Some(1) {
        return PlaybackEligibility::VipRequired;
    }
    if play_bits_off {
        return PlaybackEligibility::NoCopyright;
    }
    if let Some(file) = raw.get("file").and_then(Value::as_object) {
        let sizes = file
            .iter()
            .filter(|(name, _)| name.starts_with("size_") && *name != "size_try")
            .map(|(_, value)| as_i64(value));
        let values = sizes.flatten().collect::<Vec<_>>();
        if !values.is_empty() && values.iter().all(|value| *value <= 0) {
            return PlaybackEligibility::Ineligible;
        }
    }
    if switch.is_some() {
        PlaybackEligibility::Eligible
    } else {
        PlaybackEligibility::Unknown
    }
}

fn qq_envelope_key(key: &str) -> bool {
    if matches!(key, "req" | "search") {
        return true;
    }
    let suffix = key.strip_prefix("req_").or_else(|| key.strip_prefix("req"));
    suffix.is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn qq_response_code(response: &Value) -> Option<i64> {
    qq_selected_response_envelope(response).and_then(|envelope| envelope.code)
}

fn qq_response_data(response: &Value) -> Option<&Value> {
    qq_selected_response_envelope(response).map(|envelope| envelope.data)
}

#[derive(Clone, Copy)]
struct QqResponseEnvelope<'a> {
    data: &'a Value,
    code: Option<i64>,
}

/// Choose one QQ response envelope once, then use that same envelope for data,
/// authentication and business-error decisions. Explicit code-0 envelopes win;
/// if none succeeded, the first explicit failure remains authoritative.
fn qq_selected_response_envelope(response: &Value) -> Option<QqResponseEnvelope<'_>> {
    let object = response.as_object()?;
    let mut candidates = Vec::new();

    if let Some(data) = object.get("data") {
        candidates.push(QqResponseEnvelope {
            data,
            code: qq_direct_code(object).or_else(|| qq_data_envelope_code(data)),
        });
    }
    for (key, envelope) in object {
        if !qq_envelope_key(key) {
            continue;
        }
        let Some(envelope_object) = envelope.as_object() else {
            continue;
        };
        candidates.push(QqResponseEnvelope {
            data: envelope_object.get("data").unwrap_or(envelope),
            // Only an envelope's own fields are business codes. Its data may
            // contain unrelated song metadata with a `code` field.
            code: qq_direct_code(envelope_object),
        });
    }

    // A root code without root data is only a fallback legacy envelope. It
    // must not turn `{code:0, req:{code:1000}}` into a successful response.
    if candidates.is_empty() && qq_direct_code(object).is_some() {
        candidates.push(QqResponseEnvelope {
            data: response,
            code: qq_direct_code(object),
        });
    }

    let successful = candidates
        .iter()
        .copied()
        .filter(|envelope| envelope.code == Some(0))
        .collect::<Vec<_>>();
    successful
        .iter()
        .rev()
        .copied()
        .find(|envelope| qq_data_is_meaningful(envelope.data))
        .or_else(|| successful.first().copied())
        .or_else(|| {
            candidates
                .iter()
                .copied()
                .find(|envelope| envelope.code.is_some_and(|code| code != 0))
        })
        .or_else(|| {
            candidates
                .iter()
                .copied()
                .find(|envelope| qq_data_is_meaningful(envelope.data))
        })
        .or_else(|| candidates.first().copied())
}

// Inspect only one envelope level so song metadata cannot become a provider
// business code. If direct fields contradict each other, a nonzero code wins.
fn qq_direct_code(object: &serde_json::Map<String, Value>) -> Option<i64> {
    let mut first = None;
    for field in ["code", "ret", "retcode"] {
        let Some(code) = object.get(field).and_then(as_i64) else {
            continue;
        };
        first.get_or_insert(code);
        if code != 0 {
            return Some(code);
        }
    }
    first
}

fn qq_data_envelope_code(value: &Value) -> Option<i64> {
    value.as_object().and_then(qq_direct_code)
}

fn qq_data_is_meaningful(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Array(items) => !items.is_empty(),
        Value::Object(object) => !object.is_empty(),
        Value::String(text) => !text.trim().is_empty(),
        Value::Bool(_) | Value::Number(_) => true,
    }
}

fn qq_response_payload_object(response: &Value) -> Option<&serde_json::Map<String, Value>> {
    qq_response_data(response)
        .and_then(Value::as_object)
        .or_else(|| response.as_object())
}

fn qq_is_auth_response_code(code: i64) -> bool {
    matches!(
        code,
        401 | 403 | 1000 | 1001 | 1002 | 1003 | 1004 | 1005 | 104400 | 104401
    )
}

fn qq_response_requires_auth_refresh(response: &Value) -> bool {
    qq_response_code(response).is_some_and(qq_is_auth_response_code)
}

fn classify_qq_refresh_code(code: i64) -> CatalogError {
    match code {
        401 | 403 | 1000 | 1001 | 1002 | 1003 | 1004 | 1005 | 104400 | 104401 => {
            CatalogError::AuthRequired(format!(
                "QQ Music refresh credentials expired or were rejected (code {code}); relogin is required"
            ))
        }
        2001 | 104604 => {
            CatalogError::RateLimited(format!("QQ Music refresh was rate limited (code {code})"))
        }
        20277 | 20278 | 20279 | 20450 => CatalogError::CredentialRejected(format!(
            "QQ Music account or device was rejected (code {code}); relogin may be required"
        )),
        20261 | 20271 | 20272 | 20274 => CatalogError::InvalidResponse(format!(
            "QQ Music refresh parameters were rejected (code {code})"
        )),
        2000 => CatalogError::InvalidResponse(
            "QQ Music refresh was sent to a signature-required endpoint (code 2000)".to_owned(),
        ),
        _ => CatalogError::InvalidResponse(format!(
            "QQ Music refresh returned business error code {code}"
        )),
    }
}

fn qq_response_business_error(response: &Value) -> Option<CatalogError> {
    qq_response_code(response)
        .filter(|code| *code != 0)
        .map(classify_qq_refresh_code)
}

fn lyric_value<'a>(response: &'a Value, field: &str) -> Option<&'a str> {
    qq_response_data(response)
        .and_then(|data| data.get(field))
        .or_else(|| response.get(field))
        .and_then(Value::as_str)
}

fn qq_response_has_lyrics(response: &Value) -> bool {
    ["lyric", "trans"]
        .into_iter()
        .any(|field| lyric_value(response, field).is_some_and(|value| !value.trim().is_empty()))
}

fn strip_untranslated_placeholder_lines(value: &str) -> Option<String> {
    let cleaned = value
        .lines()
        .filter(|line| {
            line.rsplit_once(']')
                .is_none_or(|(_, text)| text.trim() != "//")
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!cleaned.trim().is_empty()).then_some(cleaned)
}

fn parse_qq_json(body: &str) -> Result<Value, CatalogError> {
    let body = body.trim();
    let body = body
        .find('(')
        .filter(|_| body.ends_with(')'))
        .map_or(body, |index| &body[index + 1..body.len() - 1]);
    serde_json::from_str(body).map_err(|error| CatalogError::InvalidResponse(error.to_string()))
}

fn decode_qq_lyric(value: &str) -> String {
    let value = value.trim();
    if value.contains('[') {
        return value.to_owned();
    }
    for engine in [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
    ] {
        if let Ok(decoded) = engine.decode(value)
            && let Ok(decoded) = String::from_utf8(decoded)
            && decoded.contains('[')
        {
            return decoded;
        }
    }
    value.to_owned()
}

fn first_cookie<'a>(cookies: &'a BTreeMap<String, String>, aliases: &[&str]) -> Option<&'a str> {
    aliases.iter().find_map(|alias| {
        cookies
            .get(*alias)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn replace_cookie_alias(cookies: &mut BTreeMap<String, String>, aliases: &[&str], value: &str) {
    if aliases.is_empty() {
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
        for key in existing {
            cookies.insert(key.to_owned(), value.to_owned());
        }
    }
}

fn replace_cookie_alias_from_value(
    cookies: &mut BTreeMap<String, String>,
    aliases: &[&str],
    data: &serde_json::Map<String, Value>,
    field: &str,
) {
    replace_cookie_alias_from_values(cookies, aliases, data, &[field]);
}

fn replace_cookie_alias_from_values(
    cookies: &mut BTreeMap<String, String>,
    aliases: &[&str],
    data: &serde_json::Map<String, Value>,
    fields: &[&str],
) {
    let Some(value) = fields.iter().find_map(|field| {
        data.get(*field).and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_i64().map(|value| value.to_string()))
                .or_else(|| value.as_u64().map(|value| value.to_string()))
                .filter(|value| !value.trim().is_empty())
        })
    }) else {
        return;
    };
    replace_cookie_alias(cookies, aliases, value.trim());
}

fn qq_object_string(data: &serde_json::Map<String, Value>, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        data.get(*field).and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_i64().map(|value| value.to_string()))
                .or_else(|| value.as_u64().map(|value| value.to_string()))
                .filter(|value| !value.trim().is_empty())
        })
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QqQimei {
    q16: String,
    q36: String,
}

#[derive(Clone, Debug)]
struct QqQimeiCache {
    value: QqQimei,
    saved_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QqOsVersion {
    incremental: String,
    release: String,
    codename: String,
    sdk: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QqDevice {
    display: String,
    product: String,
    device: String,
    board: String,
    model: String,
    fingerprint: String,
    boot_id: String,
    proc_version: String,
    imei: String,
    brand: String,
    bootloader: String,
    base_band: String,
    version: QqOsVersion,
    sim_info: String,
    os_type: String,
    mac_address: String,
    wifi_bssid: String,
    wifi_ssid: String,
    imsi_md5: Vec<u8>,
    android_id: String,
    #[serde(skip)]
    path: Option<PathBuf>,
    apn: String,
    vendor_name: String,
    vendor_os_name: String,
    #[serde(default)]
    qimei: Option<QqQimei>,
    #[serde(default)]
    qimei_saved_at: Option<u64>,
}

impl QqDevice {
    fn load(path: Option<PathBuf>) -> Result<Self, std::io::Error> {
        if let Some(path) = path {
            if path.exists() {
                let mut device: Self = serde_json::from_str(&fs::read_to_string(&path)?)
                    .map_err(std::io::Error::other)?;
                device.path = Some(path);
                return Ok(device);
            }
            let mut device = Self::new();
            device.path = Some(path.clone());
            device.save_at(&path)?;
            return Ok(device);
        }
        Ok(Self::new())
    }
    fn new() -> Self {
        let mut rng = rand::thread_rng();
        let serial: u32 = rng.gen_range(100000..1_000_000);
        let build: u32 = rng.gen_range(1_000_000..10_000_000);
        let android_id: u64 = rand::random();
        Self {
            display: format!("QMAPI.{serial}.001"),
            product: "iarim".into(),
            device: "sagit".into(),
            board: "eomam".into(),
            model: "MI 6".into(),
            fingerprint: format!(
                "xiaomi/iarim/sagit:10/eomam.200122.001/{build}:user/release-keys"
            ),
            boot_id: uuid::Uuid::new_v4().to_string(),
            proc_version: format!("Linux 5.4.0-54-generic-{serial:08x} (android-build@google.com)"),
            imei: random_imei(&mut rng),
            brand: "Xiaomi".into(),
            bootloader: "U-boot".into(),
            base_band: String::new(),
            version: QqOsVersion {
                incremental: "5891938".into(),
                release: "10".into(),
                codename: "REL".into(),
                sdk: 29,
            },
            sim_info: "T-Mobile".into(),
            os_type: "android".into(),
            mac_address: "00:50:56:C0:00:08".into(),
            wifi_bssid: "00:50:56:C0:00:08".into(),
            wifi_ssid: "<unknown ssid>".into(),
            imsi_md5: md5::compute(android_id.to_be_bytes()).0.to_vec(),
            android_id: format!("{android_id:016x}"),
            apn: "wifi".into(),
            vendor_name: "MIUI".into(),
            vendor_os_name: "qmapi".into(),
            qimei: None,
            qimei_saved_at: None,
            path: None,
        }
    }
    fn save(&self) -> Result<(), std::io::Error> {
        if let Some(path) = &self.path {
            self.save_at(path)?;
        }
        Ok(())
    }
    fn save_at(&self, path: &Path) -> Result<(), std::io::Error> {
        fs::write(
            path,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )
    }
}

fn random_imei(rng: &mut impl rand::Rng) -> String {
    let mut digits: Vec<u8> = (0..14).map(|_| rng.gen_range(0..10)).collect();
    let sum: u32 = digits
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let v = if (i + 2) % 2 == 0 {
                *n as u32 * 2
            } else {
                *n as u32
            };
            v / 10 + v % 10
        })
        .sum();
    digits.push(((sum * 9) % 10) as u8);
    digits.into_iter().map(|n| char::from(b'0' + n)).collect()
}

struct QimeiRequest {
    timestamp: u64,
    body: Value,
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn qimei_is_fresh(qimei: &QqQimei, saved_at: u64, now: u64) -> bool {
    !qimei.q16.trim().is_empty()
        && !qimei.q36.trim().is_empty()
        && now >= saved_at
        && now.saturating_sub(saved_at) < QIMEI_CACHE_TTL.as_secs()
}

fn random_hex(rng: &mut impl Rng, length: usize) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    (0..length)
        .map(|_| char::from(HEX[rng.gen_range(0..HEX.len())]))
        .collect()
}

/// QIMEI's beacon is intentionally synthetic, but keeps the same field
/// layout as the Android SDK so the server can parse the device fingerprint.
fn random_beacon_id(timestamp: u64, rng: &mut impl Rng) -> String {
    let (year, month, _) = civil_from_days((timestamp / 86_400) as i64);
    let month_start = format!("{year:04}-{month:02}-01");
    let rand1 = rng.gen_range(100_000..1_000_000);
    let rand2 = rng.gen_range(100_000_000..1_000_000_000u64);
    let mut beacon = String::new();
    for index in 1..=40 {
        if matches!(
            index,
            1 | 2 | 13 | 14 | 17 | 18 | 21 | 22 | 25 | 26 | 29 | 30 | 33 | 34 | 37 | 38
        ) {
            beacon.push_str(&format!("k{index}:{month_start}{rand1}.{rand2};"));
        } else if index == 3 {
            beacon.push_str("k3:0000000000000000;");
        } else if index == 4 {
            beacon.push_str(&format!("k4:{};", random_hex_nonzero(rng, 16)));
        } else {
            beacon.push_str(&format!("k{index}:{};", rng.gen_range(0..10_000u32)));
        }
    }
    beacon
}

fn random_hex_nonzero(rng: &mut impl Rng, length: usize) -> String {
    const HEX: &[u8] = b"123456789abcdef";
    (0..length)
        .map(|_| char::from(HEX[rng.gen_range(0..HEX.len())]))
        .collect()
}

// Inverse of the civil-date conversion used by the C++/Android SDK. The
// result is Gregorian year, month, day for a UTC day count since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    (year, month as u32, day as u32)
}

fn qimei_payload(device: &QqDevice) -> Result<QimeiRequest, CatalogError> {
    let mut rng = rand::thread_rng();
    let crypt_key = random_hex(&mut rng, 16);
    let nonce = random_hex(&mut rng, 16);
    let timestamp = epoch_secs();
    let public_key = RsaPublicKey::from_public_key_pem(QIMEI_PUBLIC_KEY).map_err(|error| {
        CatalogError::InvalidResponse(format!("QIMEI RSA 公钥解析失败: {error}"))
    })?;
    let encrypted_key = public_key
        .encrypt(
            &mut rand::thread_rng(),
            Pkcs1v15Encrypt,
            crypt_key.as_bytes(),
        )
        .map_err(|error| CatalogError::InvalidResponse(format!("QIMEI RSA 加密失败: {error}")))?;
    let key = base64::engine::general_purpose::STANDARD.encode(encrypted_key);
    let reserved = json!({
        "harmony": "0",
        "clone": "0",
        "containe": "",
        "oz": "UhYmelwouA+V2nPWbOvLTgN2/m8jwGB+yUB5v9tysQg=",
        "oo": "Xecjt+9S1+f8Pz2VLSxgpw==",
        "kelong": "0",
        "uptimes": format!("{}", epoch_secs().saturating_sub(rng.gen_range(0..14_401))),
        "multiUser": "0",
        "bod": device.brand,
        "dv": device.device,
        "firstLevel": "",
        "manufact": device.brand,
        "name": device.model,
        "host": "se.infra",
        "kernel": device.proc_version,
    });
    let reserved = serde_json::to_string(&reserved).map_err(|error| {
        CatalogError::InvalidResponse(format!("QIMEI reserved 编码失败: {error}"))
    })?;
    let plain = json!({
        "androidId": device.android_id,
        "platformId": 1,
        "appKey": QIMEI_APP_KEY,
        "appVersion": QIMEI_APP_VERSION,
        "beaconIdSrc": random_beacon_id(timestamp, &mut rng),
        "brand": device.brand,
        "channelId": "10003505",
        "cid": "",
        "imei": device.imei,
        "imsi": "",
        "mac": "",
        "model": device.model,
        "networkType": "unknown",
        "oaid": "",
        "osVersion": "Android 10,level 29",
        "qimei": "",
        "qimei36": "",
        "sdkVersion": QIMEI_SDK_VERSION,
        "targetSdkVersion": "33",
        "audit": "",
        "userId": "{}",
        "packageId": "com.tencent.qqmusic",
        "deviceType": "Phone",
        "sdkName": "",
        "reserved": reserved,
    })
    .to_string();
    let cipher = Encryptor::<Aes128>::new_from_slices(crypt_key.as_bytes(), crypt_key.as_bytes())
        .map_err(|error| {
        CatalogError::InvalidResponse(format!("QIMEI AES 初始化失败: {error}"))
    })?;
    let mut buffer = plain.into_bytes();
    let length = buffer.len();
    buffer.resize(length + 16, 0);
    let encrypted = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, length)
        .map_err(|error| CatalogError::InvalidResponse(format!("QIMEI AES 加密失败: {error}")))?;
    let params = base64::engine::general_purpose::STANDARD.encode(encrypted);
    let extra = format!("{{\"appKey\":\"{QIMEI_APP_KEY}\"}}");
    let sign = md5_hex(&format!(
        "{key}{params}{}{}{}{}",
        timestamp * 1000,
        nonce,
        QIMEI_SECRET,
        extra
    ));
    Ok(QimeiRequest {
        timestamp,
        body: json!({"app":0,"os":1,"qimeiParams":{"key":key,"params":params,"time":timestamp.to_string(),"nonce":nonce,"sign":sign,"extra":extra}}),
    })
}

fn parse_qimei_response(value: &Value) -> Option<QqQimei> {
    fn visit(value: &Value, depth: u8) -> Option<QqQimei> {
        if depth > 4 {
            return None;
        }
        if let Some(object) = value.as_object() {
            if let (Some(q16), Some(q36)) = (
                object.get("q16").and_then(value_string),
                object.get("q36").and_then(value_string),
            ) {
                let result = QqQimei { q16, q36 };
                if !result.q16.trim().is_empty() && !result.q36.trim().is_empty() {
                    return Some(result);
                }
            }
            for key in ["data", "result", "qimei"] {
                if let Some(child) = object.get(key).and_then(|child| visit(child, depth + 1)) {
                    return Some(child);
                }
            }
        } else if let Some(text) = value.as_str()
            && let Ok(parsed) = serde_json::from_str::<Value>(text)
        {
            return visit(&parsed, depth + 1);
        }
        None
    }
    visit(value, 0)
}

fn value_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_i64().map(|value| value.to_string()))
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn md5_hex(value: &str) -> String {
    format!("{:x}", md5::compute(value))
}

#[allow(clippy::too_many_arguments)]
fn qq_refresh_payload(
    uin: &str,
    music_key: &str,
    open_id: &str,
    access_token: &str,
    refresh_token: &str,
    refresh_key: &str,
    union_id: &str,
    music_id: u64,
    login_type: i64,
    device: &QqDevice,
    qimei: &QqQimei,
) -> Value {
    let param = if login_type == 1 {
        json!({
            "openid": open_id,
            "refresh_token": refresh_token,
            "str_musicid": uin,
            "musickey": music_key,
            "unionid": union_id,
            "refresh_key": refresh_key,
            "loginMode": 2
        })
    } else {
        json!({
            "openid": open_id,
            "access_token": access_token,
            "refresh_token": refresh_token,
            "expired_in": 0,
            "musicid": music_id,
            "musickey": music_key,
            "refresh_key": refresh_key,
            "loginMode": 2
        })
    };
    json!({
        "comm": {
            "v": QQ_MOBILE_VERSION,
            "ct": 11,
            "cv": QQ_MOBILE_VERSION,
            "chid": "2005000982",
            "QIMEI": qimei.q16,
            "QIMEI36": qimei.q36,
            "tmeAppID": "qqmusic",
            "format": "json",
            "inCharset": "utf-8",
            "outCharset": "utf-8",
            "qq": uin,
            "authst": music_key,
            "tmeLoginType": login_type,
            "OpenUDID": device.android_id,
            "udid": device.boot_id,
            "os_ver": device.version.release,
            "aid": device.android_id,
            "phonetype": device.model,
            "devicelevel": device.version.sdk,
            "newdevicelevel": device.version.sdk,
            "nettype": "1030",
            "rom": device.fingerprint,
            "OpenUDID2": device.android_id
        },
        "req": {
            "module": "music.login.LoginServer",
            "method": "Login",
            "param": param
        }
    })
}

/// QQ Music's refresh protocol type follows the credential key family, not
/// the WebView callback's `login_type` query parameter. `Q_H_L_` is the QQ
/// mobile contract (type 2); `W_X_` is the WeChat contract (type 1).
fn qq_refresh_login_type(music_key: &str) -> i64 {
    if music_key
        .trim()
        .as_bytes()
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"W_X"))
    {
        1
    } else {
        2
    }
}

fn as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn qq_vip_flag(value: &Value, name: &str) -> bool {
    value.get(name).and_then(as_i64).is_some_and(|v| v != 0)
}

fn qq_account_is_vip(identity: &Value, data: &Value) -> bool {
    qq_vip_flag(identity, "vip")
        || qq_vip_flag(data, "svip")
        || qq_vip_flag(identity, "HugeVip")
        || qq_vip_flag(data, "star")
        || qq_vip_flag(identity, "twelve")
}

/// The response keeps historical tier end dates, so only use dates belonging
/// to a currently active tier. A user can hold both tiers; the later end date
/// is when their ability to play VIP tracks changes.
fn qq_vip_expire_at_ms(identity: &Value, now_ms: u64) -> Option<u64> {
    [
        qq_vip_flag(identity, "HugeVip").then_some("HugeVipEnd"),
        qq_vip_flag(identity, "vip").then_some("LMEnd"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|field| identity.get(field).and_then(Value::as_str))
    .filter_map(parse_qq_date_ms)
    .filter(|expires_at_ms| *expires_at_ms > now_ms)
    .max()
}

/// QQ 音乐的搜索端点可能在未登录时仍返回合法 JSON；仅接受没有明确业务错误码
/// 的响应，避免把 HTTP 200 的拒绝包写入凭据存储。
fn qq_validation_is_rejected(response: &Value) -> bool {
    qq_response_code(response).is_some_and(|code| code != 0)
}

/// 生成 QQ 音乐 VIP 类型标签（绿钻/SVIP/豪华绿钻/年费等组合）。
fn vip_vip_type_label(identity: &Value, data: &Value) -> String {
    let mut labels = Vec::new();
    if qq_vip_flag(identity, "HugeVip") {
        labels.push("豪华绿钻");
    } else if qq_vip_flag(identity, "vip") {
        labels.push("绿钻");
    }
    if qq_vip_flag(data, "svip") {
        labels.push("SVIP");
    }
    if qq_vip_flag(data, "star") {
        labels.push("星级");
    }
    if qq_vip_flag(identity, "twelve") {
        labels.push("十二平台");
    }
    if qq_vip_flag(identity, "yearflag") || qq_vip_flag(identity, "HugeYearFlag") {
        labels.push("年费");
    }
    if labels.is_empty() {
        labels.push("VIP");
    }
    labels.join("·")
}

/// QQ VIP 响应中的无时区日期使用北京时间（UTC+8）。
const QQ_VIP_TIMEZONE_OFFSET_SECONDS: i64 = 8 * 60 * 60;

/// 解析 QQ VIP 到期时间（"2026-07-20 07:50:53"）为 epoch 毫秒。
fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn parse_qq_date_ms(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || value == "-1" {
        return None;
    }
    let date = if let Some(rest) = value.strip_suffix(" 00:00:00") {
        rest
    } else {
        value
    };
    let mut parts = date
        .split(['-', ' ', ':'])
        .filter_map(|part| part.parse::<u64>().ok());
    let year = parts.next()?;
    let month = parts.next()?;
    let day = parts.next()?;
    let hour = parts.next().unwrap_or(0);
    let minute = parts.next().unwrap_or(0);
    let second = parts.next().unwrap_or(0);
    let days = days_from_civil(year as i64, month as i64, day as i64)?;
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour as i64 * 3_600 + minute as i64 * 60 + second as i64)?
        .checked_sub(QQ_VIP_TIMEZONE_OFFSET_SECONDS)?;
    u64::try_from(seconds).ok()?.checked_mul(1_000)
}

/// 公历转儒略日（days since 1970-01-01）。
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !matches!(month, 1..=12) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_adjusted = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_adjusted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

fn qq_resolver_ids(
    key: &SongKey,
    locator: Option<&ResolverLocator>,
) -> Result<(String, String), CatalogError> {
    let Some(locator) = locator else {
        return Ok((key.id.clone(), key.id.clone()));
    };
    let value = locator.as_str();
    let (song_mid, media_mid) = if let Some(value) = value.strip_prefix("qqmusic:v2:") {
        value.split_once(':').ok_or_else(|| {
            CatalogError::InvalidResolverLocator("QQ Music locator is malformed".to_owned())
        })?
    } else {
        return Err(CatalogError::InvalidResolverLocator(
            "QQ Music locator is malformed".to_owned(),
        ));
    };
    if !is_qq_mid(song_mid) || !is_qq_mid(media_mid) {
        return Err(CatalogError::InvalidResolverLocator(
            "QQ Music locator contains an invalid media id".to_owned(),
        ));
    }
    if song_mid != key.id {
        return Err(CatalogError::ResolverLocatorTrackMismatch {
            requested_id: key.id.clone(),
            locator_id: song_mid.to_owned(),
        });
    }
    Ok((song_mid.to_owned(), media_mid.to_owned()))
}

fn is_qq_mid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn validate_media_url(url: &Url) -> Result<(), CatalogError> {
    if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() {
        return Ok(());
    }
    Err(CatalogError::InvalidResponse(
        "QQ Music returned a non-HTTP media URL".to_owned(),
    ))
}

fn qq_stream_url(response: &Value) -> Result<Url, CatalogError> {
    let info = qq_media_info(response).ok_or_else(|| {
        CatalogError::InvalidResponse("QQ Music response has no media info".to_owned())
    })?;
    let purl = info
        .get("purl")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| CatalogError::Unavailable("QQ Music returned no playable URL".to_owned()))?;
    let url = if purl.starts_with("http://") || purl.starts_with("https://") {
        Url::parse(purl)
    } else {
        let host = qq_response_data(response)
            .and_then(|data| data.get("sip").or_else(|| data.get("siplist")))
            .or_else(|| response.get("sip").or_else(|| response.get("siplist")))
            .and_then(first_qq_string)
            .unwrap_or("https://isure.stream.qqmusic.qq.com/");
        Url::parse(host).and_then(|host| host.join(purl))
    }
    .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
    validate_media_url(&url)?;
    Ok(url)
}

/// Return the first vkey media-info object regardless of which request
/// envelope (`req`, `req_N`, `search`, or root `data`) QQ used.
fn qq_media_info(response: &Value) -> Option<&serde_json::Map<String, Value>> {
    let data = qq_response_data(response).unwrap_or(response).as_object()?;
    let info = data
        .get("midurlinfo")
        .or_else(|| data.get("midUrlInfo"))
        .or_else(|| data.get("mid_url_info"))?;
    if let Some(items) = info.as_array() {
        // The provider may put an empty/denied quality first and a playable
        // quality later in the same response. Prefer the first non-empty purl,
        // while retaining the first object as a fallback for error metadata.
        return items
            .iter()
            .filter_map(Value::as_object)
            .find(|item| {
                item.get("purl")
                    .and_then(Value::as_str)
                    .is_some_and(|url| !url.trim().is_empty())
            })
            .or_else(|| items.iter().find_map(Value::as_object));
    }
    info.as_object()
}

fn first_qq_string(value: &Value) -> Option<&str> {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .find(|value| !value.trim().is_empty()),
        Value::String(value) => (!value.trim().is_empty()).then_some(value.as_str()),
        _ => None,
    }
}

fn cookie_header(cookies: &BTreeMap<String, String>) -> String {
    cookies
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn merge_qq_web_set_cookie_headers(
    cookies: &mut BTreeMap<String, String>,
    headers: &reqwest::header::HeaderMap,
) {
    const NAMES: &[&str] = &[
        "uin",
        "wxuin",
        "qqmusic_key",
        "qm_keyst",
        "lqm_keyst",
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
    ];
    let mut updates = BTreeMap::new();
    for header in headers.get_all(reqwest::header::SET_COOKIE).iter() {
        let Ok(header) = header.to_str() else {
            continue;
        };
        let Some((name, value)) = header.split(';').next().unwrap_or_default().split_once('=')
        else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if NAMES.contains(&name) && !value.is_empty() {
            updates.insert(name.to_owned(), value.to_owned());
        }
    }
    merge_qq_web_cookie_updates(cookies, &updates);
}

fn merge_qq_web_cookie_updates(
    cookies: &mut BTreeMap<String, String>,
    updates: &BTreeMap<String, String>,
) {
    cookies.extend(updates.clone());
    for aliases in [
        ["qqmusic_key", "qm_keyst", "lqm_keyst"].as_slice(),
        ["uin", "wxuin"].as_slice(),
        ["wxaccess_token", "psrf_qqaccess_token", "access_token"].as_slice(),
        ["wxopenid"].as_slice(),
        ["wxrefresh_token"].as_slice(),
        ["wxunionid"].as_slice(),
        ["psrf_qqopenid", "openid"].as_slice(),
        ["psrf_qqrefresh_token", "refresh_token"].as_slice(),
        ["psrf_qqrefresh_key", "refresh_key"].as_slice(),
        ["psrf_qqunionid", "unionid"].as_slice(),
    ] {
        let Some(value) = aliases.iter().find_map(|alias| {
            updates
                .get(*alias)
                .map(String::as_str)
                .filter(|value| !value.trim().is_empty())
        }) else {
            continue;
        };
        replace_cookie_alias(cookies, aliases, value);
    }
}

fn classify_status(response: &reqwest::Response) -> Result<(), CatalogError> {
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(CatalogError::AuthRequired(
            "provider rejected credentials".to_owned(),
        )),
        StatusCode::TOO_MANY_REQUESTS => {
            Err(CatalogError::RateLimited("QQ Music rate limit".to_owned()))
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
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::{
        PlaybackEligibility, QQ_QIMEI_FALLBACK, QqDevice, QqMusicAdapter, QqQimei, QqQimeiCache,
        QqRequestOptions, civil_from_days, classify_qq_refresh_code, merge_qq_web_cookie_updates,
        parse_qimei_response, parse_qq_date_ms, parse_search_candidates, qimei_is_fresh,
        qimei_payload, qq_account_is_vip, qq_eligibility, qq_filename, qq_is_auth_response_code,
        qq_refresh_login_type, qq_resolver_ids, qq_response_business_error, qq_response_code,
        qq_response_data, qq_response_has_lyrics, qq_response_payload_object,
        qq_response_requires_auth_refresh, qq_stream_url, qq_validation_is_rejected,
        qq_vip_expire_at_ms, random_beacon_id, replace_cookie_alias,
        replace_cookie_alias_from_values, vip_vip_type_label,
    };
    use crate::catalog::{CatalogError, ProviderAccountStatus, SourceAdapter};
    use crate::credentials::{CredentialStore, ProviderCredential};
    use crate::domain::{ResolverLocator, SearchSpec, SongKey};
    use serde_json::{Value, json};
    use url::Url;
    use uuid::Uuid;

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
            Url::parse(&format!("http://{address}/resolve")).unwrap(),
            receiver,
        )
    }

    fn fixture_server_sequence(responses: Vec<Option<String>>) -> (Url, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for body in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let size = stream.read(&mut buffer).unwrap_or_default();
                    if size == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..size]);
                    let Some(header_end) =
                        request.windows(4).position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or_default();
                    if request.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
                sender
                    .send(String::from_utf8_lossy(&request).into_owned())
                    .unwrap();
                if let Some(body) = body {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                }
            }
        });
        (
            Url::parse(&format!("http://{address}/qq-fixture")).unwrap(),
            receiver,
        )
    }

    fn credentials() -> CredentialStore {
        let store = CredentialStore::memory();
        store
            .save(
                "qqmusic",
                ProviderCredential::QqMusic {
                    cookies: BTreeMap::from([
                        ("uin".to_owned(), "o42".to_owned()),
                        ("qqmusic_key".to_owned(), "secret".to_owned()),
                    ]),
                },
            )
            .unwrap();
        store
    }

    fn refresh_credentials(store: &CredentialStore) {
        store
            .save(
                "qqmusic",
                ProviderCredential::QqMusic {
                    cookies: BTreeMap::from([
                        ("uin".to_owned(), "o42".to_owned()),
                        ("qqmusic_key".to_owned(), "old-key".to_owned()),
                        ("qm_keyst".to_owned(), "old-secondary-key".to_owned()),
                        ("psrf_qqopenid".to_owned(), "open-id".to_owned()),
                        ("psrf_qqaccess_token".to_owned(), "access-token".to_owned()),
                        (
                            "psrf_qqrefresh_token".to_owned(),
                            "refresh-token".to_owned(),
                        ),
                        ("psrf_qqrefresh_key".to_owned(), "refresh-key".to_owned()),
                    ]),
                },
            )
            .unwrap();
    }

    #[test]
    fn qq_vkey_payload_uses_the_media_mid_once_in_standard_filename() {
        let payload = QqMusicAdapter::resolve_payload("songMid", "mediaMid", "42");

        assert_eq!(
            payload.pointer("/req_0/param/filename/0"),
            Some(&json!("M500mediaMid.mp3"))
        );
        assert_eq!(
            payload.pointer("/req_0/param/songmid/0"),
            Some(&json!("songMid"))
        );
    }

    #[test]
    fn qq_filename_uses_the_provider_quality_prefixes() {
        assert_eq!(qq_filename("mediaMid", "M500", "mp3"), "M500mediaMid.mp3");
        assert_eq!(qq_filename("mediaMid", "M800", "mp3"), "M800mediaMid.mp3");
        assert_eq!(qq_filename("mediaMid", "F000", "flac"), "F000mediaMid.flac");
    }

    #[test]
    fn qq_account_vip_checks_every_returned_flag() {
        let identity = json!({"vip": 0, "HugeVip": 1, "twelve": 0});
        let data = json!({"svip": 0, "star": 0});
        assert!(qq_account_is_vip(&identity, &data));

        let identity = json!({"vip": 0, "HugeVip": 0, "twelve": 0});
        let data = json!({"svip": 1, "star": 0});
        assert!(qq_account_is_vip(&identity, &data));

        let data = json!({"svip": 0, "star": 1});
        assert!(qq_account_is_vip(&identity, &data));
        assert_eq!(vip_vip_type_label(&identity, &data), "星级");
    }

    #[test]
    fn qq_account_vip_is_false_when_all_flags_are_zero() {
        let identity = json!({"vip": 0, "HugeVip": 0, "twelve": 0});
        let data = json!({"svip": 0, "star": 0});

        assert!(!qq_account_is_vip(&identity, &data));
    }

    #[test]
    fn qq_vip_expiry_uses_the_active_membership_tier() {
        let identity = json!({
            "HugeVip": 0,
            "HugeVipEnd": "2030-07-20 07:50:53",
            "vip": 1,
            "LMEnd": "2031-08-21 08:00:00"
        });

        assert_eq!(
            qq_vip_expire_at_ms(&identity, 0),
            parse_qq_date_ms("2031-08-21 08:00:00")
        );
    }

    #[test]
    fn qq_vip_dates_are_interpreted_as_beijing_time() {
        assert_eq!(parse_qq_date_ms("1970-01-01 08:00:00"), Some(0));
        assert_eq!(parse_qq_date_ms("1970-01-01 07:59:59"), None);
    }

    #[test]
    fn qq_validation_rejects_explicit_business_error_envelopes() {
        assert!(!qq_validation_is_rejected(&json!({
            "code": 0,
            "req_0": {"code": 0, "data": {}}
        })));
        assert!(qq_validation_is_rejected(&json!({
            "code": 0,
            "req_0": {"code": 1001, "data": {}}
        })));
        assert!(qq_validation_is_rejected(&json!({
            "req_0": {"code": "1001", "data": {}}
        })));
        assert!(qq_validation_is_rejected(&json!({"retcode": -1})));
        assert!(qq_validation_is_rejected(&json!({
            "search": {"code": 1001, "data": {}}
        })));
        assert!(!qq_validation_is_rejected(&json!({
            "search": {"code": 0, "data": {}}
        })));
    }

    #[test]
    fn qq_music_key_aliases_are_refreshed_together() {
        let mut cookies = BTreeMap::from([
            ("qqmusic_key".to_owned(), "old-canonical".to_owned()),
            ("qm_keyst".to_owned(), "old-secondary".to_owned()),
        ]);
        replace_cookie_alias(&mut cookies, &["qqmusic_key", "qm_keyst"], "new-key");
        assert_eq!(cookies.get("qqmusic_key"), Some(&"new-key".to_owned()));
        assert_eq!(cookies.get("qm_keyst"), Some(&"new-key".to_owned()));

        let mut only_secondary =
            BTreeMap::from([("qm_keyst".to_owned(), "old-secondary".to_owned())]);
        replace_cookie_alias(&mut only_secondary, &["qqmusic_key", "qm_keyst"], "new-key");
        assert_eq!(only_secondary.len(), 1);
        assert_eq!(only_secondary.get("qm_keyst"), Some(&"new-key".to_owned()));

        let mut empty = BTreeMap::new();
        replace_cookie_alias(&mut empty, &["qqmusic_key", "qm_keyst"], "new-key");
        assert_eq!(empty.get("qqmusic_key"), Some(&"new-key".to_owned()));
    }

    #[test]
    fn qq_response_data_accepts_root_and_numbered_envelopes() {
        assert_eq!(
            qq_response_data(&json!({"code": 0, "data": {"musickey": "key"}}))
                .and_then(|value| value.get("musickey"))
                .and_then(Value::as_str),
            Some("key")
        );
        assert_eq!(
            qq_response_data(&json!({"req_7": {"code": 0, "data": {"musicid": 42}}}))
                .and_then(|value| value.get("musicid"))
                .and_then(Value::as_i64),
            Some(42)
        );
        assert!(matches!(
            qq_response_business_error(&json!({"req": {"code": 0, "retcode": -1}})),
            Some(CatalogError::InvalidResponse(_))
        ));
        assert!(matches!(
            qq_response_business_error(&json!({"data": {"code": 1000}})),
            Some(CatalogError::AuthRequired(_))
        ));
    }

    #[test]
    fn qq_response_data_prefers_a_later_successful_envelope() {
        let response = json!({
            "req_0": {"code": 1001, "data": {"error": "expired"}},
            "req_1": {"code": 0, "data": {"musickey": "fresh-key"}}
        });
        assert_eq!(
            qq_response_data(&response)
                .and_then(|data| data.get("musickey"))
                .and_then(Value::as_str),
            Some("fresh-key")
        );

        // A successful empty envelope remains authoritative over a later
        // error envelope; callers can then classify the empty success normally.
        let response = json!({
            "req_0": {"code": 0, "data": {}},
            "req_1": {"code": 1000, "data": {"error": "expired"}}
        });
        assert!(
            qq_response_data(&response)
                .and_then(Value::as_object)
                .is_some_and(|object| object.is_empty())
        );

        // If every envelope is an error, retain the first data object for the
        // error parser rather than silently selecting a later unrelated one.
        let response = json!({
            "req_0": {"code": 1001, "data": {"first": true}},
            "req_1": {"code": 1002, "data": {"second": true}}
        });
        assert_eq!(
            qq_response_data(&response)
                .and_then(|data| data.get("first"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn qq_response_selection_keeps_data_auth_and_errors_consistent() {
        let mixed = json!({
            "req_0": {"code": 1000, "data": {"error": "expired"}},
            "req_1": {"code": 0, "data": {"musickey": "fresh-key"}}
        });
        assert_eq!(qq_response_code(&mixed), Some(0));
        assert!(!qq_response_requires_auth_refresh(&mixed));
        assert!(qq_response_business_error(&mixed).is_none());
        assert!(!qq_validation_is_rejected(&mixed));

        let root_success_without_data = json!({
            "code": 0,
            "req": {"code": 1000, "data": {"error": "expired"}}
        });
        assert_eq!(qq_response_code(&root_success_without_data), Some(1000));
        assert!(qq_response_requires_auth_refresh(
            &root_success_without_data
        ));
        assert!(matches!(
            qq_response_business_error(&root_success_without_data),
            Some(CatalogError::AuthRequired(_))
        ));

        let contradictory = json!({
            "req": {"code": 0, "retcode": -1, "data": {}}
        });
        assert_eq!(qq_response_code(&contradictory), Some(-1));
        assert!(matches!(
            qq_response_business_error(&contradictory),
            Some(CatalogError::InvalidResponse(_))
        ));

        let first_failure_wins = json!({
            "req_0": {"code": 2001, "data": {}},
            "req_1": {"code": 1000, "data": {}}
        });
        assert_eq!(qq_response_code(&first_failure_wins), Some(2001));
        assert!(!qq_response_requires_auth_refresh(&first_failure_wins));
        assert!(matches!(
            qq_response_business_error(&first_failure_wins),
            Some(CatalogError::RateLimited(_))
        ));

        let song_metadata_code = json!({
            "search": {"code": 0, "data": {"code": 1000, "song": {"id": 1}}}
        });
        assert_eq!(qq_response_code(&song_metadata_code), Some(0));
        assert!(!qq_response_requires_auth_refresh(&song_metadata_code));
        assert!(qq_response_business_error(&song_metadata_code).is_none());
    }

    #[test]
    fn qq_media_url_prefers_a_later_playable_quality_and_root_sip() {
        let response = json!({
            "data": {
                "midurlinfo": [
                    {"purl": "", "vip": 1},
                    {"purl": "song.mp3"}
                ]
            },
            "sip": ["https://cdn.example/", "https://backup.example/"]
        });
        assert_eq!(
            qq_stream_url(&response).unwrap().as_str(),
            "https://cdn.example/song.mp3"
        );
    }

    #[test]
    fn qq_empty_lyrics_trigger_legacy_fallback() {
        assert!(!qq_response_has_lyrics(&json!({
            "req_7": {"code": 0, "data": {"lyric": "", "trans": "  "}}
        })));
        assert!(qq_response_has_lyrics(&json!({
            "req_7": {"code": 0, "data": {"trans": "[00:01.00]译文"}}
        })));
    }

    #[test]
    fn qq_refresh_response_accepts_legacy_return_code_and_root_fields() {
        let response = json!({
            "retcode": 0,
            "musickey": "root-key",
            "musicid": 42
        });
        assert_eq!(qq_response_code(&response), Some(0));
        assert_eq!(
            qq_response_code(&json!({"data": {"code": 0, "musickey": "nested-key"}})),
            Some(0)
        );
        assert_eq!(
            qq_response_payload_object(&response)
                .and_then(|data| data.get("musickey"))
                .and_then(Value::as_str),
            Some("root-key")
        );
        assert!(qq_response_requires_auth_refresh(&json!({"retcode": 1000})));
    }

    #[test]
    fn qq_web_header_rotation_keeps_existing_aliases_in_sync() {
        let mut cookies = BTreeMap::from([
            ("qqmusic_key".to_owned(), "old-key".to_owned()),
            ("qm_keyst".to_owned(), "old-key".to_owned()),
            ("wxaccess_token".to_owned(), "old-token".to_owned()),
            ("access_token".to_owned(), "old-token".to_owned()),
        ]);
        let updates = BTreeMap::from([
            ("qm_keyst".to_owned(), "new-key".to_owned()),
            ("wxaccess_token".to_owned(), "new-token".to_owned()),
        ]);
        merge_qq_web_cookie_updates(&mut cookies, &updates);
        assert_eq!(cookies.get("qqmusic_key"), Some(&"new-key".to_owned()));
        assert_eq!(cookies.get("qm_keyst"), Some(&"new-key".to_owned()));
        assert_eq!(cookies.get("wxaccess_token"), Some(&"new-token".to_owned()));
        assert_eq!(cookies.get("access_token"), Some(&"new-token".to_owned()));
    }

    #[test]
    fn qq_refresh_prefers_numeric_musicid_over_string_alias() {
        let mut cookies = BTreeMap::from([("uin".to_owned(), "old".to_owned())]);
        let data = json!({"musicid": 42, "str_musicid": "o999"});
        replace_cookie_alias_from_values(
            &mut cookies,
            &["uin"],
            data.as_object().unwrap(),
            &["musicid", "str_musicid"],
        );
        assert_eq!(cookies.get("uin"), Some(&"42".to_owned()));
    }

    #[test]
    fn qq_auth_detection_covers_search_and_official_error_codes() {
        for code in [1000, 104400, 104401] {
            assert!(qq_response_requires_auth_refresh(
                &json!({"search": {"code": code}})
            ));
            assert!(qq_is_auth_response_code(code));
        }
        assert!(!qq_response_requires_auth_refresh(
            &json!({"search": {"code": 0}})
        ));
        assert!(!qq_response_requires_auth_refresh(
            &json!({"req": {"code": 2000}})
        ));
        assert!(!qq_response_requires_auth_refresh(&json!({
            "search": {"code": 0, "data": {"code": 1000}}
        })));
        assert!(qq_response_requires_auth_refresh(&json!({
            "data": {"code": 1000}
        })));
    }

    #[test]
    fn qq_refresh_error_codes_are_classified_without_false_relogin() {
        assert!(matches!(
            classify_qq_refresh_code(1000),
            CatalogError::AuthRequired(_)
        ));
        assert!(matches!(
            classify_qq_refresh_code(104401),
            CatalogError::AuthRequired(_)
        ));
        assert!(matches!(
            classify_qq_refresh_code(104400),
            CatalogError::AuthRequired(_)
        ));
        assert!(matches!(
            classify_qq_refresh_code(2001),
            CatalogError::RateLimited(_)
        ));
        assert!(matches!(
            classify_qq_refresh_code(104604),
            CatalogError::RateLimited(_)
        ));
        assert!(matches!(
            classify_qq_refresh_code(2000),
            CatalogError::InvalidResponse(_)
        ));
        assert!(matches!(
            qq_response_business_error(&json!({"search": {"code": 2001}})),
            Some(CatalogError::RateLimited(_))
        ));
        assert!(matches!(
            qq_response_business_error(&json!({"search": {"code": 20261}})),
            Some(CatalogError::InvalidResponse(_))
        ));
        assert!(matches!(
            classify_qq_refresh_code(20279),
            CatalogError::CredentialRejected(_)
        ));
    }

    #[tokio::test]
    async fn qq_account_status_rejects_auth_error_envelopes_without_long_caching() {
        let body = json!({"req_0": {"code": 1001, "data": {}}}).to_string();
        let (endpoint, requests) = fixture_server(body);
        let adapter = QqMusicAdapter::with_endpoints(
            credentials(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();

        let error = adapter.account_status().await.unwrap_err();
        assert!(matches!(error, CatalogError::CredentialRejected(_)));

        let cached = adapter.account_status().await.unwrap().unwrap();
        assert!(!cached.logged_in);
        assert!(cached.stale);
        assert_eq!(cached.last_error.as_deref(), Some("relogin_required"));
        assert!(requests.recv_timeout(Duration::from_secs(2)).is_ok());
        assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[tokio::test]
    async fn qq_cached_account_status_keeps_the_provider_vip_result() {
        let body = json!({
            "req_0": {
                "code": 0,
                "data": {
                    "identity": {
                        "vip": 1,
                        "HugeVipEnd": "2020-01-01 00:00:00"
                    }
                }
            }
        })
        .to_string();
        let (endpoint, requests) = fixture_server(body);
        let adapter = QqMusicAdapter::with_endpoints(
            credentials(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();

        let fresh = adapter.refresh_account_status().await.unwrap().unwrap();
        let cached = adapter.account_status().await.unwrap().unwrap();

        assert!(fresh.vip);
        assert!(fresh.vip_known);
        assert!(!fresh.stale);
        assert!(cached.vip);
        assert!(cached.vip_known);
        assert!(!cached.stale);
        assert!(requests.recv_timeout(Duration::from_secs(2)).is_ok());
        assert!(requests.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn qq_resolver_requires_current_v2_locator() {
        let key = SongKey::new("qqmusic", "songMid").unwrap();
        let locator = ResolverLocator::new("qqmusic:v2:songMid:mediaMid").unwrap();
        let legacy = ResolverLocator::new("qqmusic:v1:songMid").unwrap();

        assert_eq!(
            qq_resolver_ids(&key, Some(&locator)).unwrap(),
            ("songMid".to_owned(), "mediaMid".to_owned())
        );
        assert!(matches!(
            qq_resolver_ids(&key, Some(&legacy)),
            Err(CatalogError::InvalidResolverLocator(_))
        ));
    }

    #[test]
    fn qq_empty_purl_is_unavailable_not_a_broken_url() {
        let error =
            qq_stream_url(&json!({"req_0":{"data":{"midurlinfo":[{"purl":""}]}}})).unwrap_err();

        assert!(matches!(error, CatalogError::Unavailable(_)));
    }

    #[test]
    fn qq_rights_metadata_maps_to_states_without_preflight() {
        assert_eq!(
            qq_eligibility(&json!({"action":{"switch":0},"file":{"size_128mp3":0}})),
            PlaybackEligibility::NoCopyright
        );
        assert_eq!(
            qq_eligibility(&json!({"action":{"switch":1 << 14}})),
            PlaybackEligibility::NoCopyright
        );
        assert_eq!(
            qq_eligibility(&json!({"action":{"switch":14},"file":{"size_128mp3":1}})),
            PlaybackEligibility::Eligible
        );
    }

    #[test]
    fn qq_refresh_payload_keeps_the_mobile_login_contract() {
        let payload = super::qq_refresh_payload(
            "42",
            "music-key",
            "open-id",
            "access-token",
            "refresh-token",
            "refresh-key",
            "union-id",
            42,
            2,
            &QqDevice::new(),
            &QqQimei {
                q16: "".into(),
                q36: QQ_QIMEI_FALLBACK.into(),
            },
        );
        assert_eq!(payload["req"]["module"], "music.login.LoginServer");
        assert_eq!(payload["req"]["param"]["refresh_key"], "refresh-key");
        assert_eq!(payload["comm"]["qq"], "42");
        assert_eq!(payload["comm"]["tmeLoginType"], 2);
        assert_eq!(payload["req"]["param"]["musicid"], 42);
    }

    #[test]
    fn qq_refresh_payload_uses_the_wechat_contract_for_wx_keys() {
        let payload = super::qq_refresh_payload(
            "42",
            "W_X_music-key",
            "open-id",
            "",
            "refresh-token",
            "refresh-key",
            "union-id",
            42,
            1,
            &QqDevice::new(),
            &QqQimei {
                q16: "".into(),
                q36: QQ_QIMEI_FALLBACK.into(),
            },
        );
        assert_eq!(payload["comm"]["tmeLoginType"], 1);
        assert_eq!(payload["req"]["param"]["str_musicid"], "42");
        assert_eq!(payload["req"]["param"]["unionid"], "union-id");
        assert!(payload["req"]["param"].get("access_token").is_none());
    }

    #[test]
    fn qq_refresh_login_type_comes_from_music_key_prefix() {
        assert_eq!(qq_refresh_login_type("Q_H_L_key"), 2);
        assert_eq!(qq_refresh_login_type("W_X_key"), 1);
        assert_eq!(qq_refresh_login_type(" w_x_key "), 1);
        assert_eq!(qq_refresh_login_type("unknown-key"), 2);
    }

    #[test]
    fn qq_auth_response_codes_are_the_only_refresh_trigger() {
        assert!(super::qq_response_requires_auth_refresh(
            &json!({"req_0":{"code":1000}})
        ));
        assert!(!super::qq_response_requires_auth_refresh(
            &json!({"req_0":{"code":0}})
        ));
        assert!(!super::qq_response_requires_auth_refresh(
            &json!({"req_0":{"code":-1}})
        ));
        assert!(super::qq_response_requires_auth_refresh(
            &json!({"data":{"code":1000}})
        ));
    }

    #[test]
    fn qimei_cache_requires_both_values_and_expires_after_one_day() {
        let qimei = QqQimei {
            q16: "q16".to_owned(),
            q36: "q36".to_owned(),
        };
        assert!(qimei_is_fresh(&qimei, 1_000, 1_000 + 86_399));
        assert!(!qimei_is_fresh(&qimei, 1_000, 1_000 + 86_400));
        assert!(!qimei_is_fresh(
            &QqQimei {
                q16: String::new(),
                q36: "q36".to_owned(),
            },
            1_000,
            1_001
        ));
        assert!(!qimei_is_fresh(&qimei, 2_000, 1_000));
    }

    #[test]
    fn qimei_response_parser_accepts_nested_string_envelopes_and_rejects_empty_values() {
        assert_eq!(
            parse_qimei_response(&json!({
                "data": r#"{"data":{"q16":"q16-value","q36":"q36-value"}}"#
            }))
            .map(|value| (value.q16, value.q36)),
            Some(("q16-value".to_owned(), "q36-value".to_owned()))
        );
        assert!(
            parse_qimei_response(&json!({
                "data": {"q16": "", "q36": "q36-value"}
            }))
            .is_none()
        );
    }

    #[test]
    fn qimei_payload_matches_upstream_device_field_contract() {
        let payload = qimei_payload(&QqDevice::new()).unwrap();
        let params = payload.body["qimeiParams"]["params"]
            .as_str()
            .expect("encrypted params");
        assert!(!params.is_empty());
        let key = payload.body["qimeiParams"]["key"]
            .as_str()
            .expect("encrypted key");
        assert!(!key.is_empty());
        let extra = payload.body["qimeiParams"]["extra"].as_str().unwrap();
        assert_eq!(extra, r#"{"appKey":"0AND0HD6FE4HY80F"}"#);
        let beacon = payload.body["qimeiParams"].to_string();
        assert!(beacon.contains("\"nonce\""));
    }

    #[test]
    fn qq_device_load_accepts_legacy_file_without_qimei_fields() {
        let directory =
            std::env::temp_dir().join(format!("miliastra-qq-device-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("device.json");
        let mut legacy = serde_json::to_value(QqDevice::new()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("qimei");
        object.remove("qimei_saved_at");
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let loaded = QqDevice::load(Some(path.clone())).unwrap();
        assert!(loaded.qimei.is_none());
        assert!(loaded.qimei_saved_at.is_none());
        assert_eq!(loaded.path, Some(path));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn qimei_beacon_and_civil_date_are_stable_shapes() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        let mut rng = rand::thread_rng();
        let beacon = random_beacon_id(0, &mut rng);
        assert_eq!(beacon.matches("k1:").count(), 1);
        assert_eq!(beacon.matches("k40:").count(), 1);
        assert!(beacon.ends_with(';'));
    }

    #[tokio::test]
    async fn qq_implicit_refresh_is_single_flight_while_the_old_revision_is_current() {
        let store = CredentialStore::memory();
        refresh_credentials(&store);
        let snapshot = store.snapshot("qqmusic").unwrap().unwrap();
        let _lease = store
            .try_acquire_refresh_lease("qqmusic")
            .unwrap()
            .expect("the first refresh owns the provider lease");
        let endpoint = Url::parse("http://127.0.0.1:1/qq-fixture").unwrap();
        let adapter = QqMusicAdapter::with_endpoints(
            store,
            Duration::from_secs(1),
            endpoint.clone(),
            endpoint.clone(),
        )
        .unwrap();

        let error = adapter
            .retry_after_refresh_with_options(
                &snapshot.credential,
                snapshot.revision,
                &endpoint,
                json!({}),
                QqRequestOptions::default(),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, CatalogError::Transient(_)));
    }

    #[tokio::test]
    async fn qq_stale_same_value_snapshot_never_refreshes_or_overwrites_a_new_login() {
        let store = CredentialStore::memory();
        refresh_credentials(&store);
        let snapshot = store.snapshot("qqmusic").unwrap().unwrap();
        // A login completion may re-save the same cookie values. Revision, rather than value
        // equality, identifies that the old request no longer owns this session.
        store.save("qqmusic", snapshot.credential.clone()).unwrap();
        let (endpoint, requests) = fixture_server_sequence(vec![Some(
            r#"{"req":{"code":0,"data":{"musickey":"stale-refresh-key","musicid":"42","openid":"stale-open-id","access_token":"stale-access-token","refresh_token":"stale-refresh-token","refresh_key":"stale-refresh-key"}}}"#.to_owned(),
        )]);
        let mut adapter = QqMusicAdapter::with_endpoints(
            store.clone(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint.clone(),
        )
        .unwrap();
        adapter.device.qimei = Some(QqQimei {
            q16: "test-qimei".to_owned(),
            q36: QQ_QIMEI_FALLBACK.to_owned(),
        });
        adapter.device.qimei_saved_at = Some(super::epoch_secs());

        adapter
            .retry_after_refresh_with_options(
                &snapshot.credential,
                snapshot.revision,
                &endpoint,
                json!({}),
                QqRequestOptions::default(),
            )
            .await
            .unwrap();

        let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(request.starts_with("GET "));
        assert_eq!(
            store
                .get("qqmusic")
                .unwrap()
                .unwrap()
                .cookies()
                .get("qqmusic_key")
                .map(String::as_str),
            Some("old-key"),
            "the stale refresh result must not overwrite a same-value re-login"
        );
    }

    #[tokio::test]
    async fn qq_persisted_implicit_refresh_invalidates_account_status_cache() {
        let store = CredentialStore::memory();
        refresh_credentials(&store);
        let snapshot = store.snapshot("qqmusic").unwrap().unwrap();
        let endpoint = Url::parse("http://127.0.0.1:1/qq-fixture").unwrap();
        let adapter = QqMusicAdapter::with_endpoints(
            store.clone(),
            Duration::from_secs(1),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();
        {
            let mut cache = adapter.account_cache.lock().unwrap();
            let generation = cache.generation();
            assert!(cache.store_if_current(
                generation,
                ProviderAccountStatus {
                    provider: "qqmusic".to_owned(),
                    logged_in: true,
                    vip_known: true,
                    vip: true,
                    ..ProviderAccountStatus::default()
                },
                Instant::now(),
                false,
            ));
        }
        let mut cookies = snapshot.credential.cookies().clone();
        cookies.insert("qqmusic_key".to_owned(), "new-key".to_owned());

        assert!(
            adapter
                .persist_refreshed_credential(
                    snapshot.revision,
                    ProviderCredential::QqMusic { cookies },
                )
                .await
                .unwrap()
        );

        assert!(
            adapter
                .account_cache
                .lock()
                .unwrap()
                .cached(Duration::from_secs(1), Duration::from_secs(1))
                .is_none()
        );
        assert_eq!(
            store
                .get("qqmusic")
                .unwrap()
                .unwrap()
                .cookies()
                .get("qqmusic_key")
                .map(String::as_str),
            Some("new-key")
        );
    }

    #[tokio::test]
    async fn qq_auth_failure_refreshes_once_retries_and_persists_new_cookie_state() {
        let root =
            std::env::temp_dir().join(format!("miliastra-qq-refresh-{}", uuid::Uuid::new_v4()));
        let store = CredentialStore::open(root.clone()).unwrap();
        refresh_credentials(&store);
        let (endpoint, requests) = fixture_server_sequence(vec![
            Some(r#"{"search":{"code":1000}}"#.to_owned()),
            Some(
                r#"{"req":{"code":0,"data":{"musickey":"new-key","musicid":"42","openid":"new-open-id","access_token":"new-access-token","refresh_token":"new-refresh-token","refresh_key":"new-refresh-key"}}}"#.to_owned(),
            ),
            Some(r#"{"search":{"data":{"body":{"song":{"list":[]}}}}}"#.to_owned()),
        ]);
        let adapter = QqMusicAdapter::with_endpoints(
            store.clone(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();

        let result = adapter
            .search(&SearchSpec {
                keyword: "validation".to_owned(),
                sources: Vec::new(),
                limit: 1,
            })
            .await
            .unwrap();
        assert!(result.is_empty());
        let first = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        let refresh = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        let retry = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(first.starts_with("GET "));
        assert!(refresh.starts_with("POST "));
        assert!(refresh.contains("refresh_key"));
        assert!(retry.starts_with("GET "));

        let current = store.get("qqmusic").unwrap().unwrap();
        assert_eq!(current.cookies().get("qqmusic_key").unwrap(), "new-key");
        assert_eq!(current.cookies().get("qm_keyst").unwrap(), "new-key");
        assert_eq!(
            current.cookies().get("psrf_qqrefresh_key").unwrap(),
            "new-refresh-key"
        );
        let restored = CredentialStore::open(root.clone()).unwrap();
        assert_eq!(
            restored
                .get("qqmusic")
                .unwrap()
                .unwrap()
                .cookies()
                .get("qqmusic_key")
                .unwrap(),
            "new-key"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn qq_refresh_works_with_refresh_token_only_and_persists_issued_refresh_key() {
        let store = CredentialStore::memory();
        store
            .save(
                "qqmusic",
                ProviderCredential::QqMusic {
                    // 网页登录（QQ OAuth）常见形态：有 refresh_token 无 refresh_key。
                    cookies: BTreeMap::from([
                        ("uin".to_owned(), "o42".to_owned()),
                        ("qqmusic_key".to_owned(), "old-key".to_owned()),
                        ("psrf_qqopenid".to_owned(), "open-id".to_owned()),
                        ("psrf_qqaccess_token".to_owned(), "access-token".to_owned()),
                        (
                            "psrf_qqrefresh_token".to_owned(),
                            "refresh-token".to_owned(),
                        ),
                    ]),
                },
            )
            .unwrap();
        let (endpoint, requests) = fixture_server_sequence(vec![
            Some(r#"{"search":{"code":1000}}"#.to_owned()),
            Some(
                r#"{"req":{"code":0,"data":{"musickey":"new-key","musicid":"42","openid":"new-open-id","access_token":"new-access-token","refresh_token":"new-refresh-token","refresh_key":"new-refresh-key"}}}"#.to_owned(),
            ),
            Some(r#"{"search":{"data":{"body":{"song":{"list":[]}}}}}"#.to_owned()),
        ]);
        let adapter = QqMusicAdapter::with_endpoints(
            store.clone(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();

        let result = adapter
            .search(&SearchSpec {
                keyword: "validation".to_owned(),
                sources: Vec::new(),
                limit: 1,
            })
            .await
            .unwrap();
        assert!(result.is_empty());
        let _ = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        let refresh = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(refresh.starts_with("POST "));
        assert!(refresh.contains("refresh_token"));
        let _ = requests.recv_timeout(Duration::from_secs(2)).unwrap();

        let current = store.get("qqmusic").unwrap().unwrap();
        assert_eq!(current.cookies().get("qqmusic_key").unwrap(), "new-key");
        // 移动端续期响应补发的 refresh_key 已持久化，后续可继续续期。
        assert_eq!(
            current.cookies().get("psrf_qqrefresh_key").unwrap(),
            "new-refresh-key"
        );
        assert_eq!(
            current.cookies().get("psrf_qqrefresh_token").unwrap(),
            "new-refresh-token"
        );
    }

    #[tokio::test]
    async fn qq_refresh_network_ambiguity_keeps_the_previous_cookie_state() {
        let store = CredentialStore::memory();
        refresh_credentials(&store);
        let (endpoint, requests) =
            fixture_server_sequence(vec![Some(r#"{"search":{"code":1000}}"#.to_owned()), None]);
        let adapter = QqMusicAdapter::with_endpoints(
            store.clone(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();

        let error = adapter
            .search(&SearchSpec {
                keyword: "validation".to_owned(),
                sources: Vec::new(),
                limit: 1,
            })
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CatalogError::Transient(_) | CatalogError::TimedOut(_)
        ));
        let _ = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        let _ = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            store
                .get("qqmusic")
                .unwrap()
                .unwrap()
                .cookies()
                .get("qqmusic_key")
                .unwrap(),
            "old-key"
        );
    }

    #[test]
    fn qq_search_keeps_at_most_five_non_ineligible_candidates() {
        let raw = (0..10)
            .map(|index| {
                json!({
                    "mid": format!("mid-{index}"),
                    "title": format!("Song {index}"),
                    "singer": [{"name":"Artist"}],
                    "action": {"switch": if index == 0 { 0 } else { 14 }},
                    "file": {"size_128mp3": 1}
                })
            })
            .collect::<Vec<_>>();
        let parsed =
            parse_search_candidates(&json!({"search":{"data":{"body":{"song":{"list":raw}}}}}))
                .unwrap();
        assert_eq!(parsed.len(), 5);
        // switch=0 的歌曲标注无版权（保留展示），其余为可播放。
        assert_eq!(
            parsed
                .iter()
                .find(|candidate| candidate.song.key.id == "mid-0")
                .unwrap()
                .eligibility,
            PlaybackEligibility::NoCopyright
        );
        assert!(
            parsed
                .iter()
                .all(|candidate| candidate.eligibility != PlaybackEligibility::Ineligible)
        );
    }

    #[test]
    fn qq_search_locator_keeps_distinct_media_mid() {
        let parsed = parse_search_candidates(&json!({"search":{"data":{"body":{"song":{"list":[{
            "mid":"songMid", "title":"Song", "singer":[], "action":{"switch":14},
            "file":{"media_mid":"mediaMid", "size_128mp3":1}
        }]}}}}}))
        .unwrap();

        assert_eq!(
            parsed[0].song.resolver_locator.as_ref().unwrap().as_str(),
            "qqmusic:v2:songMid:mediaMid"
        );
    }

    #[test]
    fn qq_search_accepts_a_root_data_envelope() {
        let parsed = parse_search_candidates(&json!({
            "code": 0,
            "data": {
                "body": {
                    "song": {
                        "list": [{
                            "mid": "rootMid",
                            "title": "Root Song",
                            "singer": [],
                            "action": {"switch": 14},
                            "file": {"size_128mp3": 1}
                        }]
                    }
                }
            }
        }))
        .unwrap();
        assert_eq!(parsed[0].song.key.id, "rootMid");
    }

    #[tokio::test]
    async fn qq_resolve_requests_standard_filename_from_a_loopback_fixture() {
        let body = json!({
            "req_0": {"data": {
                "sip": ["https://stream.example.test/"],
                "midurlinfo": [{"purl":"M500mediaMid.mp3?vkey=token"}]
            }}
        })
        .to_string();
        let (endpoint, request) = fixture_server(body);
        let adapter = QqMusicAdapter::with_endpoints(
            credentials(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint,
        )
        .unwrap();
        let key = SongKey::new("qqmusic", "songMid").unwrap();
        let locator = ResolverLocator::new("qqmusic:v2:songMid:mediaMid").unwrap();

        let source = adapter.resolve(&key, Some(&locator)).await.unwrap();

        assert_eq!(
            source.url.as_str(),
            "https://stream.example.test/M500mediaMid.mp3?vkey=token"
        );
        assert!(!source.headers.contains_key("Cookie"));
        let request = request.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            request
                .to_ascii_lowercase()
                .contains("cookie: qqmusic_key=secret; uin=o42")
        );
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let request_url = Url::parse(&format!("http://fixture{target}")).unwrap();
        let payload = request_url
            .query_pairs()
            .find(|(name, _)| name == "data")
            .map(|(_, value)| serde_json::from_str::<serde_json::Value>(&value).unwrap())
            .unwrap();
        assert_eq!(
            payload.pointer("/req_0/param/filename/0"),
            Some(&json!("M500mediaMid.mp3"))
        );
    }

    #[tokio::test]
    async fn qq_lyrics_uses_the_native_endpoint_and_prefers_translation() {
        let body = json!({
            "req": {
                "code": 0,
                "data": {
                    "lyric": "[00:01.00]original",
                    "trans": "[00:01.00]translated"
                }
            }
        })
        .to_string();
        let (endpoint, request) = fixture_server(body);
        let adapter = QqMusicAdapter::with_endpoints(
            credentials(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint.clone(),
        )
        .unwrap()
        .with_lyrics_endpoints(endpoint.clone(), endpoint);
        let key = SongKey::new("qqmusic", "songMid").unwrap();
        let locator = ResolverLocator::new("qqmusic:v2:songMid:mediaMid").unwrap();

        let lyrics = adapter.lyrics(&key, Some(&locator)).await.unwrap().unwrap();

        assert_eq!(lyrics.line_at_ms(1_000), Some("translated"));
        let request = request.recv_timeout(Duration::from_secs(2)).unwrap();
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let request_url = Url::parse(&format!("http://fixture{target}")).unwrap();
        let payload = request_url
            .query_pairs()
            .find(|(name, _)| name == "data")
            .map(|(_, value)| serde_json::from_str::<serde_json::Value>(&value).unwrap())
            .unwrap();
        assert_eq!(
            payload.pointer("/req/method"),
            Some(&json!("GetPlayLyricInfo"))
        );
        assert_eq!(
            payload.pointer("/req/param/songMID"),
            Some(&json!("songMid"))
        );
        assert_eq!(payload.pointer("/req/param/trans"), Some(&json!(1)));
        assert!(request.to_ascii_lowercase().contains("cookie:"));
    }

    #[tokio::test]
    async fn qq_native_lyrics_retries_after_refresh_and_replays_request_options() {
        let (endpoint, requests) = fixture_server_sequence(vec![
            Some(r#"{"req":{"code":1000,"data":{}}}"#.to_owned()),
            Some(r#"{"req":{"code":0,"data":{"musickey":"Q_H_L_rotated","musicid":"42","openid":"open-id","access_token":"access-token-rotated","refresh_token":"refresh-token-rotated","refresh_key":"refresh-key-rotated"}}}"#.to_owned()),
            Some(r#"{"req":{"code":0,"data":{"lyric":"[00:01.00]original","trans":"[00:01.00]translated"}}}"#.to_owned()),
        ]);
        let store = CredentialStore::memory();
        store
            .save(
                "qqmusic",
                ProviderCredential::QqMusic {
                    cookies: BTreeMap::from([
                        ("uin".to_owned(), "o42".to_owned()),
                        ("qqmusic_key".to_owned(), "Q_H_L_old".to_owned()),
                        ("openid".to_owned(), "open-id".to_owned()),
                        ("access_token".to_owned(), "access-token".to_owned()),
                        ("refresh_token".to_owned(), "refresh-token".to_owned()),
                    ]),
                },
            )
            .unwrap();
        let adapter = QqMusicAdapter::with_endpoints(
            store.clone(),
            Duration::from_secs(2),
            endpoint.clone(),
            endpoint.clone(),
        )
        .unwrap()
        .with_lyrics_endpoints(endpoint.clone(), endpoint);
        let qimei = QqQimei {
            q16: "fixture-q16".to_owned(),
            q36: QQ_QIMEI_FALLBACK.to_owned(),
        };
        adapter.qimei_cache.lock().unwrap().replace(QqQimeiCache {
            value: qimei,
            saved_at: super::epoch_secs(),
        });

        let key = SongKey::new("qqmusic", "songMid").unwrap();
        let locator = ResolverLocator::new("qqmusic:v2:songMid:mediaMid").unwrap();
        let lyrics = adapter.lyrics(&key, Some(&locator)).await.unwrap().unwrap();
        assert_eq!(lyrics.line_at_ms(1_000), Some("translated"));

        let first = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        let third = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(first.starts_with("GET "));
        assert!(first.contains("format=json"));
        assert!(first.contains("inCharset=utf8"));
        assert!(
            first
                .to_ascii_lowercase()
                .contains("referer: https://y.qq.com/")
        );
        assert!(second.starts_with("POST "));
        assert!(!second.contains("sign="));
        assert!(third.starts_with("GET "));
        assert!(third.contains("format=json"));
        assert!(
            third
                .to_ascii_lowercase()
                .contains("referer: https://y.qq.com/")
        );
        assert!(third.to_ascii_lowercase().contains("cookie:"));
        assert_eq!(
            store
                .get("qqmusic")
                .unwrap()
                .unwrap()
                .cookies()
                .get("qqmusic_key")
                .map(String::as_str),
            Some("Q_H_L_rotated")
        );
    }

    #[test]
    fn qq_translation_placeholder_lines_are_removed() {
        assert_eq!(
            super::strip_untranslated_placeholder_lines("[00:01.00]translated\n[00:02.00]//"),
            Some("[00:01.00]translated".to_owned())
        );
        assert_eq!(
            super::strip_untranslated_placeholder_lines("[00:02.00]//"),
            None
        );
    }
}
