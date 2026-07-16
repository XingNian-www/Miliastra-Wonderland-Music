use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

mod controller;
mod dedup;
mod format;
mod matcher;
mod queue;
mod state;

use crate::config::{MatchConfig, SongDedupConfig};
use crate::features::chat_text::{CommandSyntax, command_identity, parse_prefixed_command};
pub(crate) use controller::{
    MismatchDecision, MusicPlayerBackend, PlaybackAttempt, PlaybackOutcome, PlaybackRequest,
    PlaybackVerification, PlayerController, PlayerRuntimeBackend, QueueAdvanceContext,
    QueueAdvanceDecision,
};
pub(crate) use dedup::{PersistentSongDedupHistory, SongDedupCandidate};
pub(crate) use format::{
    PlaybackSnapshot, estimated_player_status, format_lyrics, format_play_message, format_status,
    is_playing, song_title,
};
pub(crate) use queue::{PersistentQueue, QueueItem};
pub(crate) use state::{
    ActivePlaybackRequest, ConfirmedPlaybackState, HALL_EXPIRING_WARNING_MINUTES, PauseReason,
    PersistentRuntimeState, PlaybackObservation, PlaybackRuntimeState, RuntimeState,
};

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RuntimeStatePatch {
    pub(crate) hall_remaining_minutes: Option<Option<u32>>,
    pub(crate) hall_remaining_updated_at: Option<Option<u64>>,
    pub(crate) hall_expiring_warning_sent: Option<bool>,
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
    runtime_state: PersistentRuntimeState,
    song_dedup_history: PersistentSongDedupHistory,
    matching: MatchConfig,
    song_dedup: SongDedupConfig,
    external_playback_tracker: controller::ExternalPlaybackTracker,
}

impl PlaybackService {
    pub(crate) fn new(
        queue: PersistentQueue,
        runtime_state: PersistentRuntimeState,
        song_dedup_history: PersistentSongDedupHistory,
        matching: MatchConfig,
        song_dedup: SongDedupConfig,
    ) -> Self {
        Self {
            queue,
            runtime_state,
            song_dedup_history,
            matching,
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

    pub(crate) fn runtime_state_snapshot(&self) -> RuntimeState {
        self.runtime_state.state().clone()
    }

    pub(crate) fn patch_runtime_state(&mut self, patch: RuntimeStatePatch) -> Result<()> {
        let state = self.runtime_state.state_mut();
        if let Some(value) = patch.hall_remaining_minutes {
            state.hall_remaining_minutes = value;
        }
        if let Some(value) = patch.hall_remaining_updated_at {
            state.hall_remaining_updated_at = value;
        }
        if let Some(value) = patch.hall_expiring_warning_sent {
            state.hall_expiring_warning_sent = value;
        }
        self.runtime_state.save()
    }

    pub(crate) fn update_hall_remaining_minutes(&mut self, minutes: u32) -> Result<()> {
        self.runtime_state
            .state_mut()
            .update_hall_remaining_minutes(minutes);
        self.runtime_state.save()
    }

    pub(crate) fn clear_hall_remaining_minutes(&mut self) -> Result<()> {
        self.runtime_state
            .state_mut()
            .clear_hall_remaining_minutes();
        self.runtime_state.save()
    }

    pub(crate) fn clear_hall_countdown_cache(&mut self) -> Result<bool> {
        let cleared = self.runtime_state.state_mut().clear_hall_countdown_cache();
        if cleared {
            self.runtime_state.save()?;
        }
        Ok(cleared)
    }

    pub(crate) fn song_dedup_limited(&self, candidate: &SongDedupCandidate) -> bool {
        self.song_dedup_history
            .is_limited(&self.song_dedup, &self.matching, candidate)
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
        let playback = &mut self.runtime_state.state_mut().playback;
        let changed = update.apply(playback);
        if changed {
            self.runtime_state.save()?;
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
