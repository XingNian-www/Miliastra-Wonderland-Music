use std::fmt::{Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize};

use crate::catalog::{PlaybackEligibility, ProviderId};
use crate::domain::{ResolverLocator, Song, SongKey};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackKey {
    pub provider: ProviderId,
    pub id: String,
}

impl<'de> Deserialize<'de> for TrackKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct TrackKeyWire {
            provider: ProviderId,
            id: String,
        }

        let wire = TrackKeyWire::deserialize(deserializer)?;
        Self::new(wire.provider, wire.id).map_err(serde::de::Error::custom)
    }
}

impl TrackKey {
    pub fn new(provider: ProviderId, id: impl Into<String>) -> Result<Self, TrackKeyError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(TrackKeyError::EmptyId);
        }
        if id.contains('/') {
            return Err(TrackKeyError::InvalidSeparator);
        }
        Ok(Self { provider, id })
    }

    pub(crate) fn to_song_key(&self) -> Result<SongKey, TrackKeyError> {
        SongKey::new(self.provider.as_str(), self.id.clone())
            .map_err(|_| TrackKeyError::InvalidSeparator)
    }
}

impl Display for TrackKey {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "miliastra://track/{}/{}",
            self.provider.as_str(),
            self.id
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TrackKeyError {
    #[error("track id must not be empty")]
    EmptyId,
    #[error("track id must not contain slash separators")]
    InvalidSeparator,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackRef {
    pub key: TrackKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver_locator: Option<ResolverLocator>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackMetadata {
    pub title: String,
    pub artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlayableTrack {
    pub track_ref: TrackRef,
    pub metadata: TrackMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SearchCandidate {
    pub track_ref: TrackRef,
    pub metadata: TrackMetadata,
    pub eligibility: PlaybackEligibility,
    pub text: String,
}

impl SearchCandidate {
    pub(crate) fn from_song(
        song: Song,
        eligibility: PlaybackEligibility,
    ) -> Result<Self, TrackKeyError> {
        let provider = song
            .key
            .source
            .parse::<ProviderId>()
            .map_err(|_| TrackKeyError::InvalidSeparator)?;
        let key = TrackKey::new(provider, song.key.id)?;
        let metadata = TrackMetadata {
            title: song.title,
            artists: song.artists,
            album: song.album,
            duration_ms: song.duration_ms,
        };
        let text = format_candidate_text(&metadata, provider);
        Ok(Self {
            track_ref: TrackRef {
                key,
                resolver_locator: song.resolver_locator,
            },
            metadata,
            eligibility,
            text,
        })
    }

    pub fn playable_track(&self) -> PlayableTrack {
        PlayableTrack {
            track_ref: self.track_ref.clone(),
            metadata: self.metadata.clone(),
        }
    }

    pub fn selection_text(&self) -> String {
        let artists = if self.metadata.artists.is_empty() {
            "未知歌手".to_owned()
        } else {
            self.metadata.artists.join(" / ")
        };
        let duration = self
            .metadata
            .duration_ms
            .map(format_duration)
            .map(|value| format!(" [{value}]"))
            .unwrap_or_default();
        format!("{} - {}{}", self.metadata.title, artists, duration)
    }

    pub fn select_preferred_equivalent(candidates: &[Self]) -> Option<Self> {
        let first = candidates.first()?;
        let identity = comparable_identity(&first.text);
        candidates
            .iter()
            .filter(|candidate| comparable_identity(&candidate.text) == identity)
            .max_by_key(|candidate| candidate.eligibility.preference_rank())
            .cloned()
    }
}

fn comparable_identity(text: &str) -> String {
    text.trim()
        .rsplit_once('[')
        .map_or(
            text,
            |(prefix, suffix)| {
                if suffix.ends_with(']') { prefix } else { text }
            },
        )
        .trim()
        .to_ascii_lowercase()
}

fn format_candidate_text(metadata: &TrackMetadata, provider: ProviderId) -> String {
    let artists = if metadata.artists.is_empty() {
        "未知歌手".to_owned()
    } else {
        metadata.artists.join(" / ")
    };
    let source = metadata.duration_ms.map(format_duration).map_or_else(
        || format!("[{}]", provider_label(provider)),
        |duration| format!("[{} {}]", provider_label(provider), duration),
    );
    format!("{} - {} {}", metadata.title, artists, source)
}

fn format_duration(duration_ms: u64) -> String {
    let total_seconds = duration_ms / 1_000;
    format!("{:02}:{:02}", total_seconds / 60, total_seconds % 60)
}

/// 点歌展示用的平台简化中文标识。
fn provider_label(provider: ProviderId) -> &'static str {
    match provider {
        ProviderId::QqMusic => "QQ",
        ProviderId::Netease => "网易",
        ProviderId::Bilibili => "B站",
        ProviderId::Kugou => "酷狗",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchQuery {
    pub keyword: String,
    pub providers: Vec<ProviderId>,
    pub limit: usize,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            keyword: String::new(),
            providers: Vec::new(),
            limit: 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_key_deserialization_enforces_constructor_invariants() {
        for json in [
            r#"{"provider":"qqmusic","id":""}"#,
            r#"{"provider":"netease","id":"   "}"#,
            r#"{"provider":"bilibili","id":"part/one"}"#,
            r#"{"provider":"qqmusic","id":"valid","extra":true}"#,
        ] {
            assert!(serde_json::from_str::<TrackKey>(json).is_err(), "{json}");
        }

        let key = serde_json::from_str::<TrackKey>(r#"{"provider":"qqmusic","id":"valid-track"}"#)
            .expect("valid track key");
        assert_eq!(
            key,
            TrackKey::new(ProviderId::QqMusic, "valid-track").unwrap()
        );
    }

    #[test]
    fn nested_playable_track_rejects_an_invalid_track_key() {
        let json = r#"{
            "trackRef":{"key":{"provider":"qqmusic","id":"bad/id"}},
            "metadata":{"title":"test","artists":["artist"]}
        }"#;

        assert!(serde_json::from_str::<PlayableTrack>(json).is_err());
    }

    #[test]
    fn candidate_text_includes_duration_when_available() {
        let song = Song {
            key: SongKey::new("qqmusic", "test").unwrap(),
            resolver_locator: None,
            title: "晴天".to_string(),
            artists: vec!["周杰伦".to_string()],
            album: None,
            duration_ms: Some(209_000),
        };

        let candidate = SearchCandidate::from_song(song, PlaybackEligibility::Eligible).unwrap();

        assert_eq!(candidate.text, "晴天 - 周杰伦 [QQ 03:29]");
    }

    #[test]
    fn candidate_text_omits_missing_duration() {
        let song = Song {
            key: SongKey::new("qqmusic", "test").unwrap(),
            resolver_locator: None,
            title: "晴天".to_string(),
            artists: vec!["周杰伦".to_string()],
            album: None,
            duration_ms: None,
        };

        let candidate = SearchCandidate::from_song(song, PlaybackEligibility::Eligible).unwrap();

        assert_eq!(candidate.text, "晴天 - 周杰伦 [QQ]");
    }
}
