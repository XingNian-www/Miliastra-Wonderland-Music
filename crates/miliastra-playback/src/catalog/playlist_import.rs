use std::time::Duration;

use reqwest::{Client, redirect::Policy};
use url::Url;

use crate::catalog::CatalogError;
use crate::model::PlayableTrack;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedPlaylist {
    pub provider: String,
    pub id: String,
    pub title: String,
    pub creator: Option<String>,
    pub tracks: Vec<PlayableTrack>,
}

pub(crate) async fn resolve_share_url(input: &str, provider: &str) -> Result<String, CatalogError> {
    if input.chars().all(|ch| ch.is_ascii_digit()) && !input.is_empty() {
        return Ok(input.to_owned());
    }
    let mut url = Url::parse(input.trim())
        .map_err(|_| CatalogError::InvalidResponse("歌单链接格式无效".to_owned()))?;
    let allowed = match provider {
        "qqmusic" => &["y.qq.com", "i.y.qq.com", "c6.y.qq.com"][..],
        "netease" => &["music.163.com", "y.music.163.com", "163cn.tv"][..],
        _ => return Err(CatalogError::UnknownSource(provider.to_owned())),
    };
    if url.scheme() != "https"
        || url.port().is_some_and(|port| port != 443)
        || !url.username().is_empty()
        || url.password().is_some()
        || !host_allowed(&url, allowed)
    {
        return Err(CatalogError::InvalidResponse(
            "只支持对应音乐平台的 HTTPS 歌单链接".to_owned(),
        ));
    }
    if let Some(id) = playlist_id_from_url(&url, provider) {
        return Ok(id);
    }
    let short_host = match provider {
        "qqmusic" => "c6.y.qq.com",
        "netease" => "163cn.tv",
        _ => unreachable!(),
    };
    if url.host_str() != Some(short_host) {
        return Err(CatalogError::InvalidResponse(
            "链接不是受支持的歌单".to_owned(),
        ));
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent("Mozilla/5.0")
        .redirect(Policy::none())
        .build()
        .map_err(|_| CatalogError::Transient("无法初始化歌单链接读取器".to_owned()))?;
    for _ in 0..5 {
        let response = client
            .get(url.clone())
            .header("Referer", provider_referer(provider))
            .send()
            .await
            .map_err(|_| CatalogError::Transient("歌单短链接读取失败".to_owned()))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| CatalogError::InvalidResponse("歌单短链接跳转无效".to_owned()))?;
            url = url
                .join(location)
                .map_err(|_| CatalogError::InvalidResponse("歌单短链接跳转无效".to_owned()))?;
            if url.scheme() != "https"
                || url.port().is_some_and(|port| port != 443)
                || !url.username().is_empty()
                || url.password().is_some()
                || !host_allowed(&url, allowed)
            {
                return Err(CatalogError::InvalidResponse(
                    "歌单短链接跳转到不支持的地址".to_owned(),
                ));
            }
            if let Some(id) = playlist_id_from_url(&url, provider) {
                return Ok(id);
            }
            continue;
        }
        return Err(CatalogError::InvalidResponse(
            "短链接没有指向受支持的歌单".to_owned(),
        ));
    }
    Err(CatalogError::InvalidResponse(
        "歌单短链接跳转次数过多".to_owned(),
    ))
}

