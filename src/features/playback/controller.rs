use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use super::dedup::SongDedupCandidate;
use super::format::{
    format_play_message, format_time, playback_progress_restarted, playback_remaining_seconds,
};
use super::state::{
    ActivePlaybackRequest, ConfirmedPlaybackState, ObservationReliability, PauseReason,
    PlaybackObservation, PlaybackRuntimeState,
};
use crate::features::playback::{
    MatchConfig, PlaybackControllerSnapshot, PlaybackStateUpdate, PlaybackTimingConfig,
    PlayerStatus, QueueConfig,
};
use crate::runtime::clock::{Clock, Delay, WallClock};

pub(crate) trait MusicPlayerBackend: Clone + Send + Sync + 'static {
    fn status(&self) -> Result<PlayerStatus>;
    fn play_uri(&self, uri: &str) -> Result<String>;
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
}

#[derive(Clone)]
pub(crate) struct PlayerController<B: MusicPlayerBackend, S: PlaybackStatePort> {
    backend: B,
    playback_state: S,
    timing: PlaybackTimingConfig,
    queue: QueueConfig,
    matching: MatchConfig,
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
    pub(crate) uri: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PlaybackAttempt {
    pub(crate) initial_uri: String,
    pub(crate) initial_progress: f64,
    pub(crate) requested_uri: String,
    previous_playback: PlaybackRuntimeState,
}

#[cfg(test)]
impl PlaybackAttempt {
    pub(super) fn for_test(requested_uri: impl Into<String>) -> Self {
        Self {
            initial_uri: String::new(),
            initial_progress: 0.0,
            requested_uri: requested_uri.into(),
            previous_playback: PlaybackRuntimeState::default(),
        }
    }
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
    NoSource,
    MismatchedCandidate(PlaybackMismatch),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackOutcome {
    Success,
    NoSource,
    Error,
    DedupLimited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MismatchDecision {
    NoSource,
    SwitchSource,
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
        time: PlaybackTimePorts,
    ) -> Self {
        Self {
            backend,
            playback_state,
            timing: timing.clone(),
            queue: queue.clone(),
            matching: matching.clone(),
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
        let message = self.backend.pause()?;
        self.clear_external_playback_tracker()?;
        self.playback_state
            .update(PlaybackStateUpdate::UserPaused)?;
        log::info!("播放器状态转移: pause_reason=user");
        Ok(message)
    }

    pub(crate) fn resume_by_user(&self) -> Result<String> {
        let message = self.backend.resume()?;
        self.playback_state
            .update(PlaybackStateUpdate::UserResumed)?;
        log::info!("播放器状态转移: pause_reason=none");
        Ok(message)
    }

    pub(crate) fn next_external(&self) -> Result<String> {
        let message = self.backend.next()?;
        self.clear_external_playback_tracker()?;
        self.mark_external_playback()?;
        Ok(message)
    }

    pub(crate) fn previous_external(&self) -> Result<String> {
        let message = self.backend.previous()?;
        self.clear_external_playback_tracker()?;
        self.mark_external_playback()?;
        Ok(message)
    }

    pub(crate) fn set_volume(&self, volume: &str) -> Result<String> {
        self.backend.set_volume(volume)
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
            return Ok(!status.current_uri.trim().is_empty());
        }
        if status.status == "paused"
            && (playback_remaining_seconds(status).is_some()
                || !status.current_uri.trim().is_empty())
        {
            return Ok(true);
        }

        if playback.active_request.is_none() {
            return Ok(false);
        }
        if status_matches_active_request(&self.matching, playback.active_request.as_ref(), status) {
            return Ok(true);
        }
        if status.current_uri.trim().is_empty() {
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
        let candidate = request_dedup_candidate(request);
        self.playback_state.song_dedup_limited(candidate)
    }

    pub(crate) fn begin_playback_attempt(
        &self,
        request: &PlaybackRequest,
    ) -> Result<PlaybackAttempt> {
        self.clear_external_playback_tracker()?;
        let previous_playback = self.playback_snapshot()?;
        let initial = self
            .backend
            .status()
            .map(|status| (status.current_uri, status.progress))
            .unwrap_or_default();
        self.playback_state
            .update(PlaybackStateUpdate::Starting(ActivePlaybackRequest {
                keyword: request.keyword.clone(),
                source: request.source.clone(),
                prefer_accompaniment: request.prefer_accompaniment,
                requested_uri: request.uri.clone(),
                confirmed_uri: String::new(),
                song: String::new(),
                title: String::new(),
                artist: String::new(),
                started_at_ms: self.wall_clock.unix_millis(),
                guard_started_at: Some(self.clock.now()),
            }))?;
        log::info!("播放器状态转移: Starting keyword={}", request.keyword);
        Ok(PlaybackAttempt {
            initial_uri: initial.0,
            initial_progress: initial.1,
            requested_uri: request.uri.clone(),
            previous_playback: previous_playback.clone(),
        })
    }

    pub(crate) fn play_request_uri(&self, request: &PlaybackRequest) -> Result<PlaybackAttempt> {
        let attempt = self.begin_playback_attempt(request)?;
        if let Err(error) = self.backend.play_uri(&request.uri) {
            let _ = self.restore_failed_attempt(&attempt, "dispatch_failed");
            return Err(error);
        }
        Ok(attempt)
    }

    pub(crate) fn verify_playback_started(
        &self,
        request: &PlaybackRequest,
        attempt: &mut PlaybackAttempt,
    ) -> Result<PlaybackVerification> {
        self.delay
            .wait(Duration::from_millis(self.timing.search_settle_ms));

        for retry in 0..self.timing.status_retries {
            let status = match self.backend.status() {
                Ok(status) => status,
                Err(error) => {
                    log::error!("查询播放状态失败: {error:#}");
                    self.mark_unknown()?;
                    self.delay
                        .wait(Duration::from_millis(self.timing.status_poll_ms));
                    continue;
                }
            };
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

            let current_uri = status.current_uri.trim().to_string();
            let requested_uri = attempt.requested_uri.trim();
            if requested_uri.is_empty() {
                log::info!(
                    "播放请求缺少 URI，无法确认歌曲身份 ({}/{})",
                    retry + 1,
                    self.timing.status_retries
                );
                self.delay
                    .wait(Duration::from_millis(self.timing.status_poll_ms));
                continue;
            }
            if current_uri.is_empty() {
                log::info!(
                    "播放器观测缺少 URI，继续等待 ({}/{})",
                    retry + 1,
                    self.timing.status_retries
                );
                self.delay
                    .wait(Duration::from_millis(self.timing.status_poll_ms));
                continue;
            }
            if current_uri != requested_uri {
                if !attempt.initial_uri.is_empty()
                    && current_uri == attempt.initial_uri
                    && !playback_progress_restarted(attempt.initial_progress, status.progress)
                {
                    log::info!(
                        "歌曲 URI 尚未切换，继续等待播放请求生效 ({}/{})",
                        retry + 1,
                        self.timing.status_retries
                    );
                    self.delay
                        .wait(Duration::from_millis(self.timing.status_poll_ms));
                    continue;
                }
                log::info!(
                    "URI 与请求资源不同，不能用歌曲信息兜底: current={} requested={} ({}/{})",
                    current_uri,
                    requested_uri,
                    retry + 1,
                    self.timing.status_retries
                );
                return Ok(PlaybackVerification::MismatchedCandidate(
                    PlaybackMismatch {
                        status,
                        local_reason: format!(
                            "播放器 URI 与请求不一致: current={} requested={}",
                            current_uri, requested_uri
                        ),
                    },
                ));
            }

            let progress = format_time(status.progress);
            let duration = format_time(status.duration);
            if (progress == "0:00" && duration == "0:00") || duration == "error" {
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
                return Ok(PlaybackVerification::NoSource);
            }

            let message = format_play_message(&status);
            self.confirm_playback_success(request, &status)?;
            log::info!("播放成功: {}", message);
            return Ok(PlaybackVerification::Success { status, message });
        }

        log::info!("超时未播放成功");
        self.restore_failed_attempt(attempt, "verification_failed")?;
        Ok(PlaybackVerification::NoSource)
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
        if runtime_snapshot.state == ConfirmedPlaybackState::Unknown {
            return Ok(QueueAdvanceDecision::None);
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
            && active_request_track_changed(
                runtime_snapshot.active_request.as_ref(),
                &status,
                &self.matching,
            )
        {
            self.clear_active_request()?;
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

        if runtime_snapshot.active_request.is_some() && guard_active {
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

        if status.status == "stopped" || status.status == "stoped" {
            let had_active_request = runtime_snapshot.active_request.is_some();
            if had_active_request {
                self.clear_active_request()?;
                log::info!("播放器状态转移: RequestedSongPlaying -> Idle reason=stopped");
            }
            if context.command_executing || context.has_pending_playback_task || context.queue_empty
            {
                return Ok(if had_active_request {
                    QueueAdvanceDecision::PlaybackStateChanged
                } else {
                    QueueAdvanceDecision::None
                });
            }
            self.playback_state
                .update(PlaybackStateUpdate::ClearPauseReason)?;
            log::info!("队列推进决策: advance reason=stopped");
            return Ok(QueueAdvanceDecision::AdvanceQueue { reason: "停止" });
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
                        .map(|request| {
                            if request.confirmed_uri.trim().is_empty() {
                                request.requested_uri.clone()
                            } else {
                                request.confirmed_uri.clone()
                            }
                        })
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
                        .map(|observation| observation.uri.clone())
                        .unwrap_or_default(),
                    title: observation
                        .map(|observation| observation.title.clone())
                        .unwrap_or_default(),
                    artist: observation
                        .map(|observation| observation.artist.clone())
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
        let requested_uri = request.uri.trim();
        let confirmed_uri = status.current_uri.trim();
        if requested_uri.is_empty() {
            return Err(anyhow!("播放请求缺少 URI，不能确认播放成功"));
        }
        if confirmed_uri.is_empty() {
            return Err(anyhow!("播放器观测缺少 URI，不能确认播放成功"));
        }
        if confirmed_uri != requested_uri {
            return Err(anyhow!("播放器观测 URI 与请求不一致，不能确认播放成功"));
        }
        let active_request = ActivePlaybackRequest {
            keyword: request.keyword.clone(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            requested_uri: request.uri.clone(),
            confirmed_uri: confirmed_uri.to_string(),
            song: format!("{}{}", status.name, status.singer),
            title: status.name.trim().to_string(),
            artist: status.singer.trim().to_string(),
            started_at_ms: self.wall_clock.unix_millis(),
            guard_started_at: Some(self.clock.now()),
        };
        self.playback_state
            .update(PlaybackStateUpdate::Confirmed(active_request))?;
        self.record_song_dedup_playback(request, confirmed_uri, status)?;
        log::info!("播放器状态转移: Starting -> RequestedSongPlaying reason=playback_confirmed");
        Ok(())
    }

    fn record_song_dedup_playback(
        &self,
        request: &PlaybackRequest,
        confirmed_uri: &str,
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
            uri: confirmed_uri.to_string(),
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
            uri: status.current_uri.clone(),
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
                && playback.pause_reason != PauseReason::WaitingForQueue;
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
            .update(PlaybackStateUpdate::Restore(playback))
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
    let uri = status.current_uri.trim();
    (!uri.is_empty()).then(|| format!("uri:{uri}"))
}

fn status_matches_active_request(
    _matching: &MatchConfig,
    active_request: Option<&ActivePlaybackRequest>,
    status: &PlayerStatus,
) -> bool {
    let Some(active_request) = active_request else {
        return false;
    };
    let current_uri = status.current_uri.trim();
    let requested_uri = if active_request.confirmed_uri.trim().is_empty() {
        active_request.requested_uri.trim()
    } else {
        active_request.confirmed_uri.trim()
    };
    !current_uri.is_empty() && !requested_uri.is_empty() && current_uri == requested_uri
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
    let current_uri = status.current_uri.trim();
    let requested_uri = if active_request.confirmed_uri.trim().is_empty() {
        active_request.requested_uri.trim()
    } else {
        active_request.confirmed_uri.trim()
    };
    let changed =
        !current_uri.is_empty() && !requested_uri.is_empty() && current_uri != requested_uri;
    changed && !status_matches_active_request(matching, Some(active_request), status)
}

fn request_dedup_candidate(request: &PlaybackRequest) -> SongDedupCandidate {
    let (title, artist) = split_title_artist(&request.keyword);
    SongDedupCandidate {
        uri: request.uri.clone(),
        title,
        artist,
        source: request.source.clone(),
        prefer_accompaniment: request.prefer_accompaniment,
    }
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
    if status.current_uri.trim().is_empty() {
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
    use crate::features::playback::SongDedupConfig;
    use crate::runtime::clock::{Clock, Delay, ManualClock, SystemClock, WallClock};
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone)]
    struct TestPlaybackState {
        runtime: Arc<Mutex<PersistentPlaybackState>>,
        history: Arc<Mutex<PersistentSongDedupHistory>>,
        external_playback_tracker: Arc<Mutex<ExternalPlaybackTracker>>,
        song_dedup: SongDedupConfig,
    }

    impl PlaybackStatePort for TestPlaybackState {
        fn snapshot(&self) -> Result<PlaybackRuntimeState> {
            Ok(self.runtime.lock().unwrap().state().clone())
        }

        fn update(&self, update: PlaybackStateUpdate) -> Result<bool> {
            let mut runtime = self.runtime.lock().unwrap();
            let changed = update.apply(runtime.state_mut());
            if changed {
                runtime.save()?;
            }
            Ok(changed)
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
    }

    #[derive(Clone)]
    struct FakeBackend {
        statuses: Arc<Mutex<VecDeque<PlayerStatus>>>,
        paused: Arc<Mutex<u32>>,
        resumed: Arc<Mutex<u32>>,
        play_error: bool,
    }

    impl FakeBackend {
        fn new(statuses: Vec<PlayerStatus>) -> Self {
            Self {
                statuses: Arc::new(Mutex::new(statuses.into())),
                paused: Arc::new(Mutex::new(0)),
                resumed: Arc::new(Mutex::new(0)),
                play_error: false,
            }
        }

        fn with_play_error(mut self) -> Self {
            self.play_error = true;
            self
        }
    }

    impl MusicPlayerBackend for FakeBackend {
        fn status(&self) -> Result<PlayerStatus> {
            Ok(self
                .statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default())
        }

        fn play_uri(&self, _uri: &str) -> Result<String> {
            if self.play_error {
                return Err(anyhow!("play failed"));
            }
            Ok(String::new())
        }

        fn pause(&self) -> Result<String> {
            *self.paused.lock().unwrap() += 1;
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

    fn controller(backend: FakeBackend) -> PlayerController<FakeBackend, TestPlaybackState> {
        let system_time = Arc::new(SystemClock);
        controller_with_time(
            backend,
            system_time.clone(),
            system_time.clone(),
            system_time,
        )
    }

    fn controller_with_time(
        backend: FakeBackend,
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
        delay: Arc<dyn Delay>,
    ) -> PlayerController<FakeBackend, TestPlaybackState> {
        let runtime_path = temp_path("runtime");
        let history_path = temp_path("dedup");
        let _ = fs::remove_file(&runtime_path);
        let _ = fs::remove_file(&history_path);
        let runtime = PersistentPlaybackState::load(runtime_path).unwrap();
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
            stale_timeout_ms: 5000,
        }
    }

    fn request() -> PlaybackRequest {
        playback_request("目标 - 歌手", "fuo://qqmusic/songs/1")
    }

    fn playback_request(keyword: &str, uri: &str) -> PlaybackRequest {
        PlaybackRequest {
            keyword: keyword.to_string(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
            uri: uri.to_string(),
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
            status("旧歌", "fuo://qqmusic/songs/old", 30.0, 120.0),
            status("目标", "fuo://qqmusic/songs/1", 1.0, 180.0),
        ]);
        let controller = controller(backend);
        let request = request();
        let mut attempt = controller.play_request_uri(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::Success { .. }));
        assert_eq!(controller.snapshot().state, "requested_song_playing");
    }

    #[test]
    fn verification_does_not_accept_matching_title_with_different_uri() {
        let backend = FakeBackend::new(vec![
            status("旧歌", "fuo://qqmusic/songs/old", 30.0, 180.0),
            status("目标", "fuo://qqmusic/songs/other", 1.0, 180.0),
        ]);
        let controller = controller(backend);
        let request = request();
        let mut attempt = controller.play_request_uri(&request).unwrap();

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
    fn verification_rejects_missing_uri_even_when_metadata_is_present() {
        let backend = FakeBackend::new(vec![
            status("旧歌", "fuo://qqmusic/songs/old", 30.0, 180.0),
            status("目标", "", 1.0, 180.0),
        ]);
        let controller = controller(backend);
        let request = request();
        let mut attempt = controller.play_request_uri(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::NoSource));
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
            .confirm_playback_success(&request, &status("目标", request.uri.as_str(), 1.0, 180.0))
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
    fn stopped_status_clears_active_request_when_queue_is_empty() {
        let backend = FakeBackend::new(vec![
            status("目标", "fuo://qqmusic/songs/1", 100.0, 100.0),
            stopped_status(),
        ]);
        let controller = controller(backend);
        controller.begin_playback_attempt(&request()).unwrap();
        let mut playback = controller.playback_state.snapshot().unwrap();
        playback.active_request.as_mut().unwrap().guard_started_at =
            Some(Instant::now() - Duration::from_secs(60));
        controller
            .playback_state
            .update(PlaybackStateUpdate::Restore(playback))
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

        assert_eq!(decision, QueueAdvanceDecision::PlaybackStateChanged);
        assert_eq!(controller.snapshot().state, "idle");
        assert!(controller.snapshot().active_keyword.is_empty());
    }

    #[test]
    fn play_uri_failure_clears_starting_request() {
        let controller = controller(FakeBackend::new(vec![]).with_play_error());

        let result = controller.play_request_uri(&request());

        assert!(result.is_err());
        assert_eq!(controller.snapshot().state, "idle");
        assert!(controller.snapshot().active_keyword.is_empty());
    }

    #[test]
    fn unstable_external_playback_does_not_protect_current_song() {
        let controller = controller(FakeBackend::new(vec![]));
        controller.mark_external_playback().unwrap();

        let should_queue = controller
            .should_queue_until_current_song_finished(&status(
                "外部歌",
                "fuo://qqmusic/songs/external",
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
                status("外部歌", "fuo://qqmusic/songs/external", 30.0, 180.0),
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

        assert!(!tracker.observe("uri:fuo://qqmusic/songs/external", now, delay));
        assert!(!tracker.observe(
            "uri:fuo://qqmusic/songs/external",
            now + Duration::from_secs(19),
            delay
        ));
        assert!(tracker.observe(
            "uri:fuo://qqmusic/songs/external",
            now + Duration::from_secs(20),
            delay
        ));
        assert!(!tracker.observe(
            "uri:fuo://qqmusic/songs/next",
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
        let external = status("外部歌", "fuo://qqmusic/songs/external", 30.0, 180.0);
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
        let external = status("外部歌", "fuo://qqmusic/songs/external", 30.0, 180.0);
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
                status("未知歌", "fuo://qqmusic/songs/unknown", 179.0, 180.0),
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
        let old_request = playback_request("旧歌 - 歌手", "fuo://qqmusic/songs/old");
        let old_status = status("旧歌", "fuo://qqmusic/songs/old", 30.0, 180.0);
        controller
            .confirm_playback_success(&old_request, &old_status)
            .unwrap();

        let result = controller.play_request_uri(&request());

        assert!(result.is_err());
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "requested_song_playing");
        assert_eq!(snapshot.active_keyword, "旧歌 - 歌手");
        assert_eq!(snapshot.active_uri, "fuo://qqmusic/songs/old");
    }

    #[test]
    fn verification_no_source_marks_state_unknown_after_dispatch() {
        let backend = FakeBackend::new(vec![
            status("旧歌", "fuo://qqmusic/songs/old", 30.0, 180.0),
            status("短歌", "fuo://qqmusic/songs/1", 1.0, 10.0),
        ]);
        let controller = controller(backend);
        let old_request = playback_request("旧歌 - 歌手", "fuo://qqmusic/songs/old");
        let old_status = status("旧歌", "fuo://qqmusic/songs/old", 30.0, 180.0);
        controller
            .confirm_playback_success(&old_request, &old_status)
            .unwrap();
        let request = request();
        let mut attempt = controller.play_request_uri(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::NoSource));
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "unknown");
        assert!(snapshot.active_keyword.is_empty());
        assert!(snapshot.active_uri.is_empty());
    }

    #[test]
    fn verification_timeout_marks_state_unknown_after_dispatch() {
        let backend =
            FakeBackend::new(vec![status("旧歌", "fuo://qqmusic/songs/old", 30.0, 180.0)]);
        let controller = controller(backend);
        let old_request = playback_request("旧歌 - 歌手", "fuo://qqmusic/songs/old");
        let old_status = status("旧歌", "fuo://qqmusic/songs/old", 30.0, 180.0);
        controller
            .confirm_playback_success(&old_request, &old_status)
            .unwrap();
        let request = request();
        let mut attempt = controller.play_request_uri(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::NoSource));
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
        let _attempt = controller.play_request_uri(&request).unwrap();

        controller
            .reject_mismatch_as_no_source(Some(&status(
                "不匹配",
                "fuo://qqmusic/songs/other",
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
                status("目标", "fuo://qqmusic/songs/1", 179.0, 180.0),
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
                status("目标", "fuo://qqmusic/songs/1", 179.0, 180.0),
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
                status("目标", "fuo://qqmusic/songs/1", 10.0, 180.0),
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
                status("目标", "fuo://qqmusic/songs/1", 10.0, 180.0),
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
    }
}
