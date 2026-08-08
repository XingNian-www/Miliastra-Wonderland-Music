use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::catalog::{PlaybackEligibility, ProviderId};
use crate::domain::{ResolverLocator, Song, SongKey};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrackKey {
    pub provider: ProviderId,
    pub id: String,
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
}

fn format_candidate_text(metadata: &TrackMetadata, provider: ProviderId) -> String {
    let artists = if metadata.artists.is_empty() {
        "未知歌手".to_owned()
    } else {
        metadata.artists.join(" / ")
    };
    format!("{} - {} [{}]", metadata.title, artists, provider.as_str())
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
