use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PlaybackRuntimeState {
    pub state: ConfirmedPlaybackState,
    pub pause_reason: PauseReason,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub active_request: Option<ActivePlaybackRequest>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub last_observation: Option<PlaybackObservation>,
}

fn deserialize_required_option<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

impl Default for PlaybackRuntimeState {
    fn default() -> Self {
        Self {
            state: ConfirmedPlaybackState::Idle,
            pause_reason: PauseReason::None,
            active_request: None,
            last_observation: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmedPlaybackState {
    #[default]
    Idle,
    Starting,
    RequestedSongPlaying,
    PausedByUser,
    PausedWaitingForQueue,
    ExternalPlayback,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    #[default]
    None,
    User,
    WaitingForQueue,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationReliability {
    Reliable,
    Incomplete,
    Stale,
    Mismatched,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ActivePlaybackRequest {
    pub keyword: String,
    pub source: String,
    pub prefer_accompaniment: bool,
    pub requested_uri: String,
    pub confirmed_uri: String,
    pub song: String,
    pub title: String,
    pub artist: String,
    pub started_at_ms: u64,
    /// Runtime-only monotonic anchor for the short playback-start guard.
    ///
    /// Persisted wall-clock metadata must never be used to judge a business deadline. A restored
    /// request therefore has no guard and is reconciled from a fresh player observation.
    #[serde(skip)]
    pub(crate) guard_started_at: Option<Instant>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PlaybackObservation {
    pub status: String,
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub progress: f64,
    pub duration: f64,
    pub captured_at_ms: u64,
    pub reliability: ObservationReliability,
}

impl PlaybackRuntimeState {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("read playback state {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parse playback state {}", path.display()))
    }

    fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        crate::adapters::file_store::write_atomic(path, text.as_bytes(), "播放状态")
    }
}

pub struct PersistentPlaybackState {
    path: PathBuf,
    state: PlaybackRuntimeState,
}

impl PersistentPlaybackState {
    pub fn load(path: PathBuf) -> Result<Self> {
        let state = PlaybackRuntimeState::load(&path)?;
        Ok(Self { path, state })
    }

    pub fn state(&self) -> &PlaybackRuntimeState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut PlaybackRuntimeState {
        &mut self.state
    }

    pub fn save(&self) -> Result<()> {
        self.state.save(&self.path)
    }
}

impl PlaybackRuntimeState {
    pub fn clear_active_request(&mut self) {
        self.state = ConfirmedPlaybackState::Idle;
        self.pause_reason = PauseReason::None;
        self.active_request = None;
    }

    pub fn set_user_paused(&mut self) {
        self.state = ConfirmedPlaybackState::PausedByUser;
        self.pause_reason = PauseReason::User;
    }

    pub fn set_user_resumed(&mut self) {
        self.pause_reason = PauseReason::None;
        self.state = if self.active_request.is_some() {
            ConfirmedPlaybackState::RequestedSongPlaying
        } else {
            ConfirmedPlaybackState::ExternalPlayback
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_state_rejects_unknown_fields() {
        let error = serde_json::from_str::<PlaybackRuntimeState>(r#"{"unknown":true}"#)
            .expect_err("playback state must use the current schema");

        assert!(error.to_string().contains("unknown"));
    }

    #[test]
    fn persisted_playback_state_requires_all_current_fields() {
        let error = serde_json::from_str::<PlaybackRuntimeState>(
            r#"{
                "state": "idle",
                "pauseReason": "none",
                "activeRequest": null
            }"#,
        )
        .expect_err("persisted playback state must not infer missing current fields");

        assert!(error.to_string().contains("lastObservation"));
    }

    #[test]
    fn persisted_active_request_requires_all_current_fields() {
        let error = serde_json::from_str::<PlaybackRuntimeState>(
            r#"{
                "state": "starting",
                "pauseReason": "none",
                "activeRequest": {"keyword": "歌名"},
                "lastObservation": null
            }"#,
        )
        .expect_err("persisted active request must not infer missing current fields");

        assert!(error.to_string().contains("source"));
    }

    #[test]
    fn persisted_player_observation_requires_all_current_fields() {
        let error = serde_json::from_str::<PlaybackRuntimeState>(
            r#"{
                "state": "external_playback",
                "pauseReason": "none",
                "activeRequest": null,
                "lastObservation": {"status": "playing"}
            }"#,
        )
        .expect_err("persisted player observation must not infer missing current fields");

        assert!(error.to_string().contains("uri"));
    }

    #[test]
    fn monotonic_playback_guard_is_never_persisted_or_reconstructed() {
        let state = PlaybackRuntimeState {
            state: ConfirmedPlaybackState::Starting,
            active_request: Some(ActivePlaybackRequest {
                started_at_ms: 42_000,
                guard_started_at: Some(Instant::now()),
                ..ActivePlaybackRequest::default()
            }),
            ..PlaybackRuntimeState::default()
        };

        let json = serde_json::to_string(&state).expect("serialize playback state");
        assert!(!json.contains("guardStartedAt"));
        let restored: PlaybackRuntimeState =
            serde_json::from_str(&json).expect("restore playback state");
        let request = restored.active_request.expect("active request");

        assert_eq!(request.started_at_ms, 42_000);
        assert_eq!(request.guard_started_at, None);
    }
}
