use anyhow::{Result, bail};
use miliastra_playback::{MAX_LYRICS_LEAD_SECONDS, PlayableTrack};
#[cfg(test)]
use miliastra_playback::{
    PlaybackEligibility, ProviderId, SearchCandidate, TrackKey, TrackMetadata, TrackRef,
};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

mod application;
mod controller;
mod dedup;
mod format;
mod matcher;
mod queue;
mod state;

#[cfg(test)]
pub(crate) fn test_track(uri: &str, text: &str) -> PlayableTrack {
    let locator = uri
        .strip_prefix("miliastra://track/")
        .expect("test URI uses the canonical track scheme");
    let (provider, id) = locator
        .split_once('/')
        .expect("test URI contains provider and id");
    let provider = provider.parse::<ProviderId>().expect("known test provider");
    let (title, artist) = text
        .split_once(" - ")
        .map_or((text, "测试歌手"), |(title, artist)| (title, artist));
    PlayableTrack {
        track_ref: TrackRef {
            key: TrackKey::new(provider, id).expect("valid test track key"),
            resolver_locator: None,
        },
        metadata: TrackMetadata {
            title: title.to_string(),
            artists: vec![artist.to_string()],
            album: None,
            duration_ms: Some(180_000),
        },
    }
}

#[cfg(test)]
pub(crate) fn test_candidate(text: &str, uri: &str) -> SearchCandidate {
    let track = test_track(uri, text);
    SearchCandidate {
        track_ref: track.track_ref,
        metadata: track.metadata,
        eligibility: PlaybackEligibility::Unknown,
        text: text.to_string(),
    }
}

use crate::features::chat_text::{CommandSyntax, command_identity, parse_prefixed_command};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
pub(crate) use controller::{
    MusicPlayerBackend, PlaybackNavigation, PlaybackOutcome, PlaybackRequest, PlaybackStatePort,
    PlaybackTimePorts, PlaybackVerification, PlayerController, QueueAdvanceContext,
    QueueAdvanceDecision, has_restorable_playback_progress,
};
pub(crate) use dedup::{PersistentSongDedupHistory, SongDedupCandidate};
pub(crate) use format::{
    PlaybackSnapshot, estimated_player_status, format_lyrics, format_status, is_playing,
};
use miliastra_kernel::clock::WallClock;
pub(crate) use queue::{PersistentQueue, QueueItem};
#[cfg(test)]
pub(crate) use state::ObservationReliability;
pub(crate) use state::{
    ActivePlaybackIdentity, ActivePlaybackRequest, ConfirmedPlaybackState, ControlOperationRecord,
    PauseReason, PersistentPlaybackState, PlaybackAttemptRecord, PlaybackObservation,
    PlaybackRuntimeState, PlaybackSessionBinding, RequestStateStore, SessionReconciliation,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackTimingConfig {
    pub status_poll_ms: u64,
    pub monitor_tick_ms: u64,
    pub monitor_status_ms: u64,
    /// Amount added to the current playback position when selecting a lyric line.
    #[serde(default = "default_lyrics_lead_seconds")]
    pub lyrics_lead_seconds: f64,
    pub uri_stable_samples: u32,
    pub transport_stable_samples: u32,
    #[serde(deserialize_with = "deserialize_positive_u64")]
    pub stale_timeout_ms: u64,
}

impl Default for PlaybackTimingConfig {
    fn default() -> Self {
        Self {
            status_poll_ms: 1000,
            monitor_tick_ms: 200,
            monitor_status_ms: 1000,
            lyrics_lead_seconds: default_lyrics_lead_seconds(),
            uri_stable_samples: 0,
            transport_stable_samples: 0,
            stale_timeout_ms: 5000,
        }
    }
}

impl PlaybackTimingConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        for (value, field) in [
            (self.status_poll_ms, "timing.playback.status_poll_ms"),
            (self.monitor_tick_ms, "timing.playback.monitor_tick_ms"),
            (self.monitor_status_ms, "timing.playback.monitor_status_ms"),
            (self.stale_timeout_ms, "timing.playback.stale_timeout_ms"),
        ] {
            if value == 0 {
                bail!("{} 必须大于 0", field);
            }
        }
        if !self.lyrics_lead_seconds.is_finite()
            || !(0.0..=MAX_LYRICS_LEAD_SECONDS).contains(&self.lyrics_lead_seconds)
        {
            bail!(
                "timing.playback.lyrics_lead_seconds 必须是 0 到 {} 之间的有限数字",
                MAX_LYRICS_LEAD_SECONDS
            );
        }
        Ok(())
    }
}

fn default_lyrics_lead_seconds() -> f64 {
    0.0
}

fn deserialize_positive_u64<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("value must be a positive integer"));
    }
    Ok(value)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueConfig {
    pub max_size: usize,
    pub protect_current_song_until_finished: bool,
    pub external_playback_protect_after_seconds: u64,
    /// 播放池最大容量，0 表示禁用播放池
    pub pool_max_size: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_size: 5,
            protect_current_song_until_finished: true,
            external_playback_protect_after_seconds: 20,
            pool_max_size: 1000,
        }
    }
}

impl QueueConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.max_size == 0 {
            bail!("queue.max_size 必须大于 0");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SongDedupConfig {
    pub enabled: bool,
    pub window_seconds: u64,
    pub max_count: u32,
    pub console_bypass: bool,
    pub history_path: PathBuf,
}

impl Default for SongDedupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_seconds: 3600,
            max_count: 1,
            console_bypass: true,
            history_path: PathBuf::from("deps/data/song-dedup-history.json"),
        }
    }
}

impl SongDedupConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.enabled && (self.window_seconds == 0 || self.max_count == 0) {
            bail!("song_dedup.window_seconds 和 max_count 必须大于 0");
        }
        if self.enabled && self.history_path.as_os_str().is_empty() {
            bail!("song_dedup.history_path 不能为空");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    pub min_song_name_score: f64,
    pub short_chinese_song_max_miss: usize,
    pub long_chinese_song_min_score: f64,
    pub max_ocr_noise_chars: usize,
    pub enable_fuzzy_singer: bool,
    pub short_chinese_singer_max_miss: usize,
    pub long_chinese_singer_min_score: f64,
    pub en_max_edit_fraction: f64,
    pub en_singer_max_edit_fraction: f64,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            min_song_name_score: 0.5,
            short_chinese_song_max_miss: 1,
            long_chinese_song_min_score: 0.5,
            max_ocr_noise_chars: 1,
            enable_fuzzy_singer: true,
            short_chinese_singer_max_miss: 1,
            long_chinese_singer_min_score: 0.8,
            en_max_edit_fraction: 0.3,
            en_singer_max_edit_fraction: 0.35,
        }
    }
}

