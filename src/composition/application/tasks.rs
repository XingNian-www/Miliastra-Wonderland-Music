use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReloadIdleState {
    task_engine_idle: bool,
    entertainment_idle: bool,
    playback: PlaybackReloadReadiness,
    moderation_workers_idle: bool,
    login_helper_idle: bool,
    http_operations_idle: bool,
}

impl ReloadIdleState {
    const fn background_idle(self, requires_playback_idle: bool) -> bool {
        self.task_engine_idle
            && self.entertainment_idle
            && if requires_playback_idle {
                matches!(
                    self.playback,
                    PlaybackReloadReadiness::Idle | PlaybackReloadReadiness::PausedRecoverable
                )
            } else {
                !matches!(self.playback, PlaybackReloadReadiness::Unsafe)
            }
            && self.moderation_workers_idle
            && self.login_helper_idle
    }

    const fn is_idle(self, requires_playback_idle: bool) -> bool {
        self.background_idle(requires_playback_idle) && self.http_operations_idle
    }

    const fn drain_readiness(self, requires_playback_idle: bool) -> ReloadDrainReadiness {
        if !self.background_idle(requires_playback_idle) {
            ReloadDrainReadiness::Unsafe
        } else if self.http_operations_idle {
            ReloadDrainReadiness::Ready
        } else {
            ReloadDrainReadiness::WaitingForHttp
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReloadDrainReadiness {
    Unsafe,
    WaitingForHttp,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaybackReloadReadiness {
    Idle,
    PlayingRecoverable,
    PausedRecoverable,
    Unsafe,
}

fn reload_fields_require_startup(fields: &std::collections::BTreeSet<String>) -> bool {
    fields.iter().any(|field| {
        matches!(
            field.as_str(),
            "startup"
                | "startup.enabled"
                | "startup.launch_game"
                | "startup.enter_game"
                | "startup.enter_wonderland"
        )
    })
}

fn playback_reload_readiness(playback: &PlaybackRuntimeState) -> PlaybackReloadReadiness {
    if playback.active_request.is_none() && playback.state == ConfirmedPlaybackState::Idle {
        return PlaybackReloadReadiness::Idle;
    }
    let recoverable_observation = |expected_status: &str, keep_paused: bool| {
        let Some(active_track) = playback
            .active_request
            .as_ref()
            .and_then(|request| request.track.as_ref())
        else {
            return false;
        };
        let Some(observation) = playback.last_observation.as_ref() else {
            return false;
        };
        observation.status == expected_status
            && observation.track.as_ref().is_some_and(|observed_track| {
                observed_track.track_ref.key == active_track.track_ref.key
            })
            && has_restorable_playback_progress(Some(observation), keep_paused)
    };
    if playback.state == ConfirmedPlaybackState::PausedByUser
        && recoverable_observation("paused", true)
    {
        return PlaybackReloadReadiness::PausedRecoverable;
    }
    if playback.state == ConfirmedPlaybackState::RequestedSongPlaying
        && recoverable_observation("playing", false)
    {
        return PlaybackReloadReadiness::PlayingRecoverable;
    }
    PlaybackReloadReadiness::Unsafe
}

fn playback_reload_readiness_after_refresh(
    playback: &PlaybackRuntimeState,
    status: &PlayerStatus,
) -> PlaybackReloadReadiness {
    let readiness = playback_reload_readiness(playback);
    let status_matches_active_track = playback
        .active_request
        .as_ref()
        .and_then(|request| request.track.as_ref())
        .zip(status.current_track.as_ref())
        .is_some_and(|(requested, current)| requested.track_ref.key == current.track_ref.key);

    match readiness {
        PlaybackReloadReadiness::Idle
            if matches!(status.status.as_str(), "stopped" | "stoped" | "idle") =>
        {
            PlaybackReloadReadiness::Idle
        }
        PlaybackReloadReadiness::PlayingRecoverable
            if status.status == "playing" && status_matches_active_track =>
        {
            PlaybackReloadReadiness::PlayingRecoverable
        }
        PlaybackReloadReadiness::PausedRecoverable
            if status.status == "paused" && status_matches_active_track =>
        {
            PlaybackReloadReadiness::PausedRecoverable
        }
        // 持久状态可能尚未追上刚开始的外部播放，或实时播放器已经切歌、停止。
        // 最终关停必须以真实 transport 和曲目身份为准，不能截断无法恢复的音频。
        _ => PlaybackReloadReadiness::Unsafe,
    }
}

fn reload_poll_wait(requested: Duration, pending_reload: bool) -> Duration {
    if pending_reload {
        requested.min(CONFIG_RELOAD_IDLE_CHECK_INTERVAL)
    } else {
        requested
    }
}

impl ApplicationRuntime {
    pub(super) fn reload_aware_wait(&self, requested: Duration) -> Duration {
        reload_poll_wait(requested, self.lifecycle.live_configs.has_pending_reload())
    }

    pub(super) fn record_command_activity(&self, observed_at: Instant) -> Result<()> {
        self.business
            .business
            .record_command_activity(observed_at)
            .map_err(anyhow::Error::from)
    }

    pub(super) fn maybe_idle_exit(&self) -> Result<()> {
        let Some(timeout) = self.business.business.claim_idle_exit(Instant::now())? else {
            return Ok(());
        };
        log::info!(
            "闲置退出触发: {}分钟无新命令，自动暂停播放器并关闭目标游戏进程，保留软件进程",
            timeout.as_secs() / 60
        );
        self.abort_entertainment_for_context_loss("闲置退出即将关闭游戏");
        if let Err(error) = self.playback.player.pause_for_idle_exit() {
            log::error!("闲置退出自动暂停播放器失败: {error:#}");
        } else {
            log::info!("闲置退出已自动暂停播放器，防止退出后自动恢复或出队");
        }
        self.update_monitor_playback_controller();
        match self.ui.game_ui.close_window() {
            Ok(()) => self.invalidate_latest_frame(),
            Err(error) => {
                log::error!("关闭目标窗口失败: {error:#}");
            }
        }
        Ok(())
    }

    pub(super) fn maybe_reload_config_when_idle(&mut self) {
        if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
            return;
        }
        if self.lifecycle.paused.load(AtomicOrdering::SeqCst)
            || !self.lifecycle.live_configs.has_pending_reload()
        {
            self.lifecycle
                .http_reload_draining
                .store(false, AtomicOrdering::SeqCst);
            return;
        }
        let now = Instant::now();
        if now < self.lifecycle.reload_check_after {
            return;
        }
        self.lifecycle.reload_check_after = now + CONFIG_RELOAD_IDLE_CHECK_INTERVAL;
        let preliminary_requires_playback_idle = self
            .lifecycle
            .live_configs
            .pending_reload_requires_playback_idle();
        let preliminary_idle = match self.reload_idle_state() {
            Ok(idle) => idle,
            Err(error) => {
                self.lifecycle
                    .http_reload_draining
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("读取闲置重载状态失败，本轮保持运行: {error:#}");
                return;
            }
        };
        if preliminary_idle.drain_readiness(preliminary_requires_playback_idle)
            == ReloadDrainReadiness::Unsafe
        {
            self.lifecycle
                .http_reload_draining
                .store(false, AtomicOrdering::SeqCst);
            return;
        }
        // 先关闭新的 HTTP 写操作准入，再复查现有请求，避免在读取到 0 后又有
        // 登录、保存或播放器操作进入并被关停打断。
        self.lifecycle
            .http_reload_draining
            .store(true, AtomicOrdering::SeqCst);
        let idle = match self.reload_idle_state() {
            Ok(idle) => idle,
            Err(error) => {
                self.lifecycle
                    .http_reload_draining
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("进入 HTTP 排空后复查重载状态失败，本轮保持运行: {error:#}");
                return;
            }
        };
        match idle.drain_readiness(preliminary_requires_playback_idle) {
            ReloadDrainReadiness::Unsafe => {
                self.lifecycle
                    .http_reload_draining
                    .store(false, AtomicOrdering::SeqCst);
                return;
            }
            ReloadDrainReadiness::WaitingForHttp => return,
            ReloadDrainReadiness::Ready => {}
        }

        // blocker 归零后，所有已获准的保存请求都已完成；此时重新读取累计字段的
        // 最严格屏障，避免并发保存把普通重载升级为播放器重载后仍沿用旧判断。
        let requires_playback_idle = self
            .lifecycle
            .live_configs
            .pending_reload_requires_playback_idle();
        let mut idle = match self.reload_idle_state() {
            Ok(idle) => idle,
            Err(error) => {
                self.lifecycle
                    .http_reload_draining
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("HTTP 排空后复查重载状态失败，本轮保持运行: {error:#}");
                return;
            }
        };
        match idle.drain_readiness(requires_playback_idle) {
            ReloadDrainReadiness::Unsafe => {
                self.lifecycle
                    .http_reload_draining
                    .store(false, AtomicOrdering::SeqCst);
                return;
            }
            ReloadDrainReadiness::WaitingForHttp => return,
            ReloadDrainReadiness::Ready => {}
        }
        let refreshed_status = match self.refresh_playback_state_for_reload() {
            Ok(status) => status,
            Err(error) => {
                self.lifecycle
                    .http_reload_draining
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("配置重载前刷新播放器状态失败，本轮保持运行: {error:#}");
                return;
            }
        };
        idle = match self.reload_idle_state_with_status(Some(&refreshed_status)) {
            Ok(idle) => idle,
            Err(error) => {
                self.lifecycle
                    .http_reload_draining
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("刷新播放器状态后复查重载条件失败，本轮保持运行: {error:#}");
                return;
            }
        };
        if !idle.is_idle(requires_playback_idle) {
            self.lifecycle
                .http_reload_draining
                .store(false, AtomicOrdering::SeqCst);
            return;
        }
        // Reserve the reload outcome before sealing the task engine. A user exit records a
        // higher-priority reason and can therefore win on either side of this claim.
        if !self.lifecycle.shutdown.try_claim_config_reload() {
            self.lifecycle
                .http_reload_draining
                .store(false, AtomicOrdering::SeqCst);
            return;
        }
        // 空闲检查与停止接单必须在任务引擎的同一把锁内完成，避免新任务落入两者之间。
        match self.business.task_engine.begin_shutdown_if_idle() {
            Ok(true) => {}
            Ok(false) => {
                self.lifecycle.shutdown.release_config_reload_claim();
                self.lifecycle
                    .http_reload_draining
                    .store(false, AtomicOrdering::SeqCst);
                return;
            }
            Err(error) => {
                self.lifecycle.shutdown.release_config_reload_claim();
                self.lifecycle
                    .http_reload_draining
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("重载关停前原子锁定任务引擎失败，本轮保持运行: {error}");
                return;
            }
        }
        let fields = match self.lifecycle.live_configs.begin_reload() {
            Some(fields) => fields,
            None => {
                // 任务引擎已经不可逆地停止接单；即使内部待重载标记出现异常，
                // 也必须完成进程替换，不能留下一个仍运行但永远不再接收任务的 BOT。
                log::error!("任务引擎已停止接单，但待重载字段无法认领，仍强制重载子进程");
                Default::default()
            }
        };
        if reload_fields_require_startup(&fields) {
            self.lifecycle.shutdown.require_startup_after_reload();
        }
        log::info!(
            "运行环境已闲置，开始配置重载关停: fields={}",
            fields.into_iter().collect::<Vec<_>>().join(",")
        );
        self.lifecycle.running.store(false, AtomicOrdering::SeqCst);
    }

    fn reload_idle_state(&self) -> Result<ReloadIdleState> {
        self.reload_idle_state_with_status(None)
    }

    fn reload_idle_state_with_status(
        &self,
        refreshed_status: Option<&PlayerStatus>,
    ) -> Result<ReloadIdleState> {
        let task_engine_idle = self.business.task_engine.is_idle()?;
        let entertainment_idle = self.business.business.active_entertainment()?.is_none();
        let playback = self.business.business.playback_state_snapshot()?;
        let playback = refreshed_status.map_or_else(
            || playback_reload_readiness(&playback),
            |status| playback_reload_readiness_after_refresh(&playback, status),
        );
        let moderation_workers_idle = self
            .business
            .moderation_workers
            .lock()
            .map_err(|_| anyhow!("管理投票线程句柄锁已损坏"))?
            .iter()
            .all(thread::JoinHandle::is_finished);
        let login_helper_idle = !self.playback.login_helper.status().active;
        let http_operations_idle = self
            .lifecycle
            .http_reload_blockers
            .load(AtomicOrdering::SeqCst)
            == 0;
        Ok(ReloadIdleState {
            task_engine_idle,
            entertainment_idle,
            playback,
            moderation_workers_idle,
            login_helper_idle,
            http_operations_idle,
        })
    }

    fn refresh_playback_state_for_reload(&self) -> Result<PlayerStatus> {
        let status = self.playback.player.status_for_reload()?;
        let scheduler = self.business.task_engine.snapshot()?;
        let context = QueueAdvanceContext {
            queue_empty: self.playback_queue()?.is_empty(),
            has_pending_playback_task: scheduler.pending_playback_related(),
            command_executing: scheduler.is_busy(),
        };
        if matches!(
            self.playback
                .player
                .maybe_advance_queue_for_reload(status.clone(), context)?,
            QueueAdvanceDecision::AdvanceQueue { .. }
        ) {
            log::info!("配置重载待处理，刷新播放状态后抑制自动出队");
        }
        Ok(status)
    }

    pub(super) fn clear_idle_exit_timer(&self) -> Result<()> {
        self.business
            .business
            .clear_idle_exit()
            .map_err(anyhow::Error::from)
    }

    pub(super) fn execute_pending_task(
        &mut self,
        task: PendingTask,
    ) -> Result<PendingTaskExecution> {
        let label = task.label();
        let result = match task {
            PendingTask::Command(pending) => self
                .execute_pending_command(*pending)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::AdvanceQueue { reason } => self
                .execute_advance_queue_task(reason)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::ConsoleChat { text, prefix } => self
                .execute_console_chat_task(text, prefix)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::Startup(task) => {
                let kind = task.kind();
                let result = self.business.startup.execute(task, self);
                if result.is_ok() {
                    self.lifecycle.reload_startup.record_success(kind);
                }
                result.map(|_| PendingTaskExecution::Completed)
            }
            PendingTask::ClearIdleExit => self
                .clear_idle_exit_timer()
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::ModerationResult(task) => self.execute_moderation_vote_result(task),
            PendingTask::SetChatListenerMode { target } => self
                .execute_set_chat_listener_mode(target)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::SecondaryUnread { hit, discard_only } => self
                .execute_secondary_unread_task(hit, discard_only)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::RestoreSecondaryHall => self
                .execute_restore_secondary_hall_task()
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::TurtleSoupQuestion {
                question,
                observed_at,
            } => self
                .execute_turtle_soup_question(*question, observed_at)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::CardGameEffect(effect) => effect
                .execute(self)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::UndercoverEffect(effect) => effect
                .execute(self)
                .map(|_| PendingTaskExecution::Completed),
        };
        match result {
            Ok(PendingTaskExecution::Completed) => {
                log::info!("待处理任务完成: {}", label);
                Ok(PendingTaskExecution::Completed)
            }
            Err(error) => {
                log::error!("待处理任务失败 {}: {error:#}", label);
                Err(error)
            }
        }
    }

    pub(super) fn execute_pending_command(&mut self, pending: PendingCommand) -> Result<()> {
        let command_log = private_safe_command_log(&pending.routed);
        log::info!(
            "执行待处理命令: {} lock={}",
            command_log,
            if is_private_undercover_input(&pending.routed) {
                "[hidden]"
            } else {
                pending.lock_key.as_str()
            }
        );
        let _console_reply_context = if pending.routed.message_type == "控制台" {
            Some(ConsoleReplyContextGuard::new(Arc::clone(
                &self.lifecycle.console_reply_context,
            )))
        } else {
            None
        };
        let command_started = Instant::now();
        match self.execute_command(&pending.routed) {
            Ok(()) => {
                let command_ms = elapsed_ms(command_started);
                log::info!("命令执行完成: {}", command_log);
                log::info!(target: "timing",
                    "命令执行耗时: command={} success=true total={}ms",
                    command_log,
                    command_ms
                );
            }
            Err(error) => {
                let command_ms = elapsed_ms(command_started);
                log::error!("命令执行失败 {}: {error:#}", command_log);
                log::info!(target: "timing",
                    "命令执行耗时: command={} success=false total={}ms",
                    command_log,
                    command_ms
                );
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) fn log_executed_command(
        &self,
        parsed: &RoutedCommand,
        final_command: &str,
    ) -> Result<()> {
        self.log_executed_command_fields(
            &parsed.message_type,
            command_username(parsed),
            &parsed.user_command,
            final_command,
        )
    }

    pub(super) fn log_executed_command_fields(
        &self,
        message_type: &str,
        username: &str,
        user_command: &str,
        final_command: &str,
    ) -> Result<()> {
        let live_config = self.lifecycle.live_configs.snapshot();
        write_executed_command_fields(
            &self.lifecycle.monitor,
            &live_config.state.executed_commands_log_path,
            message_type,
            username,
            user_command,
            final_command,
        )
    }

    pub(super) fn pending_contains_command(&self, parsed: &RoutedCommand) -> Result<bool> {
        self.business
            .task_engine
            .contains_formal_dedup_key(&crate::runtime::scheduler::FormalTaskDedupKey::new(
                command::lock_key(parsed),
            ))
            .map_err(anyhow::Error::from)
    }

    pub(super) fn executor_is_idle(&self) -> Result<bool> {
        Ok(self.business.task_engine.snapshot()?.is_idle())
    }

    pub(super) fn push_pending_task(&self, task: PendingTask) -> Result<()> {
        let tasks = self
            .business
            .formal_tasks
            .clone()
            .ok_or_else(|| anyhow!("正式任务执行运行时尚未启动"))?;
        match tasks.enqueue(task)? {
            FormalTaskEnqueueOutcome::Queued(_) => Ok(()),
            FormalTaskEnqueueOutcome::Duplicate => {
                log::info!("正式任务已在待执行范围内，跳过重复入队");
                Ok(())
            }
        }
    }

    pub(super) fn enqueue_startup_task_if_enabled(&self) -> Result<()> {
        self.enqueue_startup_actions(ReloadStartupActions::from_startup_config(
            &self.lifecycle.config,
        ))
    }

    pub(super) fn require_reload_startup_automation(&self) -> Result<bool> {
        let actions = self
            .lifecycle
            .reload_startup
            .require_from_config(&self.lifecycle.config);
        if actions.is_empty() {
            return Ok(false);
        }
        self.enqueue_reload_startup_tasks_if_needed()?;
        Ok(true)
    }

    pub(super) fn enqueue_reload_startup_tasks_if_needed(&self) -> Result<bool> {
        let missing = self.lifecycle.reload_startup.missing_actions();
        if missing.is_empty() {
            return Ok(false);
        }
        self.enqueue_startup_actions(missing)?;
        Ok(true)
    }

    fn enqueue_startup_actions(&self, actions: ReloadStartupActions) -> Result<()> {
        if actions.includes_start_game() {
            self.push_pending_task(PendingTask::Startup(StartupTask::start_game(
                StartupSource::STARTUP_CONFIG,
            )))?;
        }
        if actions.includes_enter_wonderland() {
            self.push_pending_task(PendingTask::Startup(StartupTask::enter_wonderland(
                StartupSource::STARTUP_CONFIG,
            )))?;
        }
        Ok(())
    }

    pub(super) fn active_ui_residency(&self) -> Result<UiResidency> {
        let snapshot = self.business.business.chat_listener_snapshot()?;
        Ok(listener_residency(
            snapshot.mode,
            snapshot.temporary_primary,
        ))
    }

    pub(super) fn establish_ui_residency(
        &self,
        target: UiResidency,
        purpose: ResidencyPurpose,
    ) -> Result<()> {
        let context = purpose.label();
        let target = match target {
            UiResidency::Primary => UiResidencyTarget::Primary,
            UiResidency::SecondaryCurrentHall => UiResidencyTarget::SecondaryCurrentHall,
        };
        let outcome = self
            .ui
            .residency_ui
            .submit(EstablishResidency::new(target))
            .with_context(|| format!("{context}: 提交 UI 驻留任务失败"))?
            .wait()
            .with_context(|| format!("{context}: 等待 UI 驻留任务失败"))?;
        match outcome {
            UiResidencyOutcome::Confirmed(actual) if actual == target => Ok(()),
            UiResidencyOutcome::Confirmed(actual) => Err(anyhow!(
                "{context}: UI 驻留结果不匹配 expected={target:?} actual={actual:?}"
            )),
            UiResidencyOutcome::Failed(failure) => {
                Err(anyhow!("{context}: 未能建立 UI 驻留：{failure}"))
            }
        }
    }
}

pub(super) fn write_executed_command_fields(
    monitor: &MonitorShared,
    path: &std::path::Path,
    message_type: &str,
    username: &str,
    user_command: &str,
    final_command: &str,
) -> Result<()> {
    monitor.publish(MonitorEvent::Command(format!(
        "{} -> {}",
        user_command, final_command
    )));
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create command log directory {}", parent.display()))?;
    }
    let line = format!(
        "{}-{}-{}-{}-{}\n",
        command_log_timestamp(),
        command_log_field(command_location(message_type)),
        command_log_field(username),
        command_log_field(user_command),
        command_log_field(final_command),
    );
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open command log {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("write command log {}", path.display()))
}

#[cfg(test)]
mod reload_tests {
    use std::time::Duration;

    use super::{
        CONFIG_RELOAD_IDLE_CHECK_INTERVAL, PlaybackReloadReadiness, ReloadDrainReadiness,
        ReloadIdleState, playback_reload_readiness, playback_reload_readiness_after_refresh,
        reload_fields_require_startup, reload_poll_wait,
    };
    use crate::features::playback::{
        ActivePlaybackRequest, ConfirmedPlaybackState, ObservationReliability, PlaybackObservation,
        PlaybackRuntimeState, PlayerStatus, test_track,
    };

    fn idle_state() -> ReloadIdleState {
        ReloadIdleState {
            task_engine_idle: true,
            entertainment_idle: true,
            playback: PlaybackReloadReadiness::Idle,
            moderation_workers_idle: true,
            login_helper_idle: true,
            http_operations_idle: true,
        }
    }

    #[test]
    fn reload_requires_every_runtime_lane_to_be_idle() {
        assert!(idle_state().is_idle(false));

        let mut states = [idle_state(); 6];
        states[0].task_engine_idle = false;
        states[1].entertainment_idle = false;
        states[2].playback = PlaybackReloadReadiness::Unsafe;
        states[3].moderation_workers_idle = false;
        states[4].login_helper_idle = false;
        states[5].http_operations_idle = false;

        assert!(states.into_iter().all(|state| !state.is_idle(false)));
    }

    #[test]
    fn startup_fields_request_startup_automation_in_the_replacement() {
        assert!(reload_fields_require_startup(
            &std::collections::BTreeSet::from(["startup.enter_wonderland".to_string()])
        ));
        assert!(reload_fields_require_startup(
            &std::collections::BTreeSet::from(["startup.enabled".to_string()])
        ));
        assert!(!reload_fields_require_startup(
            &std::collections::BTreeSet::from(["startup.template_threshold".to_string()])
        ));
        assert!(!reload_fields_require_startup(
            &std::collections::BTreeSet::from(["timing.loop_idle_ms".to_string()])
        ));
    }

    #[test]
    fn reload_allows_queued_tracks_when_player_is_idle() {
        // 待重载时队列内容会留在持久化存储中，重启后的新进程继续消费；
        // 队列非空不应阻止当前歌曲结束后的重载。
        assert!(idle_state().is_idle(false));
    }

    #[test]
    fn http_drain_is_kept_only_while_existing_http_work_is_the_last_blocker() {
        let mut state = idle_state();
        state.http_operations_idle = false;
        assert_eq!(
            state.drain_readiness(false),
            ReloadDrainReadiness::WaitingForHttp
        );

        state.login_helper_idle = false;
        assert_eq!(state.drain_readiness(false), ReloadDrainReadiness::Unsafe);

        state.login_helper_idle = true;
        state.playback = PlaybackReloadReadiness::PlayingRecoverable;
        assert_eq!(state.drain_readiness(true), ReloadDrainReadiness::Unsafe);
    }

    #[test]
    fn playback_reload_readiness_distinguishes_idle_recoverable_and_unsafe_sessions() {
        let mut playback = PlaybackRuntimeState::default();
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::Idle
        );
        assert_eq!(
            playback_reload_readiness_after_refresh(
                &playback,
                &PlayerStatus {
                    status: "playing".to_string(),
                    current_track: Some(test_track(
                        "miliastra://track/qqmusic/external",
                        "刚开始的外部播放",
                    )),
                    ..Default::default()
                }
            ),
            PlaybackReloadReadiness::Unsafe,
            "真实播放器已开始外部播放时，旧 Idle 快照不得放行重载"
        );
        assert_eq!(
            playback_reload_readiness_after_refresh(
                &playback,
                &PlayerStatus {
                    status: "stopped".to_string(),
                    ..Default::default()
                }
            ),
            PlaybackReloadReadiness::Idle
        );

        playback.state = ConfirmedPlaybackState::RequestedSongPlaying;
        let requested_track = test_track("miliastra://track/qqmusic/1", "测试歌曲");
        playback.active_request = Some(ActivePlaybackRequest {
            track: Some(requested_track.clone()),
            ..Default::default()
        });
        playback.last_observation = Some(PlaybackObservation {
            status: "playing".to_string(),
            track: Some(requested_track.clone()),
            progress: 42.0,
            duration: 180.0,
            reliability: ObservationReliability::Reliable,
            ..Default::default()
        });
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::PlayingRecoverable
        );
        assert_eq!(
            playback_reload_readiness_after_refresh(
                &playback,
                &PlayerStatus {
                    status: "playing".to_string(),
                    current_track: Some(requested_track.clone()),
                    ..Default::default()
                }
            ),
            PlaybackReloadReadiness::PlayingRecoverable
        );
        assert_eq!(
            playback_reload_readiness_after_refresh(
                &playback,
                &PlayerStatus {
                    status: "paused".to_string(),
                    current_track: Some(requested_track.clone()),
                    ..Default::default()
                }
            ),
            PlaybackReloadReadiness::Unsafe,
            "实时 transport 已改变时不能按可恢复播放放行重载"
        );
        assert_eq!(
            playback_reload_readiness_after_refresh(
                &playback,
                &PlayerStatus {
                    status: "playing".to_string(),
                    current_track: Some(test_track(
                        "miliastra://track/qqmusic/external",
                        "实时外部播放",
                    )),
                    ..Default::default()
                }
            ),
            PlaybackReloadReadiness::Unsafe,
            "实时播放器切到其他歌曲时不能中断外部播放"
        );
        let mut state = idle_state();
        state.playback = PlaybackReloadReadiness::PlayingRecoverable;
        assert!(state.is_idle(false));
        assert!(!state.is_idle(true));

        playback.last_observation.as_mut().unwrap().track = Some(test_track(
            "miliastra://track/qqmusic/2",
            "后台已切换的歌曲",
        ));
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::Unsafe
        );

        let observation = playback.last_observation.as_mut().unwrap();
        observation.track = Some(requested_track.clone());
        observation.status = "stopped".to_string();
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::Unsafe
        );

        let observation = playback.last_observation.as_mut().unwrap();
        observation.status = "playing".to_string();
        observation.progress = 178.0;
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::Unsafe
        );

