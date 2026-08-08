use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use miliastra_playback::{PlayableTrack, TrackKey};

use crate::features::song_request::SearchCandidate;

use super::dedup::SongDedupCandidate;
use super::format::{
    format_play_message, format_time, playback_progress_restarted, playback_remaining_seconds,
};
use super::state::{
    ActivePlaybackRequest, ConfirmedPlaybackState, ObservationReliability, PauseReason,
    PlaybackObservation, PlaybackRuntimeState, PlaybackSessionBinding, SessionReconciliation,
};
use crate::features::playback::{
    MatchConfig, PlaybackControllerSnapshot, PlaybackStateUpdate, PlaybackTimingConfig,
    PlayerStatus, QueueConfig,
};
use crate::runtime::clock::{Clock, Delay, WallClock};

pub(crate) trait MusicPlayerBackend: Clone + Send + Sync + 'static {
    fn status(&self) -> Result<PlayerStatus>;
    fn play(&self, track: &PlayableTrack) -> Result<String>;
    /// Reports a provider-level "this candidate cannot be played" error.
    ///
    /// Backends use this hook to preserve the distinction between a stale or
    /// ineligible song and an unavailable player/transport. The default keeps
    /// existing backends unchanged.
    fn is_track_unavailable_error(&self, _error: &anyhow::Error) -> bool {
        false
    }
    fn pause(&self) -> Result<String>;
    fn resume(&self) -> Result<String>;
    fn next(&self) -> Result<String>;
    fn previous(&self) -> Result<String>;
    fn set_volume(&self, volume: &str) -> Result<String>;
}

pub(crate) trait PlaybackStatePort: Clone + Send + Sync + 'static {
    fn snapshot(&self) -> Result<PlaybackRuntimeState>;
    fn update(&self, update: PlaybackStateUpdate) -> Result<bool>;
    fn song_dedup_limited(&self, candidate: SongDedupCandidate) -> Result<bool>;
    fn record_song_dedup(&self, candidate: SongDedupCandidate) -> Result<()>;
    fn observe_external_playback(
        &self,
        identity: String,
        now: Instant,
        protect_after: Duration,
    ) -> Result<super::ExternalPlaybackObservation>;
    fn clear_external_playback_tracker(&self) -> Result<()>;
    /// Records and compares the playback runtime/session responsible for an
    /// active request. The persistent implementation makes a process restart
    /// distinguishable from an ordinary stopped transport sample.
    fn reconcile_player_session(
        &self,
        _binding: Option<PlaybackSessionBinding>,
    ) -> Result<SessionReconciliation> {
        Ok(SessionReconciliation::Unknown)
    }
    /// Atomically claims a terminal outcome. Non-persistent test ports retain
    /// the old one-shot semantics; the production state store deduplicates it.
    fn claim_terminal_outcome(
        &self,
        _request_id: u64,
        _outcome: String,
        _handled_at_ms: u64,
    ) -> Result<bool> {
        Ok(true)
    }
    fn record_playback_attempt(
        &self,
        _provider: String,
        _locator: String,
        _started_at_ms: u64,
        _result: String,
    ) -> Result<()> {
        Ok(())
    }
    fn record_control_operation(
        &self,
        _operation: String,
        _requested_at_ms: u64,
        _completed: bool,
    ) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PlaybackIdentityDecision {
    Match { score: f64, reason: String },
    NoMatch { score: f64, reason: String },
    Unavailable { reason: String },
}

enum CrossSourceReconciliation {
    Pending,
    Match {
        status: PlayerStatus,
        score: f64,
        reason: String,
    },
    NoMatch {
        status: PlayerStatus,
        score: f64,
        reason: String,
    },
    Unavailable {
        status: PlayerStatus,
        reason: String,
    },
}

pub(crate) trait PlaybackIdentityJudge: Send + Sync {
    fn judge(&self, request: &PlaybackRequest, status: &PlayerStatus) -> PlaybackIdentityDecision;
}

#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct DisabledPlaybackIdentityJudge;

