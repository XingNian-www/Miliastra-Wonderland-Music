use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use miliastra_playback::PlayableTrack;
use serde::{Deserialize, Deserializer, Serialize};

use super::queue::QueueItem;

/// The sole durable snapshot for a requested-playback session.
///
const REQUEST_STATE_SCHEMA_VERSION: u32 = 2;
const MAX_HISTORY_ENTRIES: usize = 64;

#[derive(Debug)]
struct UnsupportedRequestStateSchema {
    actual: Option<u64>,
    path: PathBuf,
}

impl std::fmt::Display for UnsupportedRequestStateSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "不支持的请求状态 schemaVersion {:?}，请删除状态文件 {} 后重启",
            self.actual,
            self.path.display()
        )
    }
}

impl std::error::Error for UnsupportedRequestStateSchema {}

fn default_history_id() -> u64 {
    1
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PlaybackSessionBinding {
    pub runtime_identity: String,
    pub session_id: String,
    pub generation: u64,
    pub bound_at_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct HandledTerminalOutcome {
    pub request_id: u64,
    pub outcome: String,
    pub handled_at_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct PlaybackAttemptRecord {
    pub request_id: u64,
    pub provider: String,
    pub locator: String,
    pub started_at_ms: u64,
    pub result: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ControlOperationRecord {
    pub operation_id: u64,
    pub operation: String,
    pub requested_at_ms: u64,
    pub completed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionReconciliation {
    NoActiveRequest,
    Bound,
    Match,
    Restarted,
    Replaced,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct RequestStateSnapshot {
    pub schema_version: u32,
    pub next_queue_item_id: u64,
    #[serde(default = "default_history_id")]
    pub next_attempt_id: u64,
    #[serde(default = "default_history_id")]
    pub next_control_operation_id: u64,
    pub queue: Vec<QueueItem>,
    pub playback: PlaybackRuntimeState,
    #[serde(default)]
    pub session_binding: Option<PlaybackSessionBinding>,
    #[serde(default)]
    pub handled_terminal_outcomes: Vec<HandledTerminalOutcome>,
    #[serde(default)]
    pub attempt_history: Vec<PlaybackAttemptRecord>,
    #[serde(default)]
    pub control_operation_history: Vec<ControlOperationRecord>,
}

impl Default for RequestStateSnapshot {
    fn default() -> Self {
        Self {
            schema_version: REQUEST_STATE_SCHEMA_VERSION,
            next_queue_item_id: 1,
            next_attempt_id: 1,
            next_control_operation_id: 1,
            queue: Vec::new(),
            playback: PlaybackRuntimeState::default(),
            session_binding: None,
            handled_terminal_outcomes: Vec::new(),
            attempt_history: Vec::new(),
            control_operation_history: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct RequestStateStore {
    path: PathBuf,
    snapshot: RequestStateSnapshot,
}

pub(crate) type SharedRequestStateStore = Arc<Mutex<RequestStateStore>>;

impl RequestStateStore {
    pub(crate) fn load(path: PathBuf) -> Result<SharedRequestStateStore> {
        let snapshot = if path.exists() {
            Self::read_current_or_recover(&path)?
        } else if backup_path(&path).exists() {
            let snapshot = Self::read_snapshot(&backup_path(&path))?;
            let text = serde_json::to_vec_pretty(&snapshot)?;
            crate::adapters::file_store::write_atomic(&path, &text, "请求状态恢复文件")?;
            log::warn!("请求状态主文件缺失，已从备份恢复: {}", path.display());
            snapshot
        } else {
            RequestStateSnapshot::default()
        };
        let store = Self { path, snapshot };
        Ok(Arc::new(Mutex::new(store)))
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> SharedRequestStateStore {
        Arc::new(Mutex::new(Self {
            path: PathBuf::new(),
            snapshot: RequestStateSnapshot::default(),
        }))
    }

    pub(crate) fn queue_snapshot(&self) -> (u64, Vec<QueueItem>) {
        (
            self.snapshot.next_queue_item_id,
            self.snapshot.queue.clone(),
        )
    }

    pub(crate) fn playback_snapshot(&self) -> PlaybackRuntimeState {
        self.snapshot.playback.clone()
    }

    pub(crate) fn update(
        &mut self,
        mutation: impl FnOnce(&mut RequestStateSnapshot) -> bool,
    ) -> Result<bool> {
        let mut next = self.snapshot.clone();
        if !mutation(&mut next) {
            return Ok(false);
        }
        validate_snapshot(&next)?;
        self.save(&next)?;
        self.snapshot = next;
        Ok(true)
    }

    pub(crate) fn reconcile_player_session(
        &mut self,
        binding: Option<PlaybackSessionBinding>,
    ) -> Result<SessionReconciliation> {
        let active = self.snapshot.playback.active_request.is_some();
        let current = self.snapshot.session_binding.clone();
        let decision = if !active {
            SessionReconciliation::NoActiveRequest
        } else {
            match (&current, &binding) {
                (_, Some(incoming)) if incoming.runtime_identity.trim().is_empty() => {
                    SessionReconciliation::Unknown
                }
                (None, Some(_)) => SessionReconciliation::Bound,
                (Some(existing), Some(incoming))
                    if existing.runtime_identity == incoming.runtime_identity
                        && existing.session_id == incoming.session_id
                        && existing.generation == incoming.generation =>
                {
                    SessionReconciliation::Match
                }
                (Some(existing), Some(incoming))
                    if existing.runtime_identity != incoming.runtime_identity =>
                {
                    SessionReconciliation::Restarted
                }
                (Some(_), Some(_)) => SessionReconciliation::Replaced,
                (_, None) => SessionReconciliation::Unknown,
            }
        };
        let should_replace = matches!(
            decision,
            SessionReconciliation::Bound
                | SessionReconciliation::Restarted
                | SessionReconciliation::Replaced
        );
        self.update(|snapshot| {
            let next = if active && should_replace {
                binding
            } else if !active {
                None
            } else {
                return false;
            };
            if snapshot.session_binding == next {
                return false;
            }
            snapshot.session_binding = next;
            true
        })?;
        Ok(decision)
    }

    pub(crate) fn record_handled_terminal_outcome(
        &mut self,
        outcome: HandledTerminalOutcome,
    ) -> Result<bool> {
        self.update(|snapshot| {
            if snapshot.handled_terminal_outcomes.iter().any(|existing| {
                existing.request_id == outcome.request_id && existing.outcome == outcome.outcome
            }) {
                return false;
            }
            snapshot.handled_terminal_outcomes.push(outcome);
            retain_recent(&mut snapshot.handled_terminal_outcomes);
            true
        })
    }

    pub(crate) fn claim_terminal_outcome(
        &mut self,
        request_id: u64,
        outcome: impl Into<String>,
        handled_at_ms: u64,
    ) -> Result<bool> {
        self.record_handled_terminal_outcome(HandledTerminalOutcome {
            request_id,
            outcome: outcome.into(),
            handled_at_ms,
        })
    }

    pub(crate) fn record_attempt(&mut self, attempt: PlaybackAttemptRecord) -> Result<bool> {
        self.update(|snapshot| {
            let mut attempt = attempt;
            if attempt.request_id == 0 {
                attempt.request_id = snapshot.next_attempt_id;
                snapshot.next_attempt_id = snapshot.next_attempt_id.wrapping_add(1).max(1);
            }
            snapshot.attempt_history.push(attempt);
            retain_recent(&mut snapshot.attempt_history);
            true
        })
    }

    pub(crate) fn record_control_operation(
        &mut self,
        operation: ControlOperationRecord,
    ) -> Result<bool> {
        self.update(|snapshot| {
            let mut operation = operation;
            if operation.operation_id == 0 {
                operation.operation_id = snapshot.next_control_operation_id;
                snapshot.next_control_operation_id =
                    snapshot.next_control_operation_id.wrapping_add(1).max(1);
            }
            snapshot.control_operation_history.push(operation);
            retain_recent(&mut snapshot.control_operation_history);
            true
        })
    }

    fn read_current_or_recover(path: &Path) -> Result<RequestStateSnapshot> {
        match Self::read_snapshot(path) {
            Ok(snapshot) => Ok(snapshot),
            Err(error)
                if error
                    .downcast_ref::<UnsupportedRequestStateSchema>()
                    .is_some() =>
            {
                Err(error)
            }
            Err(primary_error) => {
                let backup = backup_path(path);
                if !backup.exists() {
                    return Err(primary_error);
                }
                let snapshot = Self::read_snapshot(&backup).with_context(|| {
                    format!(
                        "解析请求状态主文件失败且备份也不可恢复: {}",
                        backup.display()
                    )
                })?;
                log::warn!(
                    "请求状态主文件损坏，已从备份恢复: {} ({primary_error:#})",
                    path.display()
                );
                let text = serde_json::to_vec_pretty(&snapshot)?;
                crate::adapters::file_store::write_atomic(path, &text, "请求状态恢复文件")?;
                Ok(snapshot)
            }
        }
    }

    fn read_snapshot(path: &Path) -> Result<RequestStateSnapshot> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read request state {}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("parse request state {}", path.display()))?;
        let schema_version = value
            .get("schemaVersion")
            .and_then(serde_json::Value::as_u64);
        if schema_version != Some(u64::from(REQUEST_STATE_SCHEMA_VERSION)) {
            return Err(UnsupportedRequestStateSchema {
                actual: schema_version,
                path: path.to_path_buf(),
            }
            .into());
        }
        let snapshot: RequestStateSnapshot = serde_json::from_value(value)
            .with_context(|| format!("parse request state {}", path.display()))?;
        validate_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    fn save(&self, snapshot: &RequestStateSnapshot) -> Result<()> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let text = serde_json::to_vec_pretty(snapshot)?;
        if self.path.exists() {
            let current = fs::read(&self.path).with_context(|| {
                format!("read request state before backup {}", self.path.display())
            })?;
            crate::adapters::file_store::write_atomic(
                &backup_path(&self.path),
                &current,
                "请求状态备份",
            )?;
        }
        crate::adapters::file_store::write_atomic(&self.path, &text, "请求状态")
    }
}

fn retain_recent<T>(items: &mut Vec<T>) {
    if items.len() > MAX_HISTORY_ENTRIES {
        items.drain(..items.len() - MAX_HISTORY_ENTRIES);
    }
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("request-state"))
        .to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

fn validate_snapshot(snapshot: &RequestStateSnapshot) -> Result<()> {
    if snapshot.schema_version != REQUEST_STATE_SCHEMA_VERSION {
        bail!(
            "不支持的请求状态 schemaVersion {}，当前仅支持 {}",
            snapshot.schema_version,
            REQUEST_STATE_SCHEMA_VERSION
        );
    }
    if snapshot.next_queue_item_id == 0 {
        bail!("请求状态 nextQueueItemId 必须大于 0");
    }
    if snapshot.next_attempt_id == 0 || snapshot.next_control_operation_id == 0 {
        bail!("请求状态历史 ID 分配器必须大于 0");
    }
    let mut ids = HashSet::new();
    if snapshot
        .queue
        .iter()
        .any(|item| item.id == 0 || !ids.insert(item.id))
    {
        bail!("请求状态 queue 中的 item id 必须唯一且大于 0");
    }
    if snapshot
        .queue
        .iter()
        .map(|item| item.id)
        .max()
        .is_some_and(|max| snapshot.next_queue_item_id <= max)
    {
        bail!("请求状态 nextQueueItemId 必须大于所有 queue item id");
    }
    if snapshot
        .playback
        .active_request
        .iter()
        .chain(snapshot.playback.previous_requests.iter())
        .any(|request| request.track.is_none())
    {
        bail!("请求状态中的播放请求必须包含结构化 track");
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PlaybackRuntimeState {
    pub state: ConfirmedPlaybackState,
    pub pause_reason: PauseReason,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub active_request: Option<ActivePlaybackRequest>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub last_observation: Option<PlaybackObservation>,
    #[serde(default)]
    pub previous_requests: Vec<ActivePlaybackRequest>,
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
            previous_requests: Vec::new(),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<PlayableTrack>,
    pub song: String,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub requester: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<PlayableTrack>,
    pub title: String,
    pub artist: String,
    pub progress: f64,
    pub duration: f64,
    pub captured_at_ms: u64,
    pub reliability: ObservationReliability,
}

impl PlaybackRuntimeState {
    const MAX_PREVIOUS_REQUESTS: usize = 32;
}

pub struct PersistentPlaybackState {
    state: PlaybackRuntimeState,
    request_store: SharedRequestStateStore,
}

impl PersistentPlaybackState {
    pub(crate) fn from_request_store(request_store: SharedRequestStateStore) -> Result<Self> {
        let state = request_store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .playback_snapshot();
        Ok(Self {
            state,
            request_store,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Result<Self> {
        Self::from_request_store(RequestStateStore::new_for_test())
    }

    pub fn state(&self) -> &PlaybackRuntimeState {
        &self.state
    }

    pub(crate) fn update(
        &mut self,
        mutation: impl FnOnce(&mut PlaybackRuntimeState) -> bool,
    ) -> Result<bool> {
        let mut next = self.state.clone();
        let changed = mutation(&mut next);
        if changed {
            let mut store = self
                .request_store
                .lock()
                .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?;
            store.update(|snapshot| {
                if active_request_identity(snapshot.playback.active_request.as_ref())
                    != active_request_identity(next.active_request.as_ref())
                    || next.active_request.is_none()
                {
                    // A binding only authorizes the exact active request which established it.
                    snapshot.session_binding = None;
                }
                snapshot.playback = next.clone();
                true
            })?;
            self.state = next;
        }
        Ok(changed)
    }
}

fn active_request_identity(
    request: Option<&ActivePlaybackRequest>,
) -> Option<(&miliastra_playback::TrackKey, u64)> {
    request.and_then(|request| {
        request
            .track
            .as_ref()
            .map(|track| (&track.track_ref.key, request.started_at_ms))
    })
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

    pub(crate) fn remember_active_request(&mut self) {
        if !matches!(
            self.state,
            ConfirmedPlaybackState::RequestedSongPlaying
                | ConfirmedPlaybackState::PausedByUser
                | ConfirmedPlaybackState::PausedWaitingForQueue
        ) {
            return;
        }
        let Some(active) = self.active_request.clone() else {
            return;
        };
        self.push_previous_request(active);
    }

    pub(crate) fn remember_current_playback(&mut self) {
        if self.active_request.is_some() {
            self.remember_active_request();
            return;
        }
        if self.state != ConfirmedPlaybackState::ExternalPlayback {
            return;
        }
        let Some(observation) = self.last_observation.clone() else {
            return;
        };
        if observation.reliability != ObservationReliability::Reliable {
            return;
        }
        if observation.status != "playing" && observation.status != "paused" {
            return;
        }
        let Some(track) = observation.track else {
            return;
        };
        let request = ActivePlaybackRequest {
            keyword: if observation.title.trim().is_empty() {
                track.track_ref.key.to_string()
            } else if observation.artist.trim().is_empty() {
                observation.title.clone()
            } else {
                format!(
                    "{} - {}",
                    observation.title.trim(),
                    observation.artist.trim()
                )
            },
            source: track.track_ref.key.provider.to_string(),
            prefer_accompaniment: false,
            track: Some(track),
            song: format!("{}{}", observation.title, observation.artist),
            title: observation.title,
            artist: observation.artist,
            requester: String::new(),
            started_at_ms: observation.captured_at_ms,
            guard_started_at: None,
        };
        self.push_previous_request(request);
    }

    pub(crate) fn remove_previous_request(&mut self, request: &ActivePlaybackRequest) {
        let identity = playback_identity(request);
        if identity.is_none() {
            return;
        }
        if self
            .previous_requests
            .last()
            .is_some_and(|previous| playback_identity(previous) == identity)
        {
            self.previous_requests.pop();
            return;
        }
        if let Some(index) = self
            .previous_requests
            .iter()
            .rposition(|previous| playback_identity(previous) == identity)
        {
            self.previous_requests.remove(index);
        }
    }

    fn push_previous_request(&mut self, active: ActivePlaybackRequest) {
        let identity = playback_identity(&active);
        if identity.is_none() {
            return;
        }
        if self
            .previous_requests
            .last()
            .is_some_and(|previous| playback_identity(previous) == identity)
        {
            return;
        }
        self.previous_requests.push(active);
        if self.previous_requests.len() > Self::MAX_PREVIOUS_REQUESTS {
            let excess = self.previous_requests.len() - Self::MAX_PREVIOUS_REQUESTS;
            self.previous_requests.drain(..excess);
        }
    }
}

fn playback_identity(request: &ActivePlaybackRequest) -> Option<&miliastra_playback::TrackKey> {
    request.track.as_ref().map(|track| &track.track_ref.key)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::features::playback::test_track;

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
    fn persisted_state_without_navigation_history_restores_an_empty_history() {
        let restored: PlaybackRuntimeState = serde_json::from_str(
            r#"{
                "state": "idle",
                "pauseReason": "none",
                "activeRequest": null,
                "lastObservation": null
            }"#,
        )
        .expect("older playback state remains readable");

        assert!(restored.previous_requests.is_empty());
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

        assert!(error.to_string().contains("title"));
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

    #[test]
    fn unified_request_state_rejects_unversioned_state_with_delete_hint() {
        let state_path = temp_request_state_path("legacy-migration");
        fs::write(
            &state_path,
            r#"{"state":"idle","pauseReason":"none","activeRequest":null,"lastObservation":null}"#,
        )
        .unwrap();

        let error = RequestStateStore::load(state_path.clone())
            .expect_err("unversioned state must not be migrated");

        assert!(error.to_string().contains("schemaVersion"));
        assert!(error.to_string().contains("请删除状态文件"));

        remove_request_state_path(state_path);
    }

    #[test]
    fn unified_request_state_rejects_v1_with_delete_hint() {
        let state_path = temp_request_state_path("v1-rejected");
        let mut value = serde_json::to_value(RequestStateSnapshot::default()).unwrap();
        value["schemaVersion"] = serde_json::json!(1);
        fs::write(&state_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let error =
            RequestStateStore::load(state_path.clone()).expect_err("v1 state must not be migrated");

        assert!(error.to_string().contains("schemaVersion Some(1)"));
        assert!(error.to_string().contains("请删除状态文件"));
        remove_request_state_path(state_path);
    }

    #[test]
    fn unsupported_primary_schema_never_falls_back_to_a_valid_backup() {
        let state_path = temp_request_state_path("v1-with-v2-backup");
        let mut legacy = serde_json::to_value(RequestStateSnapshot::default()).unwrap();
        legacy["schemaVersion"] = serde_json::json!(1);
        fs::write(&state_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        fs::write(
            backup_path(&state_path),
            serde_json::to_vec_pretty(&RequestStateSnapshot::default()).unwrap(),
        )
        .unwrap();

        let error = RequestStateStore::load(state_path.clone())
            .expect_err("unsupported primary schema must not use the backup");

        assert!(error.to_string().contains("schemaVersion Some(1)"));
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(persisted["schemaVersion"], 1);
        remove_request_state_path(state_path);
    }

    #[test]
    fn unified_request_state_rejects_a_stale_queue_id_allocator() {
        let state_path = temp_request_state_path("stale-queue-id");
        let mut snapshot = RequestStateSnapshot::default();
        snapshot.queue.push(QueueItem {
            id: 1,
            keyword: "歌名".to_string(),
            ..QueueItem::default()
        });
        snapshot.next_queue_item_id = 1;
        fs::write(&state_path, serde_json::to_vec_pretty(&snapshot).unwrap()).unwrap();

        let error = RequestStateStore::load(state_path.clone())
            .expect_err("queue ids must be valid in schema v2");

        assert!(error.to_string().contains("nextQueueItemId"));
        remove_request_state_path(state_path);
    }

    #[test]
    fn queue_and_playback_updates_share_one_versioned_snapshot() {
        let state_path = temp_request_state_path("shared-snapshot");
        let store = RequestStateStore::load(state_path.clone()).unwrap();
        let mut queue =
            super::super::queue::PersistentQueue::from_request_store(store.clone(), 3).unwrap();
        let mut playback = PersistentPlaybackState::from_request_store(store).unwrap();

        queue
            .push(QueueItem {
                keyword: "非 VIP 歌曲".to_string(),
                ..QueueItem::default()
            })
            .unwrap();
        playback
            .update(|state| {
                state.set_user_paused();
                true
            })
            .unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
        assert_eq!(json["schemaVersion"], REQUEST_STATE_SCHEMA_VERSION);
        assert_eq!(json["queue"].as_array().unwrap().len(), 1);
        assert_eq!(json["playback"]["state"], "paused_by_user");

        remove_request_state_path(state_path);
    }

    #[test]
    fn corrupted_primary_recovers_the_last_valid_backup() {
        let state_path = temp_request_state_path("backup-recovery");
        let store = RequestStateStore::load(state_path.clone()).unwrap();
        store
            .lock()
            .unwrap()
            .record_control_operation(ControlOperationRecord {
                operation_id: 7,
                operation: "pause".to_string(),
                requested_at_ms: 42,
                completed: true,
            })
            .unwrap();
        // A second write retains the first valid version as .bak.
        store
            .lock()
            .unwrap()
            .record_control_operation(ControlOperationRecord {
                operation_id: 8,
                operation: "resume".to_string(),
                requested_at_ms: 43,
                completed: true,
            })
            .unwrap();
        fs::write(&state_path, "{broken").unwrap();

        let recovered = RequestStateStore::load(state_path.clone()).unwrap();
        let recovered = recovered.lock().unwrap();
        assert_eq!(recovered.snapshot.control_operation_history.len(), 1);
        assert_eq!(
            recovered.snapshot.control_operation_history[0].operation,
            "pause"
        );
        assert!(
            fs::read_to_string(&state_path)
                .unwrap()
                .contains("\"schemaVersion\"")
        );

        remove_request_state_path(state_path);
    }

    #[test]
    fn terminal_claim_is_idempotent_and_session_reconciliation_is_durable() {
        let state_path = temp_request_state_path("terminal-session");
        let store = RequestStateStore::load(state_path.clone()).unwrap();
        store
            .lock()
            .unwrap()
            .update(|snapshot| {
                snapshot.playback.active_request = Some(ActivePlaybackRequest {
                    track: Some(test_track(
                        "miliastra://track/qqmusic/session-track",
                        "会话歌曲 - 测试歌手",
                    )),
                    ..ActivePlaybackRequest::default()
                });
                true
            })
            .unwrap();
        let binding = PlaybackSessionBinding {
            runtime_identity: "native-runtime-A".to_string(),
            session_id: "session-1".to_string(),
            generation: 3,
            bound_at_ms: 9,
        };
        assert_eq!(
            store
                .lock()
                .unwrap()
                .reconcile_player_session(Some(binding.clone()))
                .unwrap(),
            SessionReconciliation::Bound
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .reconcile_player_session(Some(binding))
                .unwrap(),
            SessionReconciliation::Match
        );
        assert!(
            store
                .lock()
                .unwrap()
                .claim_terminal_outcome(11, "natural_end", 10)
                .unwrap()
        );
        assert!(
            !store
                .lock()
                .unwrap()
                .claim_terminal_outcome(11, "natural_end", 11)
                .unwrap()
        );

        let mut playback = PersistentPlaybackState::from_request_store(store.clone()).unwrap();
        playback
            .update(|state| {
                state.clear_active_request();
                true
            })
            .unwrap();

        let restored = RequestStateStore::load(state_path.clone()).unwrap();
        let restored = restored.lock().unwrap();
        assert!(restored.snapshot.session_binding.is_none());
        assert_eq!(restored.snapshot.handled_terminal_outcomes.len(), 1);

        remove_request_state_path(state_path);
    }

    fn temp_request_state_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "miliastra-request-state-test-{}-{}-{}",
            std::process::id(),
            name,
            nanos
        ));
        root.with_extension("state.json")
    }

    fn remove_request_state_path(state_path: PathBuf) {
        let _ = fs::remove_file(&state_path);
        let _ = fs::remove_file(backup_path(&state_path));
    }
}
