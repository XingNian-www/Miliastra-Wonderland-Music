use anyhow::{Result, bail};
use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod application;
mod controller;
mod dedup;
mod format;
mod matcher;
mod queue;
mod state;

use crate::features::chat_text::{CommandSyntax, command_identity, parse_prefixed_command};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
use crate::runtime::clock::WallClock;
pub(crate) use controller::{
    MismatchDecision, MusicPlayerBackend, PlaybackAttempt, PlaybackOutcome, PlaybackRequest,
    PlaybackStatePort, PlaybackTimePorts, PlaybackVerification, PlayerController,
    QueueAdvanceContext, QueueAdvanceDecision,
};
pub(crate) use dedup::{PersistentSongDedupHistory, SongDedupCandidate};
pub(crate) use format::{
    PlaybackSnapshot, estimated_player_status, format_lyrics, format_play_message, format_status,
    is_playing, song_title,
};
pub(crate) use queue::{PersistentQueue, QueueItem};
pub(crate) use state::{
    ActivePlaybackRequest, ConfirmedPlaybackState, PauseReason, PersistentPlaybackState,
    PlaybackObservation, PlaybackRuntimeState,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackTimingConfig {
    pub search_settle_ms: u64,
    pub status_poll_ms: u64,
    pub status_retries: u32,
    pub skip_status_initial_ms: u64,
    pub skip_status_poll_ms: u64,
    pub skip_status_retries: u32,
    pub monitor_tick_ms: u64,
    pub monitor_status_ms: u64,
    pub uri_stable_samples: u32,
    pub transport_stable_samples: u32,
    #[serde(deserialize_with = "deserialize_positive_u64")]
    pub stale_timeout_ms: u64,
}

impl PlaybackTimingConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        for (value, field) in [
            (self.status_poll_ms, "timing.playback.status_poll_ms"),
            (
                self.skip_status_poll_ms,
                "timing.playback.skip_status_poll_ms",
            ),
            (self.monitor_tick_ms, "timing.playback.monitor_tick_ms"),
            (self.monitor_status_ms, "timing.playback.monitor_status_ms"),
            (self.stale_timeout_ms, "timing.playback.stale_timeout_ms"),
        ] {
            if value == 0 {
                bail!("{} 必须大于 0", field);
            }
        }
        for (value, field) in [
            (self.status_retries, "timing.playback.status_retries"),
            (
                self.skip_status_retries,
                "timing.playback.skip_status_retries",
            ),
        ] {
            if value == 0 {
                bail!("{} 必须大于 0", field);
            }
        }
        Ok(())
    }
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
    pub auto_advance_seconds: u64,
    pub protect_current_song_until_finished: bool,
    pub external_playback_protect_after_seconds: u64,
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
            history_path: PathBuf::from("data/song-dedup-history.json"),
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum PlaybackCommand {
    Pause,
    Resume,
    Play,
    Next,
    Previous,
    Volume(String),
    Status,
    Lyrics,
    Queue,
    QueueDelete(Vec<usize>),
    QueueClear,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct PlayerStatus {
    pub(crate) status: String,
    pub(crate) current_uri: String,
    pub(crate) name: String,
    pub(crate) singer: String,
    pub(crate) album_name: String,
    pub(crate) lyric_line_text: String,
    pub(crate) duration: f64,
    pub(crate) progress: f64,
    pub(crate) playback_rate: f64,
    pub(crate) volume: i64,
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
    pub(crate) progress: f64,
    pub(crate) duration: f64,
    pub(crate) observed_at_ms: u64,
}

impl PlaybackCommand {
    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::At
            && envelope.authority() == CommandAuthority::HallMember
            && PLAYBACK_COMMAND_PREFIXES
                .iter()
                .any(|prefix| envelope.command_text().starts_with(prefix))
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
            ("上一首", false),
            ("上一曲", false),
            ("暂停", false),
            ("继续", false),
            ("恢复", false),
            ("播放", false),
            ("音量", true),
            ("状态", false),
            ("歌词", false),
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
                "上一首" | "上一曲" => Self::Previous,
                "音量" => Self::Volume(argument.to_string()),
                "状态" => Self::Status,
                "歌词" => Self::Lyrics,
                "队列" | "列表" => Self::Queue,
                "队列删除" => Self::QueueDelete(parse_queue_indexes(argument)),
                "队列清空" => Self::QueueClear,
                _ => unreachable!("all playback prefixes are handled"),
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
            Self::Previous => "previous".to_string(),
            Self::Volume(volume) => format!("volume:{}", command_identity(volume)),
            Self::Status => "status".to_string(),
            Self::Lyrics => "lyrics".to_string(),
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

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Volume(left), Self::Volume(right)) => {
                command_identity(left) == command_identity(right)
            }
            (Self::QueueDelete(left), Self::QueueDelete(right)) => left == right,
            _ => self.lock_key() == other.lock_key(),
        }
    }
}