impl PlaybackIdentityJudge for DisabledPlaybackIdentityJudge {
    fn judge(
        &self,
        _request: &PlaybackRequest,
        _status: &PlayerStatus,
    ) -> PlaybackIdentityDecision {
        PlaybackIdentityDecision::Unavailable {
            reason: "跨源同曲判断未启用".to_string(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct PlayerController<B: MusicPlayerBackend, S: PlaybackStatePort> {
    backend: B,
    playback_state: S,
    timing: PlaybackTimingConfig,
    queue: QueueConfig,
    matching: MatchConfig,
    identity_judge: Arc<dyn PlaybackIdentityJudge>,
    clock: Arc<dyn Clock>,
    wall_clock: Arc<dyn WallClock>,
    delay: Arc<dyn Delay>,
}

#[derive(Clone)]
pub(crate) struct PlaybackTimePorts {
    clock: Arc<dyn Clock>,
    wall_clock: Arc<dyn WallClock>,
    delay: Arc<dyn Delay>,
}

impl PlaybackTimePorts {
    pub(crate) fn new(
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
        delay: Arc<dyn Delay>,
    ) -> Self {
        Self {
            clock,
            wall_clock,
            delay,
        }
    }
}

#[derive(Default)]
pub(super) struct ExternalPlaybackTracker {
    identity: String,
    playing_since: Option<Instant>,
    pub(super) protected: bool,
}

impl ExternalPlaybackTracker {
    pub(super) fn observe(
        &mut self,
        identity: &str,
        now: Instant,
        protect_after: Duration,
    ) -> bool {
        if self.identity != identity {
            self.identity = identity.to_string();
            self.playing_since = Some(now);
            self.protected = false;
        }
        if !self.protected
            && protect_after > Duration::ZERO
            && self
                .playing_since
                .is_some_and(|started| now.duration_since(started) >= protect_after)
        {
            self.protected = true;
        }
        self.protected
    }

    pub(super) fn clear(&mut self) {
        self.identity.clear();
        self.playing_since = None;
        self.protected = false;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PlaybackRequest {
    pub(crate) keyword: String,
    pub(crate) source: String,
    pub(crate) prefer_accompaniment: bool,
    pub(crate) track: Option<PlayableTrack>,
    pub(crate) requester: String,
    pub(crate) navigation: PlaybackNavigation,
    /// Immutable results from the request's initial provider search. A
    /// fallback selection must reuse these entries instead of searching again.
    pub(crate) candidate_snapshot: Vec<SearchCandidate>,
}

impl PlaybackRequest {
    pub(crate) fn uri(&self) -> String {
        self.track
            .as_ref()
            .map(|track| track.track_ref.key.to_string())
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PlaybackNavigation {
    #[default]
    Normal,
    Previous,
}

#[derive(Clone, Debug)]
struct PlaybackAttempt {
    initial_key: Option<TrackKey>,
    initial_progress: f64,
    requested_key: TrackKey,
    previous_playback: PlaybackRuntimeState,
    started_at_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct PlaybackMismatch {
    pub(crate) status: PlayerStatus,
    pub(crate) local_reason: String,
}

#[derive(Clone, Debug)]
pub(crate) enum PlaybackVerification {
    Success {
        status: PlayerStatus,
        message: String,
    },
    NoSource {
        status: Option<PlayerStatus>,
        reason: String,
    },
    MismatchedCandidate(PlaybackMismatch),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackOutcome {
    Success,
    /// A deterministic failure of this Song Request only.  The queue may
    /// discard it after the outcome is recorded and consider the next item.
    ItemScopedFailure,
    /// The player, provider, credentials, or recovery state is not known to
    /// be usable for the next request.  Keep the queue head for retry/skip.
    QueueBlockingFailure,
    DedupLimited,
}

#[derive(Clone, Debug)]
pub(crate) struct QueueAdvanceContext {
    pub(crate) queue_empty: bool,
    pub(crate) has_pending_playback_task: bool,
    pub(crate) command_executing: bool,
    pub(crate) song_command_executing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueAdvanceDecision {
    None,
    PlaybackStateChanged,
    PauseForQueue,
    ResumeIfIdle,
    AdvanceQueue { reason: &'static str },
}

impl<B: MusicPlayerBackend, S: PlaybackStatePort> PlayerController<B, S> {
    pub(crate) fn new(
        backend: B,
        playback_state: S,
        timing: &PlaybackTimingConfig,
        queue: &QueueConfig,
        matching: &MatchConfig,
        identity_judge: Arc<dyn PlaybackIdentityJudge>,
        time: PlaybackTimePorts,
    ) -> Self {
        Self {
            backend,
            playback_state,
            timing: timing.clone(),
            queue: queue.clone(),
            matching: matching.clone(),
            identity_judge,
            clock: time.clock,
            wall_clock: time.wall_clock,
            delay: time.delay,
        }
    }

    pub(crate) fn status(&self) -> Result<PlayerStatus> {
        let status = self.backend.status()?;
        self.record_observation(&status, classify_observation(&status))?;
        Ok(status)
    }

    pub(crate) fn pause_by_user(&self) -> Result<String> {
        let requested_at_ms = self.wall_clock.unix_millis();
        let result = self.backend.pause();
        self.record_control_outcome("pause", requested_at_ms, result.is_ok());
        let message = result?;
        self.clear_external_playback_tracker()?;
        self.playback_state
            .update(PlaybackStateUpdate::UserPaused)?;
        log::info!("播放器状态转移: pause_reason=user");
        Ok(message)
    }

    /// Arm the user-pause state before attempting the backend RPC.
    ///
    /// Idle exit must prevent an already-queued automatic advance even when the
    /// player backend is unavailable. The state transition is therefore kept as
    /// the first operation; the backend pause remains best effort and its error
    /// is returned to the caller for logging.
    pub(crate) fn pause_for_idle_exit(&self) -> Result<String> {
        self.playback_state
            .update(PlaybackStateUpdate::UserPaused)?;
        let tracker_result = self.clear_external_playback_tracker();
        let requested_at_ms = self.wall_clock.unix_millis();
        let pause_result = self.backend.pause();
        self.record_control_outcome("pause_idle_exit", requested_at_ms, pause_result.is_ok());
        match (tracker_result, pause_result) {
            (Ok(()), Ok(message)) => {
                log::info!("播放器状态转移: pause_reason=user reason=idle_exit");
                Ok(message)
            }
            (Err(tracker_error), Ok(_)) => Err(anyhow!(
                "闲置退出暂停成功，但清理外部播放追踪失败: {tracker_error:#}"
            )),
            (Ok(()), Err(pause_error)) => Err(pause_error),
            (Err(tracker_error), Err(pause_error)) => Err(anyhow!(
                "闲置退出清理外部播放追踪失败: {tracker_error:#}; 暂停播放器失败: {pause_error:#}"
            )),
        }
    }

    pub(crate) fn user_pause_active(&self) -> Result<bool> {
        Ok(self.playback_state.snapshot()?.pause_reason == PauseReason::User)
    }

    pub(crate) fn resume_by_user(&self) -> Result<String> {
        let requested_at_ms = self.wall_clock.unix_millis();
        let result = self.backend.resume();
        self.record_control_outcome("resume", requested_at_ms, result.is_ok());
        let message = result?;
        self.playback_state
            .update(PlaybackStateUpdate::UserResumed)?;
        log::info!("播放器状态转移: pause_reason=none");
        Ok(message)
    }

    pub(crate) fn next_external(&self) -> Result<String> {
        let requested_at_ms = self.wall_clock.unix_millis();
        let result = self.backend.next();
        self.record_control_outcome("next", requested_at_ms, result.is_ok());
        let message = result?;
        self.clear_external_playback_tracker()?;
        self.mark_external_playback()?;
        Ok(message)
    }

    pub(crate) fn previous_external(&self) -> Result<String> {
        let requested_at_ms = self.wall_clock.unix_millis();
        let result = self.backend.previous();
        self.record_control_outcome("previous", requested_at_ms, result.is_ok());
        let message = result?;
        self.clear_external_playback_tracker()?;
        self.mark_external_playback()?;
        Ok(message)
    }

    pub(crate) fn set_volume(&self, volume: &str) -> Result<String> {
        let requested_at_ms = self.wall_clock.unix_millis();
        let result = self.backend.set_volume(volume);
        self.record_control_outcome("set_volume", requested_at_ms, result.is_ok());
        result
    }

    pub(crate) fn is_track_unavailable_error(&self, error: &anyhow::Error) -> bool {
        self.backend.is_track_unavailable_error(error)
    }

    pub(crate) fn clear_active_request(&self) -> Result<()> {
        self.clear_external_playback_tracker()?;
        self.playback_state
            .update(PlaybackStateUpdate::ClearActiveRequest)
            .map(|_| ())
    }

    pub(crate) fn mark_external_playback(&self) -> Result<()> {
        self.clear_external_playback_tracker()?;
        self.playback_state
            .update(PlaybackStateUpdate::External)
            .map(|_| ())
    }

    pub(crate) fn current_status_matches_request(&self, status: &PlayerStatus) -> Result<bool> {
        let runtime = self.playback_state.snapshot()?;
        Ok(status_matches_active_request(
            &self.matching,
            runtime.active_request.as_ref(),
            status,
        ))
    }

    pub(crate) fn previous_playback_request(&self) -> Result<Option<PlaybackRequest>> {
        let runtime = self.playback_state.snapshot()?;
        let Some(previous) = runtime.previous_requests.last() else {
            return Ok(None);
        };
        let Some(track) = previous.track.clone() else {
            return Ok(None);
        };
        Ok(Some(PlaybackRequest {
            keyword: if previous.keyword.trim().is_empty() {
                previous.song.clone()
            } else {
                previous.keyword.clone()
            },
            source: previous.source.clone(),
            prefer_accompaniment: previous.prefer_accompaniment,
            track: Some(track),
            requester: previous.requester.clone(),
            navigation: PlaybackNavigation::Previous,
            candidate_snapshot: Vec::new(),
        }))
    }

    pub(crate) fn should_queue_until_current_song_finished(
        &self,
        status: &PlayerStatus,
    ) -> Result<bool> {
        if !self.queue.protect_current_song_until_finished {
            return Ok(false);
        }
        if let Some(protected) = self.observe_external_playback(status)? {
            return Ok(protected);
        }
        let runtime = self.playback_state.snapshot()?;
        let playback = &runtime;
        if playback.active_request.is_none() {
            return Ok(false);
        }
        if playback.state == ConfirmedPlaybackState::Unknown {
            return Ok(false);
        }
        if status.status == "playing" {
            return Ok(status.current_track.is_some());
        }
        if status.status == "paused"
            && (playback_remaining_seconds(status).is_some() || status.current_track.is_some())
        {
            return Ok(true);
        }

        if playback.active_request.is_none() {
            return Ok(false);
        }
        if status_matches_active_request(&self.matching, playback.active_request.as_ref(), status) {
            return Ok(true);
        }
        if status.current_track.is_none() {
            return Ok(false);
        }
        if active_request_guard_active(
            &self.timing,
            playback.active_request.as_ref(),
            self.clock.now(),
        ) {
            return Ok(true);
        }
        Ok(status.status != "stopped" && status.status != "stoped")
    }

    pub(crate) fn song_dedup_limited(&self, request: &PlaybackRequest) -> Result<bool> {
        let Some(candidate) = request_dedup_candidate(request) else {
            return Ok(false);
        };
        self.playback_state.song_dedup_limited(candidate)
    }

    fn begin_playback_attempt(&self, request: &PlaybackRequest) -> Result<PlaybackAttempt> {
        let requested_key = request
            .track
            .as_ref()
            .map(|track| track.track_ref.key.clone())
            .ok_or_else(|| anyhow!("播放请求缺少结构化曲目"))?;
        self.clear_external_playback_tracker()?;
        let previous_playback = self.playback_snapshot()?;
        let initial = self
            .backend
            .status()
            .map(|status| {
                (
                    status.current_track.map(|track| track.track_ref.key),
                    status.progress,
                )
            })
            .unwrap_or_default();
        let started_at_ms = self.wall_clock.unix_millis();
        self.playback_state.update(PlaybackStateUpdate::Starting {
            request: ActivePlaybackRequest {
                keyword: request.keyword.clone(),
                source: request.source.clone(),
                prefer_accompaniment: request.prefer_accompaniment,
                track: request.track.clone(),
                song: String::new(),
                title: String::new(),
                artist: String::new(),
                requester: request.requester.clone(),
                started_at_ms,
                guard_started_at: Some(self.clock.now()),
            },
            navigation: request.navigation,
        })?;
        log::info!("播放器状态转移: Starting keyword={}", request.keyword);
        Ok(PlaybackAttempt {
            initial_key: initial.0,
            initial_progress: initial.1,
            requested_key,
            previous_playback,
            started_at_ms,
        })
    }

    fn play_request(&self, request: &PlaybackRequest) -> Result<PlaybackAttempt> {
        let attempt = self.begin_playback_attempt(request)?;
        let result = request
            .track
            .as_ref()
            .ok_or_else(|| anyhow!("播放请求缺少结构化曲目"))
            .and_then(|track| self.backend.play(track));
        self.record_attempt_outcome(request, attempt.started_at_ms, result.is_ok());
        if let Err(error) = result {
            let _ = self.restore_failed_attempt(&attempt, "dispatch_failed");
            return Err(error);
        }
        Ok(attempt)
    }

    fn record_attempt_outcome(&self, request: &PlaybackRequest, started_at_ms: u64, success: bool) {
        if let Err(error) = self.playback_state.record_playback_attempt(
            request.source.clone(),
            request.uri(),
            started_at_ms,
            if success {
                "dispatch_acknowledged"
            } else {
                "dispatch_failed"
            }
            .to_string(),
        ) {
            log::error!("持久化播放派发历史失败: {error:#}");
        }
    }

    fn record_control_outcome(&self, operation: &str, requested_at_ms: u64, completed: bool) {
        if let Err(error) = self.playback_state.record_control_operation(
            operation.to_string(),
            requested_at_ms,
            completed,
        ) {
            log::error!("持久化播放器控制历史失败: {error:#}");
        }
    }

    fn verify_playback_started(
        &self,
        request: &PlaybackRequest,
        attempt: &mut PlaybackAttempt,
    ) -> Result<PlaybackVerification> {
        self.delay
            .wait(Duration::from_millis(self.timing.search_settle_ms));

        let mut last_status = None;
        let mut last_status_error = None;
        for retry in 0..self.timing.status_retries {
            let status = match self.backend.status() {
                Ok(status) => status,
                Err(error) => {
                    log::error!("查询播放状态失败: {error:#}");
                    last_status_error = Some(error.to_string());
                    self.mark_unknown()?;
                    self.delay
                        .wait(Duration::from_millis(self.timing.status_poll_ms));
                    continue;
                }
            };
            last_status_error = None;
            last_status = Some(status.clone());
            let reliability = classify_observation(&status);
            self.record_observation(&status, reliability)?;
            log::debug!(
                "播放器观测: raw={} uri={} title={} artist={} reliability={:?}",
                status.status,
                status.current_uri,
                status.name,
                status.singer,
                reliability
            );

            if status.status != "playing" && status.status != "paused" {
                self.delay
                    .wait(Duration::from_millis(self.timing.status_poll_ms));
                continue;
            }

            let Some(current_key) = status
                .current_track
                .as_ref()
                .map(|track| track.track_ref.key.clone())
            else {
                log::info!(
                    "播放器观测缺少结构化曲目，继续等待 ({}/{})",
                    retry + 1,
                    self.timing.status_retries
                );
                self.delay
                    .wait(Duration::from_millis(self.timing.status_poll_ms));
                continue;
            };
            let current_uri = current_key.to_string();
            if current_key != attempt.requested_key {
                if attempt.initial_key.as_ref() == Some(&current_key)
                    && !playback_progress_restarted(attempt.initial_progress, status.progress)
                {
                    log::info!(
                        "曲目尚未切换，继续等待播放请求生效 ({}/{})",
                        retry + 1,
                        self.timing.status_retries
                    );
                    self.delay
                        .wait(Duration::from_millis(self.timing.status_poll_ms));
                    continue;
                }
                if is_cross_source_track(&attempt.requested_key, &current_key) {
                    match self.reconcile_cross_source_status(request, &status)? {
                        CrossSourceReconciliation::Match {
                            status: stable_status,
                            score,
                            reason,
                        } => {
                            self.confirm_playback_fallback(request, &stable_status, &reason)?;
                            let message = format_play_message(&stable_status);
                            log::info!(
                                "跨源同曲确认成功: requested={} confirmed={} score={:.2} reason={}",
                                attempt.requested_key,
                                stable_status.current_uri,
                                score,
                                reason
                            );
                            return Ok(PlaybackVerification::Success {
                                status: stable_status,
                                message,
                            });
                        }
                        CrossSourceReconciliation::NoMatch {
                            status: stable_status,
                            score,
                            reason,
                        } => {
                            log::info!(
                                "跨源同曲判断不匹配: requested={} confirmed={} score={:.2} reason={}",
                                attempt.requested_key,
                                stable_status.current_uri,
                                score,
                                reason
                            );
                        }
                        CrossSourceReconciliation::Unavailable {
                            status: stable_status,
                            reason,
                        } => {
                            log::info!(
                                "跨源同曲判断不可用: requested={} confirmed={} reason={}",
                                attempt.requested_key,
                                stable_status.current_uri,
                                reason
                            );
                        }
                        CrossSourceReconciliation::Pending => {
                            log::debug!(
                                "跨源同曲确认尚未稳定: requested={} current={}",
                                attempt.requested_key,
                                current_uri
                            );
                        }
                    }
                }
                log::info!(
                    "URI 与请求资源不同，不能用歌曲信息兜底: current={} requested={} ({}/{})",
                    current_uri,
                    attempt.requested_key,
                    retry + 1,
                    self.timing.status_retries
                );
                return Ok(PlaybackVerification::MismatchedCandidate(
                    PlaybackMismatch {
                        status,
                        local_reason: format!(
                            "播放器 URI 与请求不一致: current={} requested={}",
                            current_key, attempt.requested_key
                        ),
                    },
                ));
            }

            if playback_status_has_no_timing(&status) {
                log::info!(
                    "0:00/0:00，等待后重试 ({}/{})",
                    retry + 1,
                    self.timing.status_retries
                );
                self.delay
                    .wait(Duration::from_millis(self.timing.status_poll_ms));
                continue;
            }
            if status.duration > 0.0 && status.duration < 20.0 {
                log::info!("歌曲时长过短 ({:.1}s)，视为无音源", status.duration);
                self.restore_failed_attempt(attempt, "verification_failed")?;
                let reason = format!("歌曲时长过短: {:.1}s", status.duration);
                return Ok(PlaybackVerification::NoSource {
                    status: Some(status),
                    reason,
                });
            }

            let message = format_play_message(&status);
            self.confirm_playback_success(request, &status)?;
            log::info!("播放成功: {}", message);
            return Ok(PlaybackVerification::Success { status, message });
        }

        log::info!("超时未播放成功");
        self.restore_failed_attempt(attempt, "verification_failed")?;
        if let Some(error) = last_status_error {
            log::error!("播放确认结束时播放器状态接口不可用: {error}");
            return Err(anyhow!("播放器状态暂不可用，请稍后再试"));
        }
        Ok(PlaybackVerification::NoSource {
            status: last_status,
            reason: "超时未播放成功".to_string(),
        })
    }

    pub(crate) fn play_and_verify(
        &self,
        request: &PlaybackRequest,
    ) -> Result<PlaybackVerification> {
        let mut attempt = self.play_request(request)?;
        self.verify_playback_started(request, &mut attempt)
    }

    fn observe_stable_fallback(&self, first: &PlayerStatus) -> Result<Option<PlayerStatus>> {
        let mut stable = first.clone();
        for _ in 1..self.timing.fallback_identity_stable_samples {
            self.delay
                .wait(Duration::from_millis(self.timing.status_poll_ms));
            let status = match self.backend.status() {
                Ok(status) => status,
                Err(error) => {
                    log::warn!("跨源同曲确认读取播放器状态失败: {error:#}");
                    return Ok(None);
                }
            };
            self.record_observation(&status, classify_observation(&status))?;
            if !stable_fallback_identity(&stable, &status) {
                log::info!("跨源同曲确认未稳定，放弃当前备用 URI");
                return Ok(None);
            }
            stable = status;
        }
        Ok(Some(stable))
    }

    fn reconcile_cross_source_status(
        &self,
        request: &PlaybackRequest,
        first: &PlayerStatus,
    ) -> Result<CrossSourceReconciliation> {
        let Some(status) = self.observe_stable_fallback(first)? else {
            return Ok(CrossSourceReconciliation::Pending);
        };
        if !fallback_status_is_playable(&status) {
            return Ok(CrossSourceReconciliation::Pending);
        }
        let decision = self.judge_cross_source_identity(request, &status);
        Ok(match decision {
            PlaybackIdentityDecision::Match { score, reason } => CrossSourceReconciliation::Match {
                status,
                score,
                reason,
            },
            PlaybackIdentityDecision::NoMatch { score, reason } => {
                CrossSourceReconciliation::NoMatch {
                    status,
                    score,
                    reason,
                }
            }
            PlaybackIdentityDecision::Unavailable { reason } => {
                CrossSourceReconciliation::Unavailable { status, reason }
            }
        })
    }

    fn judge_cross_source_identity(
        &self,
        request: &PlaybackRequest,
        status: &PlayerStatus,
    ) -> PlaybackIdentityDecision {
        match self
            .matching
            .match_song_identity(&request.keyword, &status.name, &status.singer)
        {
            super::matcher::SongIdentityMatch::Match { score, reason } => {
                PlaybackIdentityDecision::Match { score, reason }
            }
            super::matcher::SongIdentityMatch::Unknown { reason } => {
                log::debug!(
                    "跨源同曲本地判断不确定: current={} requested={} reason={}",
                    status.current_uri,
                    request.uri(),
                    reason
                );
                self.identity_judge.judge(request, status)
            }
        }
    }

    pub(crate) fn reject_mismatch_as_no_source(&self, status: Option<&PlayerStatus>) -> Result<()> {
        if status.is_some_and(|status| status.status == "playing") {
            let _ = self.backend.pause();
        }
        self.mark_unknown()
    }

    pub(crate) fn maybe_advance_queue(
        &self,
        snapshot_status: PlayerStatus,
        context: QueueAdvanceContext,
    ) -> Result<QueueAdvanceDecision> {
        let mut status = snapshot_status;
        let external_playback_protected = self.observe_external_playback(&status)?.unwrap_or(false);
        let runtime_snapshot = self.playback_state.snapshot()?;
        let session_reconciliation = self.reconcile_player_session(&runtime_snapshot, &status)?;
        if runtime_snapshot.state == ConfirmedPlaybackState::Unknown {
            return Ok(QueueAdvanceDecision::None);
        }

        if session_reconciliation == SessionReconciliation::Restarted {
            return self.recover_after_runtime_restart(&runtime_snapshot, &status);
        }
        if session_reconciliation == SessionReconciliation::Replaced {
            log::warn!(
                "播放运行时会话已更新，等待请求身份校验: session={} generation={}",
                status.session_id,
                status.generation
            );
        }
        let guard_active = active_request_guard_active(
            &self.timing,
            runtime_snapshot.active_request.as_ref(),
            self.clock.now(),
        );

        if runtime_snapshot.active_request.is_some()
            && !status_matches_active_request(
                &self.matching,
                runtime_snapshot.active_request.as_ref(),
                &status,
            )
        {
            match self.backend.status() {
                Ok(fresh_status) => {
                    log::info!(
                        "点歌状态与播放监控快照不一致，已刷新播放状态: snapshot_uri={} fresh_uri={}",
                        status.current_uri,
                        fresh_status.current_uri,
                    );
                    status = fresh_status;
                    self.record_observation(&status, classify_observation(&status))?;
                }
                Err(error) => {
                    log::error!("刷新点歌播放状态失败，暂不自动出队: {error:#}");
                    self.mark_unknown()?;
                    return Ok(QueueAdvanceDecision::None);
                }
            }
        }

        if runtime_snapshot.active_request.is_some()
            && guard_active
            && !is_notify_controller_natural_end(&status)
            && !status_matches_active_request(
                &self.matching,
                runtime_snapshot.active_request.as_ref(),
                &status,
            )
        {
            log::debug!("点歌刚开始，忽略可能过期的播放状态");
            return Ok(QueueAdvanceDecision::None);
        }

        if runtime_snapshot.active_request.is_some()
            && matches!(status.status.as_str(), "playing" | "paused")
            && active_request_track_changed(
                runtime_snapshot.active_request.as_ref(),
                &status,
                &self.matching,
            )
        {
            if let Some(active_request) = runtime_snapshot.active_request.as_ref()
                && active_request
                    .track
                    .as_ref()
                    .zip(status.current_track.as_ref())
                    .is_some_and(|(requested, current)| {
                        is_cross_source_track(&requested.track_ref.key, &current.track_ref.key)
                    })
            {
                let request = playback_request_from_active(active_request);
                match self.reconcile_cross_source_status(&request, &status)? {
                    CrossSourceReconciliation::Match {
                        status: stable_status,
                        score,
                        reason,
                    } => {
                        self.confirm_playback_reconciliation(
                            active_request,
                            &stable_status,
                            &reason,
                        )?;
                        log::info!(
                            "跨源换源同曲确认成功，保留点歌状态: requested={} confirmed={} score={:.2} reason={}",
                            active_request_expected_uri(active_request),
                            stable_status.current_uri,
                            score,
                            reason
                        );
                        return Ok(QueueAdvanceDecision::PlaybackStateChanged);
                    }
                    CrossSourceReconciliation::Pending => {
                        log::debug!(
                            "跨源换源尚未稳定，暂不转为外部播放: requested={} current={}",
                            active_request_expected_uri(active_request),
                            status.current_uri
                        );
                        return Ok(QueueAdvanceDecision::None);
                    }
                    CrossSourceReconciliation::NoMatch {
                        status: stable_status,
                        score,
                        reason,
                    } => {
                        log::info!(
                            "跨源换源判断为不同歌曲: requested={} confirmed={} score={:.2} reason={}",
                            active_request_expected_uri(active_request),
                            stable_status.current_uri,
                            score,
                            reason
                        );
                    }
                    CrossSourceReconciliation::Unavailable {
                        status: stable_status,
                        reason,
                    } => {
                        log::warn!(
                            "跨源换源无法确认歌曲身份，暂停并进入 Unknown: requested={} current={} reason={}",
                            active_request_expected_uri(active_request),
                            stable_status.current_uri,
                            reason
                        );
                        self.reject_mismatch_as_no_source(Some(&stable_status))?;
                        return Ok(QueueAdvanceDecision::PlaybackStateChanged);
                    }
                }
            }
            log::info!(
                "播放器状态转移: RequestedSongPlaying -> ExternalPlayback reason=track_changed"
            );
            self.mark_external_playback()?;
            return Ok(QueueAdvanceDecision::PlaybackStateChanged);
        }

        if !external_playback_protected
            && runtime_snapshot.state == ConfirmedPlaybackState::ExternalPlayback
            && runtime_snapshot.active_request.is_none()
            && !context.command_executing
            && !context.has_pending_playback_task
            && !context.queue_empty
        {
            log::info!("队列推进决策: advance reason=external_not_stable");
            return Ok(QueueAdvanceDecision::AdvanceQueue {
                reason: "外部播放未稳定",
            });
        }

        if runtime_snapshot.active_request.is_some()
            && guard_active
            && !is_notify_controller_natural_end(&status)
        {
            log::debug!("点歌刚开始，暂不触发队列自动出队");
            return Ok(QueueAdvanceDecision::None);
        }

        let has_pending_playback = !context.queue_empty
            || context.has_pending_playback_task
            || context.song_command_executing;

        let pause_reason = self.playback_state.snapshot()?.pause_reason;

        if pause_reason == PauseReason::User {
            return Ok(QueueAdvanceDecision::None);
        }

        if self.is_matching_natural_end(&runtime_snapshot, &status, session_reconciliation) {
            let outcome = terminal_outcome_key(&status);
            let claimed = self.playback_state.claim_terminal_outcome(
                status.generation,
                outcome,
                self.wall_clock.unix_millis(),
            )?;
            if !claimed {
                log::debug!("已处理过同一 播放运行时自然结束，忽略重复观测");
                return Ok(QueueAdvanceDecision::None);
            }
            self.clear_active_request()?;
            let _ = self.playback_state.reconcile_player_session(None)?;
            if context.command_executing || context.has_pending_playback_task || context.queue_empty
            {
                return Ok(QueueAdvanceDecision::PlaybackStateChanged);
            }
            log::info!("队列推进决策: advance reason=natural_end");
            return Ok(QueueAdvanceDecision::AdvanceQueue {
                reason: "自然结束"
            });
        }

        if status.status == "stopped" || status.status == "stoped" {
            // A bare stopped observation can be user action, a provider
            // failure, or a stale sample. It is never queue ownership.
            return Ok(QueueAdvanceDecision::None);
        }

        if context.queue_empty
            && !context.has_pending_playback_task
            && !context.command_executing
            && !context.song_command_executing
        {
            return self.resume_waiting_for_queue_if_idle();
        }

        if status.status == "paused" {
            if pause_reason == PauseReason::WaitingForQueue {
                let Some(remaining) = playback_remaining_seconds(&status) else {
                    return Ok(QueueAdvanceDecision::None);
                };
                if remaining > self.queue.auto_advance_seconds as f64 {
                    return Ok(QueueAdvanceDecision::None);
                }
                if !context.command_executing
                    && !context.has_pending_playback_task
                    && !context.queue_empty
                {
                    log::info!("队列推进决策: advance reason=near_end_paused");
                    return Ok(QueueAdvanceDecision::AdvanceQueue {
                        reason: "即将结束"
                    });
                }
                return Ok(QueueAdvanceDecision::None);
            }
            let Some(remaining) = playback_remaining_seconds(&status) else {
                return Ok(QueueAdvanceDecision::None);
            };
            if remaining > self.queue.auto_advance_seconds as f64 {
                return Ok(QueueAdvanceDecision::None);
            }
            if context.command_executing || context.has_pending_playback_task || context.queue_empty
            {
                return Ok(QueueAdvanceDecision::None);
            }
            self.playback_state
                .update(PlaybackStateUpdate::ClearPauseReason)?;
            log::info!("队列推进决策: advance reason=paused");
            return Ok(QueueAdvanceDecision::AdvanceQueue { reason: "暂停" });
        }

        if status.status != "playing" {
            return Ok(QueueAdvanceDecision::None);
        }

        if pause_reason != PauseReason::None {
            self.playback_state
                .update(PlaybackStateUpdate::MarkRequestedPlayingIfActive)?;
        }
        if let Some(remaining) = playback_remaining_seconds(&status)
            && remaining <= self.queue.auto_advance_seconds as f64
            && has_pending_playback
        {
            let paused = self.pause_for_queue()?;
            if !context.command_executing
                && !context.has_pending_playback_task
                && !context.queue_empty
            {
                log::info!("队列推进决策: advance reason=near_end");
                return Ok(QueueAdvanceDecision::AdvanceQueue {
                    reason: "即将结束"
                });
            }
            return Ok(if paused {
                QueueAdvanceDecision::PauseForQueue
            } else {
                QueueAdvanceDecision::None
            });
        }
        Ok(QueueAdvanceDecision::None)
    }

    fn reconcile_player_session(
        &self,
        runtime: &PlaybackRuntimeState,
        status: &PlayerStatus,
    ) -> Result<SessionReconciliation> {
        if runtime.active_request.is_none() {
            return Ok(SessionReconciliation::NoActiveRequest);
        }
        let runtime_identity = status.runtime_identity.trim();
        if runtime_identity.is_empty() {
            return Ok(SessionReconciliation::Unknown);
        }
        self.playback_state
            .reconcile_player_session(Some(PlaybackSessionBinding {
                runtime_identity: runtime_identity.to_string(),
                session_id: status.session_id.trim().to_string(),
                generation: status.generation,
                bound_at_ms: self.wall_clock.unix_millis(),
            }))
    }

    fn is_matching_natural_end(
        &self,
        runtime: &PlaybackRuntimeState,
        status: &PlayerStatus,
        reconciliation: SessionReconciliation,
    ) -> bool {
        runtime.active_request.is_some()
            && reconciliation == SessionReconciliation::Match
            && is_notify_controller_natural_end(status)
    }

    fn recover_after_runtime_restart(
        &self,
        runtime: &PlaybackRuntimeState,
        status: &PlayerStatus,
    ) -> Result<QueueAdvanceDecision> {
        let Some(active) = runtime.active_request.as_ref() else {
            return Ok(QueueAdvanceDecision::None);
        };
        if runtime.pause_reason == PauseReason::User {
            log::info!("播放运行时已重启，但用户暂停仍生效，跳过恢复");
            return Ok(QueueAdvanceDecision::None);
        }
        if status.session_id.trim().is_empty()
            && matches!(status.status.as_str(), "stopped" | "idle")
        {
            let request = playback_request_from_active(active);
            log::warn!(
                "检测到 playback runtime 重启，控制器授权新恢复会话: previous_uri={}",
                request.uri()
            );
            match self.play_and_verify(&request)? {
                PlaybackVerification::Success { status, .. } => {
                    let _ =
                        self.reconcile_player_session(&self.playback_state.snapshot()?, &status)?;
                    return Ok(QueueAdvanceDecision::PlaybackStateChanged);
                }
                PlaybackVerification::NoSource { reason, .. } => {
                    log::error!("播放运行时重启后的恢复会话无法播放: {reason}");
                    self.mark_unknown()?;
                    return Ok(QueueAdvanceDecision::PlaybackStateChanged);
                }
                PlaybackVerification::MismatchedCandidate(_) => {
                    self.mark_unknown()?;
                    return Ok(QueueAdvanceDecision::PlaybackStateChanged);
                }
            }
        }
        // A new runtime must not inherit an old session automatically. Wait
        // for an idle observation that the controller can explicitly recover.
        Ok(QueueAdvanceDecision::None)
    }

    pub(crate) fn snapshot(&self) -> PlaybackControllerSnapshot {
        self.playback_state.snapshot().map_or_else(
            |_| PlaybackControllerSnapshot {
                state: "unavailable".to_string(),
                pause_reason: "unknown".to_string(),
                active_keyword: String::new(),
                active_uri: String::new(),
                last_observation_reliability: "unknown".to_string(),
                backend_status: String::new(),
                current_uri: String::new(),
                title: String::new(),
                artist: String::new(),
                requester: String::new(),
                progress: 0.0,
                duration: 0.0,
                observed_at_ms: 0,
            },
            |runtime| {
                let playback = &runtime;
                let observation = playback.last_observation.as_ref();
                PlaybackControllerSnapshot {
                    state: format_state(playback.state),
                    pause_reason: format_pause_reason(playback.pause_reason),
                    active_keyword: playback
                        .active_request
                        .as_ref()
                        .map(|request| request.keyword.clone())
                        .unwrap_or_default(),
                    active_uri: playback
                        .active_request
                        .as_ref()
                        .and_then(|request| request.track.as_ref())
                        .map(|track| track.track_ref.key.to_string())
                        .unwrap_or_default(),
                    last_observation_reliability: playback
                        .last_observation
                        .as_ref()
                        .map(|observation| format_reliability(observation.reliability))
                        .unwrap_or_else(|| "unknown".to_string()),
                    backend_status: observation
                        .map(|observation| observation.status.clone())
                        .unwrap_or_default(),
                    current_uri: observation
                        .and_then(|observation| observation.track.as_ref())
                        .map(|track| track.track_ref.key.to_string())
                        .unwrap_or_default(),
                    title: observation
                        .map(|observation| observation.title.clone())
                        .unwrap_or_default(),
                    artist: observation
                        .map(|observation| observation.artist.clone())
                        .unwrap_or_default(),
                    requester: playback
                        .active_request
                        .as_ref()
                        .map(|request| request.requester.clone())
                        .unwrap_or_default(),
                    progress: observation.map_or(0.0, |observation| observation.progress),
                    duration: observation.map_or(0.0, |observation| observation.duration),
                    observed_at_ms: observation.map_or(0, |observation| observation.captured_at_ms),
                }
            },
        )
    }

    fn pause_for_queue(&self) -> Result<bool> {
        let already_waiting =
            self.playback_state.snapshot()?.pause_reason == PauseReason::WaitingForQueue;
        if already_waiting {
            return Ok(false);
        }
        log::info!("队列推进决策: pause_waiting_for_queue");
        self.backend.pause()?;
        self.playback_state
            .update(PlaybackStateUpdate::PauseWaitingForQueue)?;
        Ok(true)
    }

    fn resume_waiting_for_queue_if_idle(&self) -> Result<QueueAdvanceDecision> {
        let should_resume =
            self.playback_state.snapshot()?.pause_reason == PauseReason::WaitingForQueue;
        if !should_resume {
            return Ok(QueueAdvanceDecision::None);
        }
        log::info!("队列推进决策: resume_waiting_for_queue_idle");
        self.backend.resume()?;
        self.playback_state
            .update(PlaybackStateUpdate::ResumeWaitingForQueue)?;
        Ok(QueueAdvanceDecision::ResumeIfIdle)
    }

    fn confirm_playback_success(
        &self,
        request: &PlaybackRequest,
        status: &PlayerStatus,
    ) -> Result<()> {
        self.confirm_playback_success_with_track(request, status, true, "playback_confirmed")
    }

    fn confirm_playback_fallback(
        &self,
        request: &PlaybackRequest,
        status: &PlayerStatus,
        reason: &str,
    ) -> Result<()> {
        self.confirm_playback_success_with_track(request, status, false, reason)
    }

    fn confirm_playback_reconciliation(
        &self,
        active_request: &ActivePlaybackRequest,
        status: &PlayerStatus,
        reason: &str,
    ) -> Result<()> {
        let confirmed_track = status
            .current_track
            .clone()
            .ok_or_else(|| anyhow!("跨源换源确认缺少结构化曲目"))?;
        let mut reconciled = active_request.clone();
        reconciled.track = Some(confirmed_track);
        reconciled.song = format!("{}{}", status.name, status.singer);
        reconciled.title = status.name.trim().to_string();
        reconciled.artist = status.singer.trim().to_string();
        reconciled.guard_started_at = None;
        self.playback_state
            .update(PlaybackStateUpdate::Reconciled {
                request: reconciled,
            })?;
        let request = playback_request_from_active(active_request);
        self.record_song_dedup_playback(&request, status)?;
        log::info!(
            "播放器状态保持 RequestedSongPlaying reason={} confirmed_uri={}",
            reason,
            status.current_uri
        );
        Ok(())
    }

    fn confirm_playback_success_with_track(
        &self,
        request: &PlaybackRequest,
        status: &PlayerStatus,
        require_requested_track: bool,
        reason: &str,
    ) -> Result<()> {
        let confirmed_track = status
            .current_track
            .as_ref()
            .ok_or_else(|| anyhow!("播放器观测缺少结构化曲目，不能确认播放成功"))?;
        if require_requested_track
            && request
                .track
                .as_ref()
                .is_none_or(|track| confirmed_track.track_ref.key != track.track_ref.key)
        {
            return Err(anyhow!("播放器观测曲目与请求不一致，不能确认播放成功"));
        }
        let active_request = ActivePlaybackRequest {
            keyword: request.keyword.clone(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            track: Some(confirmed_track.clone()),
            song: format!("{}{}", status.name, status.singer),
            title: status.name.trim().to_string(),
            artist: status.singer.trim().to_string(),
            requester: request.requester.clone(),
            started_at_ms: self.wall_clock.unix_millis(),
            guard_started_at: Some(self.clock.now()),
        };
        self.playback_state.update(PlaybackStateUpdate::Confirmed {
            request: active_request,
            navigation: request.navigation,
        })?;
        self.record_song_dedup_playback(request, status)?;
        log::info!("播放器状态转移: Starting -> RequestedSongPlaying reason={reason}");
        Ok(())
    }

    fn record_song_dedup_playback(
        &self,
        request: &PlaybackRequest,
        status: &PlayerStatus,
    ) -> Result<()> {
        let (fallback_title, fallback_artist) = split_title_artist(&request.keyword);
        let title = if status.name.trim().is_empty() {
            fallback_title
        } else {
            status.name.trim().to_string()
        };
        let artist = if status.singer.trim().is_empty() {
            fallback_artist
        } else {
            status.singer.trim().to_string()
        };
        let candidate = SongDedupCandidate {
            track_key: status
                .current_track
                .as_ref()
                .map(|track| track.track_ref.key.clone())
                .or_else(|| {
                    request
                        .track
                        .as_ref()
                        .map(|track| track.track_ref.key.clone())
                })
                .ok_or_else(|| anyhow!("播放去重缺少结构化曲目"))?,
            title,
            artist,
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
        };
        self.playback_state.record_song_dedup(candidate)
    }

    fn record_observation(
        &self,
        status: &PlayerStatus,
        reliability: ObservationReliability,
    ) -> Result<()> {
        let observation = PlaybackObservation {
            status: status.status.clone(),
            track: status.current_track.clone(),
            title: status.name.clone(),
            artist: status.singer.clone(),
            progress: status.progress,
            duration: status.duration,
            captured_at_ms: self.wall_clock.unix_millis(),
            reliability,
        };
        self.playback_state
            .update(PlaybackStateUpdate::Observation(observation))
            .map(|_| ())
    }

    fn mark_unknown(&self) -> Result<()> {
        self.clear_external_playback_tracker()?;
        self.playback_state
            .update(PlaybackStateUpdate::Unknown)
            .map(|_| ())
    }

    fn observe_external_playback(&self, status: &PlayerStatus) -> Result<Option<bool>> {
        let (is_external, should_mark_external) = {
            let runtime = self.playback_state.snapshot()?;
            let playback = &runtime;
            let is_external = playback.active_request.is_none()
                && playback.state != ConfirmedPlaybackState::Unknown
                && playback.pause_reason == PauseReason::None;
            (
                is_external,
                is_external
                    && (playback.state != ConfirmedPlaybackState::ExternalPlayback
                        || playback.pause_reason != PauseReason::None),
            )
        };
        let Some(identity) = external_playback_identity(status).filter(|_| is_external) else {
            self.clear_external_playback_tracker()?;
            return Ok(None);
        };
        let protect_after = Duration::from_secs(self.queue.external_playback_protect_after_seconds);
        let observation = self.playback_state.observe_external_playback(
            identity.clone(),
            self.clock.now(),
            protect_after,
        )?;
        if should_mark_external {
            self.playback_state.update(PlaybackStateUpdate::External)?;
        }
        if observation.protected && !observation.was_protected {
            log::info!(
                "外部播放已稳定 {}s，加入当前歌曲保护: {}",
                self.queue.external_playback_protect_after_seconds,
                identity
            );
        }
        Ok(Some(observation.protected))
    }

    fn clear_external_playback_tracker(&self) -> Result<()> {
        self.playback_state.clear_external_playback_tracker()
    }

    fn playback_snapshot(&self) -> Result<PlaybackRuntimeState> {
        self.playback_state.snapshot()
    }

    fn restore_playback_state(&self, playback: PlaybackRuntimeState) -> Result<()> {
        self.playback_state
            .update(PlaybackStateUpdate::Restore(Box::new(playback)))
            .map(|_| ())
    }

    fn restore_failed_attempt(&self, attempt: &PlaybackAttempt, reason: &str) -> Result<()> {
        if reason == "dispatch_failed" {
            self.restore_playback_state(attempt.previous_playback.clone())?;
            log::info!("播放器状态转移: Starting -> previous reason={}", reason);
        } else {
            self.mark_unknown()?;
            log::info!("播放器状态转移: Starting -> Unknown reason={}", reason);
        }
        Ok(())
    }
}

fn external_playback_identity(status: &PlayerStatus) -> Option<String> {
    if status.status != "playing" {
        return None;
    }
    status
        .current_track
        .as_ref()
        .map(|track| format!("track:{}", track.track_ref.key))
}

fn is_cross_source_track(requested: &TrackKey, current: &TrackKey) -> bool {
    requested.provider != current.provider
}

fn stable_fallback_identity(previous: &PlayerStatus, current: &PlayerStatus) -> bool {
    matches!(current.status.as_str(), "playing" | "paused")
        && current
            .current_track
            .as_ref()
            .zip(previous.current_track.as_ref())
            .is_some_and(|(current, previous)| current.track_ref.key == previous.track_ref.key)
        && current.name.trim() == previous.name.trim()
        && current.singer.trim() == previous.singer.trim()
}

fn playback_status_has_no_timing(status: &PlayerStatus) -> bool {
    let progress = format_time(status.progress);
    let duration = format_time(status.duration);
    (progress == "0:00" && duration == "0:00") || duration == "error"
}

fn fallback_status_is_playable(status: &PlayerStatus) -> bool {
    !(playback_status_has_no_timing(status) || status.duration > 0.0 && status.duration < 20.0)
}

fn status_matches_active_request(
    _matching: &MatchConfig,
    active_request: Option<&ActivePlaybackRequest>,
    status: &PlayerStatus,
) -> bool {
    let Some(active_request) = active_request else {
        return false;
    };
    active_request
        .track
        .as_ref()
        .zip(status.current_track.as_ref())
        .is_some_and(|(requested, current)| requested.track_ref.key == current.track_ref.key)
}

fn active_request_expected_uri(active_request: &ActivePlaybackRequest) -> String {
    active_request
        .track
        .as_ref()
        .map(|track| track.track_ref.key.to_string())
        .unwrap_or_default()
}

fn playback_request_from_active(active_request: &ActivePlaybackRequest) -> PlaybackRequest {
    PlaybackRequest {
        keyword: active_request.keyword.clone(),
        source: active_request.source.clone(),
        prefer_accompaniment: active_request.prefer_accompaniment,
        track: active_request.track.clone(),
        requester: active_request.requester.clone(),
        navigation: PlaybackNavigation::Normal,
        candidate_snapshot: Vec::new(),
    }
}

fn terminal_outcome_key(status: &PlayerStatus) -> String {
    format!(
        "natural_end:{}:{}:{}",
        status.runtime_identity.trim(),
        status.session_id.trim(),
        status.generation
    )
}

fn is_notify_controller_natural_end(status: &PlayerStatus) -> bool {
    status.end_behavior == "notify_controller" && status.last_end_cause == "natural_end"
}

fn active_request_guard_active(
    timing: &PlaybackTimingConfig,
    active_request: Option<&ActivePlaybackRequest>,
    now: Instant,
) -> bool {
    let Some(active_request) = active_request else {
        return false;
    };
    let Some(started_at) = active_request.guard_started_at else {
        return false;
    };
    let guard_ms = timing
        .monitor_status_ms
        .max(timing.status_poll_ms)
        .saturating_mul(3)
        .max(3000);
    now.saturating_duration_since(started_at) < Duration::from_millis(guard_ms)
}

fn active_request_track_changed(
    active_request: Option<&ActivePlaybackRequest>,
    status: &PlayerStatus,
    matching: &MatchConfig,
) -> bool {
    let Some(active_request) = active_request else {
        return false;
    };
    let changed = active_request
        .track
        .as_ref()
        .zip(status.current_track.as_ref())
        .is_some_and(|(requested, current)| requested.track_ref.key != current.track_ref.key);
    changed && !status_matches_active_request(matching, Some(active_request), status)
}

fn request_dedup_candidate(request: &PlaybackRequest) -> Option<SongDedupCandidate> {
    let track = request.track.as_ref()?;
    let (title, artist) = split_title_artist(&request.keyword);
    Some(SongDedupCandidate {
        track_key: track.track_ref.key.clone(),
        title,
        artist,
        source: request.source.clone(),
        prefer_accompaniment: request.prefer_accompaniment,
    })
}

fn split_title_artist(value: &str) -> (String, String) {
    let text = value.trim();
    if let Some((title, artist)) = text.split_once(" - ") {
        return (title.trim().to_string(), artist.trim().to_string());
    }
    (text.to_string(), String::new())
}

fn classify_observation(status: &PlayerStatus) -> ObservationReliability {
    if status.status.trim().is_empty() {
        return ObservationReliability::Unknown;
    }
    if status.status != "playing" && status.status != "paused" {
        return ObservationReliability::Stale;
    }
    if status.current_track.is_none() {
        return ObservationReliability::Incomplete;
    }
    ObservationReliability::Reliable
}

fn format_state(state: ConfirmedPlaybackState) -> String {
    match state {
        ConfirmedPlaybackState::Idle => "idle",
        ConfirmedPlaybackState::Starting => "starting",
        ConfirmedPlaybackState::RequestedSongPlaying => "requested_song_playing",
        ConfirmedPlaybackState::PausedByUser => "paused_by_user",
        ConfirmedPlaybackState::PausedWaitingForQueue => "paused_waiting_for_queue",
        ConfirmedPlaybackState::ExternalPlayback => "external_playback",
        ConfirmedPlaybackState::Unknown => "unknown",
    }
    .to_string()
}

fn format_pause_reason(reason: PauseReason) -> String {
    match reason {
        PauseReason::None => "none",
        PauseReason::User => "user",
        PauseReason::WaitingForQueue => "waiting_for_queue",
    }
    .to_string()
}

fn format_reliability(reliability: ObservationReliability) -> String {
    match reliability {
        ObservationReliability::Reliable => "reliable",
        ObservationReliability::Incomplete => "incomplete",
        ObservationReliability::Stale => "stale",
        ObservationReliability::Mismatched => "mismatched",
        ObservationReliability::Unknown => "unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::super::{PersistentPlaybackState, PersistentSongDedupHistory};
    use super::*;
    use crate::features::playback::{SongDedupConfig, test_track};
    use crate::runtime::clock::{Clock, Delay, ManualClock, SystemClock, WallClock};
    use std::collections::{HashSet, VecDeque};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone)]
    struct TestPlaybackState {
        runtime: Arc<Mutex<PersistentPlaybackState>>,
        history: Arc<Mutex<PersistentSongDedupHistory>>,
        external_playback_tracker: Arc<Mutex<ExternalPlaybackTracker>>,
        session_binding: Arc<Mutex<Option<PlaybackSessionBinding>>>,
        handled_terminals: Arc<Mutex<HashSet<(u64, String)>>>,
        attempts: Arc<Mutex<Vec<(String, String, String)>>>,
        controls: Arc<Mutex<Vec<(String, bool)>>>,
        song_dedup: SongDedupConfig,
    }

    impl PlaybackStatePort for TestPlaybackState {
        fn snapshot(&self) -> Result<PlaybackRuntimeState> {
            Ok(self.runtime.lock().unwrap().state().clone())
        }

        fn update(&self, update: PlaybackStateUpdate) -> Result<bool> {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.update(|playback| update.apply(playback))
        }

        fn song_dedup_limited(&self, candidate: SongDedupCandidate) -> Result<bool> {
            Ok(self
                .history
                .lock()
                .unwrap()
                .is_limited(&self.song_dedup, &candidate))
        }

        fn record_song_dedup(&self, candidate: SongDedupCandidate) -> Result<()> {
            self.history
                .lock()
                .unwrap()
                .record_playback(&self.song_dedup, candidate)
        }

        fn observe_external_playback(
            &self,
            identity: String,
            now: Instant,
            protect_after: Duration,
        ) -> Result<super::super::ExternalPlaybackObservation> {
            let mut tracker = self.external_playback_tracker.lock().unwrap();
            let was_protected = tracker.protected;
            let protected = tracker.observe(&identity, now, protect_after);
            Ok(super::super::ExternalPlaybackObservation {
                was_protected,
                protected,
            })
        }

        fn clear_external_playback_tracker(&self) -> Result<()> {
            self.external_playback_tracker.lock().unwrap().clear();
            Ok(())
        }

        fn reconcile_player_session(
            &self,
            binding: Option<PlaybackSessionBinding>,
        ) -> Result<SessionReconciliation> {
            if self
                .runtime
                .lock()
                .unwrap()
                .state()
                .active_request
                .is_none()
            {
                return Ok(SessionReconciliation::NoActiveRequest);
            }
            let Some(incoming) = binding else {
                return Ok(SessionReconciliation::Unknown);
            };
            let mut current = self.session_binding.lock().unwrap();
            let decision = match current.as_ref() {
                None => SessionReconciliation::Bound,
                Some(existing)
                    if existing.runtime_identity == incoming.runtime_identity
                        && existing.session_id == incoming.session_id
                        && existing.generation == incoming.generation =>
                {
                    SessionReconciliation::Match
                }
                Some(existing) if existing.runtime_identity != incoming.runtime_identity => {
                    SessionReconciliation::Restarted
                }
                Some(_) => SessionReconciliation::Replaced,
            };
            if matches!(
                decision,
                SessionReconciliation::Bound
                    | SessionReconciliation::Restarted
                    | SessionReconciliation::Replaced
            ) {
                *current = Some(incoming);
            }
            Ok(decision)
        }

        fn claim_terminal_outcome(
            &self,
            request_id: u64,
            outcome: String,
            _handled_at_ms: u64,
        ) -> Result<bool> {
            Ok(self
                .handled_terminals
                .lock()
                .unwrap()
                .insert((request_id, outcome)))
        }

        fn record_playback_attempt(
            &self,
            provider: String,
            locator: String,
            _started_at_ms: u64,
            result: String,
        ) -> Result<()> {
            self.attempts
                .lock()
                .unwrap()
                .push((provider, locator, result));
            Ok(())
        }

        fn record_control_operation(
            &self,
            operation: String,
            _requested_at_ms: u64,
            completed: bool,
        ) -> Result<()> {
            self.controls.lock().unwrap().push((operation, completed));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeBackend {
        statuses: Arc<Mutex<VecDeque<PlayerStatus>>>,
        status_calls: Arc<Mutex<usize>>,
        status_error_after: Option<usize>,
        paused: Arc<Mutex<u32>>,
        resumed: Arc<Mutex<u32>>,
        play_error: bool,
        pause_error: bool,
    }

    impl FakeBackend {
        fn new(statuses: Vec<PlayerStatus>) -> Self {
            Self {
                statuses: Arc::new(Mutex::new(statuses.into())),
                status_calls: Arc::new(Mutex::new(0)),
                status_error_after: None,
                paused: Arc::new(Mutex::new(0)),
                resumed: Arc::new(Mutex::new(0)),
                play_error: false,
                pause_error: false,
            }
        }

        fn with_status_error(mut self) -> Self {
            self.status_error_after = Some(0);
            self
        }

        fn with_status_error_after(mut self, successful_reads: usize) -> Self {
            self.status_error_after = Some(successful_reads);
            self
        }

        fn with_play_error(mut self) -> Self {
            self.play_error = true;
            self
        }

        fn with_pause_error(mut self) -> Self {
            self.pause_error = true;
            self
        }
    }

    impl MusicPlayerBackend for FakeBackend {
        fn status(&self) -> Result<PlayerStatus> {
            let mut calls = self.status_calls.lock().unwrap();
            if self
                .status_error_after
                .is_some_and(|successful_reads| *calls >= successful_reads)
            {
                *calls += 1;
                return Err(anyhow!("status failed"));
            }
            *calls += 1;
            Ok(self
                .statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default())
        }

        fn play(&self, _track: &PlayableTrack) -> Result<String> {
            if self.play_error {
                return Err(anyhow!("play failed"));
            }
            Ok(String::new())
        }

        fn pause(&self) -> Result<String> {
            *self.paused.lock().unwrap() += 1;
            if self.pause_error {
                return Err(anyhow!("pause failed"));
            }
            Ok("paused".to_string())
        }

        fn resume(&self) -> Result<String> {
            *self.resumed.lock().unwrap() += 1;
            Ok("resumed".to_string())
        }

        fn next(&self) -> Result<String> {
            Ok(String::new())
        }

        fn previous(&self) -> Result<String> {
            Ok(String::new())
        }

        fn set_volume(&self, _volume: &str) -> Result<String> {
            Ok(String::new())
        }
    }

    fn status(name: &str, uri: &str, progress: f64, duration: f64) -> PlayerStatus {
        PlayerStatus {
            status: "playing".to_string(),
            current_track: (!uri.is_empty()).then(|| test_track(uri, name)),
            current_uri: uri.to_string(),
            name: name.to_string(),
            singer: "歌手".to_string(),
            progress,
            duration,
            ..PlayerStatus::default()
        }
    }

    fn stopped_status() -> PlayerStatus {
        PlayerStatus {
            status: "stopped".to_string(),
            ..PlayerStatus::default()
        }
    }

    fn stopped_status_with_uri(uri: &str) -> PlayerStatus {
        PlayerStatus {
            status: "stopped".to_string(),
            current_track: Some(test_track(uri, "测试歌曲 - 测试歌手")),
            current_uri: uri.to_string(),
            ..PlayerStatus::default()
        }
    }

    fn controller(backend: FakeBackend) -> PlayerController<FakeBackend, TestPlaybackState> {
        let system_time = Arc::new(SystemClock);
        controller_with_time(
            backend,
            system_time.clone(),
            system_time.clone(),
            system_time,
        )
    }

    #[derive(Clone, Copy)]
    struct MatchingIdentityJudge;

    impl PlaybackIdentityJudge for MatchingIdentityJudge {
        fn judge(
            &self,
            _request: &PlaybackRequest,
            _status: &PlayerStatus,
        ) -> PlaybackIdentityDecision {
            PlaybackIdentityDecision::Match {
                score: 0.99,
                reason: "测试同曲".to_string(),
            }
        }
    }

    #[derive(Clone, Copy)]
    struct NonMatchingIdentityJudge;

    impl PlaybackIdentityJudge for NonMatchingIdentityJudge {
        fn judge(
            &self,
            _request: &PlaybackRequest,
            _status: &PlayerStatus,
        ) -> PlaybackIdentityDecision {
            PlaybackIdentityDecision::NoMatch {
                score: 0.1,
                reason: "测试不同曲".to_string(),
            }
        }
    }

    fn controller_with_time(
        backend: FakeBackend,
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
        delay: Arc<dyn Delay>,
    ) -> PlayerController<FakeBackend, TestPlaybackState> {
        controller_with_time_and_judge(
            backend,
            clock,
            wall_clock,
            delay,
            Arc::new(DisabledPlaybackIdentityJudge),
        )
    }

    fn controller_with_time_and_judge(
        backend: FakeBackend,
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
        delay: Arc<dyn Delay>,
        identity_judge: Arc<dyn PlaybackIdentityJudge>,
    ) -> PlayerController<FakeBackend, TestPlaybackState> {
        let history_path = temp_path("dedup");
        let runtime = PersistentPlaybackState::new_for_test().unwrap();
        let history = PersistentSongDedupHistory::load(history_path, wall_clock.clone()).unwrap();
        let matching = MatchConfig::default();
        let song_dedup = SongDedupConfig {
            history_path: temp_path("dedup-config"),
            ..SongDedupConfig::default()
        };
        PlayerController::new(
            backend,
            TestPlaybackState {
                runtime: Arc::new(Mutex::new(runtime)),
                history: Arc::new(Mutex::new(history)),
                external_playback_tracker: Arc::new(Mutex::new(ExternalPlaybackTracker::default())),
                session_binding: Arc::new(Mutex::new(None)),
                handled_terminals: Arc::new(Mutex::new(HashSet::new())),
                attempts: Arc::new(Mutex::new(Vec::new())),
                controls: Arc::new(Mutex::new(Vec::new())),
                song_dedup,
            },
            &test_timing(),
            &QueueConfig {
                max_size: 10,
                auto_advance_seconds: 2,
                protect_current_song_until_finished: true,
                external_playback_protect_after_seconds: 20,
            },
            &matching,
            identity_judge,
            PlaybackTimePorts::new(clock, wall_clock, delay),
        )
    }

    fn test_timing() -> PlaybackTimingConfig {
        PlaybackTimingConfig {
            search_settle_ms: 0,
            status_poll_ms: 0,
            status_retries: 3,
            skip_status_initial_ms: 0,
            skip_status_poll_ms: 0,
            skip_status_retries: 1,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
            uri_stable_samples: 0,
            transport_stable_samples: 0,
            fallback_identity_stable_samples: 1,
            stale_timeout_ms: 5000,
        }
    }

    fn request() -> PlaybackRequest {
        playback_request("目标 - 歌手", "miliastra://track/qqmusic/1")
    }

    fn playback_request(keyword: &str, uri: &str) -> PlaybackRequest {
        PlaybackRequest {
            keyword: keyword.to_string(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
            track: Some(test_track(uri, keyword)),
            requester: String::new(),
            navigation: PlaybackNavigation::Normal,
            candidate_snapshot: Vec::new(),
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "miliastra-player-controller-{}-{}-{}.json",
            name,
            std::process::id(),
            seq
        ))
    }

    #[test]
    fn starting_waits_through_old_song_then_confirms_uri() {
        let backend = FakeBackend::new(vec![
            status("旧歌", "miliastra://track/qqmusic/old", 30.0, 120.0),
            status("目标", "miliastra://track/qqmusic/1", 1.0, 180.0),
        ]);
        let controller = controller(backend);
        let request = request();
        let mut attempt = controller.play_request(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::Success { .. }));
        assert_eq!(controller.snapshot().state, "requested_song_playing");
    }

    #[test]
    fn confirmed_playback_keeps_previous_uri_for_direct_previous() {
        let uri_a = "miliastra://track/qqmusic/a";
        let uri_b = "miliastra://track/qqmusic/b";
        let backend = FakeBackend::new(vec![
            stopped_status(),
            status("歌曲A", uri_a, 1.0, 180.0),
            status("歌曲A", uri_a, 2.0, 180.0),
            status("歌曲B", uri_b, 1.0, 180.0),
            status("歌曲B", uri_b, 2.0, 180.0),
            status("歌曲A", uri_a, 3.0, 180.0),
        ]);
        let controller = controller(backend);

        let first = playback_request("歌曲A", uri_a);
        let mut attempt = controller.play_request(&first).unwrap();
        assert!(matches!(
            controller.verify_playback_started(&first, &mut attempt),
            Ok(PlaybackVerification::Success { .. })
        ));

        let second = playback_request("歌曲B", uri_b);
        let mut attempt = controller.play_request(&second).unwrap();
        assert!(matches!(
            controller.verify_playback_started(&second, &mut attempt),
            Ok(PlaybackVerification::Success { .. })
        ));

        let previous = controller
            .previous_playback_request()
            .unwrap()
            .expect("confirmed previous URI");
        assert_eq!(previous.uri(), uri_a);
        assert_eq!(previous.navigation, PlaybackNavigation::Previous);

        let mut previous_attempt = controller.play_request(&previous).unwrap();
        assert!(matches!(
            controller.verify_playback_started(&previous, &mut previous_attempt),
            Ok(PlaybackVerification::Success { .. })
        ));
        assert!(controller.previous_playback_request().unwrap().is_none());
    }

    #[test]
    fn stable_external_observation_becomes_previous_uri_before_new_playback() {
        let external_uri = "miliastra://track/netease/external";
        let backend = FakeBackend::new(vec![
            status("外部歌曲", external_uri, 20.0, 180.0),
            status("新歌曲", "miliastra://track/qqmusic/new", 1.0, 180.0),
        ]);
        let controller = controller(backend);
        controller
            .playback_state
            .update(PlaybackStateUpdate::External)
            .unwrap();
        controller
            .playback_state
            .update(PlaybackStateUpdate::Observation(PlaybackObservation {
                status: "playing".to_string(),
                track: Some(test_track(external_uri, "外部歌曲 - 歌手")),
                title: "外部歌曲".to_string(),
                artist: "歌手".to_string(),
                progress: 20.0,
                duration: 180.0,
                captured_at_ms: 1,
                reliability: ObservationReliability::Reliable,
            }))
            .unwrap();

        let request = playback_request("新歌曲", "miliastra://track/qqmusic/new");
        let _attempt = controller.play_request(&request).unwrap();
        let previous = controller
            .previous_playback_request()
            .unwrap()
            .expect("external URI should be retained");

        assert_eq!(previous.uri(), external_uri);
        assert_eq!(previous.source, "netease");
    }

    #[test]
    fn verification_does_not_accept_matching_title_with_different_uri() {
        let backend = FakeBackend::new(vec![
            status("旧歌", "miliastra://track/qqmusic/old", 30.0, 180.0),
            status("目标", "miliastra://track/qqmusic/other", 1.0, 180.0),
        ]);
        let controller = controller(backend);
        let request = request();
        let mut attempt = controller.play_request(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(
            result,
            PlaybackVerification::MismatchedCandidate(PlaybackMismatch { .. })
        ));
        assert_eq!(controller.snapshot().state, "starting");
    }

    #[test]
    fn cross_source_fallback_requires_stability_and_identity_confirmation() {
        let fallback_uri = "miliastra://track/netease/fallback";
        let backend = FakeBackend::new(vec![
            status("旧歌", "miliastra://track/qqmusic/old", 30.0, 180.0),
            status("别名版本", fallback_uri, 1.0, 180.0),
            status("别名版本", fallback_uri, 2.0, 180.0),
        ]);
        let mut controller = controller_with_time_and_judge(
            backend,
            Arc::new(SystemClock),
            Arc::new(SystemClock),
            Arc::new(SystemClock),
            Arc::new(MatchingIdentityJudge),
        );
        controller.timing.fallback_identity_stable_samples = 2;
        let request = request();
        let mut attempt = controller.play_request(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::Success { .. }));
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "requested_song_playing");
        assert_eq!(snapshot.current_uri, fallback_uri);
        assert_eq!(snapshot.active_uri, fallback_uri);
    }

    #[test]
    fn post_confirmation_cross_source_switch_rechecks_identity_before_external_transition() {
        let fallback_uri = "miliastra://track/netease/fallback";
        let backend = FakeBackend::new(vec![status("目标", fallback_uri, 12.0, 180.0)]);
        let manual_clock = Arc::new(ManualClock::new(Instant::now()));
        let controller = controller_with_time_and_judge(
            backend,
            manual_clock.clone(),
            manual_clock.clone(),
            manual_clock.clone(),
            Arc::new(MatchingIdentityJudge),
        );
        let request = request();
        controller
            .confirm_playback_success(
                &request,
                &status("目标", request.uri().as_str(), 1.0, 180.0),
            )
            .unwrap();
        manual_clock.advance(Duration::from_secs(10)).unwrap();

        let decision = controller
            .maybe_advance_queue(
                status("目标", fallback_uri, 12.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::PlaybackStateChanged);
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "requested_song_playing");
        assert_eq!(snapshot.active_uri, fallback_uri);
        assert!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .previous_requests
                .is_empty()
        );
    }

    #[test]
    fn post_confirmation_cross_source_switch_waits_for_stable_identity() {
        let fallback_uri = "miliastra://track/netease/fallback";
        let other_uri = "miliastra://track/netease/other";
        let backend = FakeBackend::new(vec![
            status("目标", fallback_uri, 12.0, 180.0),
            status("目标", other_uri, 13.0, 180.0),
        ]);
        let manual_clock = Arc::new(ManualClock::new(Instant::now()));
        let mut controller = controller_with_time_and_judge(
            backend,
            manual_clock.clone(),
            manual_clock.clone(),
            manual_clock.clone(),
            Arc::new(MatchingIdentityJudge),
        );
        controller.timing.fallback_identity_stable_samples = 2;
        let request = request();
        controller
            .confirm_playback_success(
                &request,
                &status("目标", request.uri().as_str(), 1.0, 180.0),
            )
            .unwrap();
        manual_clock.advance(Duration::from_secs(10)).unwrap();

        let decision = controller
            .maybe_advance_queue(
                status("目标", fallback_uri, 12.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::None);
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "requested_song_playing");
        assert_eq!(snapshot.active_uri, request.uri());
    }

    #[test]
    fn post_confirmation_cross_source_switch_with_unavailable_identity_enters_unknown() {
        let fallback_uri = "miliastra://track/netease/fallback";
        let backend = FakeBackend::new(vec![status("别名版本", fallback_uri, 12.0, 180.0)]);
        let backend_probe = backend.clone();
        let manual_clock = Arc::new(ManualClock::new(Instant::now()));
        let controller = controller_with_time(
            backend,
            manual_clock.clone(),
            manual_clock.clone(),
            manual_clock.clone(),
        );
        let request = request();
        controller
            .confirm_playback_success(
                &request,
                &status("目标", request.uri().as_str(), 1.0, 180.0),
            )
            .unwrap();
        manual_clock.advance(Duration::from_secs(10)).unwrap();

        let decision = controller
            .maybe_advance_queue(
                status("别名版本", fallback_uri, 12.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::PlaybackStateChanged);
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "unknown");
        assert!(snapshot.active_uri.is_empty());
        assert_eq!(*backend_probe.paused.lock().unwrap(), 1);
    }

    #[test]
    fn post_confirmation_cross_source_switch_with_different_identity_becomes_external() {
        let fallback_uri = "miliastra://track/netease/fallback";
        let backend = FakeBackend::new(vec![status("别名版本", fallback_uri, 12.0, 180.0)]);
        let manual_clock = Arc::new(ManualClock::new(Instant::now()));
        let controller = controller_with_time_and_judge(
            backend,
            manual_clock.clone(),
            manual_clock.clone(),
            manual_clock.clone(),
            Arc::new(NonMatchingIdentityJudge),
        );
        let request = request();
        controller
            .confirm_playback_success(
                &request,
                &status("目标", request.uri().as_str(), 1.0, 180.0),
            )
            .unwrap();
        manual_clock.advance(Duration::from_secs(10)).unwrap();

        let decision = controller
            .maybe_advance_queue(
                status("别名版本", fallback_uri, 12.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::PlaybackStateChanged);
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "external_playback");
        assert!(snapshot.active_uri.is_empty());
    }

    #[test]
    fn stopped_cross_source_observation_does_not_complete_the_active_request() {
        let fallback_uri = "miliastra://track/netease/fallback";
        let backend = FakeBackend::new(vec![stopped_status_with_uri(fallback_uri)]);
        let manual_clock = Arc::new(ManualClock::new(Instant::now()));
        let controller = controller_with_time_and_judge(
            backend,
            manual_clock.clone(),
            manual_clock.clone(),
            manual_clock.clone(),
            Arc::new(MatchingIdentityJudge),
        );
        let request = request();
        controller
            .confirm_playback_success(
                &request,
                &status("目标", request.uri().as_str(), 1.0, 180.0),
            )
            .unwrap();
        manual_clock.advance(Duration::from_secs(10)).unwrap();

        let stopped = stopped_status_with_uri(fallback_uri);
        let decision = controller
            .maybe_advance_queue(
                stopped,
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::None);
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "requested_song_playing");
        assert_eq!(snapshot.active_uri, request.uri());
    }

    #[test]
    fn matching_durable_natural_end_advances_once() {
        let request = request();
        let terminal = PlayerStatus {
            status: "stopped".to_string(),
            current_track: request.track.clone(),
            current_uri: request.uri(),
            runtime_identity: "runtime-a".to_string(),
            session_id: "session-a".to_string(),
            generation: 41,
            end_behavior: "notify_controller".to_string(),
            last_end_cause: "natural_end".to_string(),
            ..PlayerStatus::default()
        };
        let controller = controller(FakeBackend::new(Vec::new()));
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: false,
            has_pending_playback_task: false,
            command_executing: false,
            song_command_executing: false,
        };

        // The first observation creates the durable binding; it cannot also
        // consume a terminal record observed before that binding existed.
        assert_eq!(
            controller
                .maybe_advance_queue(terminal.clone(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(
            controller.maybe_advance_queue(terminal, context).unwrap(),
            QueueAdvanceDecision::AdvanceQueue {
                reason: "自然结束"
            }
        );
        assert_eq!(controller.snapshot().state, "idle");
    }

    #[test]
    fn playback_runtime_restart_requires_and_receives_controller_recovery() {
        let request = request();
        let old_runtime = PlayerStatus {
            status: "playing".to_string(),
            current_track: request.track.clone(),
            current_uri: request.uri(),
            runtime_identity: "runtime-old".to_string(),
            session_id: "session-old".to_string(),
            generation: 7,
            end_behavior: "notify_controller".to_string(),
            ..PlayerStatus::default()
        };
        let restarted = PlayerStatus {
            status: "stopped".to_string(),
            runtime_identity: "runtime-new".to_string(),
            generation: 0,
            ..PlayerStatus::default()
        };
        let recovered = PlayerStatus {
            status: "playing".to_string(),
            current_track: request.track.clone(),
            current_uri: request.uri(),
            runtime_identity: "runtime-new".to_string(),
            session_id: "session-recovered".to_string(),
            generation: 1,
            end_behavior: "notify_controller".to_string(),
            progress: 2.0,
            duration: 180.0,
            ..PlayerStatus::default()
        };
        let controller = controller(FakeBackend::new(vec![recovered.clone(), recovered]));
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
            song_command_executing: false,
        };
        // Bind the original runtime before it becomes unavailable.
        assert_eq!(
            controller
                .maybe_advance_queue(old_runtime, context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );

        assert_eq!(
            controller.maybe_advance_queue(restarted, context).unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(controller.snapshot().state, "requested_song_playing");
        assert_eq!(controller.snapshot().active_uri, request.uri());
    }

    #[test]
    fn verification_rejects_missing_uri_even_when_metadata_is_present() {
        let backend = FakeBackend::new(vec![
            status("旧歌", "miliastra://track/qqmusic/old", 30.0, 180.0),
            status("目标", "", 1.0, 180.0),
        ]);
        let controller = controller(backend);
        let request = request();
        let mut attempt = controller.play_request(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::NoSource { .. }));
        assert_eq!(controller.snapshot().state, "unknown");
    }

    #[test]
    fn external_playback_without_uri_has_no_identity() {
        assert_eq!(
            external_playback_identity(&status("外部歌", "", 1.0, 180.0)),
            None
        );
    }

    #[test]
    fn missing_uri_does_not_protect_the_current_song() {
        let controller = controller(FakeBackend::new(vec![]));
        let request = request();
        controller
            .confirm_playback_success(
                &request,
                &status("目标", request.uri().as_str(), 1.0, 180.0),
            )
            .unwrap();

        assert!(
            !controller
                .should_queue_until_current_song_finished(&status("目标", "", 10.0, 180.0))
                .unwrap()
        );
    }

    #[test]
    fn unknown_status_does_not_advance_queue() {
        let backend = FakeBackend::new(vec![]);
        let controller = controller(backend);
        let decision = controller
            .maybe_advance_queue(
                PlayerStatus {
                    status: "unknown".to_string(),
                    ..PlayerStatus::default()
                },
                QueueAdvanceContext {
                    queue_empty: false,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::None);
    }

    #[test]
    fn stopped_status_does_not_complete_active_request_when_queue_is_empty() {
        let backend = FakeBackend::new(vec![
            status("目标", "miliastra://track/qqmusic/1", 100.0, 100.0),
            stopped_status(),
        ]);
        let controller = controller(backend);
        controller.begin_playback_attempt(&request()).unwrap();
        let mut playback = controller.playback_state.snapshot().unwrap();
        playback.active_request.as_mut().unwrap().guard_started_at =
            Some(Instant::now() - Duration::from_secs(60));
        controller
            .playback_state
            .update(PlaybackStateUpdate::Restore(Box::new(playback)))
            .unwrap();

        let decision = controller
            .maybe_advance_queue(
                stopped_status(),
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::None);
        assert_eq!(controller.snapshot().state, "starting");
        assert_eq!(controller.snapshot().active_keyword, "目标 - 歌手");
    }

    #[test]
    fn play_uri_failure_clears_starting_request() {
        let controller = controller(FakeBackend::new(vec![]).with_play_error());

        let result = controller.play_request(&request());

        assert!(result.is_err());
        assert_eq!(controller.snapshot().state, "idle");
        assert!(controller.snapshot().active_keyword.is_empty());
    }

    #[test]
    fn play_dispatch_outcome_is_recorded_for_a_failed_attempt() {
        let controller = controller(FakeBackend::new(vec![]).with_play_error());

        assert!(controller.play_request(&request()).is_err());

        assert_eq!(
            controller
                .playback_state
                .attempts
                .lock()
                .unwrap()
                .as_slice(),
            [(
                "qqmusic".to_string(),
                "miliastra://track/qqmusic/1".to_string(),
                "dispatch_failed".to_string(),
            )]
        );
    }

    #[test]
    fn unstable_external_playback_does_not_protect_current_song() {
        let controller = controller(FakeBackend::new(vec![]));
        controller.mark_external_playback().unwrap();

        let should_queue = controller
            .should_queue_until_current_song_finished(&status(
                "外部歌",
                "miliastra://track/qqmusic/external",
                30.0,
                180.0,
            ))
            .unwrap();

        assert!(!should_queue);
    }

    #[test]
    fn unstable_external_playback_allows_queue_takeover() {
        let controller = controller(FakeBackend::new(vec![]));
        controller.mark_external_playback().unwrap();

        let decision = controller
            .maybe_advance_queue(
                status("外部歌", "miliastra://track/qqmusic/external", 30.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: false,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(
            decision,
            QueueAdvanceDecision::AdvanceQueue {
                reason: "外部播放未稳定"
            }
        );
    }

    #[test]
    fn external_playback_protects_only_after_same_song_is_stable_for_configured_time() {
        let now = Instant::now();
        let mut tracker = ExternalPlaybackTracker::default();
        let delay = Duration::from_secs(20);

        assert!(!tracker.observe("track:miliastra://track/qqmusic/external", now, delay));
        assert!(!tracker.observe(
            "track:miliastra://track/qqmusic/external",
            now + Duration::from_secs(19),
            delay
        ));
        assert!(tracker.observe(
            "track:miliastra://track/qqmusic/external",
            now + Duration::from_secs(20),
            delay
        ));
        assert!(!tracker.observe(
            "track:miliastra://track/qqmusic/next",
            now + Duration::from_secs(21),
            delay
        ));
    }

    #[test]
    fn external_playback_protection_uses_the_injected_clock() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let controller = controller_with_time(
            FakeBackend::new(vec![]),
            clock.clone(),
            clock.clone(),
            clock.clone(),
        );
        let external = status("外部歌", "miliastra://track/qqmusic/external", 30.0, 180.0);
        controller.mark_external_playback().unwrap();

        assert!(
            !controller
                .should_queue_until_current_song_finished(&external)
                .unwrap()
        );
        clock.advance(Duration::from_secs(20)).unwrap();
        assert!(
            controller
                .should_queue_until_current_song_finished(&external)
                .unwrap()
        );
    }

    #[test]
    fn active_request_guard_uses_only_the_injected_monotonic_anchor() {
        let started_at = Instant::now();
        let clock = ManualClock::with_unix_seconds(started_at, 10);
        let timing = test_timing();
        let active_request = ActivePlaybackRequest {
            // Deliberately unrelated wall-clock metadata: changing it must not affect the guard.
            started_at_ms: u64::MAX,
            guard_started_at: Some(started_at),
            ..ActivePlaybackRequest::default()
        };
        let guard_ms = timing
            .monitor_status_ms
            .max(timing.status_poll_ms)
            .saturating_mul(3)
            .max(3000);

        assert!(active_request_guard_active(
            &timing,
            Some(&active_request),
            clock.now(),
        ));
        clock.advance(Duration::from_millis(guard_ms)).unwrap();
        assert!(!active_request_guard_active(
            &timing,
            Some(&active_request),
            clock.now(),
        ));

        let restored_request = ActivePlaybackRequest {
            started_at_ms: clock.unix_millis(),
            guard_started_at: None,
            ..ActivePlaybackRequest::default()
        };
        assert!(!active_request_guard_active(
            &timing,
            Some(&restored_request),
            clock.now(),
        ));
    }

    #[test]
    fn stable_external_playback_protects_current_song_from_new_requests() {
        let controller = controller(FakeBackend::new(vec![]));
        let external = status("外部歌", "miliastra://track/qqmusic/external", 30.0, 180.0);
        controller.mark_external_playback().unwrap();
        controller
            .playback_state
            .observe_external_playback(
                external_playback_identity(&external).expect("external identity"),
                Instant::now() - Duration::from_secs(20),
                Duration::from_secs(20),
            )
            .unwrap();

        assert!(
            controller
                .should_queue_until_current_song_finished(&external)
                .unwrap()
        );
    }

    #[test]
    fn unknown_state_does_not_auto_advance_queue() {
        let controller = controller(FakeBackend::new(vec![]));
        controller.mark_unknown().unwrap();

        let decision = controller
            .maybe_advance_queue(
                status("未知歌", "miliastra://track/qqmusic/unknown", 179.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: false,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::None);
    }

    #[test]
    fn request_play_uri_failure_restores_previous_request_state() {
        let controller = controller(FakeBackend::new(vec![]).with_play_error());
        let old_request = playback_request("旧歌 - 歌手", "miliastra://track/qqmusic/old");
        let old_status = status("旧歌", "miliastra://track/qqmusic/old", 30.0, 180.0);
        controller
            .confirm_playback_success(&old_request, &old_status)
            .unwrap();

        let result = controller.play_request(&request());

        assert!(result.is_err());
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "requested_song_playing");
        assert_eq!(snapshot.active_keyword, "旧歌 - 歌手");
        assert_eq!(snapshot.active_uri, "miliastra://track/qqmusic/old");
    }

    #[test]
    fn status_backend_failure_is_not_reported_as_confirmed_no_source() {
        let controller = controller(FakeBackend::new(Vec::new()).with_status_error());

        let error = controller
            .play_and_verify(&request())
            .expect_err("status backend failure must remain retryable");

        assert_eq!(error.to_string(), "播放器状态暂不可用，请稍后再试");
        assert_eq!(controller.snapshot().state, "unknown");
    }

    #[test]
    fn later_status_backend_failures_keep_the_playback_attempt_retryable() {
        let controller = controller(
            FakeBackend::new(vec![
                status("旧歌", "miliastra://track/qqmusic/old", 30.0, 180.0),
                stopped_status(),
            ])
            .with_status_error_after(2),
        );

        let error = controller
            .play_and_verify(&request())
            .expect_err("final status failures must not become confirmed no-source");

        assert_eq!(error.to_string(), "播放器状态暂不可用，请稍后再试");
        assert_eq!(controller.snapshot().state, "unknown");
    }

    #[test]
    fn verification_no_source_marks_state_unknown_after_dispatch() {
        let backend = FakeBackend::new(vec![
            status("旧歌", "miliastra://track/qqmusic/old", 30.0, 180.0),
            status("短歌", "miliastra://track/qqmusic/1", 1.0, 10.0),
        ]);
        let controller = controller(backend);
        let old_request = playback_request("旧歌 - 歌手", "miliastra://track/qqmusic/old");
        let old_status = status("旧歌", "miliastra://track/qqmusic/old", 30.0, 180.0);
        controller
            .confirm_playback_success(&old_request, &old_status)
            .unwrap();
        let request = request();
        let mut attempt = controller.play_request(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        let PlaybackVerification::NoSource {
            status: Some(status),
            reason,
        } = result
        else {
            panic!("short playback should report its observed no-source evidence");
        };
        assert_eq!(status.current_uri, "miliastra://track/qqmusic/1");
        assert_eq!(status.duration, 10.0);
        assert_eq!(reason, "歌曲时长过短: 10.0s");
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "unknown");
        assert!(snapshot.active_keyword.is_empty());
        assert!(snapshot.active_uri.is_empty());
    }

    #[test]
    fn verification_timeout_marks_state_unknown_after_dispatch() {
        let backend = FakeBackend::new(vec![status(
            "旧歌",
            "miliastra://track/qqmusic/old",
            30.0,
            180.0,
        )]);
        let controller = controller(backend);
        let old_request = playback_request("旧歌 - 歌手", "miliastra://track/qqmusic/old");
        let old_status = status("旧歌", "miliastra://track/qqmusic/old", 30.0, 180.0);
        controller
            .confirm_playback_success(&old_request, &old_status)
            .unwrap();
        let request = request();
        let mut attempt = controller.play_request(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::NoSource { .. }));
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "unknown");
        assert!(snapshot.active_keyword.is_empty());
        assert!(snapshot.active_uri.is_empty());
    }

    #[test]
    fn rejected_mismatch_marks_state_unknown_after_dispatch() {
        let backend = FakeBackend::new(vec![]);
        let controller = controller(backend.clone());
        let request = request();
        let _attempt = controller.play_request(&request).unwrap();

        controller
            .reject_mismatch_as_no_source(Some(&status(
                "不匹配",
                "miliastra://track/qqmusic/other",
                1.0,
                180.0,
            )))
            .unwrap();

        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "unknown");
        assert!(snapshot.active_keyword.is_empty());
        assert!(snapshot.active_uri.is_empty());
        assert_eq!(*backend.paused.lock().unwrap(), 1);
    }

    #[test]
    fn non_playback_pending_task_does_not_pause_near_end_song() {
        let backend = FakeBackend::new(vec![]);
        let controller = controller(backend.clone());

        let decision = controller
            .maybe_advance_queue(
                status("目标", "miliastra://track/qqmusic/1", 179.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::None);
        assert_eq!(*backend.paused.lock().unwrap(), 0);
    }

    #[test]
    fn playback_pending_task_pauses_near_end_song() {
        let backend = FakeBackend::new(vec![]);
        let controller = controller(backend.clone());

        let decision = controller
            .maybe_advance_queue(
                status("目标", "miliastra://track/qqmusic/1", 179.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: true,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::PauseForQueue);
        assert_eq!(*backend.paused.lock().unwrap(), 1);
    }

    #[test]
    fn waiting_for_queue_pause_resumes_only_when_idle() {
        let backend = FakeBackend::new(vec![]);
        let controller = controller(backend.clone());
        assert!(controller.pause_for_queue().unwrap());

        let decision = controller
            .maybe_advance_queue(
                status("目标", "miliastra://track/qqmusic/1", 10.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::ResumeIfIdle);
        assert_eq!(*backend.resumed.lock().unwrap(), 1);
    }

    #[test]
    fn user_pause_does_not_auto_resume() {
        let backend = FakeBackend::new(vec![]);
        let controller = controller(backend.clone());
        controller.pause_by_user().unwrap();

        let decision = controller
            .maybe_advance_queue(
                status("目标", "miliastra://track/qqmusic/1", 10.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                    song_command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::None);
        assert_eq!(*backend.resumed.lock().unwrap(), 0);
        assert!(controller.user_pause_active().unwrap());
    }

    #[test]
    fn idle_pause_keeps_the_auto_advance_gate_when_backend_pause_fails() {
        let backend = FakeBackend::new(vec![]).with_pause_error();
        let controller = controller(backend);

        assert!(controller.pause_for_idle_exit().is_err());
        assert!(controller.user_pause_active().unwrap());
    }

    #[test]
    fn control_history_records_a_failed_pause() {
        let controller = controller(FakeBackend::new(vec![]).with_pause_error());

        assert!(controller.pause_by_user().is_err());

        assert_eq!(
            controller
                .playback_state
                .controls
                .lock()
                .unwrap()
                .as_slice(),
            [("pause".to_string(), false)]
        );
    }
}
