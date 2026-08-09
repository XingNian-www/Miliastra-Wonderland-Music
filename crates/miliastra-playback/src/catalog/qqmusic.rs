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
use rand::{Rng, distributions::Alphanumeric};
use reqwest::{Client, StatusCode};
use rsa::pkcs8::DecodePublicKey;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::catalog::{
    CatalogError, CredentialRefreshAdapter, PlaybackEligibility, ProviderAccountStatus,
    ProviderSearchCandidate, SourceAdapter,
};
use crate::credentials::{CredentialStore, ProviderCredential};
use crate::domain::{ResolverLocator, SearchSpec, Song, SongKey, StreamSource};
use crate::lyrics::{TimedLyrics, parse_lrc_pair};

const SEARCH_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const PROVIDER: &str = "qqmusic";
const RESOLVE_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const LOGIN_URL: &str = "https://u.y.qq.com/cgi-bin/musics.fcg";
const WEB_REFRESH_URL: &str = "https://c.y.qq.com/base/fcgi-bin/login_get_musickey.fcg";
const LYRICS_URL: &str = "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg";
const LYRICS_NATIVE_URL: &str = "https://u.y.qq.com/cgi-bin/musicu.fcg";
const QQ_MOBILE_VERSION: i64 = 14_090_008;
const QQ_QIMEI_FALLBACK: &str = "6c9d3cd110abca9b16311cee10001e717614";
const QQ_DEVICE_FILE: &str = "qqmusic-device.json";
const QIMEI_URL: &str = "https://api.tencentmusic.com/tme/trpc/proxy";
const QIMEI_APP_KEY: &str = "0AND0HD6FE4HY80F";
const QIMEI_SECRET: &str = "ZdJqM15EeO2zWc08";
const QIMEI_PUBLIC_KEY: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDEIxgwoutfwoJxcGQeedgP7FG9qaIuS0qzfR8gWkrkTZKM2iWHn2ajQpBRZjMSoSf6+KJGvar2ORhBfpDXyVtZCKpqLQ+FLkpncClKVIrBwv6PHyUvuCb0rIarmgDnzkfQAqVufEtR64iazGDKatvJ9y6B9NMbHddGSAUmRTCrHQIDAQAB\n-----END PUBLIC KEY-----";
const PLAY_BITS_MASK: i64 = 0b1110;
const TRY_BIT_MASK: i64 = 1 << 14;

#[derive(Clone)]
pub struct QqMusicAdapter {
    client: Client,
    credentials: CredentialStore,
    search_url: Url,
    resolve_url: Url,
    login_url: Url,
    lyrics_url: Url,
    native_lyrics_url: Url,
    device: QqDevice,
    /// 账号 VIP 状态缓存（成功 5 分钟/失败 60 秒），避免轮询频繁打账号接口。
    account_cache: std::sync::Arc<
        std::sync::Mutex<
            Option<(
                ProviderAccountStatus,
                std::time::Instant,
                bool,
            )>,
        >,
    >,
}

/// QQ 账号状态缓存有效期（查询成功时）。
const ACCOUNT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
/// 查询失败（登录态失效/风控/超时）时的短缓存。
const ACCOUNT_CACHE_FAILED_TTL: Duration = Duration::from_secs(60);

