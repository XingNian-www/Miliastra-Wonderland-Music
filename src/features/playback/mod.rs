use anyhow::{Result, bail};
use miliastra_playback::PlayableTrack;
#[cfg(test)]
use miliastra_playback::{
    PlaybackEligibility, ProviderId, SearchCandidate, TrackKey, TrackMetadata, TrackRef,
};
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
use crate::runtime::clock::WallClock;
pub(crate) use controller::{
    MusicPlayerBackend, PlaybackIdentityDecision, PlaybackIdentityJudge, PlaybackNavigation,
    PlaybackOutcome, PlaybackRequest, PlaybackStatePort, PlaybackTimePorts, PlaybackVerification,
    PlayerController, QueueAdvanceContext, QueueAdvanceDecision,
};
pub(crate) use dedup::{PersistentSongDedupHistory, SongDedupCandidate};
pub(crate) use format::{
    PlaybackSnapshot, estimated_player_status, format_lyrics, format_play_message, format_status,
    is_playing,
};
pub(crate) use queue::{PersistentQueue, QueueItem};
pub(crate) use state::{
    ActivePlaybackRequest, ConfirmedPlaybackState, ControlOperationRecord, PauseReason,
    PersistentPlaybackState, PlaybackAttemptRecord, PlaybackObservation, PlaybackRuntimeState,
    PlaybackSessionBinding, RequestStateStore, SessionReconciliation,
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
    #[serde(default = "default_fallback_identity_stable_samples")]
    pub fallback_identity_stable_samples: u32,
    #[serde(deserialize_with = "deserialize_positive_u64")]
    pub stale_timeout_ms: u64,
}