        playback.state = ConfirmedPlaybackState::PausedByUser;
        let observation = playback.last_observation.as_mut().unwrap();
        observation.status = "paused".to_string();
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::PausedRecoverable,
            "暂停在末尾保护区内不会自然结束，必须允许钳制进度后重载"
        );
        assert_eq!(
            playback_reload_readiness_after_refresh(
                &playback,
                &PlayerStatus {
                    status: "paused".to_string(),
                    current_track: Some(requested_track.clone()),
                    ..Default::default()
                }
            ),
            PlaybackReloadReadiness::PausedRecoverable
        );
        assert_eq!(
            playback_reload_readiness_after_refresh(
                &playback,
                &PlayerStatus {
                    status: "playing".to_string(),
                    current_track: Some(requested_track.clone()),
                    ..Default::default()
                }
            ),
            PlaybackReloadReadiness::Unsafe,
            "用户暂停后实时播放器继续播放时不应作为暂停恢复会话重载"
        );
        state.playback = PlaybackReloadReadiness::PausedRecoverable;
        assert!(state.is_idle(false));
        assert!(state.is_idle(true));

        playback.last_observation.as_mut().unwrap().status = "stopped".to_string();
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::Unsafe
        );

        playback.last_observation = None;
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::Unsafe
        );

        playback.state = ConfirmedPlaybackState::ExternalPlayback;
        playback.active_request = None;
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::Unsafe
        );

        playback.state = ConfirmedPlaybackState::Unknown;
        assert_eq!(
            playback_reload_readiness(&playback),
            PlaybackReloadReadiness::Unsafe
        );
    }

    #[test]
    fn pending_reload_caps_scan_waits_to_the_idle_check_interval() {
        assert_eq!(
            reload_poll_wait(Duration::from_secs(60), true),
            CONFIG_RELOAD_IDLE_CHECK_INTERVAL
        );
        assert_eq!(
            reload_poll_wait(Duration::from_millis(250), true),
            Duration::from_millis(250)
        );
        assert_eq!(
            reload_poll_wait(Duration::from_secs(60), false),
            Duration::from_secs(60)
        );
    }
}
