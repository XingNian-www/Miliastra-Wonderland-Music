use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const HALL_EXPIRING_WARNING_MINUTES: u32 = 10;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct RuntimeState {
    pub playback: PlaybackRuntimeState,
    pub hall_remaining_minutes: Option<u32>,
    pub hall_remaining_updated_at: Option<u64>,
    pub hall_expiring_warning_sent: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PlaybackRuntimeState {
    pub state: ConfirmedPlaybackState,
    pub pause_reason: PauseReason,
    pub active_request: Option<ActivePlaybackRequest>,
    pub last_observation: Option<PlaybackObservation>,
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmedPlaybackState {
    Idle,
    Starting,
    RequestedSongPlaying,
    PausedByUser,
    PausedWaitingForQueue,
    ExternalPlayback,
    Unknown,
}

impl Default for ConfirmedPlaybackState {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    None,
    User,
    WaitingForQueue,
}

impl Default for PauseReason {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationReliability {
    Reliable,
    Incomplete,
    Stale,
    Mismatched,
    Unknown,
}

impl Default for ObservationReliability {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
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
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
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

impl RuntimeState {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("read runtime state {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("parse runtime state {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create runtime state directory {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, text).with_context(|| format!("write runtime state {}", path.display()))
    }

    pub fn update_hall_remaining_minutes(&mut self, minutes: u32) {
        if minutes == 0 {
            self.clear_hall_remaining_minutes();
            return;
        }
        self.hall_remaining_minutes = Some(minutes);
        self.hall_remaining_updated_at = Some(current_unix_seconds());
        if minutes > HALL_EXPIRING_WARNING_MINUTES {
            self.hall_expiring_warning_sent = false;
        }
    }

    pub fn clear_hall_remaining_minutes(&mut self) {
        self.hall_remaining_minutes = None;
        self.hall_remaining_updated_at = None;
        self.hall_expiring_warning_sent = false;
    }

    pub fn clear_hall_countdown_cache(&mut self) -> bool {
        let had_cache = self.hall_remaining_minutes.is_some()
            || self.hall_remaining_updated_at.is_some()
            || self.hall_expiring_warning_sent;
        if had_cache {
            self.clear_hall_remaining_minutes();
        }
        had_cache
    }

    pub fn hall_remaining_minutes_now(&self) -> Option<u32> {
        let minutes = self.hall_remaining_minutes?;
        if minutes == 0 {
            return None;
        }
        let updated_at = self.hall_remaining_updated_at?;
        let elapsed_minutes = current_unix_seconds().saturating_sub(updated_at) / 60;
        Some(minutes.saturating_sub(elapsed_minutes as u32))
    }
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub struct PersistentRuntimeState {
    path: PathBuf,
    state: RuntimeState,
}

impl PersistentRuntimeState {
    pub fn load(path: PathBuf) -> Result<Self> {
        let state = RuntimeState::load(&path)?;
        Ok(Self { path, state })
    }

    pub fn state(&self) -> &RuntimeState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut RuntimeState {
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
    fn clears_hall_countdown_cache_for_new_visual_session() {
        let mut state = RuntimeState {
            hall_remaining_minutes: Some(5),
            hall_remaining_updated_at: Some(123),
            hall_expiring_warning_sent: true,
            ..RuntimeState::default()
        };

        assert!(state.clear_hall_countdown_cache());
        assert_eq!(state.hall_remaining_minutes, None);
        assert_eq!(state.hall_remaining_updated_at, None);
        assert!(!state.hall_expiring_warning_sent);
    }

    #[test]
    fn empty_hall_countdown_cache_is_noop() {
        let mut state = RuntimeState::default();

        assert!(!state.clear_hall_countdown_cache());
    }
}
