use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const HALL_EXPIRING_WARNING_MINUTES: u32 = 10;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct RuntimeState {
    pub current_song_is_requested: bool,
    pub last_requested_uri: String,
    pub last_requested_song: String,
    pub last_requested_keyword: String,
    pub last_requested_source: String,
    pub last_requested_prefer_accompaniment: bool,
    pub last_requested_updated_at_ms: u64,
    pub paused_by_command: bool,
    pub paused_for_pending_playback: bool,
    pub hall_remaining_minutes: Option<u32>,
    pub hall_remaining_updated_at: Option<u64>,
    pub hall_expiring_warning_sent: bool,
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