const PLAYBACK_COMMAND_PREFIXES: &[&str] = &[
    "队列删除",
    "队列清空",
    "下一首",
    "下一曲",
    "上一首",
    "上一曲",
    "暂停",
    "继续",
    "恢复",
    "播放",
    "音量",
    "状态",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct QueuePushOutcome {
    pub(crate) accepted: bool,
    pub(crate) size: usize,
}

pub(crate) enum PlaybackMutationIntent {
    Push(QueueItem),
    Remove(QueueRemoval),
    Clear,
}

pub(crate) enum PlaybackMutationOutcome {
    Pushed(QueuePushOutcome),
    Removed(QueueRemoveOutcome),
    Cleared,
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
        item: QueueItem,
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
    External,
    Starting(ActivePlaybackRequest),
    ClearPauseReason,
    MarkRequestedPlayingIfActive,
    PauseWaitingForQueue,
    ResumeWaitingForQueue,
    Confirmed(ActivePlaybackRequest),
    Observation(PlaybackObservation),
    Unknown,
    Restore(PlaybackRuntimeState),
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
            Self::External => {
                playback.state = ConfirmedPlaybackState::ExternalPlayback;
                playback.pause_reason = PauseReason::None;
                playback.active_request = None;
                true
            }
            Self::Starting(request) => {
                playback.state = ConfirmedPlaybackState::Starting;
                playback.pause_reason = PauseReason::None;
                playback.active_request = Some(request);
                true
            }
            Self::ClearPauseReason => {
                playback.pause_reason = PauseReason::None;
                true
            }
            Self::MarkRequestedPlayingIfActive => {
                playback.pause_reason = PauseReason::None;
                if playback.active_request.is_some() {
                    playback.state = ConfirmedPlaybackState::RequestedSongPlaying;
                }
                true
            }
            Self::PauseWaitingForQueue => {
                playback.pause_reason = PauseReason::WaitingForQueue;
                playback.state = ConfirmedPlaybackState::PausedWaitingForQueue;
                true
            }
            Self::ResumeWaitingForQueue => {
                playback.pause_reason = PauseReason::None;
                playback.state = if playback.active_request.is_some() {
                    ConfirmedPlaybackState::RequestedSongPlaying
                } else {
                    ConfirmedPlaybackState::ExternalPlayback
                };
                true
            }
            Self::Confirmed(request) => {
                playback.state = ConfirmedPlaybackState::RequestedSongPlaying;
                playback.pause_reason = PauseReason::None;
                playback.active_request = Some(request);
                true
            }
            Self::Observation(observation) => {
                if !observation_identity_changed(playback.last_observation.as_ref(), &observation) {
                    false
                } else {
                    playback.last_observation = Some(observation);
                    true
                }
            }
            Self::Unknown => {
                playback.state = ConfirmedPlaybackState::Unknown;
                playback.pause_reason = PauseReason::None;
                playback.active_request = None;
                true
            }
            Self::Restore(previous) => {
                *playback = previous;
                true
            }
        }
    }
}

pub(crate) struct PlaybackService {
    queue: PersistentQueue,
    playback_state: PersistentPlaybackState,
    song_dedup_history: PersistentSongDedupHistory,
    song_dedup: SongDedupConfig,
    external_playback_tracker: controller::ExternalPlaybackTracker,
}

impl PlaybackService {
    pub(crate) fn load(
        queue_path: PathBuf,
        playback_state_path: PathBuf,
        song_dedup_history_path: PathBuf,
        queue_max_size: usize,
        song_dedup: SongDedupConfig,
        wall_clock: Arc<dyn WallClock>,
    ) -> Result<Self> {
        let queue = PersistentQueue::load(queue_path, queue_max_size)?;
        let playback_state = PersistentPlaybackState::load(playback_state_path)?;
        let song_dedup_history =
            PersistentSongDedupHistory::load(song_dedup_history_path, wall_clock)?;
        log::info!("已加载队列: {} 首", queue.len());
        log::info!("已加载长时间同歌去重历史: {} 条", song_dedup_history.len());
        log::info!(
            "已加载播放状态: playback_state={:?}",
            playback_state.state().state
        );
        Ok(Self::new(
            queue,
            playback_state,
            song_dedup_history,
            song_dedup,
        ))
    }

    pub(crate) fn new(
        queue: PersistentQueue,
        playback_state: PersistentPlaybackState,
        song_dedup_history: PersistentSongDedupHistory,
        song_dedup: SongDedupConfig,
    ) -> Self {
        Self {
            queue,
            playback_state,
            song_dedup_history,
            song_dedup,
            external_playback_tracker: controller::ExternalPlaybackTracker::default(),
        }
    }

    pub(crate) fn queue_snapshot(&self) -> Vec<QueueItem> {
        self.queue.items().to_vec()
    }

    pub(crate) fn queue_contains(&self, item: &QueueItem) -> bool {
        if !item.uri.trim().is_empty() {
            return self.queue.has_duplicate_uri(&item.uri);
        }
        self.queue
            .has_duplicate(&item.keyword, &item.source, item.prefer_accompaniment)
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
            item: removed.1,
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

    pub(crate) fn playback_state_snapshot(&self) -> PlaybackRuntimeState {
        self.playback_state.state().clone()
    }

    pub(crate) fn song_dedup_limited(&self, candidate: &SongDedupCandidate) -> bool {
        self.song_dedup_history
            .is_limited(&self.song_dedup, candidate)
    }

    pub(crate) fn record_song_dedup(&mut self, candidate: SongDedupCandidate) -> Result<()> {
        self.song_dedup_history
            .record_playback(&self.song_dedup, candidate)
    }

    pub(crate) fn observe_external_playback(
        &mut self,
        identity: &str,
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
        let changed = update.apply(self.playback_state.state_mut());
        if changed {
            self.playback_state.save()?;
        }
        Ok(changed)
    }
}

fn observation_identity_changed(
    previous: Option<&PlaybackObservation>,
    current: &PlaybackObservation,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    previous.status != current.status
        || previous.uri != current.uri
        || previous.title != current.title
        || previous.artist != current.artist
        || previous.reliability != current.reliability
}
pub(crate) use application::{
    PlaybackApplication, PlaybackApplicationConfig, PlaybackCommandContext, PlaybackCommandPort,
    PlaybackDecision, PlaybackExecutionPort, PlaybackMonitorPort, PlaybackPickedCandidate,
    PlaybackSearchFailure, PlaybackSelection, PlaybackWorkload,
};
