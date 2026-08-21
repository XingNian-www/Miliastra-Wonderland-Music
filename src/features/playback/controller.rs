use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use miliastra_playback::{PlayableTrack, TrackKey};

use crate::features::song_request::SearchCandidate;

use super::dedup::SongDedupCandidate;
use super::format::format_play_message;
use super::state::{
    ActivePlaybackIdentity, ActivePlaybackRequest, ConfirmedPlaybackState, ObservationReliability,
    PauseReason, PlaybackObservation, PlaybackRuntimeState, PlaybackSessionBinding,
    SessionReconciliation,
};
use crate::features::playback::{
    MatchConfig, PlaybackControllerSnapshot, PlaybackStateUpdate, PlayerStatus,
};
use miliastra_kernel::clock::{Clock, WallClock};

pub(crate) trait MusicPlayerBackend: Clone + Send + Sync + 'static {
    fn status(&self) -> Result<PlayerStatus>;
    fn play(&self, track: &PlayableTrack, requested: bool) -> Result<String>;
    /// 恢复播放：携带起始位置（秒），None 表示从头播放。
    ///
    /// 重启恢复活动歌曲时由控制器调用，走新引擎会话；默认实现从头播放，
    /// 需要 seek 的后端自行覆盖（内置引擎支持从指定进度续播）。
    fn play_restored(&self, track: &PlayableTrack, _seek_seconds: Option<f64>) -> Result<String> {
        self.play(track, false)
    }
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
    /// 内置引擎不实现队列导航（导航归应用层），实现侧一律拒绝；保留作防御。
    #[allow(dead_code)]
    fn next(&self) -> Result<String>;
    /// 内置引擎不实现队列导航（导航归应用层），实现侧一律拒绝；保留作防御。
    #[allow(dead_code)]
    fn previous(&self) -> Result<String>;
    fn set_volume(&self, volume: &str) -> Result<String>;
    fn toggle_lyrics(&self) -> Result<String>;
    /// 明确设置歌词是否使用翻译（不等价于切换）：恢复播放时应用持久化模式。
    /// 默认实现直接成功（后端不支持时无副作用），需要该能力的后端自行覆盖。
    fn set_lyrics_translation(&self, _use_translation: bool) -> Result<String> {
        Ok("ok".to_string())
    }
    /// 清除曲目的音频缓存（播放解码失败后自愈：下次播放重新下载）。
    fn invalidate_audio_cache(&self, _key: &miliastra_playback::TrackKey) -> Result<()> {
        Ok(())
    }
}

pub(crate) trait PlaybackStatePort: Clone + Send + Sync + 'static {
    fn snapshot(&self) -> Result<PlaybackRuntimeState>;
    fn update(&self, update: PlaybackStateUpdate) -> Result<bool>;
    /// Atomically records an observation only while the expected request remains active.
    ///
    /// `true` means the identity matched (the observation itself may have been throttled);
    /// `false` means a newer request owns the durable state.
    fn record_observation_if_active(
        &self,
        expected: ActivePlaybackIdentity,
        observation: PlaybackObservation,
        immediate: bool,
    ) -> Result<bool>;
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
    fn inspect_player_session(
        &self,
        _binding: Option<PlaybackSessionBinding>,
    ) -> Result<SessionReconciliation> {
        Ok(SessionReconciliation::Unknown)
    }

    /// Accepts the incoming binding after the controller has confirmed that it belongs to a live
    /// session, or after a recovery dispatch has completed.
    fn reconcile_player_session(
        &self,
        _binding: Option<PlaybackSessionBinding>,
    ) -> Result<SessionReconciliation> {
        Ok(SessionReconciliation::Unknown)
    }
    /// 原子确认播放成功并删除对应队列项（同一笔持久化）。
    ///
    /// 生产端口在同一个请求状态事务里完成「确认 + 队首出队」，消除
    /// 「确认已持久化、出队未持久化」的崩溃窗口：窗口内进程退出时，
    /// 重启不会把已确认消费的队首再次播放，也不会在确认失败前丢歌。
    /// 默认实现退化为非原子确认，出队由消费流程在确认后补偿（幂等）。
    fn confirm_playback_and_dequeue(
        &self,
        update: PlaybackStateUpdate,
        _queue_item_id: Option<u64>,
    ) -> Result<bool> {
        self.update(update)
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

pub(crate) struct PlayerController<B: MusicPlayerBackend, S: PlaybackStatePort> {
    backend: B,
    playback_state: S,
    matching: Arc<RwLock<MatchConfig>>,
    clock: Arc<dyn Clock>,
    wall_clock: Arc<dyn WallClock>,
    /// 热更新共享句柄（阶段 7）：保存成功后由 HTTP 层 apply，运行态读取点
    /// 从这里取值，不再读启动时 clone 的 AppConfig。
    queue_protect_current_song: Arc<RwLock<bool>>,
    queue_external_protect_seconds: Arc<RwLock<u64>>,
    status_poll_ms: Arc<RwLock<u64>>,
    monitor_status_ms: Arc<RwLock<u64>>,
    /// 上次空闲续播尝试的墙钟毫秒：播放尝试失败会回到 Idle，冷却窗口内不重复触发，避免热循环。
    /// 用 Arc 共享：控制器副本（worker/任务各自持有）必须看到同一计时器，
    /// 否则副本会无视冷却立即再次触发空闲续播，形成热循环。
    last_idle_advance_at_ms: Arc<AtomicU64>,
    /// 引擎不可重试失败首次出现的墙钟毫秒（0 = 无失败）。失败后先给 core 流重试
    /// （第一轮缓存、第二轮清缓存直连）留窗口，持续失败超过窗口才放弃当前曲目。
    /// 用 Arc 共享：副本各自记录起点会把同一失败当作新的首次失败，
    /// 窗口被无限重置，导致失败曲目永不推进。
    engine_failure_at_ms: Arc<AtomicU64>,
    /// A runtime restart that cannot be recovered immediately must remain recoverable on the next
    /// monitor round even though persistent session reconciliation has already seen the runtime.
    runtime_recovery: Arc<Mutex<Option<RuntimeRecoveryState>>>,
    /// Serializes monitor reconciliation/recovery with formal playback dispatches across all
    /// controller clones. A stale monitor sample must not write state while a new request starts.
    playback_operation_lease: Arc<Mutex<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RuntimeRecoveryState {
    Pending(String),
    Recovering(String),
}

impl<B: MusicPlayerBackend, S: PlaybackStatePort> Clone for PlayerController<B, S> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            playback_state: self.playback_state.clone(),
            matching: self.matching.clone(),
            clock: self.clock.clone(),
            wall_clock: self.wall_clock.clone(),
            // 共享句柄：任一副本与 HTTP 层看到同一份热更新值。
            queue_protect_current_song: self.queue_protect_current_song.clone(),
            queue_external_protect_seconds: self.queue_external_protect_seconds.clone(),
            status_poll_ms: self.status_poll_ms.clone(),
            monitor_status_ms: self.monitor_status_ms.clone(),
            // 共享计时器，而不是拷贝当前值：任一副本写入的时间戳对所有副本可见。
            last_idle_advance_at_ms: self.last_idle_advance_at_ms.clone(),
            engine_failure_at_ms: self.engine_failure_at_ms.clone(),
            runtime_recovery: self.runtime_recovery.clone(),
            playback_operation_lease: self.playback_operation_lease.clone(),
        }
    }
}

/// 空闲续播失败后的冷却窗口。
const IDLE_ADVANCE_COOLDOWN_MS: u64 = 30_000;

/// 引擎不可重试失败后等待 core 流重试的总窗口。
const ENGINE_RETRY_WINDOW_MS: u64 = 8_000;

