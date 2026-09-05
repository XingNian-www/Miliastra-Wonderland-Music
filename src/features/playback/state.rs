use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use miliastra_playback::{PlayableTrack, TrackKey};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Deserializer, Serialize};

use super::PlaybackStateUpdate;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ActivePlaybackIdentity {
    pub(crate) track_key: TrackKey,
    pub(crate) started_at_ms: u64,
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
    Idle,
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

/// 请求状态持久化存储：单行 SQLite 表保存整个快照 JSON blob，每次更新事务原子提交。
#[derive(Debug)]
pub(crate) struct RequestStateStore {
    snapshot: RequestStateSnapshot,
    connection: rusqlite::Connection,
}

pub(crate) type SharedRequestStateStore = Arc<Mutex<RequestStateStore>>;

impl RequestStateStore {
    /// 打开（必要时创建）SQLite 状态数据库并加载内存快照。
    ///
    /// `store` 参数仅为维持上层 API 稳定而保留：SQLite 模式不使用文件端口，
    /// `path` 直接视为数据库文件路径。数据库已存在但结构不匹配（含旧版 JSON 文件）
    /// 时明确失败，不做迁移。
    pub(crate) fn load(
        path: PathBuf,
        _store: Arc<dyn StateStore>,
    ) -> Result<SharedRequestStateStore> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建请求状态数据库目录失败: {}", parent.display()))?;
        }
        let connection = rusqlite::Connection::open(&path)
            .with_context(|| format!("打开请求状态数据库失败: {}", path.display()))?;
        Self::initialize_connection(&connection).with_context(|| {
            format!(
                "初始化请求状态数据库失败（文件可能不是 SQLite 数据库）: {}",
                path.display()
            )
        })?;
        Self::ensure_schema(&connection, &path)?;
        let snapshot = match Self::read_snapshot_row(&connection)? {
            Some((stored_version, text)) => {
                let value: serde_json::Value = serde_json::from_str(&text)
                    .with_context(|| format!("解析请求状态快照失败: {}", path.display()))?;
                let schema_version = value
                    .get("schemaVersion")
                    .and_then(serde_json::Value::as_u64);
                if schema_version != Some(u64::from(REQUEST_STATE_SCHEMA_VERSION)) {
                    return Err(UnsupportedRequestStateSchema {
                        actual: schema_version,
                        path: path.clone(),
                    }
                    .into());
                }
                if Some(stored_version) != schema_version {
                    bail!(
                        "请求状态数据库 schema_version 列与快照内容不一致，请删除 {} 后重启",
                        path.display()
                    );
                }
                let snapshot: RequestStateSnapshot = serde_json::from_value(value)
                    .with_context(|| format!("解析请求状态快照失败: {}", path.display()))?;
                validate_snapshot(&snapshot)?;
                snapshot
            }
            None => RequestStateSnapshot::default(),
        };
        Ok(Arc::new(Mutex::new(Self {
            snapshot,
            connection,
        })))
    }

    /// 设置连接参数：WAL、synchronous=NORMAL、foreign_keys=ON、busy_timeout=5000。
    fn initialize_connection(connection: &rusqlite::Connection) -> Result<()> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "busy_timeout", 5_000i64)?;
        Ok(())
    }

    /// 校验或创建 request_state 单行表。统一数据库允许缓存表与请求状态表共存；
    /// request_state 已存在但列结构不匹配时明确失败，不做迁移。
    fn ensure_schema(connection: &rusqlite::Connection, path: &Path) -> Result<()> {
        const EXPECTED_COLUMNS: [&str; 3] = ["id", "schema_version", "snapshot"];
        let tables = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            )?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if tables.iter().any(|name| name == "request_state") {
            let columns = connection
                .prepare("PRAGMA table_info(request_state)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if columns.len() != EXPECTED_COLUMNS.len()
                || columns
                    .iter()
                    .any(|column| !EXPECTED_COLUMNS.contains(&column.as_str()))
            {
                bail!(
                    "请求状态数据库表结构不匹配，请删除 {} 后重启",
                    path.display()
                );
            }
            return Ok(());
        }
        connection.execute_batch(
            "CREATE TABLE request_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                snapshot TEXT NOT NULL
            )",
        )?;
        Ok(())
    }

    /// 读取单行快照：无行返回 None，此时按全新状态处理。
    fn read_snapshot_row(connection: &rusqlite::Connection) -> Result<Option<(u64, String)>> {
        let row = connection
            .query_row(
                "SELECT schema_version, snapshot FROM request_state WHERE id = 1",
                [],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> SharedRequestStateStore {
        let connection = rusqlite::Connection::open_in_memory().expect("打开测试内存数据库失败");
        Self::initialize_connection(&connection).expect("初始化测试数据库失败");
        Self::ensure_schema(&connection, Path::new(":memory:")).expect("创建测试表失败");
        Arc::new(Mutex::new(Self {
            snapshot: RequestStateSnapshot::default(),
            connection,
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

    /// 从播放池删除指定歌曲(删除歌曲功能:该曲不再参与随机播放);返回是否实际移除。
    pub(crate) fn remove_pool_track(&mut self, key: &TrackKey) -> Result<bool> {
        self.update(|snapshot| {
            let before = snapshot.playback_pool.len();
            snapshot
                .playback_pool
                .retain(|track| track.track_ref.key != *key);
            snapshot.playback_pool.len() < before
        })
    }

    /// 从播放池随机挑一首，排除本轮已尝试的 TrackKey；池空或全部被排除时返回 None。
    pub(crate) fn pick_pool_track(&self, excluded: &HashSet<TrackKey>) -> Option<PlayableTrack> {
        let pool = &self.snapshot.playback_pool;
        let candidates = pool
            .iter()
            .filter(|track| !excluded.contains(&track.track_ref.key))
            .collect::<Vec<_>>();
        let count = candidates.len();
        if count == 0 {
            return None;
        }
        let index = (uuid::Uuid::new_v4().as_u128() % count as u128) as usize;
        Some(candidates[index].clone())
    }

    pub(crate) fn playback_snapshot(&self) -> PlaybackRuntimeState {
        self.snapshot.playback.clone()
    }

    /// Records an observation only while the exact active request that produced the monitor
    /// snapshot is still current. The identity check and durable update share one state-store
    /// transaction, so a newer request cannot be overwritten between a controller snapshot and
    /// observation persistence.
    pub(crate) fn record_observation_if_active(
        &mut self,
        expected: &ActivePlaybackIdentity,
        observation: PlaybackObservation,
        immediate: bool,
    ) -> Result<bool> {
        let mut active_matched = false;
        self.update(|snapshot| {
            if active_request_identity(snapshot.playback.active_request.as_ref()).as_ref()
                != Some(expected)
            {
                return false;
            }
            active_matched = true;
            let update = if immediate {
                PlaybackStateUpdate::ImmediateObservation(observation)
            } else {
                PlaybackStateUpdate::Observation(observation)
            };
            update.apply(&mut snapshot.playback)
        })?;
        Ok(active_matched)
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
        let decision = self.inspect_player_session(binding.as_ref());
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

    pub(crate) fn inspect_player_session(
        &self,
        binding: Option<&PlaybackSessionBinding>,
    ) -> SessionReconciliation {
        player_session_reconciliation(
            self.snapshot.playback.active_request.is_some(),
            self.snapshot.session_binding.as_ref(),
            binding,
        )
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

    /// 原子确认播放成功并删除对应队列项（同一笔持久化）。
    ///
    /// 把「确认播放成功」与「队首出队」合并为单次快照写入：进程在确认成功后、
    /// 出队持久化前退出时，重启后要么两者都已生效（已确认消费的队首不会重播），
    /// 要么都未生效（队首保留等待重新播放），不存在「已确认但队首未删」的中间态。
    /// 仅在 `queue_item_id` 与当前队首匹配时出队；不匹配（队列已被外部修改，
    /// 如用户删除/清空）时只确认播放状态，不误删其他队列项。
    pub(crate) fn confirm_playback_and_dequeue(
        &mut self,
        update: PlaybackStateUpdate,
        queue_item_id: Option<u64>,
    ) -> Result<bool> {
        self.update(|snapshot| {
            // 与 PersistentPlaybackState::update 保持一致：请求身份变化时作废会话绑定。
            // identity 取拥有的数据（TrackKey 克隆），避免借用阻塞 apply 的可变借用。
            let previous_identity = snapshot
                .playback
                .active_request
                .as_ref()
                .and_then(|request| {
                    request
                        .track
                        .as_ref()
                        .map(|track| (track.track_ref.key.clone(), request.started_at_ms))
                });
            if !update.apply(&mut snapshot.playback) {
                return false;
            }
            let next_identity = snapshot
                .playback
                .active_request
                .as_ref()
                .and_then(|request| {
                    request
                        .track
                        .as_ref()
                        .map(|track| (track.track_ref.key.clone(), request.started_at_ms))
                });
            if previous_identity != next_identity || snapshot.playback.active_request.is_none() {
                snapshot.session_binding = None;
            }
            if let Some(item_id) = queue_item_id
                && snapshot
                    .queue
                    .first()
                    .is_some_and(|item| item.id == item_id)
            {
                snapshot.queue.remove(0);
            }
            true
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

    /// 单行快照原子提交：整张快照序列化为 JSON blob 后在一个事务内写入，
    /// 失败时整体回滚，不产生部分写入。
    fn save(&mut self, snapshot: &RequestStateSnapshot) -> Result<()> {
        let text = serde_json::to_string(snapshot)?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT OR REPLACE INTO request_state (id, schema_version, snapshot) VALUES (1, ?1, ?2)",
            rusqlite::params![i64::from(snapshot.schema_version), text],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// 仅测试：在 request_state 表上注入 BEFORE INSERT 触发器，
    /// 使后续任何快照写入失败并整体回滚（模拟写盘故障）。
    #[cfg(test)]
    pub(crate) fn inject_write_failure(&self) -> Result<()> {
        self.connection.execute_batch(
            "CREATE TRIGGER inject_write_failure BEFORE INSERT ON request_state
             BEGIN SELECT RAISE(ABORT, '注入的写盘故障'); END",
        )?;
        Ok(())
    }
}

fn player_session_reconciliation(
    active: bool,
    current: Option<&PlaybackSessionBinding>,
    incoming: Option<&PlaybackSessionBinding>,
) -> SessionReconciliation {
    if !active {
        return SessionReconciliation::Idle;
    }
    match (current, incoming) {
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
}

fn retain_recent<T>(items: &mut Vec<T>) {
    if items.len() > MAX_HISTORY_ENTRIES {
        items.drain(..items.len() - MAX_HISTORY_ENTRIES);
    }
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
    if snapshot.playback.volume > 100 {
        bail!("请求状态播放音量必须在 0-100 之间");
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
    /// 持久化音量（0-100）：成功设置音量后写入，重启恢复播放前应用。
    #[serde(default = "default_volume")]
    pub volume: u8,
    /// 歌词是否显示翻译：成功切换歌词后写入，重启后引擎按该模式加载歌词。
    #[serde(default = "default_use_translation")]
    pub use_translation: bool,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub active_request: Option<ActivePlaybackRequest>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub last_observation: Option<PlaybackObservation>,
    #[serde(default)]
    pub previous_requests: Vec<ActivePlaybackRequest>,
}

/// 默认音量 100：新装用户与旧版快照（无该字段）都以最大音量启动。
fn default_volume() -> u8 {
    100
}

/// 默认使用翻译歌词：新装用户与旧版快照（无该字段）都启用翻译。
fn default_use_translation() -> bool {
    true
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
            volume: default_volume(),
            use_translation: default_use_translation(),
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
    /// 播放确认时观测到的引擎会话归属，用于失败分支校验：
    /// 推进队列后引擎状态更新有延迟，旧会话的失败残留不得触发再次推进（防连跳）。
    #[serde(skip)]
    pub(crate) expected_session_id: String,
    #[serde(skip)]
    pub(crate) expected_generation: u64,
}

impl ActivePlaybackRequest {
    pub(crate) fn identity(&self) -> Option<ActivePlaybackIdentity> {
        self.track.as_ref().map(|track| ActivePlaybackIdentity {
            track_key: track.track_ref.key.clone(),
            started_at_ms: self.started_at_ms,
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
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

    /// 用共享请求状态存储中的最新播放状态覆盖内存缓存。
    /// 供共享存储内的原子事务（如确认+出队）落盘后同步缓存使用。
    pub(crate) fn sync_state(&mut self, state: PlaybackRuntimeState) {
        self.state = state;
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

    pub(crate) fn record_observation_if_active(
        &mut self,
        expected: &ActivePlaybackIdentity,
        observation: PlaybackObservation,
        immediate: bool,
    ) -> Result<bool> {
        let mut store = self
            .request_store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?;
        let active_matched =
            store.record_observation_if_active(expected, observation, immediate)?;
        self.state = store.playback_snapshot();
        Ok(active_matched)
    }
}

fn active_request_identity(
    request: Option<&ActivePlaybackRequest>,
) -> Option<ActivePlaybackIdentity> {
    request.and_then(ActivePlaybackRequest::identity)
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
            expected_session_id: String::new(),
            expected_generation: 0,
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
    use crate::features::playback::{
        ConfirmedPlaybackState, PersistentQueue, PlaybackNavigation, PlaybackStateUpdate,
        test_track,
    };

    /// 构造 Confirmed 状态更新（与 Starting 使用同一 started_at_ms，保持请求身份不变）。
    fn confirmed_update(
        track: &miliastra_playback::PlayableTrack,
        started_at_ms: u64,
    ) -> PlaybackStateUpdate {
        PlaybackStateUpdate::Confirmed {
            request: ActivePlaybackRequest {
                keyword: track.metadata.title.clone(),
                source: track.track_ref.key.provider.as_str().to_string(),
                track: Some(track.clone()),
                song: track.metadata.title.clone(),
                title: track.metadata.title.clone(),
                artist: track.metadata.artists.join("/"),
                started_at_ms,
                ..ActivePlaybackRequest::default()
            },
            navigation: PlaybackNavigation::Normal,
        }
    }

    fn enter_starting(
        store: &SharedRequestStateStore,
        track: &miliastra_playback::PlayableTrack,
        started_at_ms: u64,
    ) {
        store
            .lock()
            .unwrap()
            .update(|snapshot| {
                snapshot.playback.state = ConfirmedPlaybackState::Starting;
                snapshot.playback.active_request = Some(ActivePlaybackRequest {
                    track: Some(track.clone()),
                    started_at_ms,
                    ..ActivePlaybackRequest::default()
                });
                true
            })
            .unwrap();
    }

    #[test]
    fn confirm_and_dequeue_commit_together_and_survive_reload() {
        let state_path = temp_request_state_path("confirm-dequeue-atomic");
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        let track_a = test_track("miliastra://track/qqmusic/a", "歌曲A - 歌手A");
        let track_b = test_track("miliastra://track/qqmusic/b", "歌曲B - 歌手B");
        let started_at_ms = 1234;
        {
            let mut queue = PersistentQueue::from_request_store(store.clone(), 10).unwrap();
            queue
                .push(QueueItem {
                    keyword: "歌曲A".to_string(),
                    track: Some(track_a.clone()),
                    ..QueueItem::default()
                })
                .unwrap();
            queue
                .push(QueueItem {
                    keyword: "歌曲B".to_string(),
                    track: Some(track_b.clone()),
                    ..QueueItem::default()
                })
                .unwrap();
        }
        enter_starting(&store, &track_a, started_at_ms);
        let head_id = store.lock().unwrap().queue_snapshot().1[0].id;
        assert_eq!(head_id, 1);

        // 原子确认 + 出队：单次快照写入同时完成「确认播放成功」与「删除队首」。
        assert!(
            store
                .lock()
                .unwrap()
                .confirm_playback_and_dequeue(
                    confirmed_update(&track_a, started_at_ms),
                    Some(head_id)
                )
                .unwrap()
        );

        let (next_id, items) = store.lock().unwrap().queue_snapshot();
        assert_eq!(items.len(), 1, "队首 A 已出队");
        assert_eq!(items[0].keyword, "歌曲B");
        assert_eq!(next_id, 3, "next_queue_item_id 只增不减，不受出队影响");
        let playback = store.lock().unwrap().playback_snapshot();
        assert_eq!(playback.state, ConfirmedPlaybackState::RequestedSongPlaying);
        assert_eq!(
            playback
                .active_request
                .unwrap()
                .track
                .unwrap()
                .track_ref
                .key,
            track_a.track_ref.key
        );

        // 模拟进程崩溃重启：重载数据库后确认与出队都已持久化，队首直接是 B。
        drop(store);
        let reloaded =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        let (_, items) = reloaded.lock().unwrap().queue_snapshot();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].keyword, "歌曲B");
        let playback = reloaded.lock().unwrap().playback_snapshot();
        assert_eq!(playback.state, ConfirmedPlaybackState::RequestedSongPlaying);
        assert_eq!(
            playback
                .active_request
                .unwrap()
                .track
                .unwrap()
                .track_ref
                .key,
            track_a.track_ref.key
        );

        remove_request_state_path(state_path);
    }

    #[test]
    fn confirm_and_dequeue_keeps_queue_when_head_id_mismatches() {
        let state_path = temp_request_state_path("confirm-dequeue-mismatch");
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        let track_a = test_track("miliastra://track/qqmusic/a", "歌曲A - 歌手A");
        let started_at_ms = 77;
        {
            let mut queue = PersistentQueue::from_request_store(store.clone(), 10).unwrap();
            queue
                .push(QueueItem {
                    keyword: "歌曲A".to_string(),
                    track: Some(track_a.clone()),
                    ..QueueItem::default()
                })
                .unwrap();
        }
        enter_starting(&store, &track_a, started_at_ms);

        // 队首 id(1) 与传入 id(999) 不匹配：只确认播放状态，不误删队首。
        assert!(
            store
                .lock()
                .unwrap()
                .confirm_playback_and_dequeue(confirmed_update(&track_a, started_at_ms), Some(999))
                .unwrap()
        );
        let (_, items) = store.lock().unwrap().queue_snapshot();
        assert_eq!(items.len(), 1, "队首不匹配时不得出队");
        let playback = store.lock().unwrap().playback_snapshot();
        assert_eq!(playback.state, ConfirmedPlaybackState::RequestedSongPlaying);

        remove_request_state_path(state_path);
    }

    #[test]
    fn confirm_and_dequeue_write_failure_keeps_previous_state() {
        let state_path = temp_request_state_path("confirm-dequeue-crash");
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        let track_a = test_track("miliastra://track/qqmusic/a", "歌曲A - 歌手A");
        let track_b = test_track("miliastra://track/qqmusic/b", "歌曲B - 歌手B");
        let started_at_ms = 55;
        {
            let mut queue = PersistentQueue::from_request_store(store.clone(), 10).unwrap();
            queue
                .push(QueueItem {
                    keyword: "歌曲A".to_string(),
                    track: Some(track_a.clone()),
                    ..QueueItem::default()
                })
                .unwrap();
            queue
                .push(QueueItem {
                    keyword: "歌曲B".to_string(),
                    track: Some(track_b.clone()),
                    ..QueueItem::default()
                })
                .unwrap();
        }
        enter_starting(&store, &track_a, started_at_ms);
        let head_id = store.lock().unwrap().queue_snapshot().1[0].id;

        // 通过 BEFORE INSERT 触发器注入写盘故障：任何快照写入都会失败并整体回滚。
        store.lock().unwrap().inject_write_failure().unwrap();
        assert!(
            store
                .lock()
                .unwrap()
                .confirm_playback_and_dequeue(
                    confirmed_update(&track_a, started_at_ms),
                    Some(head_id)
                )
                .is_err()
        );
        // 内存态保持原状：队首仍在，播放状态仍是 Starting。
        let (_, items) = store.lock().unwrap().queue_snapshot();
        assert_eq!(items.len(), 2, "写失败时队首不得丢失");
        assert_eq!(items[0].id, head_id);
        let playback = store.lock().unwrap().playback_snapshot();
        assert_eq!(playback.state, ConfirmedPlaybackState::Starting);

        // 故障恢复（重新打开连接，触发器不复存在）后重载：磁盘仍是事务前的状态。
        drop(store);
        let reloaded =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        let (_, items) = reloaded.lock().unwrap().queue_snapshot();
        assert_eq!(items.len(), 2, "磁盘不得出现部分写入");
        assert_eq!(items[0].id, head_id);
        assert_eq!(
            reloaded.lock().unwrap().playback_snapshot().state,
            ConfirmedPlaybackState::Starting
        );

        remove_request_state_path(state_path);
    }

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
        let mut value = serde_json::to_value(RequestStateSnapshot::default()).unwrap();
        value.as_object_mut().unwrap().remove("schemaVersion");
        write_snapshot_blob(&state_path, &value);

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
        write_snapshot_blob(&state_path, &value);

        let error =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .expect_err("v1 state must not be migrated");

        assert!(error.to_string().contains("schemaVersion Some(1)"));
        assert!(error.to_string().contains("请删除状态文件"));
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
        write_snapshot_blob(&state_path, &serde_json::to_value(&snapshot).unwrap());

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

        // 单行快照：数据库里只有一行，JSON blob 同时包含队列与播放状态。
        let connection = rusqlite::Connection::open(&state_path).unwrap();
        let (row_count, text): (i64, String) = connection
            .query_row(
                "SELECT COUNT(*), snapshot FROM request_state WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row_count, 1, "快照必须始终只有一行");
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["schemaVersion"], REQUEST_STATE_SCHEMA_VERSION);
        assert_eq!(json["queue"].as_array().unwrap().len(), 1);
        assert_eq!(json["playback"]["state"], "paused_by_user");

        // 重载后队列与播放状态都能恢复。
        let reloaded =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        let reloaded = reloaded.lock().unwrap();
        assert_eq!(reloaded.queue_snapshot().1.len(), 1);
        assert_eq!(
            reloaded.playback_snapshot().state,
            ConfirmedPlaybackState::PausedByUser
        );

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
            let mut excluded = HashSet::new();
            let picked = store
                .pick_pool_track(&excluded)
                .expect("pool has candidates");
            assert_ne!(picked.track_ref.key.id, "");
            excluded.insert(picked.track_ref.key.clone());
            let re_picked = store
                .pick_pool_track(&excluded)
                .expect("excluding one key still leaves candidates");
            assert_ne!(re_picked.track_ref.key.id, picked.track_ref.key.id);
            excluded.insert(re_picked.track_ref.key.clone());
            assert!(store.pick_pool_track(&excluded).is_some());
            // 空池返回 None。
            store
                .update(|snapshot| {
                    snapshot.playback_pool.clear();
                    true
                })
                .unwrap();
            assert!(store.pick_pool_track(&HashSet::new()).is_none());
        }
    }

    #[test]
    fn playback_pool_remove_track_takes_it_out_of_pool() {
        let state_path = temp_request_state_path("playback-pool-remove");
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        {
            let mut store = store.lock().unwrap();
            let first = test_track("miliastra://track/qqmusic/1", "歌一 - 歌手A");
            let second = test_track("miliastra://track/qqmusic/2", "歌二 - 歌手B");
            store.record_pool_track(first.clone(), 10).unwrap();
            store.record_pool_track(second.clone(), 10).unwrap();
            assert_eq!(store.playback_pool_snapshot().len(), 2);

            // 删除指定歌曲后不再出现在播放池。
            let removed = store
                .remove_pool_track(&first.track_ref.key)
                .expect("remove pool track");
            assert!(removed);
            assert_eq!(store.playback_pool_snapshot().len(), 1);
            assert_eq!(
                store.playback_pool_snapshot()[0].track_ref.key,
                second.track_ref.key
            );

            // 删除不存在的歌曲返回 false。
            let again = store
                .remove_pool_track(&first.track_ref.key)
                .expect("remove again");
            assert!(!again);

            // 全部删除后池为空。
            let _ = store.remove_pool_track(&second.track_ref.key).unwrap();
            assert!(store.pick_pool_track(&HashSet::new()).is_none());
        }
        remove_request_state_path(state_path);
    }

    #[test]
    fn conditional_observation_does_not_overwrite_a_new_active_request() {
        let state_path = temp_request_state_path("conditional-observation");
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        let old_track = test_track("miliastra://track/qqmusic/old", "旧歌 - 歌手A");
        let new_track = test_track("miliastra://track/qqmusic/new", "新歌 - 歌手B");
        let old_request = ActivePlaybackRequest {
            track: Some(old_track.clone()),
            started_at_ms: 10,
            ..ActivePlaybackRequest::default()
        };
        let expected = old_request.identity().unwrap();
        let new_observation = PlaybackObservation {
            status: "playing".to_string(),
            track: Some(new_track.clone()),
            title: "新歌".to_string(),
            artist: "歌手B".to_string(),
            progress: 3.0,
            duration: 180.0,
            captured_at_ms: 30,
            reliability: ObservationReliability::Reliable,
        };
        {
            let mut store = store.lock().unwrap();
            store
                .update(|snapshot| {
                    snapshot.playback.state = ConfirmedPlaybackState::RequestedSongPlaying;
                    snapshot.playback.active_request = Some(old_request);
                    true
                })
                .unwrap();
            store
                .update(|snapshot| {
                    snapshot.playback.active_request = Some(ActivePlaybackRequest {
                        track: Some(new_track.clone()),
                        started_at_ms: 20,
                        ..ActivePlaybackRequest::default()
                    });
                    snapshot.playback.last_observation = Some(new_observation.clone());
                    true
                })
                .unwrap();

            let accepted = store
                .record_observation_if_active(
                    &expected,
                    PlaybackObservation {
                        status: "playing".to_string(),
                        track: Some(old_track),
                        title: "旧歌".to_string(),
                        artist: "歌手A".to_string(),
                        progress: 90.0,
                        duration: 180.0,
                        captured_at_ms: 40,
                        reliability: ObservationReliability::Reliable,
                    },
                    true,
                )
                .unwrap();
            assert!(!accepted);
            let playback = store.playback_snapshot();
            assert_eq!(
                playback
                    .active_request
                    .as_ref()
                    .and_then(|request| request.track.as_ref())
                    .map(|track| &track.track_ref.key),
                Some(&new_track.track_ref.key)
            );
            let persisted_observation = playback.last_observation.unwrap();
            assert_eq!(persisted_observation.track, new_observation.track);
            assert_eq!(persisted_observation.progress, new_observation.progress);
            assert_eq!(
                persisted_observation.captured_at_ms,
                new_observation.captured_at_ms
            );
        }
        remove_request_state_path(state_path);
    }

    #[test]
    fn corrupted_database_fails_loudly_without_recovery() {
        let state_path = temp_request_state_path("corrupt-db");
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
        drop(store);

        // 破坏数据库文件：SQLite 无备份机制，损坏必须明确失败而不是静默恢复。
        fs::write(&state_path, b"not-a-sqlite-database").unwrap();
        let error =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .expect_err("损坏的数据库必须明确失败");

        assert!(error.to_string().contains("请求状态数据库"));
        remove_request_state_path(state_path);
    }

    #[test]
    fn database_schema_mismatch_fails_loudly() {
        // 表列结构不匹配：明确失败，不迁移。
        let state_path = temp_request_state_path("schema-column-mismatch");
        let connection = rusqlite::Connection::open(&state_path).unwrap();
        connection
            .execute_batch("CREATE TABLE request_state (id INTEGER PRIMARY KEY, value TEXT)")
            .unwrap();
        let error =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .expect_err("表结构不匹配必须明确失败");
        assert!(error.to_string().contains("表结构"));
        remove_request_state_path(state_path);

        // 统一数据库已存在缓存表时，应在同一文件中补建 request_state。
        let state_path = temp_request_state_path("schema-other-tables");
        let connection = rusqlite::Connection::open(&state_path).unwrap();
        connection
            .execute_batch("CREATE TABLE cached_tracks (hash TEXT PRIMARY KEY)")
            .unwrap();
        drop(connection);
        let store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .expect("缓存表与请求状态表必须能够共存");
        assert_eq!(
            store.lock().unwrap().playback_snapshot().state,
            ConfirmedPlaybackState::Idle
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
        let replacement = PlaybackSessionBinding {
            runtime_identity: "native-runtime-B".to_string(),
            session_id: String::new(),
            generation: 0,
            bound_at_ms: 10,
        };
        {
            let store = store.lock().unwrap();
            assert_eq!(
                store.inspect_player_session(Some(&replacement)),
                SessionReconciliation::Restarted
            );
            assert_eq!(
                store
                    .snapshot
                    .session_binding
                    .as_ref()
                    .map(|binding| binding.runtime_identity.as_str()),
                Some("native-runtime-A")
            );
        }
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

    /// 在测试数据库里直接写入一行快照 blob（模拟旧版本/外部写入的数据）。
    fn write_snapshot_blob(path: &Path, value: &serde_json::Value) {
        let connection = rusqlite::Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE request_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    schema_version INTEGER NOT NULL,
                    snapshot TEXT NOT NULL
                )",
            )
            .unwrap();
        let text = serde_json::to_string(value).unwrap();
        let version = value
            .get("schemaVersion")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as i64;
        connection
            .execute(
                "INSERT INTO request_state (id, schema_version, snapshot) VALUES (1, ?1, ?2)",
                rusqlite::params![version, text],
            )
            .unwrap();
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
        root.with_extension("state.sqlite")
    }

    /// 清理测试数据库及其 WAL/SHM 附属文件。
    fn remove_request_state_path(state_path: PathBuf) {
        let _ = fs::remove_file(&state_path);
        let mut wal = state_path.as_os_str().to_os_string();
        wal.push("-wal");
        let _ = fs::remove_file(wal);
        let mut shm = state_path.as_os_str().to_os_string();
        shm.push("-shm");
        let _ = fs::remove_file(shm);
    }

    #[test]
    fn playback_runtime_state_defaults_volume_100_and_translation_enabled() {
        let state = PlaybackRuntimeState::default();
        assert_eq!(state.volume, 100, "默认音量必须是 100");
        assert!(state.use_translation, "默认歌词必须使用翻译");

        // 旧版快照缺少 volume/useTranslation 字段：反序列化必须恢复默认值。
        let restored: PlaybackRuntimeState = serde_json::from_str(
            r#"{
                "state": "idle",
                "pauseReason": "none",
                "activeRequest": null,
                "lastObservation": null
            }"#,
        )
        .expect("旧版快照必须可读");
        assert_eq!(restored.volume, 100);
        assert!(restored.use_translation);
        assert!(restored.previous_requests.is_empty());

        // 序列化往返保留音量与歌词模式。
        let state = PlaybackRuntimeState {
            volume: 60,
            use_translation: false,
            ..PlaybackRuntimeState::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        let restored: PlaybackRuntimeState = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.volume, 60);
        assert!(!restored.use_translation);
    }

    #[test]
    fn volume_and_lyrics_mode_persist_and_survive_reload() {
        let state_path = temp_request_state_path("volume-lyrics-persist");
        {
            let store = RequestStateStore::load(
                state_path.clone(),
                crate::test_support::test_state_store(),
            )
            .unwrap();
            let mut playback = PersistentPlaybackState::from_request_store(store).unwrap();
            // 新装默认：音量 100、使用翻译。
            assert_eq!(playback.state().volume, 100);
            assert!(playback.state().use_translation);
            // 成功设置音量/切换歌词后的写入路径（apply 后落盘）。
            assert!(
                playback
                    .update(|state| {
                        state.volume = 60;
                        state.use_translation = false;
                        true
                    })
                    .unwrap()
            );
        }

        // 模拟重启：重载数据库后音量与歌词模式恢复。
        let reloaded =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        let playback = reloaded.lock().unwrap().playback_snapshot();
        assert_eq!(playback.volume, 60);
        assert!(!playback.use_translation);

        remove_request_state_path(state_path);
    }
}