impl QqMusicAdapter {
    pub fn new(credentials: CredentialStore, timeout: Duration) -> Result<Self, CatalogError> {
        let client = Client::builder()
            .timeout(timeout)
            .user_agent("miliastra-wonderland-music/0.1")
            .build()
            .map_err(|error| CatalogError::Transient(error.to_string()))?;
        let device = QqDevice::load(credentials.non_sensitive_path(QQ_DEVICE_FILE))
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        Ok(Self {
            client,
            credentials,
            device,
            search_url: Url::parse(SEARCH_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            resolve_url: Url::parse(RESOLVE_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            login_url: Url::parse(LOGIN_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            lyrics_url: Url::parse(LYRICS_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            native_lyrics_url: Url::parse(LYRICS_NATIVE_URL)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            account_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
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
        adapter.login_url = resolve_url;
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
        let cookie = cookie_header(credential.cookies());
        let response = self
            .client
            .get(url.clone())
            .query(&[("data", body.to_string())])
            .header("Cookie", cookie)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn request_json(&self, url: &Url, body: Value) -> Result<Value, CatalogError> {
        let credential = self.credential()?;
        match self
            .request_json_with_credential(&credential, url, body.clone())
            .await
        {
            Ok(response) if qq_response_requires_auth_refresh(&response) => {
                self.retry_after_refresh(&credential, url, body).await
            }
            Ok(response) => Ok(response),
            Err(CatalogError::AuthRequired(_)) => {
                self.retry_after_refresh(&credential, url, body).await
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
        let credential = self.credential()?;
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
        let data = body.to_string();
        let response = self
            .client
            .get(self.native_lyrics_url.clone())
            .query(&[
                ("format", "json"),
                ("inCharset", "utf8"),
                ("outCharset", "utf-8"),
                ("data", data.as_str()),
            ])
            .header("Cookie", cookie_header(credential.cookies()))
            .header("Referer", "https://y.qq.com/")
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }

    async fn lyrics_json_with_translation(&self, song_mid: &str) -> Result<Value, CatalogError> {
        match self.native_lyrics_json(song_mid).await {
            Ok(response) if response.pointer("/req/data").is_some() => Ok(response),
            Ok(_) | Err(_) => self.lyrics_json(song_mid).await,
        }
    }

    async fn retry_after_refresh(
        &self,
        credential: &ProviderCredential,
        url: &Url,
        body: Value,
    ) -> Result<Value, CatalogError> {
        let Some(refreshed) = self.refresh_credential(credential).await? else {
            return Err(CatalogError::AuthRequired(
                "QQ Music credentials require relogin".to_owned(),
            ));
        };
        self.request_json_with_credential(&refreshed, url, body)
            .await
    }

    async fn refresh_credential(
        &self,
        credential: &ProviderCredential,
    ) -> Result<Option<ProviderCredential>, CatalogError> {
        let ProviderCredential::QqMusic { cookies } = credential else {
            return Ok(None);
        };
        if let Some(refreshed) = self.refresh_web_credential(cookies).await? {
            return Ok(Some(refreshed));
        }
        let Some(uin) = first_cookie(cookies, &["uin", "wxuin"])
            .map(|value| value.trim_start_matches('o').to_owned())
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let Some(music_key) = first_cookie(cookies, &["qqmusic_key", "qm_keyst"]) else {
            return Ok(None);
        };
        let Some(open_id) = first_cookie(cookies, &["psrf_qqopenid", "openid"]) else {
            return Ok(None);
        };
        let Some(access_token) = first_cookie(cookies, &["psrf_qqaccess_token", "access_token"])
        else {
            return Ok(None);
        };
        let Some(refresh_token) = first_cookie(cookies, &["psrf_qqrefresh_token", "refresh_token"])
        else {
            return Ok(None);
        };
        let Some(refresh_key) = first_cookie(cookies, &["psrf_qqrefresh_key", "refresh_key"])
        else {
            return Ok(None);
        };
        let music_id = uin.parse::<u64>().map_err(|_| {
            CatalogError::AuthRequired("QQ Music account identifier requires relogin".to_owned())
        })?;
        let qimei = self.qimei().await;
        let payload = qq_refresh_payload(
            &uin,
            music_key,
            open_id,
            access_token,
            refresh_token,
            refresh_key,
            music_id,
            &self.device,
            &qimei,
        );
        let body = serde_json::to_string(&payload)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        let response = self
            .client
            .post(self.login_url.clone())
            .query(&[("sign", qq_sign(&body))])
            .header("Content-Type", "application/json")
            .header("User-Agent", "QQMusic")
            .body(body)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        let response = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        let req = response.get("req").unwrap_or(&response);
        if req.get("code").and_then(as_i64) != Some(0) {
            return Err(CatalogError::AuthRequired(
                "QQ Music refresh was rejected; relogin is required".to_owned(),
            ));
        }
        let Some(data) = req.get("data").and_then(Value::as_object) else {
            return Err(CatalogError::AuthRequired(
                "QQ Music refresh returned no credential data".to_owned(),
            ));
        };
        let Some(new_music_key) = data
            .get("musickey")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            return Err(CatalogError::AuthRequired(
                "QQ Music refresh returned no music key".to_owned(),
            ));
        };
        let mut refreshed = cookies.clone();
        replace_cookie_alias(&mut refreshed, &["qqmusic_key", "qm_keyst"], new_music_key);
        replace_cookie_alias_from_value(&mut refreshed, &["uin", "wxuin"], data, "musicid");
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
        let refreshed = ProviderCredential::QqMusic { cookies: refreshed };
        self.credentials
            .save("qqmusic", refreshed.clone())
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        Ok(Some(refreshed))
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
        let Some(music_key) = first_cookie(cookies, &["qqmusic_key", "qm_keyst"]) else {
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
        let body = response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        if body.get("code").and_then(as_i64) != Some(0) {
            return Ok(None);
        }
        let Some(data) = body.as_object() else {
            return Ok(None);
        };
        let Some(access_token) = data
            .get("wxaccess_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let mut refreshed = cookies.clone();
        if let Some(new_music_key) = data
            .get("musickey")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            replace_cookie_alias(&mut refreshed, &["qqmusic_key", "qm_keyst"], new_music_key);
        }
        replace_cookie_alias_from_value(&mut refreshed, &["uin", "wxuin"], data, "musicuin");
        replace_cookie_alias_from_value(
            &mut refreshed,
            &["wxaccess_token", "psrf_qqaccess_token", "access_token"],
            data,
            "wxaccess_token",
        );
        if refreshed
            .get("wxaccess_token")
            .or_else(|| refreshed.get("psrf_qqaccess_token"))
            .or_else(|| refreshed.get("access_token"))
            .is_none_or(|value| value != access_token)
        {
            return Ok(None);
        }
        let refreshed = ProviderCredential::QqMusic { cookies: refreshed };
        self.credentials
            .save("qqmusic", refreshed.clone())
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
        Ok(Some(refreshed))
    }

    async fn qimei(&self) -> QqQimei {
        if let Some(qimei) = self.device.qimei.clone() {
            return qimei;
        }
        let Ok(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| qimei_payload(&self.device)))
        else {
            return QqQimei {
                q16: String::new(),
                q36: QQ_QIMEI_FALLBACK.to_owned(),
            };
        };
        let result = self
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
            .ok();
        let result = if let Some(response) = result {
            response.json::<Value>().await.ok()
        } else {
            None
        }
        .and_then(|body| body.get("data").cloned())
        .and_then(|data| {
            if data.is_string() {
                serde_json::from_str(data.as_str()?).ok()
            } else {
                Some(data)
            }
        })
        .and_then(|data| data.get("data").cloned())
        .and_then(|data| {
            Some(QqQimei {
                q16: data.get("q16")?.to_string().trim_matches('"').to_owned(),
                q36: data.get("q36")?.to_string().trim_matches('"').to_owned(),
            })
        });
        if let Some(result) = result {
            let mut device = self.device.clone();
            device.qimei = Some(result.clone());
            let _ = device.save();
            return result;
        }
        QqQimei {
            q16: String::new(),
            q36: QQ_QIMEI_FALLBACK.to_owned(),
        }
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

    async fn query_account_status(&self) -> Result<Option<ProviderAccountStatus>, CatalogError> {
        let Ok(credential) = self.credential() else {
            return Ok(Some(ProviderAccountStatus {
                logged_in: false,
                ..ProviderAccountStatus::default()
            }));
        };
        let cookies = credential.cookies();
        let configured = ["qqmusic_key", "qm_keyst", "uin", "wxuin"]
            .iter()
            .any(|name| cookies.get(*name).is_some_and(|value| !value.is_empty()));
        let user_id = first_cookie(cookies, &["uin", "wxuin"])
            .map(str::to_owned)
            .filter(|value| value != "0" && !value.is_empty());
        // login_type：1=QQ 登录，2=微信授权登录（y.qq.com 微信扫码入口）。
        let login_method = match cookies.get("login_type").map(String::as_str) {
            Some("1") => Some("qq".to_owned()),
            Some("2") => Some("wechat".to_owned()),
            _ => cookies
                .get("uin")
                .is_some_and(|value| value != "0")
                .then(|| "qq".to_owned()),
        };
        if !configured {
            return Ok(Some(ProviderAccountStatus {
                logged_in: false,
                user_id,
                login_method,
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
        let Some(data) = value.pointer("/req_0/data").filter(|v| v.is_object()) else {
            // 登录态失效或接口拒绝：视为查询失败，走失败缓存快速重试。
            return Err(CatalogError::InvalidResponse(
                "QQ Music vip_login_base returned no data".to_owned(),
            ));
        };
        let identity = data.get("identity").unwrap_or(data);
        let vip = identity
            .get("vip")
            .or_else(|| data.get("svip"))
            .or_else(|| identity.get("HugeVip"))
            .or_else(|| identity.get("star"))
            .or_else(|| identity.get("twelve"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            != 0;
        let vip_type = vip_vip_type_label(identity, data);
        let vip_expire_at_ms = identity
            .get("HugeVipEnd")
            .or_else(|| identity.get("LMEnd"))
            .and_then(Value::as_str)
            .and_then(parse_qq_date_ms);
        Ok(Some(ProviderAccountStatus {
            logged_in: true,
            user_id,
            nickname: None,
            vip,
            vip_type: vip.then_some(vip_type).or_else(|| Some("非VIP".to_owned())),
            vip_expire_at_ms,
            login_method,
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
        let uin = cookies
            .get("uin")
            .or_else(|| cookies.get("wxuin"))
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
        let code = response
            .get("retcode")
            .or_else(|| response.get("code"))
            .or_else(|| response.pointer("/req/code"))
            .and_then(as_i64)
            .unwrap_or_default();
        if code != 0 {
            return if qq_response_requires_auth_refresh(&response) {
                Err(CatalogError::AuthRequired(
                    "QQ Music lyric request requires relogin".to_owned(),
                ))
            } else {
                Err(CatalogError::Unavailable(
                    "QQ Music returned no lyrics for this track".to_owned(),
                ))
            };
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
        let now = std::time::Instant::now();
        if let Ok(mut guard) = self.account_cache.lock()
            && let Some((status, cached_at, failed)) = guard.as_ref()
            && cached_at.elapsed() < if *failed { ACCOUNT_CACHE_FAILED_TTL } else { ACCOUNT_CACHE_TTL }
        {
            return Ok(Some(status.clone()));
        }
        let result = self.query_account_status().await;
        if let Ok(mut guard) = self.account_cache.lock() {
            match &result {
                Ok(Some(status)) => *guard = Some((status.clone(), now, false)),
                // 失败（登录态失效/风控/超时）时保留上次成功值并标记失败。
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

pub(crate) fn parse_search_candidates(
    response: &Value,
) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
    let songs = response
        .pointer("/search/data/body/song/list")
        .or_else(|| response.pointer("/search/data/body/songlist/list"))
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
    let info = response
        .pointer("/req_0/data/midurlinfo/0")
        .or_else(|| response.pointer("/req_0/data/midurlinfo"));
    let vip = info
        .and_then(|value| {
            value
                .as_array()
                .and_then(|items| items.first())
                .or(Some(value))
        })
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

fn qq_response_requires_auth_refresh(response: &Value) -> bool {
    ["/req_0/code", "/req/code", "/code"]
        .iter()
        .filter_map(|pointer| response.pointer(pointer).and_then(as_i64))
        .any(|code| matches!(code, 401 | 403 | 1000 | 1001 | 1002 | 1003 | 1004 | 1005))
}

fn lyric_value<'a>(response: &'a Value, field: &str) -> Option<&'a str> {
    response
        .get(field)
        .or_else(|| response.pointer(&format!("/data/{field}")))
        .or_else(|| response.pointer(&format!("/req/data/{field}")))
        .and_then(Value::as_str)
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
    aliases
        .iter()
        .find_map(|alias| cookies.get(*alias).map(String::as_str))
        .filter(|value| !value.is_empty())
}

fn replace_cookie_alias(cookies: &mut BTreeMap<String, String>, aliases: &[&str], value: &str) {
    let key = aliases
        .iter()
        .find(|alias| cookies.contains_key(**alias))
        .copied()
        .unwrap_or(aliases[0]);
    cookies.insert(key.to_owned(), value.to_owned());
}

fn replace_cookie_alias_from_value(
    cookies: &mut BTreeMap<String, String>,
    aliases: &[&str],
    data: &serde_json::Map<String, Value>,
    field: &str,
) {
    let Some(value) = data.get(field).and_then(|value| {
        value
            .as_str()
            .map(str::to_owned)
            .or_else(|| value.as_i64().map(|value| value.to_string()))
    }) else {
        return;
    };
    if !value.is_empty() {
        replace_cookie_alias(cookies, aliases, &value);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct QqQimei {
    q16: String,
    q36: String,
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
    qimei: Option<QqQimei>,
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
fn qimei_payload(device: &QqDevice) -> QimeiRequest {
    let crypt_key: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    let nonce: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(16)
        .map(char::from)
        .collect();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let key = base64::engine::general_purpose::STANDARD.encode(
        RsaPublicKey::from_public_key_pem(QIMEI_PUBLIC_KEY)
            .unwrap()
            .encrypt(
                &mut rand::thread_rng(),
                Pkcs1v15Encrypt,
                crypt_key.as_bytes(),
            )
            .unwrap(),
    );
    let plain = json!({"androidId":device.android_id,"platformId":1,"appKey":QIMEI_APP_KEY,"appVersion":"14.9.0","brand":device.brand,"channelId":"10003505","imei":device.imei,"model":device.model,"osVersion":"Android 10,level 29","qimei":"","qimei36":"","sdkVersion":"1.2.13.6","targetSdkVersion":"33","packageId":"com.tencent.qqmusic"}).to_string();
    let cipher =
        Encryptor::<Aes128>::new_from_slices(crypt_key.as_bytes(), crypt_key.as_bytes()).unwrap();
    let mut buffer = plain.into_bytes();
    let length = buffer.len();
    buffer.resize(length + 16, 0);
    let encrypted = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut buffer, length)
        .unwrap();
    let params = base64::engine::general_purpose::STANDARD.encode(encrypted);
    let extra = format!("{{\"appKey\":\"{QIMEI_APP_KEY}\"}}");
    let sign = md5_hex(&format!(
        "{key}{params}{}{}{}{}",
        timestamp * 1000,
        nonce,
        QIMEI_SECRET,
        extra
    ));
    QimeiRequest {
        timestamp,
        body: json!({"app":0,"os":1,"qimeiParams":{"key":key,"params":params,"time":timestamp.to_string(),"nonce":nonce,"sign":sign,"extra":extra}}),
    }
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
    music_id: u64,
    device: &QqDevice,
    qimei: &QqQimei,
) -> Value {
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
            "tmeLoginType": 2,
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
            "param": {
                "openid": open_id,
                "access_token": access_token,
                "refresh_token": refresh_token,
                "expired_in": 0,
                "musicid": music_id,
                "musickey": music_key,
                "refresh_key": refresh_key,
                "loginMode": 2
            }
        }
    })
}

fn qq_sign(payload: &str) -> String {
    const PART_1: [usize; 7] = [23, 14, 6, 36, 16, 7, 19];
    const PART_2: [usize; 8] = [16, 1, 32, 12, 19, 27, 8, 5];
    const SCRAMBLE: [u8; 20] = [
        89, 39, 179, 150, 218, 82, 58, 252, 177, 52, 186, 123, 120, 64, 242, 133, 143, 161, 121,
        179,
    ];
    let digest = sha1_digest(payload.as_bytes());
    let hex = digest
        .iter()
        .flat_map(|byte| [hex_digit(byte >> 4), hex_digit(byte & 0x0f)])
        .collect::<Vec<_>>();
    let part_1 = PART_1
        .iter()
        .map(|index| hex[*index] as char)
        .collect::<String>();
    let part_2 = PART_2
        .iter()
        .map(|index| hex[*index] as char)
        .collect::<String>();
    let scrambled = SCRAMBLE
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                ^ u8::from_str_radix(&String::from_utf8_lossy(&hex[index * 2..index * 2 + 2]), 16)
                    .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(scrambled)
        .replace(['\\', '/', '+', '='], "")
        .to_lowercase();
    format!("zzc{part_1}{encoded}{part_2}").to_lowercase()
}

fn hex_digit(value: u8) -> u8 {
    match value {
        0..=9 => b'0' + value,
        _ => b'A' + value - 10,
    }
}

fn sha1_digest(input: &[u8]) -> [u8; 20] {
    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    let mut state = [
        0x67452301u32,
        0xefcdab89,
        0x98badcfe,
        0x10325476,
        0xc3d2e1f0,
    ];
    for chunk in message.chunks_exact(64) {
        let mut words = [0u32; 80];
        for (index, word) in words[..16].iter_mut().enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) =
            (state[0], state[1], state[2], state[3], state[4]);
        for (index, word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let temporary = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temporary;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }
    let mut output = [0u8; 20];
    for (index, value) in state.iter().enumerate() {
        output[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    output
}

fn as_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

/// 生成 QQ 音乐 VIP 类型标签（绿钻/SVIP/豪华绿钻/年费等组合）。
fn vip_vip_type_label(identity: &Value, data: &Value) -> String {
    let flag = |name: &str, value: &Value| {
        value
            .get(name)
            .and_then(Value::as_i64)
            .is_some_and(|v| v != 0)
    };
    let mut labels = Vec::new();
    if flag("HugeVip", identity) {
        labels.push("豪华绿钻");
    } else if flag("vip", identity) {
        labels.push("绿钻");
    }
    if flag("svip", data) {
        labels.push("SVIP");
    }
    if flag("star", identity) {
        labels.push("星级");
    }
    if flag("twelve", identity) {
        labels.push("十二平台");
    }
    if flag("yearflag", identity) || flag("HugeYearFlag", identity) {
        labels.push("年费");
    }
    if labels.is_empty() {
        labels.push("VIP");
    }
    labels.join("·")
}

/// 解析 QQ VIP 到期时间（"2026-07-20 07:50:53"）为 epoch 毫秒。
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
    let mut parts = date.split(['-', ' ', ':']).filter_map(|part| part.parse::<u64>().ok());
    let year = parts.next()?;
    let month = parts.next()?;
    let day = parts.next()?;
    let hour = parts.next().unwrap_or(0);
    let minute = parts.next().unwrap_or(0);
    let second = parts.next().unwrap_or(0);
    let days = days_from_civil(year as i64, month as i64, day as i64)?;
    Some(
        (days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64) as u64 * 1000,
    )
}

/// 公历转儒略日（days since 1970-01-01）。
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || day < 1 || day > 31 {
        return None;
    }
    let adjusted_year = if month <= 2 { year - 1 } else { year };
    let era = if adjusted_year >= 0 { adjusted_year } else { adjusted_year - 399 } / 400;
    let year_of_era = adjusted_year - era * 400;
    let month_adjusted = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_adjusted + 2) / 5 + day - 1;
    let day_of_era =
        year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
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
    let info = response
        .pointer("/req_0/data/midurlinfo/0")
        .or_else(|| response.pointer("/req_0/data/midurlinfo"))
        .and_then(|value| {
            value
                .as_array()
                .and_then(|items| items.first())
                .or(Some(value))
        })
        .ok_or_else(|| {
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
        let host = response
            .pointer("/req_0/data/sip/0")
            .and_then(Value::as_str)
            .unwrap_or("https://isure.stream.qqmusic.qq.com/");
        Url::parse(host).and_then(|host| host.join(purl))
    }
    .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?;
    validate_media_url(&url)?;
    Ok(url)
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{
        PlaybackEligibility, QQ_QIMEI_FALLBACK, QqDevice, QqMusicAdapter, QqQimei,
        parse_search_candidates, qq_eligibility, qq_filename, qq_resolver_ids, qq_stream_url,
    };
    use crate::catalog::{CatalogError, SourceAdapter};
    use crate::credentials::{CredentialStore, ProviderCredential};
    use crate::domain::{ResolverLocator, SearchSpec, SongKey};
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
    fn qq_refresh_signature_matches_the_reference_protocol() {
        assert_eq!(
            super::qq_sign(r#"{"a":1,"b":"中文"}"#),
            "zzc8b37edfxsyzq9obzkzbgwezihhqkstxalgecaff800"
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
            42,
            &QqDevice::new(),
            &QqQimei {
                q16: "".into(),
                q36: QQ_QIMEI_FALLBACK.into(),
            },
        );
        assert_eq!(payload["req"]["module"], "music.login.LoginServer");
        assert_eq!(payload["req"]["param"]["refresh_key"], "refresh-key");
        assert_eq!(payload["comm"]["qq"], "42");
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
    }

    #[tokio::test]
    async fn qq_auth_failure_refreshes_once_retries_and_persists_new_cookie_state() {
        let root =
            std::env::temp_dir().join(format!("miliastra-qq-refresh-{}", uuid::Uuid::new_v4()));
        let store = CredentialStore::open(root.clone()).unwrap();
        refresh_credentials(&store);
        let (endpoint, requests) = fixture_server_sequence(vec![
            Some(r#"{"req_0":{"code":1000}}"#.to_owned()),
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
    async fn qq_refresh_network_ambiguity_keeps_the_previous_cookie_state() {
        let store = CredentialStore::memory();
        refresh_credentials(&store);
        let (endpoint, requests) =
            fixture_server_sequence(vec![Some(r#"{"req_0":{"code":1000}}"#.to_owned()), None]);
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