/// 失败信号持续到可推进队列的最小间隔（防抖，避免瞬时失败误推进）。
const ENGINE_RETRY_MIN_INTERVAL_MS: u64 = 1_000;

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
    /// 队列消费来源时携带队首 queue_item_id：确认成功时与播放状态原子出队，
    /// 崩溃后重启不会重播已确认消费的队首。手动点歌/恢复播放为 None。
    pub(crate) queue_item_id: Option<u64>,
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
    Restore,
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
        time: PlaybackTimePorts,
        live: crate::config::LiveConfigs,
    ) -> Self {
        Self {
            backend,
            playback_state,
            matching: live.matching.clone(),
            clock: time.clock,
            wall_clock: time.wall_clock,
            queue_protect_current_song: live.queue_protect_current_song,
            queue_external_protect_seconds: live.queue_external_protect_seconds,
            status_poll_ms: live.status_poll_ms,
            monitor_status_ms: live.monitor_status_ms,
            last_idle_advance_at_ms: Arc::new(AtomicU64::new(0)),
            engine_failure_at_ms: Arc::new(AtomicU64::new(0)),
            runtime_recovery: Arc::new(Mutex::new(None)),
            playback_operation_lease: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn status(&self) -> Result<PlayerStatus> {
        let mut status = self.backend.status()?;
        let playback = self.playback_state.snapshot()?;
        let matches_active_request = {
            let matching = self
                .matching
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            status_matches_active_request(&matching, playback.active_request.as_ref(), &status)
        };
        if matches_active_request {
            status.requester = playback
                .active_request
                .as_ref()
                .map(|request| request.requester.clone())
                .unwrap_or_default();
        }
        Ok(status)
    }

    /// 配置重载关停前读取播放器。持久化必须在 runtime 身份校准之后进行，
    /// 因此由 `maybe_advance_queue_for_reload` 以立即写入策略完成。
    pub(crate) fn status_for_reload(&self) -> Result<PlayerStatus> {
        self.status()
    }

    /// 监控循环读取播放器状态。观测要等 `maybe_advance_queue` 完成 runtime
    /// 身份校准后再持久化，避免新 runtime 的初始 stopped 状态覆盖可恢复进度。
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

    pub(crate) fn set_volume(&self, volume: &str) -> Result<String> {
        let requested_at_ms = self.wall_clock.unix_millis();
        let result = self.backend.set_volume(volume);
        self.record_control_outcome("set_volume", requested_at_ms, result.is_ok());
        let message = result?;
        // 设置成功后把音量写入持久化播放状态（SQLite），供重启恢复播放前应用。
        if let Ok(volume) = volume.trim().parse::<u8>() {
            self.playback_state
                .update(PlaybackStateUpdate::Volume(volume))?;
        }
        Ok(message)
    }

    pub(crate) fn toggle_lyrics(&self) -> Result<String> {
        let requested_at_ms = self.wall_clock.unix_millis();
        let result = self.backend.toggle_lyrics();
        self.record_control_outcome("toggle_lyrics", requested_at_ms, result.is_ok());
        let message = result?;
        // 切换成功后把歌词模式写入持久化播放状态（SQLite），供重启恢复。
        if let Some(use_translation) = lyrics_mode_from_message(&message) {
            self.playback_state
                .update(PlaybackStateUpdate::LyricsMode(use_translation))?;
        }
        Ok(message)
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

    /// 外部播放器时代遗留：仅测试使用，运行时无调用点；保留以便回归验证外部状态转移。
    #[allow(dead_code)]
    pub(crate) fn mark_external_playback(&self) -> Result<()> {
        self.clear_external_playback_tracker()?;
        self.playback_state
            .update(PlaybackStateUpdate::External)
            .map(|_| ())
    }

    pub(crate) fn current_status_matches_request(&self, status: &PlayerStatus) -> Result<bool> {
        let runtime = self.playback_state.snapshot()?;
        let matching = self
            .matching
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(status_matches_active_request(
            &matching,
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
            queue_item_id: None,
        }))
    }

    pub(crate) fn should_queue_until_current_song_finished(
        &self,
        status: &PlayerStatus,
    ) -> Result<bool> {
        if !*self
            .queue_protect_current_song
            .read()
            .expect("队列当前歌曲保护共享锁已中毒")
        {
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
        Ok(matches!(status.status.as_str(), "playing" | "paused"))
    }

    pub(crate) fn song_dedup_limited(&self, request: &PlaybackRequest) -> Result<bool> {
        let Some(candidate) = request_dedup_candidate(request) else {
            return Ok(false);
        };
        self.playback_state.song_dedup_limited(candidate)
    }

    fn begin_playback_attempt(&self, request: &PlaybackRequest) -> Result<PlaybackAttempt> {
        self.clear_external_playback_tracker()?;
        // 新播放周期开始：重置失败重试窗口时间戳。上一首歌的失败残留若不清除，
        // 会在本次起播（解析/起播可能耗时数秒，引擎状态仍是旧失败）期间被误判为已超窗，
        // 导致当前请求被清掉并推进队列（连跳）。
        self.engine_failure_at_ms.store(0, Ordering::Relaxed);
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
                expected_session_id: String::new(),
                expected_generation: 0,
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
            .and_then(|track| self.backend.play(track, !request.requester.is_empty()));
        self.record_attempt_outcome(request, attempt.started_at_ms, result.is_ok());
        if let Err(error) = result {
            if let Err(restore_error) = self.restore_failed_attempt(&attempt, "dispatch_failed") {
                log::error!(
                    "播放失败且状态恢复也失败: play_error={error:#} restore_error={restore_error:#}"
                );
            }
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
        initial_progress: Option<f64>,
    ) -> Result<PlaybackVerification> {
        let mut status = status_from_request(request);
        if let Some(progress) = initial_progress {
            status.progress = progress;
        }
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
        let _lease = self
            .playback_operation_lease
            .lock()
            .map_err(|_| anyhow!("播放操作 lease 锁已损坏"))?;
        let mut attempt = self.play_request(request)?;
        self.verify_playback_started(request, &mut attempt, None)
    }

    /// 重启后恢复上次会话的活动歌曲（新引擎会话），返回是否发起了恢复。
    ///
    /// 恢复规则：
    /// - 仅恢复已确认播放（RequestedSongPlaying / PausedByUser）且带结构化曲目的请求；
    /// - 最后观测明确已停止或已切换到其它曲目时丢弃过期请求，不复活旧歌；
    /// - 起始位置采用最后可靠观测进度：非法/负数/距结尾不足 5 秒时从头播放；
    /// - 恢复前先应用持久化音量（引擎启动默认 100，与上次会话音量可能不同）；
    /// - 上次会话是用户暂停时，恢复歌曲后立即保持暂停语义。
    pub(crate) fn play_restored(&self) -> Result<bool> {
        let _lease = self
            .playback_operation_lease
            .lock()
            .map_err(|_| anyhow!("播放操作 lease 锁已损坏"))?;
        self.play_restored_with_lease(None)
    }

    fn play_restored_with_lease(
        &self,
        expected_active: Option<&ActivePlaybackIdentity>,
    ) -> Result<bool> {
        let runtime = self.playback_state.snapshot()?;
        let Some(active) = runtime.active_request.clone() else {
            return Ok(false);
        };
        if let Some(expected) = expected_active
            && active.identity().as_ref() != Some(expected)
        {
            log::info!("跳过过期的播放恢复：活动请求已在取得播放 lease 后发生变化");
            return Ok(false);
        }
        if !matches!(
            runtime.state,
            ConfirmedPlaybackState::RequestedSongPlaying | ConfirmedPlaybackState::PausedByUser
        ) {
            return Ok(false);
        }
        let Some(active_track) = active.track.as_ref() else {
            log::warn!("跳过恢复缺少结构化曲目的活动请求，并清理过期播放状态");
            self.playback_state
                .update(PlaybackStateUpdate::ClearActiveRequest)?;
            return Ok(false);
        };
        if runtime
            .last_observation
            .as_ref()
            .is_some_and(|observation| restoration_observation_conflicts(active_track, observation))
        {
            log::warn!(
                "最后播放器观测与活动请求冲突，跳过恢复并清理过期播放状态: uri={}",
                active_track.track_ref.key
            );
            self.playback_state
                .update(PlaybackStateUpdate::ClearActiveRequest)?;
            return Ok(false);
        }
        let request = playback_request_from_active(&active);
        let keep_paused = runtime.pause_reason == PauseReason::User;
        let seek_seconds = restored_seek_seconds_for_track(
            active_track,
            runtime.last_observation.as_ref(),
            keep_paused,
        );
        // 恢复前先应用持久化音量：引擎重启后默认 100，必须恢复到上次会话的音量。
        if let Err(error) = self.backend.set_volume(&runtime.volume.to_string()) {
            log::warn!("恢复播放前设置音量失败: {error:#}");
        }
        let mut attempt = self.begin_playback_attempt(&request)?;
        let result = self.backend.play_restored(active_track, seek_seconds);
        self.record_attempt_outcome(&request, attempt.started_at_ms, result.is_ok());
        if let Err(error) = result {
            if let Err(restore_error) = self.restore_failed_attempt(&attempt, "dispatch_failed") {
                log::error!(
                    "恢复播放失败且状态恢复也失败: play_error={error:#} restore_error={restore_error:#}"
                );
            }
            return Err(error);
        }
        let verification = self.verify_playback_started(&request, &mut attempt, seek_seconds)?;
        // 应用持久化歌词模式：引擎起播默认使用翻译，恢复时必须按上次会话的设置。
        // 明确设置（非 toggle），引擎加载中的歌词任务完成后按该模式显示。
        if let Err(error) = self.backend.set_lyrics_translation(runtime.use_translation) {
            log::warn!("恢复播放后设置歌词模式失败: {error:#}");
        }
        // 原用户暂停：歌曲已开始播放，立即暂停以保持用户暂停语义。
        if keep_paused {
            match self.backend.pause() {
                Ok(_) => {
                    self.playback_state
                        .update(PlaybackStateUpdate::UserPaused)?;
                    log::info!("恢复播放后保持用户暂停: uri={}", request.uri());
                }
                Err(error) => log::warn!("恢复播放后保持暂停失败: {error:#}"),
            }
        }
        // 恢复会话是新引擎会话：立即绑定，避免监控把新会话误判为 runtime 重启。
        let PlaybackVerification::Success { status, .. } = &verification;
        let _ = self.reconcile_player_session(&self.playback_state.snapshot()?, status)?;
        log::info!(
            "播放器状态转移: restored uri={} seek={:?} keep_paused={}",
            request.uri(),
            seek_seconds,
            keep_paused
        );
        Ok(true)
    }

    pub(crate) fn maybe_advance_queue(
        &self,
        snapshot_status: PlayerStatus,
        context: QueueAdvanceContext,
    ) -> Result<QueueAdvanceDecision> {
        let _lease = self
            .playback_operation_lease
            .lock()
            .map_err(|_| anyhow!("播放操作 lease 锁已损坏"))?;
        self.maybe_advance_queue_with_observation_policy(snapshot_status, context, false)
    }

    pub(crate) fn maybe_advance_queue_for_reload(
        &self,
        snapshot_status: PlayerStatus,
        context: QueueAdvanceContext,
    ) -> Result<QueueAdvanceDecision> {
        let _lease = self
            .playback_operation_lease
            .lock()
            .map_err(|_| anyhow!("播放操作 lease 锁已损坏"))?;
        self.maybe_advance_queue_with_observation_policy(snapshot_status, context, true)
    }

    fn maybe_advance_queue_with_observation_policy(
        &self,
        snapshot_status: PlayerStatus,
        context: QueueAdvanceContext,
        immediate_observation: bool,
    ) -> Result<QueueAdvanceDecision> {
        let status = snapshot_status;
        // 失败信号消失（重试成功或干净结束）时重置重试窗口计时，避免残留时间戳
        // 导致下一首歌的首次失败被误判为已超窗。
        if status.failure_code.is_empty() {
            self.engine_failure_at_ms.store(0, Ordering::Relaxed);
        }
        let external_playback_ended_this_round = self.playback_state.snapshot()?.state
            == ConfirmedPlaybackState::ExternalPlayback
            && matches!(status.status.as_str(), "stopped" | "stoped" | "idle");
        let external_playback_protected = self.observe_external_playback(&status)?.unwrap_or(false);
        let runtime_snapshot = self.playback_state.snapshot()?;
        let runtime_identity = status.runtime_identity.trim();
        let command_busy = context.command_executing || context.has_pending_playback_task;
        let runtime_has_session = !status.session_id.trim().is_empty();
        let runtime_is_idle = matches!(status.status.as_str(), "stopped" | "stoped" | "idle");
        let runtime_is_active = matches!(status.status.as_str(), "playing" | "paused");
        let status_matches_active = runtime_snapshot
            .active_request
            .as_ref()
            .and_then(|request| request.track.as_ref())
            .zip(status.current_track.as_ref())
            .is_some_and(|(active, observed)| active.track_ref.key == observed.track_ref.key);
        let has_terminal_evidence =
            !status.last_end_cause.trim().is_empty() || !status.failure_code.trim().is_empty();
        let incoming_live_session_verified =
            runtime_is_active && runtime_has_session && status_matches_active;
        // A first observed session may legitimately be a terminal notification. It is not a
        // replacement of an existing runtime, and binding it is required to process the matching
        // natural-end/failure event once. Replacements themselves still require live transport.
        let incoming_initial_terminal_session_verified =
            runtime_has_session && status_matches_active && has_terminal_evidence;
        let restart_recovery_ready = runtime_is_idle
            && (!runtime_has_session
                || (!status.failure_code.trim().is_empty() && !status_matches_active));

        {
            let mut recovery = self
                .runtime_recovery
                .lock()
                .map_err(|_| anyhow!("待恢复播放器 runtime 锁已损坏"))?;
            match recovery.clone() {
                Some(RuntimeRecoveryState::Pending(_)) if runtime_identity.is_empty() => {
                    // An identity-less sample cannot supersede a known replacement runtime.
                    return Ok(QueueAdvanceDecision::None);
                }
                Some(RuntimeRecoveryState::Recovering(expected_runtime)) => {
                    if runtime_identity.is_empty() {
                        return Ok(QueueAdvanceDecision::None);
                    }
                    if expected_runtime == runtime_identity {
                        if incoming_live_session_verified {
                            // The recovery dispatch is now observable. Accept its session below.
                        } else if has_terminal_evidence {
                            *recovery =
                                Some(RuntimeRecoveryState::Pending(runtime_identity.to_string()));
                        } else {
                            // Suppress duplicate recovery and stale stopped/unknown persistence
                            // until the dispatched session becomes active or terminal.
                            return Ok(QueueAdvanceDecision::None);
                        }
                    } else if incoming_live_session_verified {
                        // A newer runtime established a live session; accept it below.
                    } else {
                        *recovery =
                            Some(RuntimeRecoveryState::Pending(runtime_identity.to_string()));
                    }
                }
                _ => {}
            }
        }

        let mut session_reconciliation = self.inspect_player_session(&runtime_snapshot, &status)?;
        match session_reconciliation {
            SessionReconciliation::Restarted => {
                if incoming_live_session_verified {
                    let _ = self.reconcile_player_session(&runtime_snapshot, &status)?;
                    *self
                        .runtime_recovery
                        .lock()
                        .map_err(|_| anyhow!("待恢复播放器 runtime 锁已损坏"))? = None;
                    session_reconciliation = SessionReconciliation::Match;
                } else if command_busy || !restart_recovery_ready {
                    *self
                        .runtime_recovery
                        .lock()
                        .map_err(|_| anyhow!("待恢复播放器 runtime 锁已损坏"))? =
                        Some(RuntimeRecoveryState::Pending(runtime_identity.to_string()));
                    return Ok(QueueAdvanceDecision::None);
                } else {
                    let recovery_identity = runtime_identity.to_string();
                    {
                        let mut recovery = self
                            .runtime_recovery
                            .lock()
                            .map_err(|_| anyhow!("待恢复播放器 runtime 锁已损坏"))?;
                        if matches!(recovery.as_ref(), Some(RuntimeRecoveryState::Recovering(_))) {
                            return Ok(QueueAdvanceDecision::None);
                        }
                        *recovery =
                            Some(RuntimeRecoveryState::Recovering(recovery_identity.clone()));
                    }
                    return self.recover_runtime_restart_serialized(
                        &runtime_snapshot,
                        &status,
                        &context,
                        &recovery_identity,
                    );
                }
            }
            SessionReconciliation::Bound => {
                if !incoming_live_session_verified && !incoming_initial_terminal_session_verified {
                    log::debug!(
                        "忽略未验证的播放器会话: reconciliation={:?} runtime={} session={} status={}",
                        session_reconciliation,
                        runtime_identity,
                        status.session_id,
                        status.status
                    );
                    return Ok(QueueAdvanceDecision::None);
                }
                let _ = self.reconcile_player_session(&runtime_snapshot, &status)?;
                *self
                    .runtime_recovery
                    .lock()
                    .map_err(|_| anyhow!("待恢复播放器 runtime 锁已损坏"))? = None;
            }
            SessionReconciliation::Replaced => {
                if !incoming_live_session_verified {
                    log::debug!(
                        "忽略未验证的播放器会话: reconciliation={:?} runtime={} session={} status={}",
                        session_reconciliation,
                        runtime_identity,
                        status.session_id,
                        status.status
                    );
                    return Ok(QueueAdvanceDecision::None);
                }
                let _ = self.reconcile_player_session(&runtime_snapshot, &status)?;
                *self
                    .runtime_recovery
                    .lock()
                    .map_err(|_| anyhow!("待恢复播放器 runtime 锁已损坏"))? = None;
                session_reconciliation = SessionReconciliation::Match;
            }
            SessionReconciliation::Idle => {
                let _ = self.reconcile_player_session(&runtime_snapshot, &status)?;
            }
            SessionReconciliation::Match | SessionReconciliation::Unknown => {}
        }
        if runtime_snapshot.active_request.is_some()
            && runtime_identity.is_empty()
            && !runtime_is_active
        {
            // A stopped/unknown sample without runtime identity cannot replace a durable resume point;
            // a later identified sample reconciles it.
            return Ok(QueueAdvanceDecision::None);
        }
        if runtime_snapshot.active_request.is_some()
            && status.current_track.is_some()
            && !status_matches_active
        {
            log::debug!("忽略与当前活动 TrackKey 不一致的播放器观测");
            return Ok(QueueAdvanceDecision::None);
        }
        // Only observations from the accepted runtime may replace the durable resume point.
        // In particular, a replacement runtime starts out stopped before recovery is dispatched.
        if let Some(active) = runtime_snapshot.active_request.as_ref() {
            let Some(expected_active) = active.identity() else {
                return Ok(QueueAdvanceDecision::None);
            };
            if !self.record_observation_for_active_with_policy(
                expected_active,
                &status,
                classify_observation(&status),
                immediate_observation,
            )? {
                log::debug!("活动请求已变化，丢弃过期播放器观测");
                return Ok(QueueAdvanceDecision::None);
            }
        } else {
            self.record_observation_with_policy(
                &status,
                classify_observation(&status),
                immediate_observation,
            )?;
        }
        if runtime_snapshot.state == ConfirmedPlaybackState::Unknown {
            return Ok(QueueAdvanceDecision::None);
        }
        let guard_active = active_request_guard_active(
            *self
                .monitor_status_ms
                .read()
                .expect("播放状态校准间隔共享锁已中毒"),
            *self
                .status_poll_ms
                .read()
                .expect("播放状态查询间隔共享锁已中毒"),
            runtime_snapshot.active_request.as_ref(),
            self.clock.now(),
        );

        // 内置播放后端持有已解析的实际音源 URL。监控中的 TrackKey 经过独立稳定化，
        // 起播换歌时可能短暂保留上一首身份，因此不能据此中断当前请求或推进队列。
        // 当前请求只由传输状态、绑定会话的自然结束和明确失败信号驱动。

        // 播放器防闲置：引擎空闲（无点播请求、无外部播放、无失败信号、无用户暂停）
        // 且队列或播放池有内容时自动续播（覆盖启动后、自然结束兜底、异常恢复）。
        // 播放尝试失败会回到 Idle，按冷却窗口退避，避免连续失败时热循环。
        if runtime_snapshot.state == ConfirmedPlaybackState::Idle
            && runtime_snapshot.active_request.is_none()
            && runtime_snapshot.pause_reason == PauseReason::None
            && !external_playback_ended_this_round
            && matches!(status.status.as_str(), "stopped" | "stoped")
            && status.failure_code.is_empty()
            && !context.command_executing
            && !context.has_pending_playback_task
            && (!context.queue_empty || self.playback_state.playback_pool_available()?)
        {
            let now_ms = self.wall_clock.unix_millis();
            if now_ms.saturating_sub(self.last_idle_advance_at_ms.load(Ordering::Relaxed))
                < IDLE_ADVANCE_COOLDOWN_MS
            {
                log::debug!("空闲续播冷却中，暂不触发");
                return Ok(QueueAdvanceDecision::None);
            }
            self.last_idle_advance_at_ms
                .store(now_ms, Ordering::Relaxed);
            log::info!("队列推进决策: advance reason=idle_keep_alive");
            return Ok(QueueAdvanceDecision::AdvanceQueue {
                reason: "空闲续播"
            });
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
        // 失败观测放在点歌保护之前：失败是明确终止信号，不应被起步保护忽略。
        if !status.failure_code.is_empty() && runtime_snapshot.active_request.is_some() {
            // 用户暂停中：保留暂停状态，失败不推进队列，避免静默解除暂停并自动播放下一首。
            // （暂停命令发出后引擎流可能仍在缓冲，源站断流时会产生失败信号。）
            if self.playback_state.snapshot()?.pause_reason == PauseReason::User {
                log::debug!(
                    "用户暂停中，播放失败保留暂停状态: code={} message={}",
                    status.failure_code,
                    status.failure_message
                );
                return Ok(QueueAdvanceDecision::None);
            }
            // 只处理归属当前播放会话的失败：advance 后引擎状态更新有延迟，旧会话的失败
            // 残留（session/generation 不匹配）不得触发推进，否则会连跳下一首。
            // 未确认的请求（expected 为空）不阻断，交给重试窗口兜底。
            let belongs_to_active =
                runtime_snapshot
                    .active_request
                    .as_ref()
                    .is_none_or(|request| {
                        request.expected_session_id.is_empty()
                            || (request.expected_session_id == status.session_id.trim()
                                && request.expected_generation == status.generation)
                    });
            if !belongs_to_active {
                log::debug!(
                    "忽略非当前会话的失败残留: code={} session={} generation={}",
                    status.failure_code,
                    status.session_id,
                    status.generation
                );
                return Ok(QueueAdvanceDecision::None);
            }
            if status.failure_retryable {
                log::error!(
                    "引擎播放失败: code={} message={}",
                    status.failure_code,
                    status.failure_message
                );
                log::warn!(
                    "失败可重试，保留队首等待重新播放: uri={}",
                    status.current_uri
                );
                return Ok(QueueAdvanceDecision::None);
            }
            // 不可重试失败：core 层仍会流重试（第一轮缓存、第二轮清缓存直连），
            // 这里给重试留窗口，持续失败超过窗口才清除缓存并推进队列。
            let now_ms = self.wall_clock.unix_millis();
            let first_seen = self.engine_failure_at_ms.load(Ordering::Relaxed);
            if first_seen == 0 {
                self.engine_failure_at_ms.store(now_ms, Ordering::Relaxed);
                log::error!(
                    "引擎播放失败: code={} message={}，等待流重试(窗口{}ms)",
                    status.failure_code,
                    status.failure_message,
                    ENGINE_RETRY_WINDOW_MS
                );
                return Ok(QueueAdvanceDecision::None);
            }
            let elapsed = now_ms.saturating_sub(first_seen);
            if elapsed < ENGINE_RETRY_MIN_INTERVAL_MS {
                log::debug!(
                    "引擎播放失败持续，最小间隔内暂不决策: code={} message={}",
                    status.failure_code,
                    status.failure_message
                );
                return Ok(QueueAdvanceDecision::None);
            }
            if elapsed < ENGINE_RETRY_WINDOW_MS {
                log::debug!(
                    "引擎播放失败持续，等待流重试(剩余{}ms): code={} message={}",
                    ENGINE_RETRY_WINDOW_MS - elapsed,
                    status.failure_code,
                    status.failure_message
                );
                return Ok(QueueAdvanceDecision::None);
            }
            log::warn!(
                "流重试窗口超时，放弃当前曲目: code={} message={}",
                status.failure_code,
                status.failure_message
            );
            self.engine_failure_at_ms.store(0, Ordering::Relaxed);
            // 自动推进前保留上一曲历史，与手动点歌路径一致（@上一曲可用）。
            self.playback_state
                .update(PlaybackStateUpdate::RememberCurrentPlayback)?;
            // 清除曲目音频缓存，下次播放重新下载自愈。
            if let Some(key) = runtime_snapshot
                .active_request
                .as_ref()
                .and_then(|request| request.track.as_ref())
                .map(|track| track.track_ref.key.clone())
                && let Err(error) = self.backend.invalidate_audio_cache(&key)
            {
                log::warn!("清除曲目音频缓存失败: {error:#}");
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
            // 自然结束清空前保留上一曲历史，与手动点歌路径一致（@上一曲可用）。
            self.playback_state
                .update(PlaybackStateUpdate::RememberCurrentPlayback)?;
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
            return Ok(SessionReconciliation::Idle);
        }
        let runtime_identity = status.runtime_identity.trim();
        if runtime_identity.is_empty() {
            return Ok(SessionReconciliation::Unknown);
        }
        self.playback_state.reconcile_player_session(Some(
            self.player_session_binding(status)
                .expect("non-empty runtime identity checked above"),
        ))
    }

    fn inspect_player_session(
        &self,
        runtime: &PlaybackRuntimeState,
        status: &PlayerStatus,
    ) -> Result<SessionReconciliation> {
        if runtime.active_request.is_none() {
            return Ok(SessionReconciliation::Idle);
        }
        let Some(binding) = self.player_session_binding(status) else {
            return Ok(SessionReconciliation::Unknown);
        };
        self.playback_state.inspect_player_session(Some(binding))
    }

    fn player_session_binding(&self, status: &PlayerStatus) -> Option<PlaybackSessionBinding> {
        let runtime_identity = status.runtime_identity.trim();
        if runtime_identity.is_empty() {
            return None;
        }
        Some(PlaybackSessionBinding {
            runtime_identity: runtime_identity.to_string(),
            session_id: status.session_id.trim().to_string(),
            generation: status.generation,
            bound_at_ms: self.wall_clock.unix_millis(),
        })
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

    fn recover_runtime_restart_serialized(
        &self,
        runtime: &PlaybackRuntimeState,
        status: &PlayerStatus,
        context: &QueueAdvanceContext,
        runtime_identity: &str,
    ) -> Result<QueueAdvanceDecision> {
        let result = self.recover_after_runtime_restart(runtime, status, context);
        let dispatched = result
            .as_ref()
            .is_ok_and(|decision| *decision == QueueAdvanceDecision::PlaybackStateChanged)
            && self.playback_state.snapshot().is_ok_and(|playback| {
                playback.active_request.is_some()
                    && matches!(
                        playback.state,
                        ConfirmedPlaybackState::RequestedSongPlaying
                            | ConfirmedPlaybackState::PausedByUser
                    )
            });
        let mut recovery = self
            .runtime_recovery
            .lock()
            .map_err(|_| anyhow!("待恢复播放器 runtime 锁已损坏"))?;
        if !dispatched
            && matches!(
                recovery.as_ref(),
                Some(RuntimeRecoveryState::Recovering(identity))
                    if identity == runtime_identity
            )
        {
            *recovery = None;
        }
        result
    }

    fn recover_after_runtime_restart(
        &self,
        runtime: &PlaybackRuntimeState,
        status: &PlayerStatus,
        context: &QueueAdvanceContext,
    ) -> Result<QueueAdvanceDecision> {
        // 正式播放任务或用户命令执行中不触发恢复：monitor 若此时 play 会与
        // 播放任务并发起播，新会话互相覆盖。恢复由播放任务完成后的下一次
        // 决策接管（其 context 中这两个标志已复位）。
        if context.command_executing || context.has_pending_playback_task {
            log::debug!("播放运行时已重启，但播放任务仍在执行，跳过恢复");
            return Ok(QueueAdvanceDecision::None);
        }
        let Some(active) = runtime.active_request.as_ref() else {
            return Ok(QueueAdvanceDecision::None);
        };
        let Some(expected_active) = active.identity() else {
            return Ok(QueueAdvanceDecision::None);
        };
        if matches!(status.status.as_str(), "stopped" | "stoped" | "idle")
            && (status.session_id.trim().is_empty() || !status.failure_code.trim().is_empty())
        {
            let request = playback_request_from_active(active);
            log::warn!(
                "检测到 playback runtime 重启，控制器授权新恢复会话: previous_uri={}",
                request.uri()
            );
            // `maybe_advance_queue*` already holds the shared playback-operation lease. Recheck
            // the exact active identity before dispatch so a request that won the lease earlier
            // cannot be replaced by this stale recovery decision.
            match self.play_restored_with_lease(Some(&expected_active)) {
                Ok(true) => return Ok(QueueAdvanceDecision::PlaybackStateChanged),
                Ok(false) => return Ok(QueueAdvanceDecision::None),
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
        // 沿用 Starting 阶段的发起时刻，而不是确认时刻：起播（解析/流请求）可能耗时数秒，
        // 若改写为确认时刻，active_request_identity（TrackKey + started_at_ms）会变化，
        // 持久层据此判定请求身份变更而清空 session binding，导致已绑定的引擎会话被无谓作废、
        // 需要重新绑定（期间可能被误判为会话重启/替换）。无 Starting 上下文（如直接确认）
        // 时退回当前墙钟，保持原有行为。
        let started_at_ms = self
            .playback_state
            .snapshot()?
            .active_request
            .as_ref()
            .map(|request| request.started_at_ms)
            .unwrap_or_else(|| self.wall_clock.unix_millis());
        let active_request = ActivePlaybackRequest {
            keyword: request.keyword.clone(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            track: Some(confirmed_track.clone()),
            song: format!("{}{}", status.name, status.singer),
            title: status.name.trim().to_string(),
            artist: status.singer.trim().to_string(),
            requester: request.requester.clone(),
            started_at_ms,
            guard_started_at: Some(self.clock.now()),
            expected_session_id: status.session_id.trim().to_string(),
            expected_generation: status.generation,
        };
        self.playback_state.confirm_playback_and_dequeue(
            PlaybackStateUpdate::Confirmed {
                request: active_request,
                navigation: request.navigation,
            },
            request.queue_item_id,
        )?;
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
        self.record_observation_with_policy(status, reliability, false)
    }

    fn record_observation_with_policy(
        &self,
        status: &PlayerStatus,
        reliability: ObservationReliability,
        immediate: bool,
    ) -> Result<()> {
        let observation = self.playback_observation(status, reliability);
        let update = if immediate {
            PlaybackStateUpdate::ImmediateObservation(observation)
        } else {
            PlaybackStateUpdate::Observation(observation)
        };
        self.playback_state.update(update).map(|_| ())
    }

    fn record_observation_for_active_with_policy(
        &self,
        expected: ActivePlaybackIdentity,
        status: &PlayerStatus,
        reliability: ObservationReliability,
        immediate: bool,
    ) -> Result<bool> {
        self.playback_state.record_observation_if_active(
            expected,
            self.playback_observation(status, reliability),
            immediate,
        )
    }

    fn playback_observation(
        &self,
        status: &PlayerStatus,
        reliability: ObservationReliability,
    ) -> PlaybackObservation {
        PlaybackObservation {
            status: status.status.clone(),
            track: status.current_track.clone(),
            title: status.name.clone(),
            artist: status.singer.clone(),
            progress: status.progress,
            duration: status.duration,
            captured_at_ms: self.wall_clock.unix_millis(),
            reliability,
        }
    }

    fn mark_unknown(&self) -> Result<()> {
        self.clear_external_playback_tracker()?;
        self.playback_state
            .update(PlaybackStateUpdate::Unknown)
            .map(|_| ())
    }

    fn observe_external_playback(&self, status: &PlayerStatus) -> Result<Option<bool>> {
        let (is_external, was_external, should_mark_external) = {
            let runtime = self.playback_state.snapshot()?;
            let playback = &runtime;
            let is_external = playback.active_request.is_none()
                && playback.state != ConfirmedPlaybackState::Unknown
                && playback.pause_reason == PauseReason::None;
            (
                is_external,
                is_external && playback.state == ConfirmedPlaybackState::ExternalPlayback,
                is_external
                    && (playback.state != ConfirmedPlaybackState::ExternalPlayback
                        || playback.pause_reason != PauseReason::None),
            )
        };
        let Some(identity) = external_playback_identity(status).filter(|_| is_external) else {
            self.clear_external_playback_tracker()?;
            // 外部播放结束后没有 active request 可由自然结束分支清理，
            // 因此必须在明确的停止观测处把状态收敛回 Idle；否则待重载会永久
            // 认为仍有外部歌曲，且后续队列续播也无法恢复。暂停/加载/异常观测
            // 不代表歌曲已结束，保留 ExternalPlayback 保护状态。
            if was_external && matches!(status.status.as_str(), "stopped" | "stoped" | "idle") {
                self.playback_state
                    .update(PlaybackStateUpdate::ClearActiveRequest)?;
            }
            return Ok(None);
        };
        let protect_after = Duration::from_secs(
            *self
                .queue_external_protect_seconds
                .read()
                .expect("外部播放保护时间共享锁已中毒"),
        );
        let observation = self.playback_state.observe_external_playback(
            identity.clone(),
            self.clock.now(),
            protect_after,
        )?;
        if should_mark_external {
            self.playback_state.update(PlaybackStateUpdate::External)?;
            // 外部播放身份确认时立即补一次观测；常规进度由监控循环按阈值刷新。
            self.record_observation(status, classify_observation(status))?;
        }
        if observation.protected && !observation.was_protected {
            log::info!(
                "外部播放已稳定 {}s，加入当前歌曲保护: {}",
                *self
                    .queue_external_protect_seconds
                    .read()
                    .expect("外部播放保护时间共享锁已中毒"),
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

/// 恢复起始位置：采用最后可靠观测进度。
///
/// 观测缺失、进度非法（NaN/无穷）或为负数、或距结尾不足 5 秒（歌曲即将结束）
/// 时返回 None，表示从头播放。
fn restored_seek_seconds(observation: Option<&PlaybackObservation>) -> Option<f64> {
    let observation = observation?;
    // 只有可靠观测的进度才能作为续播起点：不可靠观测的进度可能是残留旧值。
    if observation.reliability != ObservationReliability::Reliable {
        return None;
    }
    let progress = observation.progress;
    if !progress.is_finite() || progress < 0.0 {
        return None;
    }
    let duration = observation.duration;
    if duration.is_finite() && duration > 0.0 && duration - progress < 5.0 {
        return None;
    }
    Some(progress)
}

fn restored_seek_seconds_for_track(
    active_track: &PlayableTrack,
    observation: Option<&PlaybackObservation>,
    keep_paused: bool,
) -> Option<f64> {
    let observation = observation?;
    if !observation
        .track
        .as_ref()
        .is_some_and(|observed_track| observed_track.track_ref.key == active_track.track_ref.key)
    {
        return None;
    }
    if keep_paused {
        paused_restored_seek_seconds(Some(observation))
    } else {
        restored_seek_seconds(Some(observation))
    }
}

/// 用户暂停的歌曲不会自然越过末尾保护区。恢复时允许该状态重载，并把
/// 起点最多回退到距结尾 5 秒，给新播放器留出完成起播和重新暂停的时间。
fn paused_restored_seek_seconds(observation: Option<&PlaybackObservation>) -> Option<f64> {
    let observation = observation?;
    if observation.reliability != ObservationReliability::Reliable {
        return None;
    }
    let progress = observation.progress;
    if !progress.is_finite() || progress < 0.0 {
        return None;
    }
    let duration = observation.duration;
    if duration.is_finite() && duration > 0.0 {
        return Some(progress.min((duration - 5.0).max(0.0)));
    }
    Some(progress)
}

fn restoration_observation_conflicts(
    active_track: &PlayableTrack,
    observation: &PlaybackObservation,
) -> bool {
    if observation.reliability == ObservationReliability::Mismatched
        || !matches!(observation.status.as_str(), "playing" | "paused")
    {
        return true;
    }
    observation
        .track
        .as_ref()
        .is_some_and(|observed_track| observed_track.track_ref.key != active_track.track_ref.key)
}

pub(crate) fn has_restorable_playback_progress(
    observation: Option<&PlaybackObservation>,
    keep_paused: bool,
) -> bool {
    if keep_paused {
        paused_restored_seek_seconds(observation).is_some()
    } else {
        restored_seek_seconds(observation).is_some()
    }
}

/// 从歌词切换的回报消息解析当前歌词模式：translation=使用翻译，original=原文。
/// 后端返回其它格式时返回 None（不持久化，保持原值）。
fn lyrics_mode_from_message(message: &str) -> Option<bool> {
    match message {
        "translation" => Some(true),
        "original" => Some(false),
        _ => None,
    }
}

fn playback_request_from_active(active_request: &ActivePlaybackRequest) -> PlaybackRequest {
    PlaybackRequest {
        keyword: active_request.keyword.clone(),
        source: active_request.source.clone(),
        prefer_accompaniment: active_request.prefer_accompaniment,
        track: active_request.track.clone(),
        requester: active_request.requester.clone(),
        navigation: PlaybackNavigation::Restore,
        candidate_snapshot: Vec::new(),
        // 恢复播放不消费队列：不得携带队列项，避免恢复时误出队。
        queue_item_id: None,
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
    monitor_status_ms: u64,
    status_poll_ms: u64,
    active_request: Option<&ActivePlaybackRequest>,
    now: Instant,
) -> bool {
    let Some(active_request) = active_request else {
        return false;
    };
    let Some(started_at) = active_request.guard_started_at else {
        return false;
    };
    let guard_ms = monitor_status_ms
        .max(status_poll_ms)
        .saturating_mul(3)
        .max(3000);
    now.saturating_duration_since(started_at) < Duration::from_millis(guard_ms)
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
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
        /// 原子确认端口收到的 queue_item_id（None 表示手动点歌/恢复播放）。
        confirm_dequeues: Arc<Mutex<Vec<Option<u64>>>>,
    }

    impl PlaybackStatePort for TestPlaybackState {
        fn snapshot(&self) -> Result<PlaybackRuntimeState> {
            Ok(self.runtime.lock().unwrap().state().clone())
        }

        fn update(&self, update: PlaybackStateUpdate) -> Result<bool> {
            let mut runtime = self.runtime.lock().unwrap();
            runtime.update(|playback| update.apply(playback))
        }

        fn record_observation_if_active(
            &self,
            expected: ActivePlaybackIdentity,
            observation: PlaybackObservation,
            immediate: bool,
        ) -> Result<bool> {
            let mut runtime = self.runtime.lock().unwrap();
            if runtime
                .state()
                .active_request
                .as_ref()
                .and_then(ActivePlaybackRequest::identity)
                .as_ref()
                != Some(&expected)
            {
                return Ok(false);
            }
            let update = if immediate {
                PlaybackStateUpdate::ImmediateObservation(observation)
            } else {
                PlaybackStateUpdate::Observation(observation)
            };
            runtime.update(|playback| update.apply(playback))?;
            Ok(true)
        }

        fn song_dedup_limited(&self, candidate: SongDedupCandidate) -> Result<bool> {
            Ok(self
                .history
                .lock()
                .unwrap()
                .is_limited(&self.song_dedup, &candidate))
        }

        fn confirm_playback_and_dequeue(
            &self,
            update: PlaybackStateUpdate,
            queue_item_id: Option<u64>,
        ) -> Result<bool> {
            self.confirm_dequeues.lock().unwrap().push(queue_item_id);
            self.update(update)
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

        fn inspect_player_session(
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
                return Ok(SessionReconciliation::Idle);
            }
            let Some(incoming) = binding else {
                return Ok(SessionReconciliation::Unknown);
            };
            let current = self.session_binding.lock().unwrap();
            Ok(match current.as_ref() {
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
            })
        }

        fn reconcile_player_session(
            &self,
            binding: Option<PlaybackSessionBinding>,
        ) -> Result<SessionReconciliation> {
            let decision = self.inspect_player_session(binding.clone())?;
            if matches!(
                decision,
                SessionReconciliation::Bound
                    | SessionReconciliation::Restarted
                    | SessionReconciliation::Replaced
            ) {
                *self.session_binding.lock().unwrap() = binding;
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
        play_calls: Arc<AtomicU32>,
        play_error: bool,
        pause_error: bool,
        /// 恢复播放收到的起始位置（None 表示从头播放）。
        restored_seeks: Arc<Mutex<Vec<Option<f64>>>>,
        /// 设置音量收到的目标值（字符串原样记录）。
        set_volumes: Arc<Mutex<Vec<String>>>,
        /// 明确设置歌词模式收到的值（true=使用翻译）。
        lyrics_translations: Arc<Mutex<Vec<bool>>>,
        restore_barriers: Option<(Arc<Barrier>, Arc<Barrier>)>,
    }

    impl FakeBackend {
        fn new(statuses: Vec<PlayerStatus>) -> Self {
            Self {
                statuses: Arc::new(Mutex::new(statuses.into())),
                paused: Arc::new(Mutex::new(0)),
                resumed: Arc::new(Mutex::new(0)),
                play_calls: Arc::new(AtomicU32::new(0)),
                play_error: false,
                pause_error: false,
                restored_seeks: Arc::new(Mutex::new(Vec::new())),
                set_volumes: Arc::new(Mutex::new(Vec::new())),
                lyrics_translations: Arc::new(Mutex::new(Vec::new())),
                restore_barriers: None,
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

        fn with_restore_barriers(mut self, entered: Arc<Barrier>, release: Arc<Barrier>) -> Self {
            self.restore_barriers = Some((entered, release));
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

        fn play(&self, _track: &PlayableTrack, _requested: bool) -> Result<String> {
            self.play_calls.fetch_add(1, Ordering::Relaxed);
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

        fn set_volume(&self, volume: &str) -> Result<String> {
            self.set_volumes.lock().unwrap().push(volume.to_string());
            Ok(String::new())
        }

        fn toggle_lyrics(&self) -> Result<String> {
            Ok("translation".to_string())
        }

        fn play_restored(
            &self,
            track: &PlayableTrack,
            seek_seconds: Option<f64>,
        ) -> Result<String> {
            self.restored_seeks.lock().unwrap().push(seek_seconds);
            if let Some((entered, release)) = &self.restore_barriers {
                entered.wait();
                release.wait();
            }
            // 模拟真实后端语义：恢复播放仍是一次播放派发。
            self.play(track, false)
        }

        fn set_lyrics_translation(&self, use_translation: bool) -> Result<String> {
            self.lyrics_translations
                .lock()
                .unwrap()
                .push(use_translation);
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
                confirm_dequeues: Arc::new(Mutex::new(Vec::new())),
            },
            PlaybackTimePorts::new(clock, wall_clock),
            // 测试构造：热更新共享值用默认配置初始化；需要覆盖时直接改写共享值。
            crate::config::LiveConfigs::from_config(&crate::config::AppConfig::default()),
        )
    }

    fn rebuild_controller(
        previous: &PlayerController<FakeBackend, TestPlaybackState>,
        backend: FakeBackend,
    ) -> PlayerController<FakeBackend, TestPlaybackState> {
        let system_time = Arc::new(SystemClock);
        PlayerController::new(
            backend,
            previous.playback_state.clone(),
            PlaybackTimePorts::new(system_time.clone(), system_time),
            crate::config::LiveConfigs::from_config(&crate::config::AppConfig::default()),
        )
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
            queue_item_id: None,
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
            .verify_playback_started(&request, &mut attempt, None)
            .unwrap();

        let PlaybackVerification::Success { status, message } = result;
        assert_eq!(status.volume, 70);
        assert!(message.contains("音量70"), "message: {message}");
        assert!(!message.contains("音量0"), "message: {message}");
    }

    #[test]
    fn confirmation_forwards_queue_item_id_to_the_atomic_dequeue_port() {
        let backend = FakeBackend::new(vec![
            status("目标", "miliastra://track/qqmusic/1", 1.0, 180.0),
            status("目标", "miliastra://track/qqmusic/1", 2.0, 180.0),
        ]);
        let controller = controller(backend);
        // 队列消费来源：确认时必须携带队首 queue_item_id，供原子出队。
        let mut queued = playback_request("目标 - 歌手", "miliastra://track/qqmusic/1");
        queued.queue_item_id = Some(42);
        let mut attempt = controller.play_request(&queued).unwrap();
        controller
            .verify_playback_started(&queued, &mut attempt, None)
            .unwrap();
        assert_eq!(
            *controller.playback_state.confirm_dequeues.lock().unwrap(),
            [Some(42)]
        );

        // 恢复播放/手动点歌：不携带队列项，确认时不触发任何出队。
        let mut restored = playback_request("目标 - 歌手", "miliastra://track/qqmusic/1");
        restored.queue_item_id = None;
        let mut attempt = controller.play_request(&restored).unwrap();
        controller
            .verify_playback_started(&restored, &mut attempt, None)
            .unwrap();
        assert_eq!(
            *controller.playback_state.confirm_dequeues.lock().unwrap(),
            [Some(42), None]
        );
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
            .verify_playback_started(&request, &mut attempt, None)
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
            controller.verify_playback_started(&first, &mut attempt, None),
            Ok(PlaybackVerification::Success { .. })
        ));

        let second = playback_request("歌曲B", uri_b);
        let mut attempt = controller.play_request(&second).unwrap();
        assert!(matches!(
            controller.verify_playback_started(&second, &mut attempt, None),
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
            controller.verify_playback_started(&previous, &mut previous_attempt, None),
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
            .verify_playback_started(&request, &mut attempt, None)
            .unwrap();

        assert!(matches!(result, PlaybackVerification::Success { .. }));
        let snapshot = controller.snapshot();
        assert_eq!(snapshot.state, "requested_song_playing");
        assert_eq!(snapshot.current_uri, request.uri());
        assert_eq!(snapshot.active_uri, request.uri());
    }

    /// 构造已确认播放的活动请求（与 Starting 同一 started_at_ms，保持身份稳定）。
    fn restored_active_request(track: &miliastra_playback::PlayableTrack) -> ActivePlaybackRequest {
        ActivePlaybackRequest {
            keyword: "目标".to_string(),
            source: "qqmusic".to_string(),
            prefer_accompaniment: false,
            track: Some(track.clone()),
            song: "目标歌手".to_string(),
            title: "目标".to_string(),
            artist: "歌手".to_string(),
            requester: String::new(),
            started_at_ms: 1,
            ..ActivePlaybackRequest::default()
        }
    }

    /// 构造「已确认播放 + 可靠进度观测」的持久化状态，模拟上次会话的活动歌曲。
    fn enter_confirmed_playing(controller: &PlayerController<FakeBackend, TestPlaybackState>) {
        let track = test_track("miliastra://track/qqmusic/1", "目标 - 歌手");
        controller
            .playback_state
            .update(PlaybackStateUpdate::Starting {
                request: restored_active_request(&track),
                navigation: PlaybackNavigation::Normal,
            })
            .unwrap();
        controller
            .playback_state
            .update(PlaybackStateUpdate::Confirmed {
                request: restored_active_request(&track),
                navigation: PlaybackNavigation::Normal,
            })
            .unwrap();
        controller
            .playback_state
            .update(PlaybackStateUpdate::Observation(PlaybackObservation {
                status: "playing".to_string(),
                track: Some(track),
                title: "目标".to_string(),
                artist: "歌手".to_string(),
                progress: 42.0,
                duration: 180.0,
                captured_at_ms: 2,
                reliability: ObservationReliability::Reliable,
            }))
            .unwrap();
    }

    #[test]
    fn set_volume_persists_the_applied_volume() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);

        controller.set_volume("70").unwrap();

        // 成功设置音量后必须写入持久化播放状态（SQLite），供重启恢复。
        assert_eq!(controller.playback_state.snapshot().unwrap().volume, 70);
        assert_eq!(*controller.backend.set_volumes.lock().unwrap(), ["70"]);
    }

    #[test]
    fn toggle_lyrics_persists_the_translation_mode() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);

        controller.toggle_lyrics().unwrap();

        // 成功切换歌词后必须写入持久化播放状态（SQLite），供重启恢复。
        assert!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .use_translation
        );
    }

    #[test]
    fn play_restored_resumes_from_last_reliable_progress_with_persisted_volume() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);
        enter_confirmed_playing(&controller);
        controller
            .playback_state
            .update(PlaybackStateUpdate::Volume(55))
            .unwrap();
        let previous_count_before = controller
            .playback_state
            .snapshot()
            .unwrap()
            .previous_requests
            .len();

        assert!(controller.play_restored().unwrap());

        // 恢复前必须应用持久化音量（引擎重启后默认 100，与上次会话不同）。
        assert_eq!(*controller.backend.set_volumes.lock().unwrap(), ["55"]);
        // 恢复播放携带最后可靠进度，从该位置续播而不是整首重播。
        assert_eq!(
            *controller.backend.restored_seeks.lock().unwrap(),
            [Some(42.0)]
        );
        // 恢复后应用持久化歌词模式（默认使用翻译）。
        assert_eq!(
            *controller.backend.lyrics_translations.lock().unwrap(),
            [true]
        );
        let state = controller.playback_state.snapshot().unwrap();
        assert_eq!(state.state, ConfirmedPlaybackState::RequestedSongPlaying);
        assert!(state.active_request.is_some());
        assert_eq!(state.previous_requests.len(), previous_count_before);
        assert_eq!(
            state
                .last_observation
                .as_ref()
                .map(|observation| observation.progress),
            Some(42.0)
        );

        // A replacement that exits after restoration but before listener readiness must leave
        // the same durable seek point for the watchdog's next replacement attempt.
        assert!(controller.play_restored().unwrap());
        assert_eq!(
            *controller.backend.restored_seeks.lock().unwrap(),
            [Some(42.0), Some(42.0)]
        );
        assert_eq!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .previous_requests
                .len(),
            previous_count_before
        );
    }

    #[test]
    fn play_restored_applies_the_persisted_lyrics_mode() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);
        enter_confirmed_playing(&controller);
        // 上次会话关闭了歌词翻译：恢复后必须明确设置为原文，而不是默认的翻译。
        controller
            .playback_state
            .update(PlaybackStateUpdate::LyricsMode(false))
            .unwrap();

        assert!(controller.play_restored().unwrap());

        assert_eq!(
            *controller.backend.lyrics_translations.lock().unwrap(),
            [false]
        );
        assert!(
            !controller
                .playback_state
                .snapshot()
                .unwrap()
                .use_translation
        );
    }

    #[test]
    fn play_restored_keeps_a_user_paused_song_paused() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);
        enter_confirmed_playing(&controller);
        controller
            .playback_state
            .update(PlaybackStateUpdate::UserPaused)
            .unwrap();

        assert!(controller.play_restored().unwrap());

        // 原用户暂停：恢复歌曲后必须立即暂停并保持暂停状态，不得自动播放。
        assert_eq!(*controller.backend.paused.lock().unwrap(), 1);
        let state = controller.playback_state.snapshot().unwrap();
        assert_eq!(state.state, ConfirmedPlaybackState::PausedByUser);
        assert_eq!(state.pause_reason, PauseReason::User);
        assert!(state.active_request.is_some());
    }

    #[test]
    fn play_restored_clamps_a_paused_song_inside_the_end_guard() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);
        enter_confirmed_playing(&controller);
        controller
            .playback_state
            .update(PlaybackStateUpdate::UserPaused)
            .unwrap();
        let active_track = controller
            .playback_state
            .snapshot()
            .unwrap()
            .active_request
            .and_then(|request| request.track)
            .expect("active track");
        controller
            .playback_state
            .update(PlaybackStateUpdate::ImmediateObservation(
                PlaybackObservation {
                    status: "paused".to_string(),
                    track: Some(active_track),
                    progress: 178.0,
                    duration: 180.0,
                    reliability: ObservationReliability::Reliable,
                    ..Default::default()
                },
            ))
            .unwrap();

        assert!(controller.play_restored().unwrap());
        assert_eq!(
            *controller.backend.restored_seeks.lock().unwrap(),
            [Some(175.0)]
        );
        assert_eq!(
            controller.playback_state.snapshot().unwrap().state,
            ConfirmedPlaybackState::PausedByUser
        );
    }

    #[test]
    fn play_restored_without_an_active_request_returns_false() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);

        assert!(!controller.play_restored().unwrap());
        assert!(controller.backend.restored_seeks.lock().unwrap().is_empty());
    }

    #[test]
    fn play_restored_discards_an_active_request_when_the_observed_track_changed() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);
        enter_confirmed_playing(&controller);
        controller
            .playback_state
            .update(PlaybackStateUpdate::Observation(PlaybackObservation {
                status: "playing".to_string(),
                track: Some(test_track(
                    "miliastra://track/qqmusic/2",
                    "后台已切换的歌曲 - 歌手",
                )),
                progress: 30.0,
                duration: 180.0,
                reliability: ObservationReliability::Reliable,
                ..Default::default()
            }))
            .unwrap();

        assert!(!controller.play_restored().unwrap());
        assert!(controller.backend.restored_seeks.lock().unwrap().is_empty());
        let state = controller.playback_state.snapshot().unwrap();
        assert_eq!(state.state, ConfirmedPlaybackState::Idle);
        assert!(state.active_request.is_none());
    }

    #[test]
    fn play_restored_discards_an_active_request_after_a_stopped_observation() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);
        enter_confirmed_playing(&controller);
        let active_track = controller
            .playback_state
            .snapshot()
            .unwrap()
            .active_request
            .and_then(|request| request.track)
            .expect("active track");
        controller
            .playback_state
            .update(PlaybackStateUpdate::Observation(PlaybackObservation {
                status: "stopped".to_string(),
                track: Some(active_track),
                reliability: ObservationReliability::Stale,
                ..Default::default()
            }))
            .unwrap();

        assert!(!controller.play_restored().unwrap());
        assert!(controller.backend.restored_seeks.lock().unwrap().is_empty());
        let state = controller.playback_state.snapshot().unwrap();
        assert_eq!(state.state, ConfirmedPlaybackState::Idle);
        assert!(state.active_request.is_none());
    }

    #[test]
    fn play_restored_skips_unconfirmed_starting_state() {
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend);
        let track = test_track("miliastra://track/qqmusic/1", "目标 - 歌手");
        controller
            .playback_state
            .update(PlaybackStateUpdate::Starting {
                request: restored_active_request(&track),
                navigation: PlaybackNavigation::Normal,
            })
            .unwrap();

        // 上次会话只停留在 Starting（确认前退出）：不自动恢复，避免重播未确认的歌曲。
        assert!(!controller.play_restored().unwrap());
        assert!(controller.backend.restored_seeks.lock().unwrap().is_empty());
    }

    #[test]
    fn restored_seek_requires_reliable_finite_progress_away_from_the_end() {
        let observation = |progress: f64,
                           duration: f64,
                           reliability: ObservationReliability|
         -> PlaybackObservation {
            PlaybackObservation {
                status: "playing".to_string(),
                track: Some(test_track("miliastra://track/qqmusic/obs", "观测 - 歌手")),
                title: "观测".to_string(),
                artist: "歌手".to_string(),
                progress,
                duration,
                captured_at_ms: 1,
                reliability,
            }
        };
        let reliable = ObservationReliability::Reliable;
        assert_eq!(restored_seek_seconds(None), None);
        // 非法（NaN/无穷）与负数进度：从头播放。
        assert_eq!(
            restored_seek_seconds(Some(&observation(f64::NAN, 180.0, reliable))),
            None
        );
        assert_eq!(
            restored_seek_seconds(Some(&observation(-1.0, 180.0, reliable))),
            None
        );
        // 距结尾不足 5 秒：从头播放。
        assert_eq!(
            restored_seek_seconds(Some(&observation(178.0, 180.0, reliable))),
            None
        );
        // 恰 5 秒仍可续播。
        assert_eq!(
            restored_seek_seconds(Some(&observation(175.0, 180.0, reliable))),
            Some(175.0)
        );
        // 正常进度：从该位置续播。
        assert_eq!(
            restored_seek_seconds(Some(&observation(42.0, 180.0, reliable))),
            Some(42.0)
        );
        // 不可靠观测的进度不得作为续播起点。
        assert_eq!(
            restored_seek_seconds(Some(&observation(
                42.0,
                180.0,
                ObservationReliability::Stale
            ))),
            None
        );
        // duration 非法（NaN）时仍采用有限进度。
        assert_eq!(
            restored_seek_seconds(Some(&observation(42.0, f64::NAN, reliable))),
            Some(42.0)
        );

        let active_track = test_track("miliastra://track/qqmusic/active", "活动歌曲 - 歌手");
        let matching = PlaybackObservation {
            track: Some(active_track.clone()),
            ..observation(42.0, 180.0, reliable)
        };
        assert_eq!(
            restored_seek_seconds_for_track(&active_track, Some(&matching), false),
            Some(42.0)
        );
        let matching_near_end = PlaybackObservation {
            track: Some(active_track.clone()),
            ..observation(178.0, 180.0, reliable)
        };
        assert_eq!(
            restored_seek_seconds_for_track(&active_track, Some(&matching_near_end), false),
            None
        );
        assert_eq!(
            restored_seek_seconds_for_track(&active_track, Some(&matching_near_end), true),
            Some(175.0)
        );
        assert_eq!(
            restored_seek_seconds_for_track(
                &active_track,
                Some(&observation(42.0, 180.0, reliable)),
                false,
            ),
            None
        );
    }

    #[test]
    fn lyrics_mode_from_message_parses_translation_and_original() {
        assert_eq!(lyrics_mode_from_message("translation"), Some(true));
        assert_eq!(lyrics_mode_from_message("original"), Some(false));
        assert_eq!(lyrics_mode_from_message("ok"), None);
        assert_eq!(lyrics_mode_from_message(""), None);
    }

    #[test]
    fn confirmed_playback_keeps_the_starting_started_at_ms() {
        let clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 10));
        let controller =
            controller_with_time(FakeBackend::new(Vec::new()), clock.clone(), clock.clone());
        let request = request();
        let mut attempt = controller.play_request(&request).unwrap();

        // Starting 阶段记录发起时刻（unix_millis = 10 * 1000）。
        let starting = controller
            .playback_state
            .snapshot()
            .unwrap()
            .active_request
            .expect("Starting 状态应有 active_request");
        assert_eq!(starting.started_at_ms, 10_000);

        // 起播到确认之间墙钟前进（流解析/起播可能耗时数秒）：确认不得改写发起时刻。
        clock.advance(Duration::from_secs(5)).unwrap();
        controller
            .verify_playback_started(&request, &mut attempt, None)
            .unwrap();

        let confirmed = controller
            .playback_state
            .snapshot()
            .unwrap()
            .active_request
            .expect("确认后应有 active_request");
        // 沿用 Starting 的发起时刻，保证 active_request_identity（TrackKey + started_at_ms）
        // 在 Starting -> Confirmed 间不变，避免持久层误判请求身份变更而清空 session binding。
        assert_eq!(confirmed.started_at_ms, starting.started_at_ms);
        assert_eq!(
            (
                confirmed
                    .track
                    .as_ref()
                    .map(|track| track.track_ref.key.clone()),
                confirmed.started_at_ms,
            ),
            (
                starting
                    .track
                    .as_ref()
                    .map(|track| track.track_ref.key.clone()),
                starting.started_at_ms,
            )
        );
        assert_eq!(controller.snapshot().state, "requested_song_playing");
    }

    #[test]
    fn playing_observation_with_stale_track_keeps_active_request() {
        let stale_uri = "miliastra://track/netease/previous";
        let manual_clock = Arc::new(ManualClock::new(Instant::now()));
        let controller = controller_with_time(
            FakeBackend::new(Vec::new()),
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
                status("上一首", stale_uri, 12.0, 180.0),
                QueueAdvanceContext {
                    queue_empty: false,
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
        let manual_clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 10));
        let controller = controller_with_time(
            FakeBackend::new(Vec::new()),
            manual_clock.clone(),
            manual_clock.clone(),
        );
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: false,
            has_pending_playback_task: false,
            command_executing: false,
        };

        // 不可重试的播放中失败：先给 core 流重试留窗口，首 tick 只记录不推进。
        assert_eq!(
            controller
                .maybe_advance_queue(failure.clone(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(controller.snapshot().active_uri, request.uri());

        // 窗口内仍失败：继续等待。
        manual_clock.advance(Duration::from_secs(5)).unwrap();
        assert_eq!(
            controller
                .maybe_advance_queue(failure.clone(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );

        // 超过窗口仍失败：丢弃当前请求并推进到队列下一首。
        manual_clock.advance(Duration::from_secs(5)).unwrap();
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
    fn engine_failure_recovery_resets_retry_window() {
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
        let manual_clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 10));
        let controller = controller_with_time(
            FakeBackend::new(Vec::new()),
            manual_clock.clone(),
            manual_clock.clone(),
        );
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: false,
            has_pending_playback_task: false,
            command_executing: false,
        };

        // 首次失败：记录窗口起点。
        assert_eq!(
            controller
                .maybe_advance_queue(failure.clone(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        manual_clock.advance(Duration::from_secs(3)).unwrap();

        // 流重试成功：失败信号消失，窗口重置。
        let recovered = status("目标", &request.uri(), 30.0, 180.0);
        assert_eq!(
            controller
                .maybe_advance_queue(recovered, context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );

        // 同一首歌再次失败：重新开始完整的重试窗口，而不是按旧时间戳立即推进。
        manual_clock.advance(Duration::from_secs(5)).unwrap();
        assert_eq!(
            controller
                .maybe_advance_queue(failure.clone(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        manual_clock.advance(Duration::from_secs(9)).unwrap();
        assert_eq!(
            controller.maybe_advance_queue(failure, context).unwrap(),
            QueueAdvanceDecision::AdvanceQueue {
                reason: "播放失败"
            }
        );
    }

    #[test]
    fn engine_failure_window_is_shared_across_clones() {
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
        let manual_clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 10));
        let controller = controller_with_time(
            FakeBackend::new(Vec::new()),
            manual_clock.clone(),
            manual_clock.clone(),
        );
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        // 副本 B 与副本 A 共享同一失败窗口计时器（worker/任务各持一份副本）。
        let clone = controller.clone();
        let context = QueueAdvanceContext {
            queue_empty: false,
            has_pending_playback_task: false,
            command_executing: false,
        };

        // 副本 A 首次观察失败：记录窗口起点。
        assert_eq!(
            controller
                .maybe_advance_queue(failure.clone(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        // 窗口内副本 B 观察同一失败：共享起点，仍在等待流重试。
        manual_clock.advance(Duration::from_secs(5)).unwrap();
        assert_eq!(
            clone
                .maybe_advance_queue(failure.clone(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        // 超过窗口后副本 B 观察：若计时器未共享，副本 B 会把同一失败当作新的首次失败
        // 重新开始窗口并返回 None，失败曲目将永不推进。
        manual_clock.advance(Duration::from_secs(4)).unwrap();
        assert_eq!(
            clone.maybe_advance_queue(failure, context).unwrap(),
            QueueAdvanceDecision::AdvanceQueue {
                reason: "播放失败"
            }
        );
        assert!(clone.snapshot().active_uri.is_empty());
    }

    #[test]
    fn stale_failure_from_previous_session_does_not_advance() {
        let request = request();
        // 确认播放时绑定当前会话（session-a / generation 41）。
        let mut confirmed = status("目标", &request.uri(), 1.0, 180.0);
        confirmed.session_id = "session-a".to_string();
        confirmed.generation = 41;
        let manual_clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 10));
        let controller = controller_with_time(
            FakeBackend::new(Vec::new()),
            manual_clock.clone(),
            manual_clock.clone(),
        );
        controller
            .confirm_playback_success(&request, &confirmed)
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: false,
            has_pending_playback_task: false,
            command_executing: false,
        };

        // 推进队列后引擎状态更新有延迟：旧会话（session-b）的失败残留不得触发推进。
        let stale_failure = PlayerStatus {
            status: "stopped".to_string(),
            current_track: request.track.clone(),
            current_uri: request.uri(),
            runtime_identity: "runtime-a".to_string(),
            session_id: "session-b".to_string(),
            generation: 42,
            end_behavior: "notify_controller".to_string(),
            last_end_cause: "decode_failure".to_string(),
            failure_code: "decode_failure".to_string(),
            failure_message: "上一首的失败残留".to_string(),
            failure_retryable: false,
            ..PlayerStatus::default()
        };
        manual_clock.advance(Duration::from_secs(9)).unwrap();
        assert_eq!(
            controller
                .maybe_advance_queue(stale_failure.clone(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        // 残留状态不记录重试窗口起点：再多 tick 也不会推进。
        manual_clock.advance(Duration::from_secs(9)).unwrap();
        assert_eq!(
            controller
                .maybe_advance_queue(stale_failure, context)
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(controller.snapshot().active_uri, request.uri());
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
        let manual_clock = Arc::new(ManualClock::with_unix_seconds(Instant::now(), 10));
        let controller = controller_with_time(
            FakeBackend::new(Vec::new()),
            manual_clock.clone(),
            manual_clock.clone(),
        );
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        // 首次失败：记录窗口起点，等待流重试。
        assert_eq!(
            controller
                .maybe_advance_queue(failure.clone(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        // 队列空且无播放池：窗口超时后清空请求但不推进，与自然结束行为一致。
        manual_clock.advance(Duration::from_secs(9)).unwrap();
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
    fn monitor_status_records_latest_progress_for_restart_recovery() {
        let first = status("目标", "miliastra://track/qqmusic/1", 10.0, 180.0);
        let second = status("目标", "miliastra://track/qqmusic/1", 16.0, 180.0);
        let controller = controller(FakeBackend::new(vec![first, second]));
        assert!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .is_none()
        );

        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };
        let first = controller.monitor_status().unwrap();
        controller
            .maybe_advance_queue(first, context.clone())
            .unwrap();
        assert_eq!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .as_ref()
                .map(|observation| observation.progress),
            Some(10.0)
        );

        // 进度推进超过 5 秒阈值后必须刷新持久化观测，供下次启动 seek。
        let second = controller.monitor_status().unwrap();
        controller.maybe_advance_queue(second, context).unwrap();
        assert_eq!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .as_ref()
                .map(|observation| observation.progress),
            Some(16.0)
        );
    }

    #[test]
    fn reload_status_forces_the_latest_progress_past_normal_persistence_throttling() {
        let first = status("目标", "miliastra://track/qqmusic/1", 10.0, 180.0);
        let second = status("目标", "miliastra://track/qqmusic/1", 12.0, 180.0);
        let controller = controller(FakeBackend::new(vec![first, second]));

        let first = controller.monitor_status().unwrap();
        controller
            .maybe_advance_queue(
                first,
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                },
            )
            .unwrap();
        let second = controller.status_for_reload().unwrap();
        assert_eq!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .as_ref()
                .map(|observation| observation.progress),
            Some(10.0),
            "status reads must not persist before runtime reconciliation"
        );
        controller
            .maybe_advance_queue_for_reload(
                second,
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                },
            )
            .unwrap();

        assert_eq!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .as_ref()
                .map(|observation| observation.progress),
            Some(12.0)
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
            status: "stoped".to_string(),
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
        let backend = FakeBackend::new(vec![recovered.clone(), recovered]);
        let controller = controller(backend.clone());
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
        // 无并发任务时恢复确实发起 play（与抑制测试形成对照）。
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn monitored_runtime_restart_preserves_resume_progress_until_recovery() {
        let request = request();
        let old_runtime = PlayerStatus {
            status: "playing".to_string(),
            current_track: request.track.clone(),
            current_uri: request.uri(),
            runtime_identity: "runtime-old".to_string(),
            session_id: "session-old".to_string(),
            generation: 7,
            end_behavior: "notify_controller".to_string(),
            progress: 42.0,
            duration: 180.0,
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
            progress: 42.0,
            duration: 180.0,
            ..PlayerStatus::default()
        };
        let backend = FakeBackend::new(vec![old_runtime, restarted, recovered]);
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        let old_status = controller.monitor_status().unwrap();
        assert_eq!(
            controller
                .maybe_advance_queue(old_status, context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        let restarted_status = controller.monitor_status().unwrap();
        assert_eq!(
            controller
                .maybe_advance_queue(restarted_status, context)
                .unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );

        assert_eq!(*backend.restored_seeks.lock().unwrap(), [Some(42.0)]);
        assert_eq!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .as_ref()
                .map(|observation| observation.progress),
            Some(42.0)
        );
    }

    /// 构造「运行时已重启、可恢复」的固定场景：确认播放并绑定旧运行时，
    /// 随后观察新 runtime identity 的空闲状态，返回触发恢复所需的 status。
    /// 各抑制测试复用同一场景，只改变 QueueAdvanceContext。
    fn restarted_recovery_scenario(
        controller: &PlayerController<FakeBackend, TestPlaybackState>,
        request: &PlaybackRequest,
    ) -> PlayerStatus {
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
        // 先观察旧运行时建立绑定，再观察新 identity：reconcile 判为 Restarted。
        assert_eq!(
            controller
                .maybe_advance_queue(
                    old_runtime,
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        PlayerStatus {
            status: "stopped".to_string(),
            runtime_identity: "runtime-new".to_string(),
            generation: 0,
            ..PlayerStatus::default()
        }
    }

    #[test]
    fn restarted_unknown_transport_stays_recoverable_after_busy_command_finishes() {
        let request = request();
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let old_runtime = PlayerStatus {
            status: "playing".to_string(),
            current_track: request.track.clone(),
            current_uri: request.uri(),
            runtime_identity: "runtime-old".to_string(),
            session_id: "session-old".to_string(),
            generation: 7,
            progress: 42.0,
            duration: 180.0,
            ..PlayerStatus::default()
        };
        assert_eq!(
            controller
                .maybe_advance_queue(
                    old_runtime,
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );

        let unknown = PlayerStatus {
            status: "unknown".to_string(),
            runtime_identity: "runtime-new".to_string(),
            ..PlayerStatus::default()
        };
        let busy = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: true,
        };
        assert_eq!(
            controller
                .maybe_advance_queue(unknown.clone(), busy.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(
            controller.runtime_recovery.lock().unwrap().clone(),
            Some(RuntimeRecoveryState::Pending("runtime-new".to_string()))
        );
        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "unknown".to_string(),
                        ..PlayerStatus::default()
                    },
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(
            controller.runtime_recovery.lock().unwrap().clone(),
            Some(RuntimeRecoveryState::Pending("runtime-new".to_string()))
        );
        // Repeated unstable samples while the command remains busy must not consume the restart.
        assert_eq!(
            controller.maybe_advance_queue(unknown, busy).unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 0);

        let stopped = PlayerStatus {
            status: "stopped".to_string(),
            runtime_identity: "runtime-new".to_string(),
            ..PlayerStatus::default()
        };
        assert_eq!(
            controller
                .maybe_advance_queue(
                    stopped,
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(*backend.restored_seeks.lock().unwrap(), [Some(42.0)]);
        assert_eq!(
            controller.runtime_recovery.lock().unwrap().clone(),
            Some(RuntimeRecoveryState::Recovering("runtime-new".to_string()))
        );
    }

    #[test]
    fn restarted_unknown_transport_stays_recoverable_without_a_busy_command() {
        let request = request();
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let _ = restarted_recovery_scenario(&controller, &request);

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "unknown".to_string(),
                        runtime_identity: "runtime-new".to_string(),
                        ..PlayerStatus::default()
                    },
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 0);

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "stopped".to_string(),
                        runtime_identity: "runtime-new".to_string(),
                        ..PlayerStatus::default()
                    },
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn deferred_restart_survives_controller_rebuild_before_transport_stabilizes() {
        let request = request();
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let _ = restarted_recovery_scenario(&controller, &request);
        let busy = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: true,
        };
        controller
            .maybe_advance_queue(
                PlayerStatus {
                    status: "unknown".to_string(),
                    runtime_identity: "runtime-new".to_string(),
                    ..PlayerStatus::default()
                },
                busy,
            )
            .unwrap();
        assert_eq!(
            controller
                .playback_state
                .session_binding
                .lock()
                .unwrap()
                .as_ref()
                .map(|binding| binding.runtime_identity.as_str()),
            Some("runtime-old"),
            "an unconfirmed replacement runtime must not be persisted"
        );

        let rebuilt = rebuild_controller(&controller, backend.clone());
        assert!(rebuilt.runtime_recovery.lock().unwrap().is_none());
        assert_eq!(
            rebuilt
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "stopped".to_string(),
                        runtime_identity: "runtime-new".to_string(),
                        ..PlayerStatus::default()
                    },
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn stopped_or_unknown_session_does_not_consume_deferred_restart() {
        let request = request();
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let _ = restarted_recovery_scenario(&controller, &request);
        let idle_context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        for transport in ["unknown", "stopped"] {
            assert_eq!(
                controller
                    .maybe_advance_queue(
                        PlayerStatus {
                            status: transport.to_string(),
                            runtime_identity: "runtime-new".to_string(),
                            session_id: "unconfirmed-session".to_string(),
                            ..PlayerStatus::default()
                        },
                        idle_context.clone(),
                    )
                    .unwrap(),
                QueueAdvanceDecision::None
            );
            assert_eq!(
                controller.runtime_recovery.lock().unwrap().clone(),
                Some(RuntimeRecoveryState::Pending("runtime-new".to_string()))
            );
        }
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 0);

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "stopped".to_string(),
                        runtime_identity: "runtime-new".to_string(),
                        ..PlayerStatus::default()
                    },
                    idle_context,
                )
                .unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn live_replacement_session_cancels_deferred_recovery_and_accepts_binding() {
        let request = request();
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let _ = restarted_recovery_scenario(&controller, &request);
        controller
            .maybe_advance_queue(
                PlayerStatus {
                    status: "unknown".to_string(),
                    runtime_identity: "runtime-new".to_string(),
                    ..PlayerStatus::default()
                },
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: true,
                },
            )
            .unwrap();

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "playing".to_string(),
                        current_track: request.track.clone(),
                        current_uri: request.uri(),
                        runtime_identity: "runtime-new".to_string(),
                        session_id: "session-new".to_string(),
                        generation: 1,
                        progress: 3.0,
                        duration: 180.0,
                        ..PlayerStatus::default()
                    },
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert!(controller.runtime_recovery.lock().unwrap().is_none());
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            controller
                .playback_state
                .session_binding
                .lock()
                .unwrap()
                .as_ref()
                .map(|binding| binding.runtime_identity.as_str()),
            Some("runtime-new")
        );
    }

    #[test]
    fn replacement_runtime_for_another_track_stays_pending_and_recovers() {
        let request = request();
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 42.0, 180.0))
            .unwrap();
        let _ = restarted_recovery_scenario(&controller, &request);
        let previous_observation = controller
            .playback_state
            .snapshot()
            .unwrap()
            .last_observation
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "playing".to_string(),
                        current_track: Some(test_track(
                            "miliastra://track/qqmusic/other",
                            "另一首歌 - 歌手",
                        )),
                        runtime_identity: "runtime-new".to_string(),
                        session_id: "session-new".to_string(),
                        generation: 1,
                        progress: 3.0,
                        duration: 180.0,
                        ..PlayerStatus::default()
                    },
                    context.clone(),
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(
            controller.runtime_recovery.lock().unwrap().clone(),
            Some(RuntimeRecoveryState::Pending("runtime-new".to_string()))
        );
        assert_eq!(
            controller
                .playback_state
                .session_binding
                .lock()
                .unwrap()
                .as_ref()
                .map(|binding| binding.runtime_identity.as_str()),
            Some("runtime-old")
        );
        let persisted = controller
            .playback_state
            .snapshot()
            .unwrap()
            .last_observation
            .unwrap();
        assert_eq!(persisted.track, previous_observation.track);
        assert_eq!(persisted.progress, previous_observation.progress);
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 0);

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "stopped".to_string(),
                        runtime_identity: "runtime-new".to_string(),
                        ..PlayerStatus::default()
                    },
                    context,
                )
                .unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn replacement_runtime_without_session_stays_pending_and_recovers() {
        let request = request();
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 42.0, 180.0))
            .unwrap();
        let _ = restarted_recovery_scenario(&controller, &request);
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "playing".to_string(),
                        current_track: request.track.clone(),
                        current_uri: request.uri(),
                        runtime_identity: "runtime-new".to_string(),
                        progress: 3.0,
                        duration: 180.0,
                        ..PlayerStatus::default()
                    },
                    context.clone(),
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(
            controller.runtime_recovery.lock().unwrap().clone(),
            Some(RuntimeRecoveryState::Pending("runtime-new".to_string()))
        );
        assert_eq!(
            controller
                .playback_state
                .session_binding
                .lock()
                .unwrap()
                .as_ref()
                .map(|binding| binding.runtime_identity.as_str()),
            Some("runtime-old")
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 0);

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "stopped".to_string(),
                        runtime_identity: "runtime-new".to_string(),
                        ..PlayerStatus::default()
                    },
                    context,
                )
                .unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn unverified_bound_and_replaced_sessions_do_not_take_over_or_persist() {
        let request = request();
        let controller = controller(FakeBackend::new(Vec::new()));
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };
        let other_track = test_track("miliastra://track/qqmusic/other", "另一首歌 - 歌手");

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "playing".to_string(),
                        current_track: Some(other_track.clone()),
                        runtime_identity: "runtime-live".to_string(),
                        session_id: "session-unverified".to_string(),
                        generation: 1,
                        ..PlayerStatus::default()
                    },
                    context.clone(),
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert!(
            controller
                .playback_state
                .session_binding
                .lock()
                .unwrap()
                .is_none()
        );
        assert!(
            controller
                .playback_state
                .snapshot()
                .unwrap()
                .last_observation
                .is_none()
        );

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "playing".to_string(),
                        current_track: request.track.clone(),
                        current_uri: request.uri(),
                        runtime_identity: "runtime-live".to_string(),
                        session_id: "session-old".to_string(),
                        generation: 1,
                        progress: 8.0,
                        duration: 180.0,
                        ..PlayerStatus::default()
                    },
                    context.clone(),
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        let previous_observation = controller
            .playback_state
            .snapshot()
            .unwrap()
            .last_observation
            .unwrap();

        assert_eq!(
            controller
                .maybe_advance_queue(
                    PlayerStatus {
                        status: "playing".to_string(),
                        current_track: Some(other_track),
                        runtime_identity: "runtime-live".to_string(),
                        session_id: "session-new".to_string(),
                        generation: 2,
                        progress: 90.0,
                        duration: 180.0,
                        ..PlayerStatus::default()
                    },
                    context,
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(
            controller
                .playback_state
                .session_binding
                .lock()
                .unwrap()
                .as_ref()
                .map(|binding| binding.session_id.as_str()),
            Some("session-old")
        );
        let persisted = controller
            .playback_state
            .snapshot()
            .unwrap()
            .last_observation
            .unwrap();
        assert_eq!(persisted.track, previous_observation.track);
        assert_eq!(persisted.progress, previous_observation.progress);
    }

    #[test]
    fn concurrent_controller_clone_cannot_dispatch_duplicate_restart_recovery() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let backend =
            FakeBackend::new(Vec::new()).with_restore_barriers(entered.clone(), release.clone());
        let request = request();
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let stopped = restarted_recovery_scenario(&controller, &request);
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };
        let first_controller = controller.clone();
        let first_status = stopped.clone();
        let first_context = context.clone();
        let first = std::thread::spawn(move || {
            first_controller.maybe_advance_queue(first_status, first_context)
        });
        entered.wait();
        let (second_entered_tx, second_entered_rx) = mpsc::channel();
        let (second_result_tx, second_result_rx) = mpsc::channel();
        let second_controller = controller.clone();
        let second = std::thread::spawn(move || {
            second_entered_tx.send(()).unwrap();
            second_result_tx
                .send(second_controller.maybe_advance_queue(stopped, context))
                .unwrap();
        });
        second_entered_rx.recv().unwrap();
        assert!(matches!(
            second_result_rx.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        ));
        release.wait();
        assert_eq!(
            first.join().unwrap().unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(
            second_result_rx.recv().unwrap().unwrap(),
            QueueAdvanceDecision::None
        );
        second.join().unwrap();
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 1);
        assert_eq!(backend.restored_seeks.lock().unwrap().len(), 1);
    }

    #[test]
    fn formal_play_waits_for_restart_recovery_lease_and_wins_after_it() {
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let backend =
            FakeBackend::new(Vec::new()).with_restore_barriers(entered.clone(), release.clone());
        let old_request = request();
        let new_request = playback_request("新目标 - 歌手", "miliastra://track/qqmusic/new-target");
        let new_uri = new_request.uri();
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(
                &old_request,
                &status("目标", &old_request.uri(), 1.0, 180.0),
            )
            .unwrap();
        let stopped = restarted_recovery_scenario(&controller, &old_request);
        let recovery_controller = controller.clone();
        let recovery = std::thread::spawn(move || {
            recovery_controller.maybe_advance_queue(
                stopped,
                QueueAdvanceContext {
                    queue_empty: true,
                    has_pending_playback_task: false,
                    command_executing: false,
                },
            )
        });
        entered.wait();

        let (play_entered_tx, play_entered_rx) = mpsc::channel();
        let (play_result_tx, play_result_rx) = mpsc::channel();
        let play_controller = controller.clone();
        let formal_play = std::thread::spawn(move || {
            play_entered_tx.send(()).unwrap();
            play_result_tx
                .send(play_controller.play_and_verify(&new_request))
                .unwrap();
        });
        play_entered_rx.recv().unwrap();
        assert!(matches!(
            play_result_rx.recv_timeout(Duration::from_millis(100)),
            Err(RecvTimeoutError::Timeout)
        ));

        release.wait();
        assert_eq!(
            recovery.join().unwrap().unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert!(matches!(
            play_result_rx.recv().unwrap(),
            Ok(PlaybackVerification::Success { .. })
        ));
        formal_play.join().unwrap();
        assert_eq!(controller.snapshot().active_uri, new_uri);
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn restarted_recovery_is_suppressed_while_command_is_executing() {
        let request = request();
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let restarted = restarted_recovery_scenario(&controller, &request);

        // 用户命令执行中：monitor 不得并发 play 恢复，等命令完成后由下一次决策接管。
        assert_eq!(
            controller
                .maybe_advance_queue(
                    restarted.clone(),
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: true,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 0);
        assert_eq!(
            controller
                .maybe_advance_queue(
                    restarted,
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn restarted_recovery_is_suppressed_while_playback_task_is_pending() {
        let request = request();
        let backend = FakeBackend::new(Vec::new());
        let controller = controller(backend.clone());
        controller
            .confirm_playback_success(&request, &status("目标", &request.uri(), 1.0, 180.0))
            .unwrap();
        let restarted = restarted_recovery_scenario(&controller, &request);

        // 正式播放任务未完成：monitor 不得并发 play 恢复，避免两个任务同时起播。
        assert_eq!(
            controller
                .maybe_advance_queue(
                    restarted.clone(),
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: true,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::None
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 0);
        // 恢复请求未被消费：active_request 保留，任务完成后的下一次决策仍可恢复。
        assert_eq!(controller.snapshot().active_uri, request.uri());
        assert_eq!(
            controller
                .maybe_advance_queue(
                    restarted,
                    QueueAdvanceContext {
                        queue_empty: true,
                        has_pending_playback_task: false,
                        command_executing: false,
                    },
                )
                .unwrap(),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(backend.play_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn external_playback_without_uri_has_no_identity() {
        assert_eq!(
            external_playback_identity(&status("外部歌", "", 1.0, 180.0)),
            None
        );
    }

    #[test]
    fn playing_transport_without_track_identity_protects_the_current_song() {
        let controller = controller(FakeBackend::new(vec![]));
        let request = request();
        controller
            .confirm_playback_success(
                &request,
                &status("目标", request.uri().as_str(), 1.0, 180.0),
            )
            .unwrap();
        let playing_without_identity = PlayerStatus {
            status: "playing".to_string(),
            ..PlayerStatus::default()
        };

        assert!(
            controller
                .should_queue_until_current_song_finished(&playing_without_identity)
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
    fn idle_stopped_status_with_pool_advances_once_per_cooldown() {
        // 非零墙钟基准：冷却判断用 unix 毫秒，初始 0 会误判为冷却中。
        let clock = Arc::new(ManualClock::with_unix_seconds(
            Instant::now(),
            1_700_000_000,
        ));
        let controller =
            controller_with_pool(FakeBackend::new(vec![]), clock.clone(), clock.clone(), true);
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        // 启动后引擎空闲 + 播放池有歌：触发空闲续播。
        assert_eq!(
            controller
                .maybe_advance_queue(stopped_status(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::AdvanceQueue {
                reason: "空闲续播"
            }
        );
        // 冷却窗口内不重复触发（播放失败回到 Idle 时防热循环）。
        assert_eq!(
            controller
                .maybe_advance_queue(stopped_status(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        // 冷却过后允许重试。
        clock
            .advance(Duration::from_millis(IDLE_ADVANCE_COOLDOWN_MS + 1))
            .unwrap();
        assert_eq!(
            controller
                .maybe_advance_queue(stopped_status(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::AdvanceQueue {
                reason: "空闲续播"
            }
        );
    }

    #[test]
    fn idle_advance_cooldown_is_shared_across_clones() {
        // 非零墙钟基准：冷却判断用 unix 毫秒，初始 0 会误判为冷却中。
        let clock = Arc::new(ManualClock::with_unix_seconds(
            Instant::now(),
            1_700_000_000,
        ));
        let controller =
            controller_with_pool(FakeBackend::new(vec![]), clock.clone(), clock.clone(), true);
        // 副本 B 与副本 A 共享同一冷却计时器。
        let clone = controller.clone();
        let context = QueueAdvanceContext {
            queue_empty: true,
            has_pending_playback_task: false,
            command_executing: false,
        };

        // 副本 A 触发空闲续播并记录冷却时间戳。
        assert_eq!(
            controller
                .maybe_advance_queue(stopped_status(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::AdvanceQueue {
                reason: "空闲续播"
            }
        );
        // 副本 B 立即观察同一空闲状态：冷却共享，不得无视冷却再次触发（否则播放失败回到
        // Idle 时各副本轮流触发，形成热循环）。
        assert_eq!(
            clone
                .maybe_advance_queue(stopped_status(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );
        // 冷却过后副本 B 允许再次触发。
        clock
            .advance(Duration::from_millis(IDLE_ADVANCE_COOLDOWN_MS + 1))
            .unwrap();
        assert_eq!(
            clone
                .maybe_advance_queue(stopped_status(), context)
                .unwrap(),
            QueueAdvanceDecision::AdvanceQueue {
                reason: "空闲续播"
            }
        );
    }

    #[test]
    fn idle_stopped_status_advances_with_queue_items_even_without_pool() {
        let controller = controller(FakeBackend::new(vec![]));
        let decision = controller
            .maybe_advance_queue(
                stopped_status(),
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
                reason: "空闲续播"
            }
        );
    }

    #[test]
    fn idle_stopped_status_does_not_advance_when_user_paused_or_engine_failed() {
        let paused_controller = controller(FakeBackend::new(vec![]));
        let mut paused = paused_controller.playback_state.snapshot().unwrap();
        paused.pause_reason = PauseReason::User;
        paused_controller
            .playback_state
            .update(PlaybackStateUpdate::Restore(Box::new(paused)))
            .unwrap();
        let context = QueueAdvanceContext {
            queue_empty: false,
            has_pending_playback_task: false,
            command_executing: false,
        };

        assert_eq!(
            paused_controller
                .maybe_advance_queue(stopped_status(), context.clone())
                .unwrap(),
            QueueAdvanceDecision::None
        );

        let failed_controller = controller(FakeBackend::new(vec![]));
        let decision = failed_controller
            .maybe_advance_queue(
                PlayerStatus {
                    status: "stopped".to_string(),
                    failure_code: "decode_failure".to_string(),
                    ..PlayerStatus::default()
                },
                context,
            )
            .unwrap();

        assert_eq!(decision, QueueAdvanceDecision::None);
    }

    #[test]
    fn idle_stopped_status_does_not_advance_when_everything_is_empty() {
        let controller = controller(FakeBackend::new(vec![]));
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
    fn stopped_external_playback_returns_to_idle_for_reload_and_queue_recovery() {
        let controller = controller(FakeBackend::new(vec![]));
        controller.mark_external_playback().unwrap();

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
        assert_eq!(controller.snapshot().state, "idle");
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
    fn protect_current_song_hot_reload_changes_queueing_behavior() {
        let controller = controller(FakeBackend::new(vec![]));
        let request = request();
        controller
            .confirm_playback_success(
                &request,
                &status("目标", request.uri().as_str(), 1.0, 180.0),
            )
            .unwrap();
        let playing_without_identity = PlayerStatus {
            status: "playing".to_string(),
            ..PlayerStatus::default()
        };
        // 默认 protect=true：确认播放的歌曲受保护，新点歌排队。
        assert!(
            controller
                .should_queue_until_current_song_finished(&playing_without_identity)
                .unwrap()
        );
        // 热更新共享值（对应保存 queue.protect_current_song_until_finished=false）：
        // 立即放行，不再排队等待当前歌曲结束。
        *controller.queue_protect_current_song.write().unwrap() = false;
        assert!(
            !controller
                .should_queue_until_current_song_finished(&playing_without_identity)
                .unwrap()
        );
        // 改回 true：保护恢复。
        *controller.queue_protect_current_song.write().unwrap() = true;
        assert!(
            controller
                .should_queue_until_current_song_finished(&playing_without_identity)
                .unwrap()
        );
    }

    #[test]
    fn external_protect_seconds_hot_reload_changes_protection_timing() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let controller =
            controller_with_time(FakeBackend::new(vec![]), clock.clone(), clock.clone());
        let external = status("外部歌", "miliastra://track/qqmusic/external", 30.0, 180.0);
        controller.mark_external_playback().unwrap();
        // 默认 20s 保护：19s 时未受保护。
        clock.advance(Duration::from_secs(19)).unwrap();
        assert!(
            !controller
                .should_queue_until_current_song_finished(&external)
                .unwrap()
        );
        // 热更新共享值（对应保存 external_playback_protect_after_seconds=5）：
        // 同一外部播放再播 5s 后立即受保护（原 20s 不会保护）。
        *controller.queue_external_protect_seconds.write().unwrap() = 5;
        clock.advance(Duration::from_secs(5)).unwrap();
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
        // 与运行态热更新值一致：监控/查询间隔共享值（默认 1000ms）。
        let monitor_status_ms: u64 = 1000;
        let status_poll_ms: u64 = 1000;
        let active_request = ActivePlaybackRequest {
            // Deliberately unrelated wall-clock metadata: changing it must not affect the guard.
            started_at_ms: u64::MAX,
            guard_started_at: Some(started_at),
            ..ActivePlaybackRequest::default()
        };
        let guard_ms = monitor_status_ms
            .max(status_poll_ms)
            .saturating_mul(3)
            .max(3000);

        assert!(active_request_guard_active(
            monitor_status_ms,
            status_poll_ms,
            Some(&active_request),
            clock.now(),
        ));
        clock.advance(Duration::from_millis(guard_ms)).unwrap();
        assert!(!active_request_guard_active(
            monitor_status_ms,
            status_poll_ms,
            Some(&active_request),
            clock.now(),
        ));

        let restored_request = ActivePlaybackRequest {
            started_at_ms: clock.unix_millis(),
            guard_started_at: None,
            ..ActivePlaybackRequest::default()
        };
        assert!(!active_request_guard_active(
            monitor_status_ms,
            status_poll_ms,
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