pub(crate) async fn response_json(
    mut response: reqwest::Response,
    provider: &str,
) -> Result<serde_json::Value, CatalogError> {
    const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(CatalogError::InvalidResponse(format!(
            "{provider}歌单响应过大"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| CatalogError::Transient(format!("{provider}歌单响应读取失败")))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(CatalogError::InvalidResponse(format!(
                "{provider}歌单响应过大"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|_| CatalogError::InvalidResponse(format!("{provider}歌单响应格式无效")))
}

pub(crate) fn playlist_id_from_url(url: &Url, provider: &str) -> Option<String> {
    let mut candidates = vec![url.clone()];
    if provider == "netease" {
        if let Some(fragment) = url.fragment() {
            let fragment = fragment.trim_start_matches('/');
            if let Ok(fragment_url) = Url::parse(&format!("https://music.163.com/{fragment}")) {
                candidates.push(fragment_url);
            }
        }
    }
    candidates.iter().find_map(|candidate| match provider {
        "qqmusic" => {
            let path = candidate.path();
            let marker = [
                "/n/ryqq_v2/playlist/",
                "/n/ryqq/playlist/",
                "/n/yqq/playlist/",
            ]
            .iter()
            .find_map(|marker| path.split_once(marker).map(|(_, id)| id))
            .and_then(|id| id.strip_suffix(".html").or(Some(id)));
            marker
                .filter(|id| !id.is_empty() && id.chars().all(|ch| ch.is_ascii_digit()))
                .map(str::to_owned)
                .or_else(|| {
                    if !candidate.path().contains("/n2/m/share/details/taoge.html") {
                        return None;
                    }
                    candidate
                        .query_pairs()
                        .find(|(key, value)| key == "id" && !value.is_empty())
                        .map(|(_, value)| value.into_owned())
                        .filter(|id| id.chars().all(|ch| ch.is_ascii_digit()))
                })
        }
        "netease" => {
            if !candidate.path().ends_with("/playlist") {
                return None;
            }
            candidate
                .query_pairs()
                .find(|(key, value)| key == "id" && !value.is_empty())
                .map(|(_, value)| value.into_owned())
                .filter(|id| id.chars().all(|ch| ch.is_ascii_digit()))
        }
        _ => None,
    })
}

fn host_allowed(url: &Url, allowed: &[&str]) -> bool {
    url.host_str().is_some_and(|host| allowed.contains(&host))
}

fn provider_referer(provider: &str) -> &'static str {
    match provider {
        "qqmusic" => "https://y.qq.com/",
        "netease" => "https://music.163.com/",
        _ => "https://example.invalid/",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qq_and_netease_playlist_ids_from_supported_share_shapes() {
        for (url, provider, id) in [
            (
                "https://y.qq.com/n/ryqq_v2/playlist/8162013709",
                "qqmusic",
                "8162013709",
            ),
            (
                "https://y.qq.com/n/ryqq/playlist/8162013709",
                "qqmusic",
                "8162013709",
            ),
            (
                "https://y.qq.com/n/yqq/playlist/8162013709.html",
                "qqmusic",
                "8162013709",
            ),
            (
                "https://i.y.qq.com/n2/m/share/details/taoge.html?id=8162013709",
                "qqmusic",
                "8162013709",
            ),
            (
                "https://music.163.com/playlist?id=6787973700&uct2=ignored",
                "netease",
                "6787973700",
            ),
            (
                "https://music.163.com/#/playlist?id=6787973700",
                "netease",
                "6787973700",
            ),
            (
                "https://y.music.163.com/m/playlist?id=6787973700&userid=42",
                "netease",
                "6787973700",
            ),
        ] {
            let parsed = Url::parse(url).unwrap();
            assert_eq!(playlist_id_from_url(&parsed, provider).as_deref(), Some(id));
        }
    }

    #[tokio::test]
    async fn accepts_numeric_ids_and_rejects_untrusted_urls() {
        assert_eq!(
            resolve_share_url("8162013709", "qqmusic").await.unwrap(),
            "8162013709"
        );
        assert_eq!(
            resolve_share_url("6787973700", "netease").await.unwrap(),
            "6787973700"
        );
        assert!(
            resolve_share_url("http://y.qq.com/n/ryqq_v2/playlist/123", "qqmusic")
                .await
                .is_err()
        );
        assert!(
            resolve_share_url("https://example.com/?id=123", "netease")
                .await
                .is_err()
        );
        assert!(
            resolve_share_url("https://music.163.com:444/playlist?id=123", "netease")
                .await
                .is_err()
        );
        assert!(
            resolve_share_url("https://user@music.163.com/playlist?id=123", "netease")
                .await
                .is_err()
        );
        assert!(
            playlist_id_from_url(&Url::parse("https://y.qq.com/?id=123").unwrap(), "qqmusic")
                .is_none()
        );
    }
}
