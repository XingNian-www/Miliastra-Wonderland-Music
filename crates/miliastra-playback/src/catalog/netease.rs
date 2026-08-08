//! Minimal native NetEase Cloud Music adapter.

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

const SEARCH_URL: &str = "https://music.163.com/api/search/get";
const PROVIDER: &str = "netease";
const MEDIA_URL: &str = "https://music.163.com/api/song/enhance/player/url/v1";
const LYRICS_URL: &str = "https://music.163.com/song/lyric";

#[derive(Clone)]
pub struct NeteaseAdapter {
    client: Client,
    credentials: CredentialStore,
    search_url: Url,
    media_url: Url,
    lyrics_url: Url,
}

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
        let response = self
            .client
            .get(self.media_url.clone())
            .query(&[
                ("ids", format!("[{id}]")),
                ("level", "standard".to_owned()),
                ("encodeType", "mp3".to_owned()),
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
        let (url, expiry_seconds) = netease_media_url(&response)?;
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
        (Some(1), _, _) => PlaybackEligibility::Unknown,
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
            PlaybackEligibility::Unknown
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