fn default_fallback_identity_stable_samples() -> u32 {
    2
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
            (
                self.fallback_identity_stable_samples,
                "timing.playback.fallback_identity_stable_samples",
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
    pub(crate) fn match_song_identity(
        &self,
        request: &str,
        observed_title: &str,
        observed_artist: &str,
    ) -> matcher::SongIdentityMatch {
        matcher::match_song_identity(self, request, observed_title, observed_artist)
    }

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
    /// Session reference carried by playerd terminal outcomes.
    pub(crate) session_id: String,
    pub(crate) generation: u64,
    /// Playerd end behavior and durable terminal outcome metadata.
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
                "上一首" | "上一曲" => Self::Previous,
                "音量" => Self::Volume(argument.to_string()),
                "状态" => Self::Status,
                "停止歌词" => Self::StopBackgroundLyrics,
                "后台歌词" => Self::BackgroundLyrics,
                "单曲歌词" => Self::SingleSongLyrics,
                "持续歌词" => Self::ContinuousLyrics,
                "歌词" if argument.is_empty() => Self::Lyrics,
                "歌词" => Self::LyricsFor(parse_lyrics_duration(argument)?),
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

    #[cfg(test)]
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
    External,
    Starting {
        request: ActivePlaybackRequest,
        navigation: PlaybackNavigation,
    },
    ClearPauseReason,
    MarkRequestedPlayingIfActive,
    PauseWaitingForQueue,
    ResumeWaitingForQueue,
    Confirmed {
        request: ActivePlaybackRequest,
        navigation: PlaybackNavigation,
    },
    Reconciled {
        request: ActivePlaybackRequest,
    },
    Observation(PlaybackObservation),
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
            Self::Confirmed {
                request,
                navigation,
            } => {
                if navigation == PlaybackNavigation::Previous {
                    playback.remove_previous_request(&request);
                } else {
                    playback.remember_current_playback();
                }
                playback.state = ConfirmedPlaybackState::RequestedSongPlaying;
                playback.pause_reason = PauseReason::None;
                playback.active_request = Some(request);
                true
            }
            Self::Reconciled { request } => {
                if playback.active_request.is_none() {
                    return false;
                }
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
    song_dedup: SongDedupConfig,
    external_playback_tracker: controller::ExternalPlaybackTracker,
}

impl PlaybackService {
    pub(crate) fn load(
        playback_state_path: PathBuf,
        song_dedup_history_path: PathBuf,
        queue_max_size: usize,
        song_dedup: SongDedupConfig,
        wall_clock: Arc<dyn WallClock>,
    ) -> Result<Self> {
        let request_state = RequestStateStore::load(playback_state_path)?;
        let queue = PersistentQueue::from_request_store(request_state.clone(), queue_max_size)?;
        let playback_state = PersistentPlaybackState::from_request_store(request_state.clone())?;
        let song_dedup_history =
            PersistentSongDedupHistory::load(song_dedup_history_path, wall_clock)?;
        log::info!("已加载队列: {} 首", queue.len());
        log::info!("已加载长时间同歌去重历史: {} 条", song_dedup_history.len());
        log::info!(
            "已加载播放状态: playback_state={:?}",
            playback_state.state().state
        );
        let mut service = Self::new(queue, playback_state, song_dedup_history, song_dedup);
        service.request_state = Some(request_state);
        Ok(service)
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
            request_state: None,
            song_dedup_history,
            song_dedup,
            external_playback_tracker: controller::ExternalPlaybackTracker::default(),
        }
    }

    pub(crate) fn queue_snapshot(&self) -> Vec<QueueItem> {
        self.queue.items().to_vec()
    }

    pub(crate) fn queue_contains(&self, item: &QueueItem) -> bool {
        if let Some(track) = item.track.as_ref() {
            return self.queue.has_duplicate_track(&track.track_ref.key);
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
        self.playback_state
            .update(|playback| update.apply(playback))
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

fn observation_identity_changed(
    previous: Option<&PlaybackObservation>,
    current: &PlaybackObservation,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    previous.status != current.status
        || previous.track != current.track
        || previous.title != current.title
        || previous.artist != current.artist
        || previous.reliability != current.reliability
}

pub(crate) use application::{
    LyricTracker, PlaybackApplication, PlaybackApplicationConfig, PlaybackCommandContext,
    PlaybackCommandPort, PlaybackExecutionPort, PlaybackMonitorPort, PlaybackPickedCandidate,
    PlaybackResult, PlaybackSearchFailure, PlaybackSelection, PlaybackWorkload,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::clock::SystemClock;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

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
    fn timed_lyrics_uses_a_different_lock_from_one_shot_lyrics() {
        assert_ne!(
            PlaybackCommand::Lyrics.lock_key(),
            PlaybackCommand::LyricsFor(5).lock_key()
        );
        assert!(!PlaybackCommand::Lyrics.same_request(&PlaybackCommand::LyricsFor(5)));
        assert!(PlaybackCommand::LyricsFor(5).same_request(&PlaybackCommand::LyricsFor(300)));
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
    fn failed_playback_state_write_keeps_the_previous_runtime_state() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let blocker = std::env::temp_dir().join(format!(
            "miliastra-playback-write-blocker-{}-{suffix}",
            std::process::id()
        ));
        fs::write(&blocker, "not a directory").unwrap();
        let state_path = blocker.join("playback-state.json");
        let queue_path = std::env::temp_dir().join(format!(
            "miliastra-playback-queue-unused-{}-{suffix}.json",
            std::process::id()
        ));
        let history_path = std::env::temp_dir().join(format!(
            "miliastra-playback-history-unused-{}-{suffix}.json",
            std::process::id()
        ));
        let mut service = PlaybackService::new(
            PersistentQueue::load(queue_path, 10).unwrap(),
            PersistentPlaybackState::load(state_path).unwrap(),
            PersistentSongDedupHistory::load(history_path, Arc::new(SystemClock)).unwrap(),
            SongDedupConfig::default(),
        );

        let error = service
            .apply_playback_state_update(PlaybackStateUpdate::UserPaused)
            .expect_err("blocked parent must reject the state write");
        let snapshot = service.playback_state_snapshot();

        assert!(error.to_string().contains("播放状态目录"));
        assert_eq!(snapshot.state, ConfirmedPlaybackState::Idle);
        assert_eq!(snapshot.pause_reason, PauseReason::None);
        fs::remove_file(blocker).unwrap();
    }
}
