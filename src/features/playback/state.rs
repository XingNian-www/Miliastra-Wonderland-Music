use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use miliastra_playback::{PlayableTrack, TrackKey};
use serde::{Deserialize, Deserializer, Serialize};

use super::queue::QueueItem;
use miliastra_contracts::StateStore;

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
    /// 已确认播放过的歌曲，队列播完后随机播放；按 TrackKey 去重，持久化。
    #[serde(default)]
    pub playback_pool: Vec<PlayableTrack>,
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
            playback_pool: Vec::new(),
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
    store: Arc<dyn StateStore>,
}

pub(crate) type SharedRequestStateStore = Arc<Mutex<RequestStateStore>>;

impl RequestStateStore {
    pub(crate) fn load(
        path: PathBuf,
        store: Arc<dyn StateStore>,
    ) -> Result<SharedRequestStateStore> {
        let snapshot = if store.exists(&path) {
            Self::read_current_or_recover(&path, store.as_ref())?
        } else if store.exists(&backup_path(&path)) {
            let snapshot = Self::read_snapshot(&backup_path(&path), store.as_ref())?;
            let text = serde_json::to_vec_pretty(&snapshot)?;
            store.write_atomic(&path, &text, "请求状态恢复文件")?;
            log::warn!("请求状态主文件缺失，已从备份恢复: {}", path.display());
            snapshot
        } else {
            RequestStateSnapshot::default()
        };
        let request_store = Self {
            path,
            snapshot,
            store,
        };
        Ok(Arc::new(Mutex::new(request_store)))
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> SharedRequestStateStore {
        Arc::new(Mutex::new(Self {
            path: PathBuf::new(),
            snapshot: RequestStateSnapshot::default(),
            store: crate::test_support::test_state_store(),
        }))
    }

    pub(crate) fn queue_snapshot(&self) -> (u64, Vec<QueueItem>) {
        (
            self.snapshot.next_queue_item_id,
            self.snapshot.queue.clone(),
        )
    }

    pub(crate) fn playback_pool_snapshot(&self) -> Vec<PlayableTrack> {
        self.snapshot.playback_pool.clone()
    }

    /// 把已确认播放的歌曲写入播放池；同 TrackKey 已在池中时忽略，不重复进入。
    /// `max_size == 0` 表示播放池禁用，直接忽略。超过上限时淘汰最旧一首。
    pub(crate) fn record_pool_track(
        &mut self,
        track: PlayableTrack,
        max_size: usize,
    ) -> Result<bool> {
        if max_size == 0 {
            return Ok(false);
        }
        self.update(|snapshot| {
            let pool = &mut snapshot.playback_pool;
            if pool
                .iter()
                .any(|existing| existing.track_ref.key == track.track_ref.key)
            {
                return false;
            }
            if pool.len() >= max_size {
                pool.remove(0);
            }
            pool.push(track);
            true
        })
    }

    /// 从播放池随机挑一首，排除 `exclude` 指定的 TrackKey；池空或全部被排除时返回 None。
    pub(crate) fn pick_pool_track(&self, exclude: Option<&TrackKey>) -> Option<PlayableTrack> {
        let pool = &self.snapshot.playback_pool;
        let candidates = pool
            .iter()
            .filter(|track| exclude != Some(&track.track_ref.key))
            .collect::<Vec<_>>();
        let count = candidates.len();
        if count == 0 {
            return None;
        }
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos() as usize);
        Some(candidates[seed % count].clone())
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

    fn read_current_or_recover(
        path: &Path,
        store: &dyn StateStore,
    ) -> Result<RequestStateSnapshot> {
        match Self::read_snapshot(path, store) {
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
                if !store.exists(&backup) {
                    return Err(primary_error);
                }
                let snapshot = Self::read_snapshot(&backup, store).with_context(|| {
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
                store.write_atomic(path, &text, "请求状态恢复文件")?;
                Ok(snapshot)
            }
        }
    }

    fn read_snapshot(path: &Path, store: &dyn StateStore) -> Result<RequestStateSnapshot> {
        let text = store
            .read_to_string(path)
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
        if self.store.exists(&self.path) {
            let current = self.store.read(&self.path).with_context(|| {
                format!("read request state before backup {}", self.path.display())
            })?;
            self.store
                .write_atomic(&backup_path(&self.path), &current, "请求状态备份")?;
        }
        self.store.write_atomic(&self.path, &text, "请求状态")
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
    ExternalPlayback,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    #[default]
    None,
    User,
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
            ConfirmedPlaybackState::RequestedSongPlaying | ConfirmedPlaybackState::PausedByUser
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

        let error =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
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
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .expect_err("v1 state must not be migrated");

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

        let error =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
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

        let error =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .expect_err("queue ids must be valid in schema v2");

        assert!(error.to_string().contains("nextQueueItemId"));
        remove_request_state_path(state_path);
    }

    #[test]
    fn queue_and_playback_updates_share_one_versioned_snapshot() {
        let state_path = temp_request_state_path("shared-snapshot");
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
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
    fn playback_pool_deduplicates_by_track_key_and_respects_max_size() {
        let state_path = temp_request_state_path("playback-pool");
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        {
            let mut store = store.lock().unwrap();
            assert!(store
                .record_pool_track(
                    test_track("miliastra://track/qqmusic/1", "歌一 - 歌手A"),
                    3,
                )
                .unwrap());
            // 同 TrackKey 再次写入被忽略，不重复进入。
            assert!(!store
                .record_pool_track(
                    test_track("miliastra://track/qqmusic/1", "歌一 - 歌手A"),
                    3,
                )
                .unwrap());
            assert_eq!(store.playback_pool_snapshot().len(), 1);
            store
                .record_pool_track(test_track("miliastra://track/qqmusic/2", "歌二 - 歌手B"), 3)
                .unwrap();
            store
                .record_pool_track(test_track("miliastra://track/qqmusic/3", "歌三 - 歌手C"), 3)
                .unwrap();
            // 达到上限后新歌淘汰最旧一首。
            store
                .record_pool_track(test_track("miliastra://track/qqmusic/4", "歌四 - 歌手D"), 3)
                .unwrap();
            let snapshot = store.playback_pool_snapshot();
            let keys = snapshot
                .iter()
                .map(|track| track.track_ref.key.id.as_str())
                .collect::<Vec<_>>();
            assert_eq!(keys, ["2", "3", "4"]);
            // max_size == 0 表示禁用，直接忽略。
            assert!(!store
                .record_pool_track(
                    test_track("miliastra://track/qqmusic/5", "歌五 - 歌手E"),
                    0,
                )
                .unwrap());
        }
        remove_request_state_path(state_path);
    }

    #[test]
    fn playback_pool_pick_excludes_the_requested_key() {
        let state_path = temp_request_state_path("playback-pool-pick");
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        {
            let mut store = store.lock().unwrap();
            for id in ["1", "2", "3"] {
                store
                    .record_pool_track(
                        test_track(
                            &format!("miliastra://track/qqmusic/{id}"),
                            &format!("歌{id} - 歌手{id}"),
                        ),
                        10,
                    )
                    .unwrap();
            }
            let picked = store.pick_pool_track(None).expect("pool has candidates");
            assert_ne!(picked.track_ref.key.id, "");
            let re_picked = store
                .pick_pool_track(Some(&picked.track_ref.key))
                .expect("excluding one key still leaves candidates");
            assert_ne!(re_picked.track_ref.key.id, picked.track_ref.key.id);
            // 排除不在池中的 key 仍能选到候选。
            let last_key = store
                .playback_pool_snapshot()
                .iter()
                .find(|track| {
                    track.track_ref.key.id != picked.track_ref.key.id
                        && track.track_ref.key.id != re_picked.track_ref.key.id
                })
                .unwrap()
                .track_ref
                .key
                .clone();
            assert!(store.pick_pool_track(Some(&last_key)).is_some());
            // 空池返回 None。
            store
                .update(|snapshot| {
                    snapshot.playback_pool.clear();
                    true
                })
                .unwrap();
            assert!(store.pick_pool_track(None).is_none());
        }
        remove_request_state_path(state_path);
    }

    #[test]
    fn corrupted_primary_recovers_the_last_valid_backup() {
        let state_path = temp_request_state_path("backup-recovery");
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
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

        let recovered =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
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
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
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

        let restored =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
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
