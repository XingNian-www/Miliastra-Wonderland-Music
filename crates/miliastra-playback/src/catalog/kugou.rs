//! 酷狗音乐公开接口适配器。

use std::collections::BTreeMap;
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

#[derive(Clone, Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KugouListenReport {
    pub accepted: bool,
    pub vip_days: u32,
    pub message: String,
}

#[derive(Clone)]
pub struct KugouAdapter {
    client: Client,
    credentials: CredentialStore,
    api_base_url: Url,
}

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
        Ok(ProviderCredential::Kugou {
            token: new_token,
            userid: new_userid,
            dfid: new_dfid,
            cookies,
        })
    }

    /// 查询账户与 VIP 状态。仅返回脱敏状态。
    pub async fn account_status(&self) -> Result<KugouAccountStatus, CatalogError> {
        let credential = self.credential()?;
        let detail = self.account_json("/user/detail", &credential).await?;
        let vip = self
            .account_json("/user/vip/detail", &credential)
            .await
            .unwrap_or(Value::Null);
        let detail_data = response_data(&detail);
        let vip_data = response_data(&vip);
        let user_id = detail_data.get("userid").and_then(value_string);
        let logged_in = user_id.is_some();
        Ok(KugouAccountStatus {
            logged_in,
            user_id,
            nickname: detail_data
                .get("nickname")
                .and_then(Value::as_str)
                .map(str::to_owned),
            vip: vip_data
                .get("is_vip")
                .and_then(value_u64)
                .is_some_and(|v| v > 0)
                || vip_data
                    .get("vip")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            vip_type: vip_data.get("vip_type").and_then(value_string),
            vip_expire_at_ms: vip_data
                .get("expire_time")
                .and_then(value_u64)
                .map(|v| if v < 10_000_000_000 { v * 1000 } else { v }),
            listen_report_available: logged_in,
        })
    }

    /// 上报听歌行为，领取酷狗青春版 VIP 权益。
    pub async fn report_listen_song(
        &self,
        mixsongid: &str,
    ) -> Result<KugouListenReport, CatalogError> {
        if !mixsongid
            .chars()
            .all(|character| character.is_ascii_digit())
            || mixsongid.is_empty()
        {
            return Err(CatalogError::InvalidResponse(
                "Kugou mixsongid is invalid".to_owned(),
            ));
        }
        let credential = self.credential()?;
        let value = self
            .account_post_json(
                "/youth/v2/report/listen_song",
                vec![("clientver", "10566".to_owned())],
                serde_json::json!({"mixsongid": mixsongid}),
                &credential,
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
                .unwrap_or("酷狗已接受听歌上报")
                .to_owned(),
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

    async fn account_post_json(
        &self,
        path: &str,
        query: Vec<(&str, String)>,
        body: Value,
        credential: &ProviderCredential,
    ) -> Result<Value, CatalogError> {
        let (token, userid, dfid) = Self::credential_fields(credential)?;
        let url = self.api_url(path)?;
        let response = self
            .client
            .post(url)
            .query(&query)
            .query(&[("token", token), ("userid", userid), ("dfid", dfid)])
            .header("Cookie", Self::credential_cookie_header(credential))
            .header(
                "User-Agent",
                "Android13-1070-10566-201-0-ReportPlaySongToServerProtocol-wifi",
            )
            .header("Content-Type", "application/json; charset=utf-8")
            .json(&body)
            .send()
            .await
            .map_err(classify_request_error)?;
        classify_status(&response)?;
        response
            .json::<Value>()
            .await
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
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
    async fn report_listen_song(&self, mixsongid: &str) -> Result<KugouListenReport, CatalogError> {
        self.report_listen_song(mixsongid).await
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
        let url = response
            .pointer("/data/url")
            .or_else(|| response.pointer("/data/play_url"))
            .or_else(|| response.pointer("/data/play_backup_url"))
            .or_else(|| response.get("url"))
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .ok_or_else(|| {
                CatalogError::Unavailable("Kugou returned no playable stream".to_owned())
            })?;
        Ok(StreamSource {
            url: Url::parse(url)
                .map_err(|error| CatalogError::InvalidResponse(error.to_string()))?,
            headers: BTreeMap::from([("Referer".to_owned(), "https://www.kugou.com/".to_owned())]),
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
        parse_lrc_pair(&lyric, None)
            .map_err(|error| CatalogError::InvalidResponse(error.to_string()))
    }
}

fn parse_search_candidates(response: &Value) -> Result<Vec<ProviderSearchCandidate>, CatalogError> {
    let songs = response
        .pointer("/data/info")
        .or_else(|| response.pointer("/data/records"))
        .or_else(|| response.pointer("/data/song_list"))
        .and_then(Value::as_array)
        .ok_or_else(|| CatalogError::InvalidResponse("Kugou search list is missing".to_owned()))?;
    Ok(songs
        .iter()
        .filter_map(|raw| {
            let hash = raw.get("hash").and_then(Value::as_str)?.trim();
            let title = raw
                .get("songname")
                .or_else(|| raw.get("song_name"))
                .or_else(|| raw.get("filename"))
                .and_then(Value::as_str)?
                .trim();
            if hash.is_empty() || title.is_empty() {
                return None;
            }
            let album_id = raw
                .get("album_id")
                .and_then(value_string)
                .unwrap_or_else(|| "0".to_owned());
            let album_audio_id = raw
                .get("album_audio_id")
                .or_else(|| raw.get("mixsongid"))
                .and_then(value_string)
                .unwrap_or_else(|| "0".to_owned());
            let artists = raw
                .get("singername")
                .or_else(|| raw.get("singer_name"))
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
                .get("duration")
                .or_else(|| raw.get("duration_ms"))
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
                    .get("album_name")
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .map(str::to_owned),
                duration_ms,
            };
            Some(ProviderSearchCandidate {
                song,
                eligibility: PlaybackEligibility::Unknown,
            })
        })
        .collect::<Vec<_>>())
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

    use super::{PROVIDER, decode_lrc_content, parse_search_candidates, resolver_parts};
    use crate::catalog::CatalogError;
    use crate::credentials::ProviderCredential;
    use crate::domain::{ResolverLocator, SongKey};

    #[test]
    fn search_preserves_kugou_playback_and_vip_identifiers() {
        let parsed = parse_search_candidates(&json!({"data": {"info": [{
            "hash": "ABCDEF0123456789",
            "songname": "测试歌曲",
            "singername": "歌手甲、歌手乙",
            "album_id": 42,
            "album_audio_id": 314159,
            "album_name": "测试专辑",
            "duration": 180
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
    fn search_accepts_records_shape_and_mixsongid_alias() {
        let parsed = parse_search_candidates(&json!({"data": {"records": [{
            "hash": "0123456789ABCDEF",
            "song_name": "另一首歌",
            "singer_name": "歌手",
            "album_id": "7",
            "mixsongid": "88",
            "duration_ms": 180500
        }]}}))
        .unwrap();

        assert_eq!(parsed[0].song.duration_ms, Some(180_500));
        assert_eq!(
            parsed[0].song.resolver_locator.as_ref().unwrap().as_str(),
            "kugou:0123456789ABCDEF:7:88"
        );
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
}