impl MatchConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        for (value, field) in [
            (self.min_song_name_score, "matching.min_song_name_score"),
            (
                self.long_chinese_song_min_score,
                "matching.long_chinese_song_min_score",
            ),
            (
                self.long_chinese_singer_min_score,
                "matching.long_chinese_singer_min_score",
            ),
            (self.en_max_edit_fraction, "matching.en_max_edit_fraction"),
            (
                self.en_singer_max_edit_fraction,
                "matching.en_singer_max_edit_fraction",
            ),
        ] {
            if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                bail!("{} 必须是 0 到 1 之间的有限小数", field);
            }
        }
        Ok(())
    }
}

/// 播放控制命令：点歌、切歌、暂停、恢复、歌词、队列和音量操作。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum PlaybackCommand {
    Pause,
    Resume,
    Play,
    Next,
    DeleteCurrentPoolTrack,
    Previous,
    Volume(String),
    Status,
    Lyrics,
    ToggleLyrics,
    LyricsFor(u16),
    ContinuousLyrics,
    BackgroundLyrics,
    SingleSongLyrics,
    StopBackgroundLyrics,
    Queue,
    QueueDelete(Vec<usize>),
    QueueClear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackgroundLyricsScope {
    AllSongs,
    CurrentSong,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PlayerStatus {
    pub(crate) status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) current_track: Option<PlayableTrack>,
    pub(crate) current_uri: String,
    pub(crate) name: String,
    pub(crate) singer: String,
    pub(crate) album_name: String,
    pub(crate) lyric_line_text: String,
    pub(crate) duration: f64,
    pub(crate) progress: f64,
    pub(crate) playback_rate: f64,
    pub(crate) volume: i64,
    pub(crate) requester: String,
    /// The player runtime which produced this observation, when the backend
    /// exposes a stable process identity.
    pub(crate) runtime_identity: String,
    /// Session reference carried by native playback terminal outcomes.
    pub(crate) session_id: String,
    pub(crate) generation: u64,
    /// Native playback end behavior and durable terminal outcome metadata.
    pub(crate) end_behavior: String,
    pub(crate) last_end_cause: String,
    pub(crate) failure_code: String,
    pub(crate) failure_message: String,
    pub(crate) failure_retryable: bool,
    pub(crate) failure_provider: String,
    pub(crate) failure_retry_after_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaybackControllerSnapshot {
    pub(crate) state: String,
    pub(crate) pause_reason: String,
    pub(crate) active_keyword: String,
    pub(crate) active_uri: String,
    pub(crate) last_observation_reliability: String,
    pub(crate) backend_status: String,
    pub(crate) current_uri: String,
    pub(crate) title: String,
    pub(crate) artist: String,
    pub(crate) requester: String,
    pub(crate) progress: f64,
    pub(crate) duration: f64,
    pub(crate) observed_at_ms: u64,
}

impl PlaybackCommand {
    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        if envelope.prefix() != CommandPrefix::At {
            return false;
        }
        match envelope.authority() {
            CommandAuthority::HallMember => PLAYBACK_COMMAND_PREFIXES
                .iter()
                .filter(|prefix| **prefix != "删除")
                .any(|prefix| envelope.command_text().starts_with(prefix)),
            CommandAuthority::Friend => envelope.command_text() == "删除",
        }
    }

    pub(crate) fn parse_chat(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_chat(envelope) {
            return None;
        }
        let parsed = Self::parse_hall(envelope.command_text())?;
        let raw = if parsed.argument.is_empty() {
            parsed.matched.to_string()
        } else {
            format!("{} {}", parsed.matched, parsed.argument)
        };
        Some(FeatureCommandMatch::new(
            parsed.matched,
            raw,
            parsed.command,
        ))
    }

    pub(crate) fn parse_hall(text: &str) -> Option<CommandSyntax<'_, Self>> {
        for (prefix, allows_argument) in [
            ("队列删除", true),
            ("队列清空", false),
            ("下一首", false),
            ("下一曲", false),
            ("删除", false),
            ("上一首", false),
            ("上一曲", false),
            ("暂停", false),
            ("继续", false),
            ("恢复", false),
            ("播放", false),
            ("音量", true),
            ("状态", false),
            ("切换", false),
            ("停止歌词", false),
            ("后台歌词", false),
            ("单曲歌词", false),
            ("持续歌词", false),
            ("歌词", true),
            ("队列", false),
            ("列表", false),
        ] {
            let Some(argument) = parse_prefixed_command(text, prefix, allows_argument) else {
                continue;
            };
            let command = match prefix {
                "暂停" => Self::Pause,
                "继续" | "恢复" => Self::Resume,
                "播放" => Self::Play,
                "下一首" | "下一曲" => Self::Next,
                "删除" => Self::DeleteCurrentPoolTrack,
                "上一首" | "上一曲" => Self::Previous,
                "音量" => Self::Volume(argument.to_string()),
                "状态" => Self::Status,
                "切换" => Self::ToggleLyrics,
                "停止歌词" => Self::StopBackgroundLyrics,
                "后台歌词" => Self::BackgroundLyrics,
                "单曲歌词" => Self::SingleSongLyrics,
                "持续歌词" => Self::ContinuousLyrics,
                "歌词" if argument.is_empty() => Self::Lyrics,
                "歌词" => Self::LyricsFor(parse_lyrics_duration(argument)?),
                "队列" | "列表" => Self::Queue,
                "队列删除" => Self::QueueDelete(parse_queue_indexes(argument)),
                "队列清空" => Self::QueueClear,
                _ => return None,
            };
            return Some(CommandSyntax {
                matched: prefix,
                argument,
                command,
            });
        }
        None
    }

    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Pause => "pause".to_string(),
            Self::Resume | Self::Play => "play".to_string(),
            Self::Next => "next".to_string(),
            Self::DeleteCurrentPoolTrack => "delete_current_pool_track".to_string(),
            Self::Previous => "previous".to_string(),
            Self::Volume(volume) => format!("volume:{}", command_identity(volume)),
            Self::Status => "status".to_string(),
            Self::Lyrics => "lyrics".to_string(),
            Self::ToggleLyrics => "lyrics_toggle".to_string(),
            Self::LyricsFor(_) => "lyrics_for".to_string(),
            Self::ContinuousLyrics => "lyrics_continuous".to_string(),
            Self::BackgroundLyrics => "lyrics_background".to_string(),
            Self::SingleSongLyrics => "lyrics_single_song".to_string(),
            Self::StopBackgroundLyrics => "lyrics_background_stop".to_string(),
            Self::Queue => "queue".to_string(),
            Self::QueueDelete(indexes) => format!(
                "queue_delete:{}",
                indexes
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::QueueClear => "queue_clear".to_string(),
        }
    }
}

const PLAYBACK_COMMAND_PREFIXES: &[&str] = &[
    "队列删除",
    "队列清空",
    "下一首",
    "下一曲",
    "删除",
    "上一首",
    "上一曲",
    "暂停",
    "继续",
    "恢复",
    "播放",
    "音量",
    "状态",
    "切换",
    "停止歌词",
    "后台歌词",
    "单曲歌词",
    "持续歌词",
    "歌词",
    "队列",
    "列表",
];

