use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use miliastra_playback::{PlayableTrack, TrackKey};

use crate::features::song_request::SearchCandidate;

use super::dedup::SongDedupCandidate;
use super::format::{format_play_message, playback_remaining_seconds};
use super::state::{
    ActivePlaybackRequest, ConfirmedPlaybackState, ObservationReliability, PauseReason,
    PlaybackObservation, PlaybackRuntimeState, PlaybackSessionBinding, SessionReconciliation,
};
use crate::features::playback::{
    MatchConfig, PlaybackControllerSnapshot, PlaybackStateUpdate, PlaybackTimingConfig,
    PlayerStatus, QueueConfig,
};
use miliastra_kernel::clock::{Clock, WallClock};

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
    /// 把确认播放成功的歌曲写入持久播放池，队列播完后随机播放。
    fn record_playback_pool_track(&self, _track: PlayableTrack) -> Result<()> {
        Ok(())
    }
    /// 播放池是否可用（已启用且非空），用于队列空时的随机播放决策。
    fn playback_pool_available(&self) -> Result<bool> {
        Ok(false)
    }
    fn observe_external_playback(
        &self,
        identity: TrackKey,
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

#[derive(Clone)]
pub(crate) struct PlayerController<B: MusicPlayerBackend, S: PlaybackStatePort> {
    backend: B,
    playback_state: S,
    timing: PlaybackTimingConfig,
    queue: QueueConfig,
    matching: MatchConfig,
    clock: Arc<dyn Clock>,
    wall_clock: Arc<dyn WallClock>,
}

#[derive(Clone)]
pub(crate) struct PlaybackTimePorts {
    clock: Arc<dyn Clock>,
    wall_clock: Arc<dyn WallClock>,
}

impl PlaybackTimePorts {
    pub(crate) fn new(clock: Arc<dyn Clock>, wall_clock: Arc<dyn WallClock>) -> Self {
        Self { clock, wall_clock }
    }
}

#[derive(Default)]
pub(super) struct ExternalPlaybackTracker {
    identity: Option<TrackKey>,
    playing_since: Option<Instant>,
    pub(super) protected: bool,
}

impl ExternalPlaybackTracker {
    pub(super) fn observe(
        &mut self,
        identity: &TrackKey,
        now: Instant,
        protect_after: Duration,
    ) -> bool {
        if self.identity.as_ref() != Some(identity) {
            self.identity = Some(identity.clone());
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
        self.identity = None;
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
    previous_playback: PlaybackRuntimeState,
    started_at_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) enum PlaybackVerification {
    Success {
        status: PlayerStatus,
        message: String,
    },
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueAdvanceDecision {
    None,
    PlaybackStateChanged,
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
        }
    }

    pub(crate) fn status(&self) -> Result<PlayerStatus> {
        let status = self.backend.status()?;
        self.record_observation(&status, classify_observation(&status))?;
        Ok(status)
    }

    /// 监控循环使用的轻量读取：不做观测记录，避免每轮轮询重复记录。
    /// 必要的观测记录由决策路径（状态转移、外部播放确认）负责。
    pub(crate) fn monitor_status(&self) -> Result<PlayerStatus> {
        self.backend.status()
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
        self.clear_external_playback_tracker()?;
        let previous_playback = self.playback_snapshot()?;
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

    /// 内置 ffmpeg 引擎的 play 同步回报成功/失败（失败已在 play_request 处理），
    /// 无需轮询外部播放器状态确认。播放信息直接采用请求时的结构化元数据。
    fn verify_playback_started(
        &self,
        request: &PlaybackRequest,
        _attempt: &mut PlaybackAttempt,
    ) -> Result<PlaybackVerification> {
        let mut status = status_from_request(request);
        // 播放成功消息里的音量显示引擎当前实际音量（音量命令是异步渐变，
        // 请求元数据里没有音量，之前硬编码 0 导致「播放: … 音量0」）。
        if let Ok(current) = self.backend.status() {
            status.volume = current.volume;
        }
        self.record_observation(&status, classify_observation(&status))?;
        let message = format_play_message(&status);
        self.confirm_playback_success(request, &status)?;
        log::info!("播放成功: {}", message);
        Ok(PlaybackVerification::Success { status, message })
    }

    pub(crate) fn play_and_verify(
        &self,
        request: &PlaybackRequest,
    ) -> Result<PlaybackVerification> {
        let mut attempt = self.play_request(request)?;
        self.verify_playback_started(request, &mut attempt)
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
                    if status.current_uri != fresh_status.current_uri
                        || status.status != fresh_status.status
                    {
                        // 监控快照确实过期：刷新并记录，后续 tick 复用一致状态。
                        log::info!(
                            "点歌状态与播放监控快照不一致，已刷新播放状态: snapshot_uri={} fresh_uri={}",
                            status.current_uri,
                            fresh_status.current_uri,
                        );
                        status = fresh_status;
                        self.record_observation(&status, classify_observation(&status))?;
                    } else {
                        // 快照与引擎实时一致且与点歌请求不匹配（外部切歌/引擎异常）：
                        // 属持续状态而非过期快照，降级为 debug 避免每轮轮询刷屏，
                        // 由后续 track_changed/自然结束路径处理。
                        log::debug!(
                            "点歌状态与引擎持续不一致，等待状态转移处理: uri={}",
                            fresh_status.current_uri,
                        );
                    }
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
            // 内置引擎只播请求的 URI；曲目变化只可能来自用户手动控制或引擎异常，
            // 直接视为外部播放，不再做跨源同曲确认。
            log::info!(
                "播放器状态转移: RequestedSongPlaying -> ExternalPlayback reason=track_changed"
            );
            self.record_observation(&status, classify_observation(&status))?;
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

        // 引擎报告播放失败（解码失败/流被拒等），播放已终止。
        // 可重试失败保留队首等待用户处理；不可重试失败丢弃当前请求继续下一首。
        // 失败观测放在点歌保护之前：失败是明确终止信号，不应被起步保护忽略。
        if !status.failure_code.is_empty() && runtime_snapshot.active_request.is_some() {
            log::error!(
                "引擎播放失败: code={} message={}",
                status.failure_code,
                status.failure_message
            );
            if status.failure_retryable {
                log::warn!(
                    "失败可重试，保留队首等待重新播放: uri={}",
                    status.current_uri
                );
                return Ok(QueueAdvanceDecision::None);
            }
            self.clear_active_request()?;
            let _ = self.playback_state.reconcile_player_session(None)?;
            if context.command_executing
                || context.has_pending_playback_task
                || (context.queue_empty && !self.playback_state.playback_pool_available()?)
            {
                return Ok(QueueAdvanceDecision::PlaybackStateChanged);
            }
            log::info!("队列推进决策: advance reason=playback_failure");
            return Ok(QueueAdvanceDecision::AdvanceQueue {
                reason: "播放失败"
            });
        }

        if runtime_snapshot.active_request.is_some()
            && guard_active
            && !is_notify_controller_natural_end(&status)
        {
            log::debug!("点歌刚开始，暂不触发队列自动出队");
            return Ok(QueueAdvanceDecision::None);
        }

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
            // 自然结束已清空 active_request：队列有歌直接推进；队列空时
            // 仅当播放池可用才继续随机播放（能匹配到自然结束本身就是点歌播放）。
            if context.command_executing
                || context.has_pending_playback_task
                || (context.queue_empty && !self.playback_state.playback_pool_available()?)
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

        // 内置引擎播完自然结束，由 natural_end 检测推进；播放中无需预切。
        if status.status != "playing" {
            return Ok(QueueAdvanceDecision::None);
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
            match self.play_and_verify(&request) {
                Ok(PlaybackVerification::Success { status, .. }) => {
                    let _ =
                        self.reconcile_player_session(&self.playback_state.snapshot()?, &status)?;
                    return Ok(QueueAdvanceDecision::PlaybackStateChanged);
                }
                Err(error) => {
                    log::error!("播放运行时重启后的恢复会话无法播放: {error:#}");
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

    fn confirm_playback_success(
        &self,
        request: &PlaybackRequest,
        status: &PlayerStatus,
    ) -> Result<()> {
        self.confirm_playback_success_with_track(request, status, true, "playback_confirmed")
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
        self.playback_state
            .record_playback_pool_track(confirmed_track.clone())?;
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
            // 外部播放确认时补一次观测记录，保证 last_observation 新鲜
            // （监控循环改为轻量读取后不再高频记录）。
            self.record_observation(status, classify_observation(status))?;
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

fn external_playback_identity(status: &PlayerStatus) -> Option<TrackKey> {
    if status.status != "playing" {
        return None;
    }
    status
        .current_track
        .as_ref()
        .map(|track| track.track_ref.key.clone())
}

/// 用点歌请求自身的结构化元数据构造播放确认状态。
/// 内置引擎 play 同步回报成功，无需等待外部状态采样。
fn status_from_request(request: &PlaybackRequest) -> PlayerStatus {
    let track = request.track.clone();
    let metadata = track.as_ref().map(|track| &track.metadata);
    PlayerStatus {
        status: "playing".to_string(),
        current_track: track.clone(),
        current_uri: track
            .as_ref()
            .map(|track| track.track_ref.key.to_string())
            .unwrap_or_default(),
        name: metadata
            .map(|metadata| metadata.title.clone())
            .unwrap_or_default(),
        singer: metadata
            .map(|metadata| metadata.artists.join(" / "))
            .unwrap_or_default(),
        album_name: metadata
            .and_then(|metadata| metadata.album.clone())
            .unwrap_or_default(),
        lyric_line_text: String::new(),
        duration: metadata
            .and_then(|metadata| metadata.duration_ms)
            .map_or(0.0, |millis| millis as f64 / 1000.0),
        progress: 0.0,
        playback_rate: 1.0,
        volume: 0,
        requester: request.requester.clone(),
        ..PlayerStatus::default()
    }
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
        ConfirmedPlaybackState::ExternalPlayback => "external_playback",
        ConfirmedPlaybackState::Unknown => "unknown",
    }
    .to_string()
}

fn format_pause_reason(reason: PauseReason) -> String {
    match reason {
        PauseReason::None => "none",
        PauseReason::User => "user",
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
    use miliastra_kernel::clock::{Clock, ManualClock, SystemClock, WallClock};
    use std::collections::{HashSet, VecDeque};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

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
        pool: Arc<Mutex<Vec<PlayableTrack>>>,
        pool_available: bool,
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

        fn record_playback_pool_track(&self, track: PlayableTrack) -> Result<()> {
            let mut pool = self.pool.lock().unwrap();
            if !pool
                .iter()
                .any(|existing| existing.track_ref.key == track.track_ref.key)
            {
                pool.push(track);
            }
            Ok(())
        }

        fn playback_pool_available(&self) -> Result<bool> {
            Ok(self.pool_available && !self.pool.lock().unwrap().is_empty())
        }

        fn observe_external_playback(
            &self,
            identity: TrackKey,
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
        paused: Arc<Mutex<u32>>,
        resumed: Arc<Mutex<u32>>,
        play_error: bool,
        pause_error: bool,
    }

    impl FakeBackend {
        fn new(statuses: Vec<PlayerStatus>) -> Self {
            Self {
                statuses: Arc::new(Mutex::new(statuses.into())),
                paused: Arc::new(Mutex::new(0)),
                resumed: Arc::new(Mutex::new(0)),
                play_error: false,
                pause_error: false,
            }
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

    fn track_key(uri: &str) -> TrackKey {
        test_track(uri, "test track - test artist").track_ref.key
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
        controller_with_time(backend, system_time.clone(), system_time.clone())
    }

    fn controller_with_time(
        backend: FakeBackend,
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
    ) -> PlayerController<FakeBackend, TestPlaybackState> {
        controller_with_pool(backend, clock, wall_clock, false)
    }

    /// 与 `controller` 相同，但允许启用播放池（用于测试队列空时的随机播放决策）。
    fn controller_with_pool(
        backend: FakeBackend,
        clock: Arc<dyn Clock>,
        wall_clock: Arc<dyn WallClock>,
        pool_available: bool,
    ) -> PlayerController<FakeBackend, TestPlaybackState> {
        let history_path = temp_path("dedup");
        let runtime = PersistentPlaybackState::new_for_test().unwrap();
        let history = PersistentSongDedupHistory::load(
            history_path,
            wall_clock.clone(),
            crate::test_support::test_state_store(),
        )
        .unwrap();
        let matching = MatchConfig::default();
        let song_dedup = SongDedupConfig {
            history_path: temp_path("dedup-config"),
            ..SongDedupConfig::default()
        };
        let pool = if pool_available {
            vec![test_track(
                "miliastra://track/qqmusic/pool-1",
                "池歌一 - 歌手A",
            )]
        } else {
            Vec::new()
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
                pool: Arc::new(Mutex::new(pool)),
                pool_available,
            },
            &test_timing(),
            &QueueConfig {
                max_size: 10,
                protect_current_song_until_finished: true,
                external_playback_protect_after_seconds: 20,
                pool_max_size: if pool_available { 200 } else { 0 },
            },
            &matching,
            PlaybackTimePorts::new(clock, wall_clock),
        )
    }

    fn test_timing() -> PlaybackTimingConfig {
        PlaybackTimingConfig {
            status_poll_ms: 0,
            monitor_tick_ms: 50,
            monitor_status_ms: 50,
            uri_stable_samples: 0,
            transport_stable_samples: 0,
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
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "miliastra-player-controller-{}-{}-{}-{}.json",
            name,
            std::process::id(),
            created_at,
            seq
        ))
    }

    #[test]
    fn playback_started_message_reports_the_engine_volume_instead_of_zero() {
        let mut observed = status("目标", "miliastra://track/qqmusic/1", 1.0, 180.0);
        observed.volume = 70;
        let backend = FakeBackend::new(vec![observed]);
        let controller = controller(backend);
        let request = request();
        let mut attempt = controller.play_request(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        let PlaybackVerification::Success { status, message } = result;
        assert_eq!(status.volume, 70);
        assert!(message.contains("音量70"), "message: {message}");
        assert!(!message.contains("音量0"), "message: {message}");
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
    fn verification_confirms_from_request_metadata_without_observation() {
        let backend = FakeBackend::new(vec![]);
        let controller = controller(backend);
        let request = request();
        let mut attempt = controller.play_request(&request).unwrap();

        let result = controller
            .verify_playback_started(&request, &mut attempt)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::Success { .. }));
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "requested_song_playing");
        assert_eq!(snapshot.current_uri, request.uri());
        assert_eq!(snapshot.active_uri, request.uri());
    }

    #[test]
    fn track_changed_observation_transitions_to_external_playback() {
        let fallback_uri = "miliastra://track/netease/fallback";
        let backend = FakeBackend::new(vec![status("目标", fallback_uri, 12.0, 180.0)]);
        let manual_clock = Arc::new(ManualClock::new(Instant::now()));
        let controller = controller_with_time(backend, manual_clock.clone(), manual_clock.clone());
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
        let controller = controller_with_time(backend, manual_clock.clone(), manual_clock.clone());
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
    fn natural_end_with_empty_queue_advances_from_pool_when_available() {
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
        let controller = controller_with_pool(
            FakeBackend::new(Vec::new()),
            Arc::new(SystemClock),
            Arc::new(SystemClock),
            true,
        );
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        // 队列空但播放池可用时，自然结束应推进到播放池随机播放。
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
    fn external_playback_with_pool_available_never_advances_from_pool() {
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
        let controller = controller_with_pool(
            FakeBackend::new(Vec::new()),
            Arc::new(SystemClock),
            Arc::new(SystemClock),
            true,
        );
        // 外部手动播放：无点歌请求，播放池可用也不得随机插歌。
        controller
            .playback_state
            .update(PlaybackStateUpdate::External)
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        assert_eq!(
            controller.maybe_advance_queue(terminal, context).unwrap(),
            QueueAdvanceDecision::None
        );
    }

    #[test]
    fn engine_failure_drops_active_request_and_advances() {
        let request = request();
        let failure = PlayerStatus {
            status: "stopped".to_string(),
            current_track: request.track.clone(),
            current_uri: request.uri(),
            runtime_identity: "runtime-a".to_string(),
            session_id: "session-a".to_string(),
            generation: 41,
            end_behavior: "notify_controller".to_string(),
            last_end_cause: "decode_failure".to_string(),
            failure_code: "decode_failure".to_string(),
            failure_message: "音源解码失败".to_string(),
            failure_retryable: false,
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
        };

        // 不可重试的播放中失败：丢弃当前请求并推进到队列下一首。
        assert_eq!(
            controller.maybe_advance_queue(failure, context).unwrap(),
            QueueAdvanceDecision::AdvanceQueue {
                reason: "播放失败"
            }
        );
        let snapshot = controller.snapshot();
        assert!(snapshot.active_uri.is_empty());
    }

    #[test]
    fn engine_failure_with_no_next_track_returns_state_changed() {
        let request = request();
        let failure = PlayerStatus {
            status: "stopped".to_string(),
            current_track: request.track.clone(),
            current_uri: request.uri(),
            runtime_identity: "runtime-a".to_string(),
            session_id: "session-a".to_string(),
            generation: 41,
            end_behavior: "notify_controller".to_string(),
            last_end_cause: "decode_failure".to_string(),
            failure_code: "decode_failure".to_string(),
            failure_retryable: false,
            ..PlayerStatus::default()
        };
        let controller = controller(FakeBackend::new(Vec::new()));
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        // 队列空且无播放池：清空请求但不推进，与自然结束行为一致。
        assert_eq!(
            controller.maybe_advance_queue(failure, context).unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(controller.snapshot().state, "idle");
    }

    #[test]
    fn retryable_engine_failure_keeps_active_request() {
        let request = request();
        let failure = PlayerStatus {
            status: "stopped".to_string(),
            current_track: request.track.clone(),
            current_uri: request.uri(),
            runtime_identity: "runtime-a".to_string(),
            session_id: "session-a".to_string(),
            generation: 41,
            end_behavior: "notify_controller".to_string(),
            last_end_cause: "decode_failure".to_string(),
            failure_code: "decode_failure".to_string(),
            failure_message: "音源暂时不可用".to_string(),
            failure_retryable: true,
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
        };

        // 可重试失败：保留队首等待用户处理，不自动推进。
        assert_eq!(
            controller.maybe_advance_queue(failure, context).unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(controller.snapshot().active_uri, request.uri());
    }

    #[test]
    fn monitor_status_reads_without_recording_observation() {
        let backend_status = status("目标", "miliastra://track/qqmusic/1", 10.0, 180.0);
        let controller = controller(FakeBackend::new(vec![
            backend_status.clone(),
            backend_status,
        ]));
        assert!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .is_none()
        );

        // 普通读取会记录观测。
        controller.status().unwrap();
        assert!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .is_some()
        );

        // 监控循环的轻量读取不改变观测记录。
        let before = controller
            .playback_state
            .snapshot()
            .unwrap()
            .last_observation
            .clone();
        controller.monitor_status().unwrap();
        controller.monitor_status().unwrap();
        assert_eq!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .as_ref()
                .map(|observation| observation.captured_at_ms),
            before
                .as_ref()
                .map(|observation| observation.captured_at_ms)
        );
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
        let external = track_key("miliastra://track/qqmusic/external");
        let next = track_key("miliastra://track/qqmusic/next");

        assert!(!tracker.observe(&external, now, delay));
        assert!(!tracker.observe(&external, now + Duration::from_secs(19), delay));
        assert!(tracker.observe(&external, now + Duration::from_secs(20), delay));
        assert!(!tracker.observe(&next, now + Duration::from_secs(21), delay));
    }

    #[test]
    fn external_playback_protection_uses_the_injected_clock() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let controller =
            controller_with_time(FakeBackend::new(vec![]), clock.clone(), clock.clone());
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
