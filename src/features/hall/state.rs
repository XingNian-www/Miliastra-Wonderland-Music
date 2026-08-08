use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use miliastra_contracts::StateStore;
use serde::{Deserialize, Deserializer, Serialize};

use super::HALL_EXPIRING_WARNING_MINUTES;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct HallRuntimeState {
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) remaining_minutes: Option<u32>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub(crate) remaining_updated_at: Option<u64>,
    pub(crate) expiring_warning_sent: bool,
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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HallStatePatch {
    pub(crate) remaining_minutes: Option<Option<u32>>,
    pub(crate) remaining_updated_at: Option<Option<u64>>,
    pub(crate) expiring_warning_sent: Option<bool>,
}

impl HallRuntimeState {
    pub(crate) fn update_remaining_minutes(&mut self, minutes: u32, updated_at: u64) {
        if minutes == 0 {
            self.clear_remaining_minutes();
            return;
        }
        self.remaining_minutes = Some(minutes);
        self.remaining_updated_at = Some(updated_at);
        if minutes > HALL_EXPIRING_WARNING_MINUTES {
            self.expiring_warning_sent = false;
        }
    }

    pub(crate) fn clear_remaining_minutes(&mut self) {
        self.remaining_minutes = None;
        self.remaining_updated_at = None;
        self.expiring_warning_sent = false;
    }

    pub(crate) fn clear_countdown_cache(&mut self) -> bool {
        let had_cache = self.remaining_minutes.is_some()
            || self.remaining_updated_at.is_some()
            || self.expiring_warning_sent;
        if had_cache {
            self.clear_remaining_minutes();
        }
        had_cache
    }

    pub(crate) fn remaining_minutes_now(&self) -> Option<u32> {
        self.remaining_minutes
    }

    pub(crate) fn apply_patch(&mut self, patch: HallStatePatch) {
        if let Some(value) = patch.remaining_minutes {
            self.remaining_minutes = value;
        }
        if let Some(value) = patch.remaining_updated_at {
            self.remaining_updated_at = value;
        }
        if let Some(value) = patch.expiring_warning_sent {
            self.expiring_warning_sent = value;
        }
    }

    fn load(path: &Path, store: &dyn StateStore) -> Result<Self> {
        if !store.exists(path) {
            return Ok(Self::default());
        }
        let text = store
            .read_to_string(path)
            .with_context(|| format!("read hall state {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parse hall state {}", path.display()))
    }

    fn save(&self, path: &Path, store: &dyn StateStore) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        store.write_atomic(path, text.as_bytes(), "大厅状态")
    }
}

pub(crate) struct PersistentHallState {
    path: PathBuf,
    state: HallRuntimeState,
    store: Arc<dyn StateStore>,
}

impl PersistentHallState {
    pub(crate) fn load(path: PathBuf, store: Arc<dyn StateStore>) -> Result<Self> {
        let state = HallRuntimeState::load(&path, store.as_ref())?;
        Ok(Self { path, state, store })
    }

    pub(crate) fn state(&self) -> &HallRuntimeState {
        &self.state
    }

    pub(crate) fn update(
        &mut self,
        mutation: impl FnOnce(&mut HallRuntimeState) -> bool,
    ) -> Result<bool> {
        let mut next = self.state.clone();
        let changed = mutation(&mut next);
        if changed {
            next.save(&self.path, self.store.as_ref())?;
            self.state = next;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clears_countdown_cache_for_a_new_visual_session() {
        let mut state = HallRuntimeState {
            remaining_minutes: Some(5),
            remaining_updated_at: Some(123),
            expiring_warning_sent: true,
        };

        assert!(state.clear_countdown_cache());
        assert_eq!(state, HallRuntimeState::default());
    }

    #[test]
    fn empty_countdown_cache_is_a_noop() {
        let mut state = HallRuntimeState::default();

        assert!(!state.clear_countdown_cache());
    }

    #[test]
    fn persisted_hall_state_rejects_unknown_fields() {
        let error = serde_json::from_str::<HallRuntimeState>(r#"{"unknown":true}"#)
            .expect_err("hall state must use the current schema");

        assert!(error.to_string().contains("unknown"));
    }

    #[test]
    fn persisted_hall_state_requires_all_current_fields() {
        let error = serde_json::from_str::<HallRuntimeState>(
            r#"{
                "remainingMinutes": null,
                "remainingUpdatedAt": null
            }"#,
        )
        .expect_err("persisted hall state must not infer missing current fields");

        assert!(error.to_string().contains("expiringWarningSent"));
    }

    #[test]
    fn persisted_hall_state_requires_explicit_optional_fields() {
        let error = serde_json::from_str::<HallRuntimeState>(
            r#"{
                "remainingMinutes": null,
                "expiringWarningSent": false
            }"#,
        )
        .expect_err("persisted hall state must include optional current fields explicitly");

        assert!(error.to_string().contains("remainingUpdatedAt"));
    }
}
