use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

const SONG_KEY_PREFIX: &str = "miliastra://track/";
pub const MAX_RESOLVER_LOCATOR_BYTES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl Failure {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        let retryable = matches!(
            code.as_str(),
            "provider_rate_limited" | "provider_timeout" | "provider_transient"
        );
        Self {
            code,
            message: message.into(),
            retryable,
            provider: None,
            retry_after_ms: None,
        }
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn with_retry_after_ms(mut self, retry_after_ms: u64) -> Self {
        self.retry_after_ms = Some(retry_after_ms);
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct SongKey {
    pub source: String,
    pub id: String,
}

impl SongKey {
    pub fn new(source: impl Into<String>, id: impl Into<String>) -> Result<Self, SongKeyError> {
        let key = Self {
            source: source.into(),
            id: id.into(),
        };
        key.validate()?;
        Ok(key)
    }

    fn validate(&self) -> Result<(), SongKeyError> {
        if self.source.is_empty() || self.id.is_empty() {
            return Err(SongKeyError::MissingPart);
        }
        if self.source.contains('/') || self.id.contains('/') {
            return Err(SongKeyError::InvalidSeparator);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for SongKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct SongKeyParts {
            source: String,
            id: String,
        }

        let parts = SongKeyParts::deserialize(deserializer)?;
        Self::new(parts.source, parts.id).map_err(serde::de::Error::custom)
    }
}

impl Display for SongKey {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{SONG_KEY_PREFIX}{}/{}", self.source, self.id)
    }
}

impl FromStr for SongKey {
    type Err = SongKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value
            .strip_prefix(SONG_KEY_PREFIX)
            .ok_or(SongKeyError::InvalidScheme)?;
        let (source, id) = value.split_once('/').ok_or(SongKeyError::InvalidShape)?;
        Self::new(source, id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SongKeyError {
    #[error("song key must start with miliastra://track/")]
    InvalidScheme,
    #[error("song key must have the form miliastra://track/<source>/<id>")]
    InvalidShape,
    #[error("song source and id must not be empty")]
    MissingPart,
    #[error("song source and id must not contain slash separators")]
    InvalidSeparator,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ResolverLocator(String);

impl ResolverLocator {
    pub fn new(value: impl Into<String>) -> Result<Self, ResolverLocatorError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ResolverLocatorError::Empty);
        }
        if value.len() > MAX_RESOLVER_LOCATOR_BYTES {
            return Err(ResolverLocatorError::TooLarge {
                max_bytes: MAX_RESOLVER_LOCATOR_BYTES,
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ResolverLocator {
    type Error = ResolverLocatorError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ResolverLocator> for String {
    fn from(locator: ResolverLocator) -> Self {
        locator.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolverLocatorError {
    #[error("resolver locator must not be empty")]
    Empty,
    #[error("resolver locator exceeds the {max_bytes}-byte limit")]
    TooLarge { max_bytes: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Song {
    pub key: SongKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver_locator: Option<ResolverLocator>,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchSpec {
    pub keyword: String,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
}

fn default_search_limit() -> usize {
    10
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamSource {
    pub url: Url,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub expires_at_epoch_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRef {
    pub session_id: Uuid,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndBehavior {
    Stop,
    RepeatCurrent,
    #[default]
    NotifyController,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    #[default]
    Idle,
    Resolving,
    Loading,
    Playing,
    Paused,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndCause {
    NaturalEnd,
    Replaced,
    StoppedByController,
    StreamRejected,
    DecodeFailure,
    RecoveryPositionUnknown,
    EngineExited,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackSnapshot {
    /// Unique for the lifetime of one playerd process. Session references from
    /// another runtime must never be treated as valid after a restart.
    #[serde(default)]
    pub runtime_identity: String,
    pub generation: u64,
    pub session_id: Option<Uuid>,
    pub state: EngineState,
    pub song_key: Option<SongKey>,
    pub end_behavior: Option<EndBehavior>,
    pub position_seconds: Option<f64>,
    pub duration_seconds: Option<f64>,
    pub volume: u8,
    pub last_end_cause: Option<EndCause>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            runtime_identity: String::new(),
            generation: 0,
            session_id: None,
            state: EngineState::Idle,
            song_key: None,
            end_behavior: None,
            position_seconds: None,
            duration_seconds: None,
            volume: 100,
            last_end_cause: None,
            error: None,
            failure: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{MAX_RESOLVER_LOCATOR_BYTES, ResolverLocator, ResolverLocatorError, SongKey};

    #[test]
    fn song_key_round_trips_the_native_uri_contract() {
        let key = SongKey::from_str("miliastra://track/tx/0039MnYb0qxYhV").unwrap();

        assert_eq!(key.source, "tx");
        assert_eq!(key.id, "0039MnYb0qxYhV");
        assert_eq!(key.to_string(), "miliastra://track/tx/0039MnYb0qxYhV");
    }

    #[test]
    fn song_key_rejects_foreign_or_malformed_uris() {
        assert!(SongKey::from_str("legacy://song/1").is_err());
        assert!(SongKey::from_str("https://example.test/song.mp3").is_err());
        assert!(SongKey::from_str("miliastra://track/tx/").is_err());
    }

    #[test]
    fn resolver_locator_is_opaque_and_size_bounded() {
        let locator = ResolverLocator::new("provider-v1:opaque-data").unwrap();
        assert_eq!(locator.as_str(), "provider-v1:opaque-data");

        assert_eq!(
            ResolverLocator::new("x".repeat(MAX_RESOLVER_LOCATOR_BYTES + 1)),
            Err(ResolverLocatorError::TooLarge {
                max_bytes: MAX_RESOLVER_LOCATOR_BYTES,
            })
        );
    }
}