fn parse_queue_indexes(argument: &str) -> Vec<usize> {
    argument
        .chars()
        .filter_map(|ch| ch.to_digit(10))
        .filter(|value| (1..=9).contains(value))
        .map(|value| value as usize - 1)
        .collect()
}

const MAX_TIMED_LYRICS_SECONDS: u16 = 300;

fn parse_lyrics_duration(argument: &str) -> Option<u16> {
    let argument = argument.trim();
    if argument.is_empty() || !argument.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let seconds = argument.parse::<u16>().ok()?;
    (1..=MAX_TIMED_LYRICS_SECONDS)
        .contains(&seconds)
        .then_some(seconds)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QueuePushOutcome {
    pub(crate) accepted: bool,
    pub(crate) size: usize,
}

pub(crate) enum PlaybackMutationIntent {
    Push(Box<QueueItem>),
    Remove(QueueRemoval),
    Clear,
    /// 删除指定歌曲:从播放池移除(不再随机播放)。
    RemovePoolTrack(miliastra_playback::TrackKey),
}

pub(crate) enum PlaybackMutationOutcome {
    Pushed(QueuePushOutcome),
    Removed(QueueRemoveOutcome),
    Cleared,
    PoolTrackRemoved(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueRemoval {
    Id(u64),
    Index(usize),
    Front,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QueueRemoveOutcome {
    Removed {
        index: usize,
        item: Box<QueueItem>,
        size: usize,
    },
    MissingId,
    InvalidIndex,
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExternalPlaybackObservation {
    pub(crate) was_protected: bool,
    pub(crate) protected: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum PlaybackStateUpdate {
    UserPaused,
    UserResumed,
    ClearActiveRequest,
    /// 把当前播放保留进上一曲历史（自动推进清空请求前调用，与手动点歌路径对齐）。
    RememberCurrentPlayback,
    External,
    Starting {
        request: ActivePlaybackRequest,
        navigation: PlaybackNavigation,
    },
    Confirmed {
        request: ActivePlaybackRequest,
        navigation: PlaybackNavigation,
    },
    /// 持久化音量（0-100）：后端设置成功后写入，供重启恢复。
    Volume(u8),
    /// 持久化歌词翻译开关：后端切换成功后写入，供重启恢复。
    LyricsMode(bool),
    Observation(PlaybackObservation),
    /// 配置重载关停前的最后观测必须绕过常规进度节流，供新进程精确续播。
    ImmediateObservation(PlaybackObservation),
    Unknown,
    Restore(Box<PlaybackRuntimeState>),
}

impl PlaybackStateUpdate {
    pub(crate) fn apply(self, playback: &mut PlaybackRuntimeState) -> bool {
        match self {
            Self::UserPaused => {
                playback.set_user_paused();
                true
            }
            Self::UserResumed => {
                playback.set_user_resumed();
                true
            }
            Self::ClearActiveRequest => {
                playback.clear_active_request();
                true
            }
            Self::RememberCurrentPlayback => {
                playback.remember_current_playback();
                true
            }
            Self::External => {
                playback.remember_current_playback();
                playback.state = ConfirmedPlaybackState::ExternalPlayback;
                playback.pause_reason = PauseReason::None;
                playback.active_request = None;
                true
            }
            Self::Starting {
                request,
                navigation,
            } => {
                if navigation == PlaybackNavigation::Normal {
                    playback.remember_current_playback();
                }
                playback.state = ConfirmedPlaybackState::Starting;
                playback.pause_reason = PauseReason::None;
                playback.active_request = Some(request);
                true
            }
            Self::Confirmed {
                request,
                navigation,
            } => {
                match navigation {
                    PlaybackNavigation::Normal => playback.remember_current_playback(),
                    PlaybackNavigation::Previous => playback.remove_previous_request(&request),
                    PlaybackNavigation::Restore => {}
                }
                // play 是异步操作，确认返回前用户可能已暂停：此时引擎实际处于暂停，
                // 必须保留 User 暂停状态，否则暂停的歌不会自然结束，队列自动推进被永久卡住。
                playback.state = if playback.pause_reason == PauseReason::User {
                    ConfirmedPlaybackState::PausedByUser
                } else {
                    ConfirmedPlaybackState::RequestedSongPlaying
                };
                playback.active_request = Some(request);
                true
            }
            Self::Volume(volume) => {
                if playback.volume != volume {
                    playback.volume = volume;
                    true
                } else {
                    false
                }
            }
            Self::LyricsMode(use_translation) => {
                if playback.use_translation != use_translation {
                    playback.use_translation = use_translation;
                    true
                } else {
                    false
                }
            }
            Self::Observation(observation) => {
                if !observation_requires_persist(playback.last_observation.as_ref(), &observation) {
                    false
                } else {
                    playback.last_observation = Some(observation);
                    true
                }
            }
            Self::ImmediateObservation(observation) => {
                playback.last_observation = Some(observation);
                true
            }
            Self::Unknown => {
                playback.state = ConfirmedPlaybackState::Unknown;
                playback.pause_reason = PauseReason::None;
                playback.active_request = None;
                true
            }
            Self::Restore(previous) => {
                *playback = *previous;
                true
            }
        }
    }
}

pub(crate) struct PlaybackService {
    queue: PersistentQueue,
    playback_state: PersistentPlaybackState,
    request_state: Option<state::SharedRequestStateStore>,
    song_dedup_history: PersistentSongDedupHistory,
    /// 热更新共享句柄（阶段 7）：保存成功后由 HTTP 层 apply，
    /// enabled/window_seconds/max_count 立即作用于去重判定。
    song_dedup: Arc<RwLock<SongDedupConfig>>,
    external_playback_tracker: controller::ExternalPlaybackTracker,
    pool_max_size: usize,
}

impl PlaybackService {
    pub(crate) fn load(
        playback_state_path: PathBuf,
        song_dedup_history_path: PathBuf,
        queue_max_size: usize,
        pool_max_size: usize,
        song_dedup: Arc<RwLock<SongDedupConfig>>,
        wall_clock: Arc<dyn WallClock>,
        store: Arc<dyn miliastra_contracts::StateStore>,
    ) -> Result<Self> {
        let request_state = RequestStateStore::load(playback_state_path, store.clone())?;
        let queue = PersistentQueue::from_request_store(request_state.clone(), queue_max_size)?;
        let playback_state = PersistentPlaybackState::from_request_store(request_state.clone())?;
        let song_dedup_history =
            PersistentSongDedupHistory::load(song_dedup_history_path, wall_clock, store)?;
        let pool_size = request_state
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .playback_pool_snapshot()
            .len();
        log::info!("已加载队列: {} 首", queue.len());
        log::info!("已加载播放池: {} 首", pool_size);
        log::info!("已加载长时间同歌去重历史: {} 条", song_dedup_history.len());
        log::info!(
            "已加载播放状态: playback_state={:?}",
            playback_state.state().state
        );
        let mut service = Self::new(
            queue,
            playback_state,
            song_dedup_history,
            song_dedup,
            pool_max_size,
        );
        service.request_state = Some(request_state);
        Ok(service)
    }

    pub(crate) fn new(
        queue: PersistentQueue,
        playback_state: PersistentPlaybackState,
        song_dedup_history: PersistentSongDedupHistory,
        song_dedup: Arc<RwLock<SongDedupConfig>>,
        pool_max_size: usize,
    ) -> Self {
        Self {
            queue,
            playback_state,
            request_state: None,
            song_dedup_history,
            song_dedup,
            external_playback_tracker: controller::ExternalPlaybackTracker::default(),
            pool_max_size,
        }
    }

    pub(crate) fn queue_snapshot(&self) -> Vec<QueueItem> {
        self.queue.items().to_vec()
    }

    pub(crate) fn queue_contains(&self, item: &QueueItem) -> bool {
        self.queue.contains_duplicate(item)
    }

    pub(crate) fn push_queue(&mut self, item: QueueItem) -> Result<QueuePushOutcome> {
        let accepted = self.queue.push(item)?;
        Ok(QueuePushOutcome {
            accepted,
            size: self.queue.len(),
        })
    }

    pub(crate) fn remove_queue(&mut self, removal: QueueRemoval) -> Result<QueueRemoveOutcome> {
        let removed = match removal {
            QueueRemoval::Id(id) => {
                let Some(removed) = self.queue.remove_id(id)? else {
                    return Ok(QueueRemoveOutcome::MissingId);
                };
                removed
            }
            QueueRemoval::Index(index) => {
                if index >= self.queue.len() {
                    return Ok(QueueRemoveOutcome::InvalidIndex);
                }
                self.queue
                    .remove_indexes(&[index])?
                    .into_iter()
                    .next()
                    .expect("validated queue index produces one removed item")
            }
            QueueRemoval::Front => {
                if self.queue.is_empty() {
                    return Ok(QueueRemoveOutcome::Empty);
                }
                self.queue
                    .remove_indexes(&[0])?
                    .into_iter()
                    .next()
                    .expect("non-empty queue produces one removed front item")
            }
        };
        Ok(QueueRemoveOutcome::Removed {
            index: removed.0,
            item: Box::new(removed.1),
            size: self.queue.len(),
        })
    }

    pub(crate) fn remove_queue_indexes(
        &mut self,
        indexes: Vec<usize>,
    ) -> Result<Vec<(usize, QueueItem)>> {
        self.queue.remove_indexes(&indexes)
    }

    pub(crate) fn clear_queue(&mut self) -> Result<usize> {
        self.queue.clear()
    }

    pub(crate) fn playback_pool_snapshot(&self) -> Result<Vec<PlayableTrack>> {
        let Some(store) = &self.request_state else {
            return Ok(Vec::new());
        };
        store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))
            .map(|store| store.playback_pool_snapshot())
    }

    pub(crate) fn record_playback_pool_track(&mut self, track: PlayableTrack) -> Result<()> {
        let Some(store) = &self.request_state else {
            return Ok(());
        };
        store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .record_pool_track(track, self.pool_max_size)
            .map(|_| ())
    }

    pub(crate) fn pick_playback_pool_track(
        &mut self,
        excluded: &HashSet<miliastra_playback::TrackKey>,
    ) -> Result<Option<PlayableTrack>> {
        let Some(store) = &self.request_state else {
            return Ok(None);
        };
        Ok(store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .pick_pool_track(excluded))
    }

    pub(crate) fn playback_pool_available(&self) -> Result<bool> {
        if self.pool_max_size == 0 {
            return Ok(false);
        }
        Ok(!self.playback_pool_snapshot()?.is_empty())
    }

    /// 从播放池删除指定歌曲(不再参与随机播放)。
    pub(crate) fn remove_playback_pool_track(
        &mut self,
        key: &miliastra_playback::TrackKey,
    ) -> Result<bool> {
        let Some(store) = &self.request_state else {
            return Ok(false);
        };
        store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .remove_pool_track(key)
    }

    pub(crate) fn playback_state_snapshot(&self) -> PlaybackRuntimeState {
        self.playback_state.state().clone()
    }

    pub(crate) fn song_dedup_limited(&self, candidate: &SongDedupCandidate) -> bool {
        let song_dedup = self
            .song_dedup
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.song_dedup_history.is_limited(&song_dedup, candidate)
    }

    pub(crate) fn record_song_dedup(&mut self, candidate: SongDedupCandidate) -> Result<()> {
        let song_dedup = self
            .song_dedup
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.song_dedup_history
            .record_playback(&song_dedup, candidate)
    }

    pub(crate) fn observe_external_playback(
        &mut self,
        identity: &miliastra_playback::TrackKey,
        now: Instant,
        protect_after: Duration,
    ) -> ExternalPlaybackObservation {
        let was_protected = self.external_playback_tracker.protected;
        let protected = self
            .external_playback_tracker
            .observe(identity, now, protect_after);
        ExternalPlaybackObservation {
            was_protected,
            protected,
        }
    }

    pub(crate) fn clear_external_playback_tracker(&mut self) {
        self.external_playback_tracker.clear();
    }

    pub(crate) fn apply_playback_state_update(
        &mut self,
        update: PlaybackStateUpdate,
    ) -> Result<bool> {
        self.playback_state
            .update(|playback| update.apply(playback))
    }

    pub(crate) fn record_observation_if_active(
        &mut self,
        expected: ActivePlaybackIdentity,
        observation: PlaybackObservation,
        immediate: bool,
    ) -> Result<bool> {
        self.playback_state
            .record_observation_if_active(&expected, observation, immediate)
    }

    /// 原子确认播放成功并从队列删除对应项（同一笔持久化）。
    ///
    /// 共享请求状态存储存在时，确认与队首出队在同一事务内落盘：
    /// 崩溃发生在事务前，队首保留等待重播；发生在事务后，队首已出队，
    /// 重启不会再次播放同一队首。仅当 `queue_item_id` 与当前队首匹配才出队。
    /// 无共享存储（测试构造）时退化为非原子确认，出队由消费流程补偿。
    pub(crate) fn confirm_playback_and_dequeue(
        &mut self,
        update: PlaybackStateUpdate,
        queue_item_id: Option<u64>,
    ) -> Result<bool> {
        let Some(store) = &self.request_state else {
            return self
                .playback_state
                .update(|playback| update.apply(playback));
        };
        let changed = store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .confirm_playback_and_dequeue(update, queue_item_id)?;
        if changed {
            // 共享存储内的事务已直接修改落盘快照，刷新内存缓存保持一致。
            self.sync_from_request_store()?;
        }
        Ok(changed)
    }

    /// 从共享请求状态存储重读队列与播放状态，覆盖本服务的内存缓存。
    fn sync_from_request_store(&mut self) -> Result<()> {
        let store = self
            .request_state
            .clone()
            .ok_or_else(|| anyhow::anyhow!("请求状态存储未初始化"))?;
        let (next_id, items, playback) = {
            let guard = store
                .lock()
                .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?;
            let (next_id, items) = guard.queue_snapshot();
            let playback = guard.playback_snapshot();
            (next_id, items, playback)
        };
        self.queue.sync_snapshot(next_id, items);
        self.playback_state.sync_state(playback);
        Ok(())
    }

    pub(crate) fn reconcile_player_session(
        &mut self,
        binding: Option<PlaybackSessionBinding>,
    ) -> Result<SessionReconciliation> {
        let Some(store) = &self.request_state else {
            return Ok(SessionReconciliation::Unknown);
        };
        store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .reconcile_player_session(binding)
    }

    pub(crate) fn inspect_player_session(
        &self,
        binding: Option<PlaybackSessionBinding>,
    ) -> Result<SessionReconciliation> {
        let Some(store) = &self.request_state else {
            return Ok(SessionReconciliation::Unknown);
        };
        Ok(store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .inspect_player_session(binding.as_ref()))
    }

    pub(crate) fn claim_terminal_outcome(
        &mut self,
        request_id: u64,
        outcome: impl Into<String>,
        handled_at_ms: u64,
    ) -> Result<bool> {
        let Some(store) = &self.request_state else {
            return Ok(false);
        };
        store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .claim_terminal_outcome(request_id, outcome, handled_at_ms)
    }

    pub(crate) fn record_playback_attempt(
        &mut self,
        provider: String,
        locator: String,
        started_at_ms: u64,
        result: String,
    ) -> Result<()> {
        let Some(store) = &self.request_state else {
            return Ok(());
        };
        store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .record_attempt(PlaybackAttemptRecord {
                request_id: 0,
                provider,
                locator,
                started_at_ms,
                result,
            })
            .map(|_| ())
    }

    pub(crate) fn record_control_operation(
        &mut self,
        operation: String,
        requested_at_ms: u64,
        completed: bool,
    ) -> Result<()> {
        let Some(store) = &self.request_state else {
            return Ok(());
        };
        store
            .lock()
            .map_err(|_| anyhow::anyhow!("请求状态存储锁已中毒"))?
            .record_control_operation(ControlOperationRecord {
                operation_id: 0,
                operation,
                requested_at_ms,
                completed,
            })
            .map(|_| ())
    }
}

/// 观测 progress 变化达到该秒数才持久化，节流高频观测写盘（如 50ms 轮询）。
const OBSERVATION_PROGRESS_PERSIST_THRESHOLD_SECS: f64 = 5.0;

/// 观测记录持久化兜底间隔：距上次持久化超过该毫秒数时即使进度未达阈值也刷新，
/// 保证 progress/duration/captured_at 等观测元数据不会永久冻结。
const OBSERVATION_PERSIST_FALLBACK_INTERVAL_MS: u64 = 10_000;

/// 判断观测记录是否需要持久化。
///
/// 身份字段（状态、曲目、标题、歌手、可靠性）变化时立即持久化；
/// 仅进度、时长、观测时间变化时按阈值节流，避免每个观测周期都写盘。
fn observation_requires_persist(
    previous: Option<&PlaybackObservation>,
    current: &PlaybackObservation,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if previous.status != current.status
        || previous.track != current.track
        || previous.title != current.title
        || previous.artist != current.artist
        || previous.reliability != current.reliability
    {
        return true;
    }
    // 进度推进达到阈值：节流写盘。
    if (current.progress - previous.progress).abs() >= OBSERVATION_PROGRESS_PERSIST_THRESHOLD_SECS {
        return true;
    }
    // 时长变化：立即持久化。
    if current.duration != previous.duration {
        return true;
    }
    // 观测时间超过兜底间隔：定期刷新，避免观测元数据永久冻结。
    current
        .captured_at_ms
        .saturating_sub(previous.captured_at_ms)
        >= OBSERVATION_PERSIST_FALLBACK_INTERVAL_MS
}

pub(crate) use application::{
    LyricTracker, PlaybackApplication, PlaybackApplicationConfig, PlaybackCommandContext,
    PlaybackCommandPort, PlaybackExecutionPort, PlaybackMonitorPort, PlaybackPickedCandidate,
    PlaybackResult, PlaybackSearchFailure, PlaybackSelection, PlaybackWorkload,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::playback::state::ObservationReliability;
    use miliastra_kernel::clock::SystemClock;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn hidden_delete_command_parses_without_an_argument() {
        let parsed = PlaybackCommand::parse_hall("删除").expect("hidden delete command");
        assert_eq!(parsed.command, PlaybackCommand::DeleteCurrentPoolTrack);
        assert_eq!(parsed.argument, "");
        assert_eq!(PlaybackCommand::parse_hall("删除 1"), None);
    }

    #[test]
    fn queue_aliases_parse_as_the_full_queue_command() {
        for text in ["队列", "列表"] {
            let parsed = PlaybackCommand::parse_hall(text).expect("queue command");
            assert_eq!(parsed.command, PlaybackCommand::Queue);
            assert_eq!(parsed.argument, "");
        }
        assert_eq!(PlaybackCommand::parse_hall("完整队列"), None);
        assert_eq!(PlaybackCommand::parse_hall("完整列表"), None);
    }

    #[test]
    fn lyrics_command_accepts_a_duration_from_one_to_three_hundred_seconds() {
        assert_eq!(
            PlaybackCommand::parse_hall("歌词")
                .expect("one-shot lyrics command")
                .command,
            PlaybackCommand::Lyrics
        );
        assert_eq!(
            PlaybackCommand::parse_hall("歌词 5")
                .expect("timed lyrics command")
                .command,
            PlaybackCommand::LyricsFor(5)
        );
        assert_eq!(
            PlaybackCommand::parse_hall("歌词 300")
                .expect("maximum timed lyrics command")
                .command,
            PlaybackCommand::LyricsFor(300)
        );
        assert_eq!(PlaybackCommand::parse_hall("歌词 0"), None);
        assert_eq!(PlaybackCommand::parse_hall("歌词 301"), None);
        assert_eq!(PlaybackCommand::parse_hall("歌词 5秒"), None);
    }

    #[test]
    fn hidden_toggle_command_has_a_dedicated_lock() {
        let parsed = PlaybackCommand::parse_hall("切换").expect("lyrics toggle command");
        assert_eq!(parsed.command, PlaybackCommand::ToggleLyrics);
        assert_eq!(parsed.argument, "");
        assert_eq!(parsed.command.lock_key(), "lyrics_toggle");
        assert_eq!(PlaybackCommand::parse_hall("切换 原文"), None);
    }

    #[test]
    fn timed_lyrics_uses_a_different_lock_from_one_shot_lyrics() {
        assert_ne!(
            PlaybackCommand::Lyrics.lock_key(),
            PlaybackCommand::LyricsFor(5).lock_key()
        );
        assert_eq!(
            PlaybackCommand::LyricsFor(5).lock_key(),
            PlaybackCommand::LyricsFor(300).lock_key()
        );
    }

    #[test]
    fn continuous_lyrics_has_a_dedicated_command_and_lock() {
        let parsed = PlaybackCommand::parse_hall("持续歌词").expect("continuous lyrics command");
        assert_eq!(parsed.command, PlaybackCommand::ContinuousLyrics);
        assert_eq!(parsed.argument, "");
        assert_eq!(parsed.matched, "持续歌词");
        assert_eq!(
            PlaybackCommand::ContinuousLyrics.lock_key(),
            "lyrics_continuous"
        );
        assert_ne!(
            PlaybackCommand::ContinuousLyrics.lock_key(),
            PlaybackCommand::LyricsFor(300).lock_key()
        );
        assert_eq!(PlaybackCommand::parse_hall("持续歌词 1"), None);
    }

    #[test]
    fn background_lyrics_commands_have_distinct_lifecycle_locks() {
        assert_eq!(
            PlaybackCommand::parse_hall("后台歌词")
                .expect("background lyrics command")
                .command,
            PlaybackCommand::BackgroundLyrics
        );
        assert_eq!(
            PlaybackCommand::parse_hall("停止歌词")
                .expect("stop background lyrics command")
                .command,
            PlaybackCommand::StopBackgroundLyrics
        );
        assert_ne!(
            PlaybackCommand::BackgroundLyrics.lock_key(),
            PlaybackCommand::StopBackgroundLyrics.lock_key()
        );
    }

    #[test]
    fn single_song_lyrics_command_is_easy_to_distinguish_from_background_lyrics() {
        let parsed = PlaybackCommand::parse_hall("单曲歌词").expect("single-song lyrics command");
        assert_eq!(parsed.command, PlaybackCommand::SingleSongLyrics);
        assert_eq!(parsed.argument, "");
        assert_eq!(parsed.matched, "单曲歌词");
        assert_eq!(
            PlaybackCommand::SingleSongLyrics.lock_key(),
            "lyrics_single_song"
        );
        assert_ne!(
            PlaybackCommand::SingleSongLyrics.lock_key(),
            PlaybackCommand::BackgroundLyrics.lock_key()
        );
        assert_eq!(PlaybackCommand::parse_hall("单曲歌词 1"), None);
    }

    #[test]
    fn playback_confirmation_keeps_a_user_pause_set_during_the_async_play_window() {
        let mut playback = PlaybackRuntimeState::default();
        playback.set_user_paused();
        let request = ActivePlaybackRequest {
            keyword: "song".to_owned(),
            source: "qqmusic".to_owned(),
            prefer_accompaniment: false,
            track: None,
            song: String::new(),
            title: String::new(),
            artist: String::new(),
            requester: String::new(),
            started_at_ms: 1,
            guard_started_at: None,
            expected_session_id: String::new(),
            expected_generation: 0,
        };
        PlaybackStateUpdate::Confirmed {
            request,
            navigation: PlaybackNavigation::Normal,
        }
        .apply(&mut playback);

        // 异步 play 确认返回时用户暂停已生效：引擎实际暂停，状态必须保留暂停语义，
        // 否则暂停的歌不会自然结束，队列自动推进被永久卡住。
        assert_eq!(playback.state, ConfirmedPlaybackState::PausedByUser);
        assert_eq!(playback.pause_reason, PauseReason::User);
        assert!(playback.active_request.is_some());
    }

    #[test]
    fn failed_playback_state_write_keeps_the_previous_runtime_state() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let state_path = service_temp_path("write-blocker", suffix);
        let history_path = service_temp_path("write-blocker-dedup", suffix);
        let request_store =
            RequestStateStore::load(state_path.clone(), crate::test_support::test_state_store())
                .unwrap();
        // 注入 SQLite 写盘故障：状态写入必须失败且内存态保持不变。
        request_store
            .lock()
            .unwrap()
            .inject_write_failure()
            .unwrap();
        let mut service = PlaybackService::new(
            PersistentQueue::from_request_store(request_store.clone(), 10).unwrap(),
            PersistentPlaybackState::from_request_store(request_store).unwrap(),
            PersistentSongDedupHistory::load(
                history_path.clone(),
                Arc::new(SystemClock),
                crate::test_support::test_state_store(),
            )
            .unwrap(),
            Arc::new(RwLock::new(SongDedupConfig::default())),
            0,
        );

        let error = service
            .apply_playback_state_update(PlaybackStateUpdate::UserPaused)
            .expect_err("注入的写故障必须拒绝状态写入");
        let snapshot = service.playback_state_snapshot();

        assert!(error.to_string().contains("注入的写盘故障"));
        assert_eq!(snapshot.state, ConfirmedPlaybackState::Idle);
        assert_eq!(snapshot.pause_reason, PauseReason::None);
        fs::remove_file(&state_path).ok();
        fs::remove_file(format!("{}-wal", state_path.display())).ok();
        fs::remove_file(format!("{}-shm", state_path.display())).ok();
        fs::remove_file(&history_path).ok();
    }

    #[test]
    fn volume_and_lyrics_mode_updates_apply_and_skip_unchanged_values() {
        let mut playback = PlaybackRuntimeState::default();
        assert_eq!(playback.volume, 100);
        assert!(playback.use_translation);

        // 值变化：应用并标记需要持久化。
        assert!(PlaybackStateUpdate::Volume(60).apply(&mut playback));
        assert_eq!(playback.volume, 60);
        assert!(PlaybackStateUpdate::LyricsMode(false).apply(&mut playback));
        assert!(!playback.use_translation);

        // 相同值再次写入：不产生持久化（节流重复写盘）。
        assert!(!PlaybackStateUpdate::Volume(60).apply(&mut playback));
        assert!(!PlaybackStateUpdate::LyricsMode(false).apply(&mut playback));
    }

    fn observation(status: &str, progress: f64, captured_at_ms: u64) -> PlaybackObservation {
        PlaybackObservation {
            status: status.to_string(),
            track: Some(test_track(
                "miliastra://track/qqmusic/obs",
                "观测歌曲 - 测试歌手",
            )),
            title: "观测歌曲".to_string(),
            artist: "测试歌手".to_string(),
            progress,
            duration: 180.0,
            captured_at_ms,
            reliability: ObservationReliability::Reliable,
        }
    }

    #[test]
    fn observation_identity_change_persists_immediately() {
        let mut playback = PlaybackRuntimeState::default();
        assert!(
            PlaybackStateUpdate::Observation(observation("playing", 1.0, 1_000))
                .apply(&mut playback)
        );
        // 状态变化即使进度差异很小也立即持久化。
        assert!(
            PlaybackStateUpdate::Observation(observation("paused", 1.5, 1_100))
                .apply(&mut playback)
        );
        let last = playback.last_observation.as_ref().expect("observation");
        assert_eq!(last.status, "paused");
        assert_eq!(last.progress, 1.5);
    }

    #[test]
    fn observation_progress_below_threshold_skips_persist() {
        let mut playback = PlaybackRuntimeState::default();
        assert!(
            PlaybackStateUpdate::Observation(observation("playing", 10.0, 1_000))
                .apply(&mut playback)
        );

        // 进度推进不足 5 秒且未到兜底间隔：不写盘，保持上次观测，避免高频持久化。
        assert!(
            !PlaybackStateUpdate::Observation(observation("playing", 12.0, 2_000))
                .apply(&mut playback)
        );
        let last = playback.last_observation.as_ref().expect("observation");
        assert_eq!(last.progress, 10.0);
        assert_eq!(last.captured_at_ms, 1_000);
    }

    #[test]
    fn immediate_observation_bypasses_progress_persistence_throttling() {
        let mut playback = PlaybackRuntimeState::default();
        assert!(
            PlaybackStateUpdate::Observation(observation("playing", 10.0, 1_000))
                .apply(&mut playback)
        );

        assert!(
            PlaybackStateUpdate::ImmediateObservation(observation("playing", 12.0, 2_000))
                .apply(&mut playback)
        );
        let last = playback.last_observation.as_ref().expect("observation");
        assert_eq!(last.progress, 12.0);
        assert_eq!(last.captured_at_ms, 2_000);
    }

    #[test]
    fn observation_progress_at_threshold_persists() {
        let mut playback = PlaybackRuntimeState::default();
        assert!(
            PlaybackStateUpdate::Observation(observation("playing", 10.0, 1_000))
                .apply(&mut playback)
        );

        // 进度推进达到 5 秒阈值：即使间隔小于兜底间隔也写盘。
        assert!(
            PlaybackStateUpdate::Observation(observation("playing", 15.0, 2_000))
                .apply(&mut playback)
        );
        let last = playback.last_observation.as_ref().expect("observation");
        assert_eq!(last.progress, 15.0);
        assert_eq!(last.captured_at_ms, 2_000);
    }

    #[test]
    fn observation_refreshes_when_capture_age_reaches_fallback_interval() {
        let mut playback = PlaybackRuntimeState::default();
        assert!(
            PlaybackStateUpdate::Observation(observation("playing", 10.0, 1_000))
                .apply(&mut playback)
        );

        // 进度推进不足阈值但观测时间差达到兜底间隔：定期刷新，不永久冻结。
        assert!(
            PlaybackStateUpdate::Observation(observation(
                "playing",
                11.0,
                1_000 + OBSERVATION_PERSIST_FALLBACK_INTERVAL_MS,
            ))
            .apply(&mut playback)
        );
        let last = playback.last_observation.as_ref().expect("observation");
        assert_eq!(last.progress, 11.0);
        assert_eq!(
            last.captured_at_ms,
            1_000 + OBSERVATION_PERSIST_FALLBACK_INTERVAL_MS
        );
    }

    #[test]
    fn observation_duration_change_persists_immediately() {
        let mut playback = PlaybackRuntimeState::default();
        assert!(
            PlaybackStateUpdate::Observation(observation("playing", 10.0, 1_000))
                .apply(&mut playback)
        );

        let mut changed = observation("playing", 10.5, 2_000);
        changed.duration = 200.0;
        assert!(PlaybackStateUpdate::Observation(changed).apply(&mut playback));
        assert_eq!(
            playback
                .last_observation
                .as_ref()
                .expect("observation")
                .duration,
            200.0
        );
    }

    #[test]
    fn observation_requires_persist_accepts_exact_threshold_boundaries() {
        let previous = observation("playing", 10.0, 1_000);
        // 差 5.0 秒：达到阈值。
        let at_threshold = observation("playing", 15.0, 2_000);
        assert!(observation_requires_persist(Some(&previous), &at_threshold));
        // 差 4.999 秒：低于阈值，且间隔未到兜底。
        let below_threshold = observation("playing", 14.999, 2_000);
        assert!(!observation_requires_persist(
            Some(&previous),
            &below_threshold
        ));
        // 无上次观测：首次观测必须持久化。
        assert!(observation_requires_persist(None, &previous));
    }

    fn service_temp_path(name: &str, suffix: u128) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "miliastra-playback-service-{}-{}-{suffix}.json",
            std::process::id(),
            name
        ))
    }

    /// 队列项播放确认成功后到删除持久化前进程退出，重启后不会再次播放同一队首：
    /// 确认与出队在同一笔持久化中完成，重载后队首直接是下一首。
    #[test]
    fn confirmed_dequeue_survives_reload_without_replaying_the_head() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let state_path = service_temp_path("crash-state", suffix);
        let history_path = service_temp_path("crash-dedup", suffix);
        let wall_clock: Arc<dyn WallClock> = Arc::new(SystemClock);
        let track_a = test_track("miliastra://track/qqmusic/a", "歌曲A - 歌手A");
        let track_b = test_track("miliastra://track/qqmusic/b", "歌曲B - 歌手B");
        let dedup = Arc::new(RwLock::new(SongDedupConfig {
            history_path: history_path.clone(),
            ..SongDedupConfig::default()
        }));

        let mut service = PlaybackService::load(
            state_path.clone(),
            history_path.clone(),
            10,
            0,
            dedup.clone(),
            wall_clock.clone(),
            crate::test_support::test_state_store(),
        )
        .unwrap();
        service
            .push_queue(QueueItem {
                keyword: "歌曲A".to_string(),
                track: Some(track_a.clone()),
                ..QueueItem::default()
            })
            .unwrap();
        service
            .push_queue(QueueItem {
                keyword: "歌曲B".to_string(),
                track: Some(track_b.clone()),
                ..QueueItem::default()
            })
            .unwrap();
        let head_id = service.queue_snapshot()[0].id;
        assert_eq!(head_id, 1);

        // 队列消费起播：进入 Starting。
        let started_at_ms = 4242;
        service
            .apply_playback_state_update(PlaybackStateUpdate::Starting {
                request: ActivePlaybackRequest {
                    keyword: "歌曲A".to_string(),
                    source: "qqmusic".to_string(),
                    track: Some(track_a.clone()),
                    started_at_ms,
                    ..ActivePlaybackRequest::default()
                },
                navigation: PlaybackNavigation::Normal,
            })
            .unwrap();

        // 确认播放成功：与队首出队在同一事务内落盘。
        service
            .confirm_playback_and_dequeue(
                PlaybackStateUpdate::Confirmed {
                    request: ActivePlaybackRequest {
                        keyword: "歌曲A".to_string(),
                        source: "qqmusic".to_string(),
                        track: Some(track_a.clone()),
                        started_at_ms,
                        ..ActivePlaybackRequest::default()
                    },
                    navigation: PlaybackNavigation::Normal,
                },
                Some(head_id),
            )
            .unwrap();

        // 内存缓存与落盘一致：队首已是 B，播放状态已确认 A。
        let queue = service.queue_snapshot();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].keyword, "歌曲B");
        let playback = service.playback_state_snapshot();
        assert_eq!(playback.state, ConfirmedPlaybackState::RequestedSongPlaying);
        assert_eq!(
            playback
                .active_request
                .as_ref()
                .unwrap()
                .track
                .as_ref()
                .unwrap()
                .track_ref
                .key,
            track_a.track_ref.key
        );

        // 模拟进程崩溃退出后重启：重载同一文件。
        drop(service);
        let reloaded = PlaybackService::load(
            state_path.clone(),
            history_path.clone(),
            10,
            0,
            dedup,
            wall_clock,
            crate::test_support::test_state_store(),
        )
        .unwrap();
        let queue = reloaded.queue_snapshot();
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue[0].keyword, "歌曲B",
            "已确认消费的队首 A 不得在重启后残留"
        );
        let playback = reloaded.playback_state_snapshot();
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
        // 重启后再次消费：直接从 B 开始，A 不会重播。
        assert_eq!(reloaded.queue_snapshot()[0].id, head_id + 1);

        fs::remove_file(&state_path).ok();
        fs::remove_file(format!("{}-wal", state_path.display())).ok();
        fs::remove_file(format!("{}-shm", state_path.display())).ok();
        fs::remove_file(&history_path).ok();
    }

    /// 播放确认失败前不得丢歌：确认事务写盘失败时整体回滚，
    /// 队首保留且播放状态未确认，重启后仍可重播。
    #[test]
    fn failed_confirmed_dequeue_keeps_the_head_for_replay() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let state_path = service_temp_path("keep-head-state", suffix);
        let history_path = service_temp_path("keep-head-dedup", suffix);
        let wall_clock: Arc<dyn WallClock> = Arc::new(SystemClock);
        let track_a = test_track("miliastra://track/qqmusic/a", "歌曲A - 歌手A");

        let mut service = PlaybackService::load(
            state_path.clone(),
            history_path.clone(),
            10,
            0,
            Arc::new(RwLock::new(SongDedupConfig {
                history_path: history_path.clone(),
                ..SongDedupConfig::default()
            })),
            wall_clock.clone(),
            crate::test_support::test_state_store(),
        )
        .unwrap();
        service
            .push_queue(QueueItem {
                keyword: "歌曲A".to_string(),
                track: Some(track_a.clone()),
                ..QueueItem::default()
            })
            .unwrap();
        let head_id = service.queue_snapshot()[0].id;

        // 注入 SQLite 写盘故障：确认+出队事务持久化失败，整体回滚。
        service
            .request_state
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .inject_write_failure()
            .unwrap();
        assert!(
            service
                .confirm_playback_and_dequeue(
                    PlaybackStateUpdate::Confirmed {
                        request: ActivePlaybackRequest {
                            keyword: "歌曲A".to_string(),
                            source: "qqmusic".to_string(),
                            track: Some(track_a.clone()),
                            started_at_ms: 7,
                            ..ActivePlaybackRequest::default()
                        },
                        navigation: PlaybackNavigation::Normal,
                    },
                    Some(head_id),
                )
                .is_err()
        );
        // 内存缓存整体回滚：队首保留，播放状态未确认（不丢歌、无部分写入）。
        assert_eq!(service.queue_snapshot().len(), 1);
        assert_eq!(service.queue_snapshot()[0].id, head_id);
        assert_ne!(
            service.playback_state_snapshot().state,
            ConfirmedPlaybackState::RequestedSongPlaying
        );

        // 故障恢复后重载（模拟重启）：磁盘仍是事务前状态，队首可重播。
        drop(service);
        let reloaded = PlaybackService::load(
            state_path.clone(),
            history_path.clone(),
            10,
            0,
            Arc::new(RwLock::new(SongDedupConfig {
                history_path: history_path.clone(),
                ..SongDedupConfig::default()
            })),
            wall_clock,
            crate::test_support::test_state_store(),
        )
        .unwrap();
        assert_eq!(reloaded.queue_snapshot().len(), 1);
        assert_eq!(reloaded.queue_snapshot()[0].id, head_id);
        assert_eq!(
            reloaded.playback_state_snapshot().state,
            ConfirmedPlaybackState::Idle,
            "确认未提交时播放状态保持空闲，队首等待重新播放"
        );

        fs::remove_file(&state_path).ok();
        fs::remove_file(format!("{}-wal", state_path.display())).ok();
        fs::remove_file(format!("{}-shm", state_path.display())).ok();
        fs::remove_file(&history_path).ok();
    }

    /// 热更新共享 SongDedupConfig 后，去重判定必须立即变化（不重启）。
    #[test]
    fn song_dedup_config_hot_reload_changes_limited_judgement() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let state_path = service_temp_path("dedup-hot-state", suffix);
        let history_path = service_temp_path("dedup-hot-history", suffix);
        let song_dedup = Arc::new(RwLock::new(SongDedupConfig {
            history_path: history_path.clone(),
            ..SongDedupConfig::default()
        }));
        // 初始关闭去重：任何候选都不受限。
        song_dedup.write().unwrap().enabled = false;
        let mut service = PlaybackService::load(
            state_path.clone(),
            history_path.clone(),
            10,
            0,
            song_dedup.clone(),
            Arc::new(SystemClock),
            crate::test_support::test_state_store(),
        )
        .unwrap();
        let track = test_track("miliastra://track/qqmusic/hot", "热更新歌曲 - 歌手A");
        let candidate = SongDedupCandidate {
            track_key: track.track_ref.key,
            title: "热更新歌曲".to_string(),
            artist: "歌手A".to_string(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
        };
        assert!(!service.song_dedup_limited(&candidate));
        // 热更新共享值：启用去重、窗口 3600s、最多 1 次。
        {
            let mut config = song_dedup.write().unwrap();
            config.enabled = true;
            config.window_seconds = 3600;
            config.max_count = 1;
        }
        // 启用后记录一次播放 → 同一首歌立即受限（判定读取共享值，未重启）。
        service.record_song_dedup(candidate.clone()).unwrap();
        assert!(service.song_dedup_limited(&candidate));
        // 热更新 max_count=2 → 判定立即变化，不再受限。
        song_dedup.write().unwrap().max_count = 2;
        assert!(!service.song_dedup_limited(&candidate));

        fs::remove_file(&state_path).ok();
        fs::remove_file(format!("{}-wal", state_path.display())).ok();
        fs::remove_file(format!("{}-shm", state_path.display())).ok();
        fs::remove_file(&history_path).ok();
    }
}
