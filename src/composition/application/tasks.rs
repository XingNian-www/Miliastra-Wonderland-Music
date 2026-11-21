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

/// 自动恢复时「启动游戏」这一半由谁负责。
///
/// 拆开关闭与启动两个动作的原因：**游戏必须由聊天桥创建进程，DLL 才能注入**。
/// 反作弊只允许「创建游戏进程的一方」注入，所以启用外部聊天源时启动这一步
/// 不能由本进程 `Command::new` 硬拉——那会得到一个没有聊天源的游戏进程，
/// 失联判定随后会再次成立，形成「重开 → 没聊天源 → 再重开」的死循环。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GameRelaunch {
    /// 由 Miliastra-Wonderland-ChatBridge 拉起（`chat_source.auto_start` 时
    /// 外部聊天源线程每轮重连都会 `ensure_running`，桥自己创建游戏进程并注入）。
    ViaChatBridge,
    /// 由启动流程按 `startup.exe_path` / 注册表查找后拉起。
    ViaStartupLaunch,
    /// 没有任何可用的自动拉起路径：不要关游戏，否则就再也回不来了。
    None,
}

/// 判定「启动游戏」由谁负责（纯函数，便于单测）。
///
/// 优先级：聊天桥（唯一能注入 DLL 的路径）> 启动流程直接拉起 > 不可用。
/// `for_chat_source` 为真时**只接受**聊天桥路径：聊天源失联是因为注入的
/// 链路断了，直接拉 exe 得到的新进程没有聊天源，重启只会无限循环。
pub(super) fn game_relaunch_mode_for(
    config: &crate::config::AppConfig,
    for_chat_source: bool,
) -> GameRelaunch {
    if config.chat_source.enabled && config.chat_source.auto_start {
        return GameRelaunch::ViaChatBridge;
    }
    if for_chat_source {
        return GameRelaunch::None;
    }
    if config.startup.enabled && config.startup.launch_game {
        return GameRelaunch::ViaStartupLaunch;
    }
    GameRelaunch::None
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

fn apply_hall_expiry_pause(
    pause_applied: &mut bool,
    notice_sent: &mut bool,
    mut pause: impl FnMut() -> Result<()>,
    mut enqueue_notice: impl FnMut() -> Result<()>,
) -> Result<()> {
    if !*pause_applied {
        pause()?;
        *pause_applied = true;
    }
    if !*notice_sent {
        enqueue_notice()?;
        *notice_sent = true;
    }
    Ok(())
}

impl AppServices {
    pub(super) fn reload_aware_wait(&self, requested: Duration) -> Duration {
        reload_poll_wait(requested, self.shared.live_configs.has_pending_reload())
    }

    pub(super) fn record_command_activity(&self, observed_at: Instant) -> Result<()> {
        self.business
            .business
            .record_command_activity(observed_at)
            .map_err(anyhow::Error::from)
    }

    pub(super) fn maybe_pause_expired_hall(
        &self,
        pause_applied: &mut bool,
        notice_sent: &mut bool,
    ) -> Result<bool> {
        if self
            .business
            .business
            .hall_state_snapshot()?
            .remaining_minutes_now()
            != Some(0)
        {
            return Ok(false);
        }
        apply_hall_expiry_pause(
            pause_applied,
            notice_sent,
            || {
                self.playback.player.pause_for_hall_expiry()?;
                self.update_monitor_playback_controller();
                log::info!("大厅倒计时已结束但未确认进入新大厅，已暂停播放");
                Ok(())
            },
            || {
                super::enqueue_current_hall_reply(
                    &self.business.task_engine,
                    "大厅即将到期,已自动暂停音乐",
                )
            },
        )?;
        self.stop_background_lyrics_output()?;
        Ok(true)
    }

    /// 判断当前配置能不能把游戏重新拉起来（**只看配置，不产生副作用**）。
    ///
    /// `for_chat_source` 区分两种触发来源：聊天源失联时必须走桥注入路径，
    /// 直接拉 exe 会得到一个没有聊天源的进程。
    pub(super) fn game_relaunch_mode(
        &self,
        config: &crate::config::AppConfig,
        for_chat_source: bool,
    ) -> GameRelaunch {
        game_relaunch_mode_for(config, for_chat_source)
    }

    /// 「重启游戏」的**第一步：只关闭**。
    ///
    /// 暂停播放、关闭游戏窗口并清掉失联计时。不在这里启动游戏——
    /// 启动交给 [`Self::request_game_relaunch`]，避免本进程绕过聊天桥硬拉进程。
    pub(super) fn close_game_for_restart(
        &self,
        reason: &'static str,
        external_chat: &crate::composition::application::external_chat::ExternalChatState,
    ) {
        log::warn!("自动重启游戏第 1 步：关闭游戏（原因：{reason}），先暂停播放器");
        self.abort_entertainment_for_context_loss("自动重启游戏");
        if let Err(error) = self.playback.player.pause_for_auto_restart() {
            // 播放状态已置为「用户暂停」，自动推进不会再继续；
            // 这里失败的只是播放器后端的暂停 RPC，所以不会"自行恢复播放"。
            log::error!(
                "自动重启游戏前暂停播放器后端失败（播放状态已是暂停，不会自动续播；仅后端 RPC 未成功）：{error:#}"
            );
        }
        self.update_monitor_playback_controller();
        match self.ui.game_ui.close_window() {
            Ok(()) => log::info!("已关闭游戏窗口（原因：{reason}），等待自动拉起"),
            Err(error) => {
                // 窗口可能已经因为崩溃自己没了；那正好，继续走拉起流程即可。
                // 也不能直接当成功：若窗口其实还在，拉起会复用旧进程（等于没重启）。
                log::warn!(
                    "关闭游戏窗口失败（步骤：自动重启游戏第 1 步），游戏窗口可能已自行消失，继续尝试重新拉起：{error:#}"
                );
            }
        }
        self.invalidate_latest_frame();
        // 失联起点清零：重启后再从零计时，避免用旧起点立刻再次触发。
        external_chat.clear_loss();
    }

    /// 「重启游戏」的**第二步：请既有的自动拉起机制重新启动游戏**。
    ///
    /// 不自建启动逻辑：走桥时由外部聊天源线程的 `ensure_running` 拉起桥，
    /// 桥再创建游戏进程并注入 DLL；不走桥时才排队一次启动流程任务。
    pub(super) fn request_game_relaunch(&self, mode: GameRelaunch) -> Result<()> {
        match mode {
            GameRelaunch::ViaChatBridge => {
                log::warn!(
                    "自动重启游戏第 2 步：由 Miliastra-Wonderland-ChatBridge 重新拉起游戏并注入 DLL（本进程不直接启动游戏进程，否则拿不到注入）"
                );
                Ok(())
            }
            GameRelaunch::ViaStartupLaunch => {
                let task = StartupTask::start_game(StartupSource::AUTO_RECOVERY);
                log::warn!("自动重启游戏第 2 步：排队执行启动流程：{}", task.label());
                self.push_pending_task(PendingTask::Startup(task))
            }
            GameRelaunch::None => {
                log::warn!(
                    "自动重启游戏已放弃：当前配置没有可用的自动拉起路径（需要 chat_source.auto_start 或 startup.launch_game），关掉游戏后将无法自动恢复"
                );
                Ok(())
            }
        }
    }

    pub(super) fn maybe_idle_exit(&self) -> Result<()> {
        let Some(timeout) = self.business.business.claim_idle_exit(Instant::now())? else {
            return Ok(());
        };
        log::info!(
            "闲置退出已触发：{} 分钟没有新命令，自动暂停播放器并关闭目标游戏进程，本程序继续运行",
            timeout.as_secs() / 60
        );
        self.abort_entertainment_for_context_loss("闲置退出即将关闭游戏");
        if let Err(error) = self.playback.player.pause_for_idle_exit() {
            log::error!("闲置退出时暂停播放器失败（步骤：闲置退出前暂停播放），播放可能自行继续，退出后仍可能出队：{error:#}");
        } else {
            log::info!("闲置退出已暂停播放器，避免退出后自动恢复播放或继续出队");
        }
        self.update_monitor_playback_controller();
        // 主动关游戏：抑制「失联后重启游戏」判定，否则回落阈值一到就会把
        // 刚被闲置退出关掉的游戏重新拉起来，闲置退出等于没做。
        self.shared
            .external_chat
            .suppress_restart_after_intentional_close();
        match self.ui.game_ui.close_window() {
            Ok(()) => self.invalidate_latest_frame(),
            Err(error) => {
                log::error!("关闭目标游戏窗口失败（步骤：闲置退出关闭游戏），游戏窗口可能仍然打开：{error:#}");
            }
        }
        Ok(())
    }

    /// 空闲时执行待处理的配置重载。
    ///
    /// 返回本轮是否已经把 `running` 置为 false（即重载已提交、主循环应当退出）。
    /// `reload_check_after` 是重载检查自身的节流时钟，属于**这次调用方的循环状态**，
    /// 因此由调用方持有并传进来，共享服务里不存任何「某项检查的进度」。
    pub(super) fn maybe_reload_config_when_idle(&self, reload_check_after: &mut Instant) -> bool {
        if !self.shared.running.load(AtomicOrdering::SeqCst) {
            return false;
        }
        if self.shared.paused.load(AtomicOrdering::SeqCst)
            || !self.shared.live_configs.has_pending_reload()
        {
            self.shared
                .http_reload.draining()
                .store(false, AtomicOrdering::SeqCst);
            return false;
        }
        let now = Instant::now();
        if now < *reload_check_after {
            return false;
        }
        *reload_check_after = now + CONFIG_RELOAD_IDLE_CHECK_INTERVAL;
        let preliminary_requires_playback_idle = self.shared.live_configs
            .pending_reload_requires_playback_idle();
        let preliminary_idle = match self.reload_idle_state() {
            Ok(idle) => idle,
            Err(error) => {
                self.shared
                    .http_reload.draining()
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("读取闲置重载状态失败，本轮保持运行：{error:#}");
                return false;
            }
        };
        if preliminary_idle.drain_readiness(preliminary_requires_playback_idle)
            == ReloadDrainReadiness::Unsafe
        {
            self.shared
                .http_reload.draining()
                .store(false, AtomicOrdering::SeqCst);
            return false;
        }
        // 先关闭新的 HTTP 写操作准入，再复查现有请求，避免在读取到 0 后又有
        // 登录、保存或播放器操作进入并被关停打断。
        self.shared
            .http_reload.draining()
            .store(true, AtomicOrdering::SeqCst);
        let idle = match self.reload_idle_state() {
            Ok(idle) => idle,
            Err(error) => {
                self.shared
                    .http_reload.draining()
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("进入 HTTP 排空后复查重载状态失败，本轮保持运行：{error:#}");
                return false;
            }
        };
        match idle.drain_readiness(preliminary_requires_playback_idle) {
            ReloadDrainReadiness::Unsafe => {
                self.shared
                    .http_reload.draining()
                    .store(false, AtomicOrdering::SeqCst);
                return false;
            }
            ReloadDrainReadiness::WaitingForHttp => return false,
            ReloadDrainReadiness::Ready => {}
        }

        // blocker 归零后，所有已获准的保存请求都已完成；此时重新读取累计字段的
        // 最严格屏障，避免并发保存把普通重载升级为播放器重载后仍沿用旧判断。
        let requires_playback_idle = self.shared.live_configs
            .pending_reload_requires_playback_idle();
        let mut idle = match self.reload_idle_state() {
            Ok(idle) => idle,
            Err(error) => {
                self.shared
                    .http_reload.draining()
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("HTTP 排空后复查重载状态失败，本轮保持运行：{error:#}");
                return false;
            }
        };
        match idle.drain_readiness(requires_playback_idle) {
            ReloadDrainReadiness::Unsafe => {
                self.shared
                    .http_reload.draining()
                    .store(false, AtomicOrdering::SeqCst);
                return false;
            }
            ReloadDrainReadiness::WaitingForHttp => return false,
            ReloadDrainReadiness::Ready => {}
        }
        let refreshed_status = match self.refresh_playback_state_for_reload() {
            Ok(status) => status,
            Err(error) => {
                self.shared
                    .http_reload.draining()
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("配置重载前刷新播放器状态失败，本轮保持运行：{error:#}");
                return false;
            }
        };
        idle = match self.reload_idle_state_with_status(Some(&refreshed_status)) {
            Ok(idle) => idle,
            Err(error) => {
                self.shared
                    .http_reload.draining()
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("刷新播放器状态后复查重载条件失败，本轮保持运行：{error:#}");
                return false;
            }
        };
        if !idle.is_idle(requires_playback_idle) {
            self.shared
                .http_reload.draining()
                .store(false, AtomicOrdering::SeqCst);
            return false;
        }
        // 在封存任务引擎前预留重载结果。用户退出记录的优先级更高，因此可以在此声明前后覆盖它。
        if !self.shared.shutdown.try_claim_config_reload() {
            self.shared
                .http_reload.draining()
                .store(false, AtomicOrdering::SeqCst);
            return false;
        }
        // 空闲检查与停止接单必须在任务引擎的同一把锁内完成，避免新任务落入两者之间。
        match self.business.task_engine.begin_shutdown_if_idle() {
            Ok(true) => {}
            Ok(false) => {
                self.shared.shutdown.release_config_reload_claim();
                self.shared
                    .http_reload.draining()
                    .store(false, AtomicOrdering::SeqCst);
                return false;
            }
            Err(error) => {
                self.shared.shutdown.release_config_reload_claim();
                self.shared
                    .http_reload.draining()
                    .store(false, AtomicOrdering::SeqCst);
                log::warn!("重载关停前原子锁定任务引擎失败，本轮保持运行：{error}");
                return false;
            }
        }
        let fields = match self.shared.live_configs.begin_reload() {
            Some(fields) => fields,
            None => {
                // 任务引擎已经不可逆地停止接单；即使内部待重载标记出现异常，
                // 也必须完成进程替换，不能留下一个仍运行但永远不再接收任务的 BOT。
                log::error!("任务引擎已停止接单，但待重载字段无法认领，仍强制重载子进程");
                Default::default()
            }
        };
        if reload_fields_require_startup(&fields) {
            self.shared.shutdown.require_startup_after_reload();
        }
        log::info!(
            "运行环境已闲置，开始配置重载关停（需要重载的配置项：{}）",
            fields.into_iter().collect::<Vec<_>>().join("、")
        );
        self.shared.running.store(false, AtomicOrdering::SeqCst);
        true
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
            .shared
            .http_reload.blockers()
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
            log::debug!("配置重载待处理，刷新播放状态后抑制自动出队");
        }
        Ok(status)
    }

    pub(super) fn clear_idle_exit_timer(&self) -> Result<()> {
        self.business
            .business
            .clear_idle_exit()
            .map_err(anyhow::Error::from)
    }

    /// 在一个正式任务工作线程上执行一条待处理任务。
    ///
    /// 只用 `&self`：任务的全部副作用都落在共享句柄（业务运行时、UI 例程、
    /// 播放器、配置快照）上，执行方不需要拥有任何运行时，
    /// 也不需要一份「重建出来的应用壳」。
    pub(super) fn execute_pending_task(&self, task: PendingTask) -> Result<PendingTaskExecution> {
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
                let mut port = self.port();
                let result = self.business.startup.execute(task, &mut port);
                if result.is_ok() {
                    self.shared.reload_startup.record_success(kind);
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
            PendingTask::CardGameEffect(effect) => {
                let mut port = self.port();
                effect
                    .execute(&mut port)
                    .map(|_| PendingTaskExecution::Completed)
            }
            PendingTask::UndercoverEffect(effect) => {
                let mut port = self.port();
                effect
                    .execute(&mut port)
                    .map(|_| PendingTaskExecution::Completed)
            }
        };
        match result {
            Ok(PendingTaskExecution::Completed) => {
                log::info!("待处理任务已完成：{label}");
                Ok(PendingTaskExecution::Completed)
            }
            Err(error) => {
                log::error!("待处理任务失败（步骤：执行待处理任务「{label}」），该任务已放弃，不会自动重试：{error:#}");
                Err(error)
            }
        }
    }

    pub(super) fn execute_pending_command(&self, pending: PendingCommand) -> Result<()> {
        let command_log = private_safe_command_log(&pending.routed);
        log::debug!(
            "开始执行待处理命令：{}（命令锁：{}）",
            command_log,
            if is_private_undercover_input(&pending.routed) {
                "已隐藏"
            } else {
                pending.lock_key.as_str()
            }
        );
        let _console_reply_context = if pending.routed.message_type == "控制台" {
            Some(ConsoleReplyContextGuard::new(Arc::clone(
                &self.shared.console_reply_context,
            )))
        } else {
            None
        };
        let command_started = Instant::now();
        match self.execute_command(&pending.routed) {
            Ok(()) => {
                let command_ms = elapsed_ms(command_started);
                log::info!("命令执行完成：{command_log}");
                log::info!(target: "timing",
                    "命令执行耗时: command={} success=true total={}ms",
                    command_log,
                    command_ms
                );
            }
            Err(error) => {
                let command_ms = elapsed_ms(command_started);
                log::error!("命令执行失败（步骤：执行命令「{command_log}」），这条命令没有生效，玩家不会收到执行结果：{error:#}");
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
        let live_config = self.shared.live_configs.snapshot();
        write_executed_command_fields(
            &self.shared.monitor,
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
        match self.formal_tasks()?.enqueue(task)? {
            FormalTaskEnqueueOutcome::Queued(_) => Ok(()),
            FormalTaskEnqueueOutcome::Duplicate => {
                log::debug!("正式任务已在待执行范围内，跳过重复入队");
                Ok(())
            }
        }
    }

    pub(super) fn enqueue_startup_task_if_enabled(&self) -> Result<()> {
        self.enqueue_startup_actions(ReloadStartupActions::from_startup_config(
            &self.shared.config,
        ))
    }

    pub(super) fn require_reload_startup_automation(&self) -> Result<bool> {
        let actions = self
            .shared
            .reload_startup
            .require_from_config(&self.shared.config);
        if actions.is_empty() {
            return Ok(false);
        }
        self.enqueue_reload_startup_tasks_if_needed()?;
        Ok(true)
    }

    pub(super) fn enqueue_reload_startup_tasks_if_needed(&self) -> Result<bool> {
        let missing = self.shared.reload_startup.missing_actions();
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
mod recovery_restart_tests {
    use super::{GameRelaunch, game_relaunch_mode_for};
    use crate::config::{AppConfig, ChatSourceDisconnectAction};

    /// 聊天源失联且开了 auto_start：必须走桥（只有桥能注入 DLL）。
    #[test]
    fn chat_source_loss_relaunches_via_chat_bridge_when_auto_start() {
        let mut config = AppConfig::default();
        config.chat_source.enabled = true;
        config.chat_source.auto_start = true;
        assert_eq!(
            game_relaunch_mode_for(&config, true),
            GameRelaunch::ViaChatBridge
        );
    }

    /// 聊天源失联但没开 auto_start：没有任何注入路径，**不许**关游戏。
    /// 否则重启出来的进程没有聊天源，失联判定会再次成立 → 无限重开。
    #[test]
    fn chat_source_loss_refuses_to_restart_without_injection_path() {
        let mut config = AppConfig::default();
        config.chat_source.enabled = true;
        config.chat_source.auto_start = false;
        config.startup.enabled = true;
        config.startup.launch_game = true; // 有启动路径也不能用：那会绕过 Dll 注入
        assert_eq!(game_relaunch_mode_for(&config, true), GameRelaunch::None);
    }

    /// 加载超时重启（非聊天源场景）可以走启动流程直接拉起。
    #[test]
    fn loading_timeout_relaunches_via_startup_launch_when_configured() {
        let mut config = AppConfig::default();
        config.startup.enabled = true;
        config.startup.launch_game = true;
        assert_eq!(
            game_relaunch_mode_for(&config, false),
            GameRelaunch::ViaStartupLaunch
        );
    }

    /// 加载超时但既没开启动流程也没开桥：不关游戏。
    #[test]
    fn loading_timeout_refuses_to_restart_without_any_launch_path() {
        let config = AppConfig::default();
        assert_eq!(game_relaunch_mode_for(&config, false), GameRelaunch::None);
    }

    /// 开着 auto_start 时优先走桥，即使同时配了 startup.launch_game。
    #[test]
    fn chat_bridge_wins_over_direct_launch() {
        let mut config = AppConfig::default();
        config.chat_source.enabled = true;
        config.chat_source.auto_start = true;
        config.startup.enabled = true;
        config.startup.launch_game = true;
        assert_eq!(
            game_relaunch_mode_for(&config, false),
            GameRelaunch::ViaChatBridge
        );
    }

    /// 默认动作必须是回落 OCR：这是历史行为，不能让升级后的用户游戏被动重启。
    #[test]
    fn default_disconnect_action_is_fallback_ocr() {
        assert_eq!(
            AppConfig::default().chat_source.disconnect_action,
            ChatSourceDisconnectAction::FallbackOcr
        );
    }
}

#[cfg(test)]
mod reload_tests {
    use std::time::Duration;

    use super::{
        CONFIG_RELOAD_IDLE_CHECK_INTERVAL, PlaybackReloadReadiness, ReloadDrainReadiness,
        ReloadIdleState, apply_hall_expiry_pause, playback_reload_readiness,
        playback_reload_readiness_after_refresh, reload_fields_require_startup, reload_poll_wait,
    };
    use crate::features::playback::{
        ActivePlaybackRequest, ConfirmedPlaybackState, ObservationReliability, PlaybackObservation,
        PlaybackRuntimeState, PlayerStatus, test_track,
    };

    #[test]
    fn hall_expiry_pause_failure_does_not_send_notice() {
        let mut pause_applied = false;
        let mut notice_sent = false;
        let mut notice_calls = 0;

        let result = apply_hall_expiry_pause(
            &mut pause_applied,
            &mut notice_sent,
            || anyhow::bail!("pause failed"),
            || {
                notice_calls += 1;
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(!pause_applied);
        assert!(!notice_sent);
        assert_eq!(notice_calls, 0);
    }

    #[test]
    fn hall_expiry_notice_follows_successful_pause_and_is_not_repeated() {
        let mut pause_applied = false;
        let mut notice_sent = false;
        let mut pause_calls = 0;
        let mut notice_calls = 0;

        apply_hall_expiry_pause(
            &mut pause_applied,
            &mut notice_sent,
            || {
                pause_calls += 1;
                Ok(())
            },
            || {
                notice_calls += 1;
                Ok(())
            },
        )
        .unwrap();
        apply_hall_expiry_pause(
            &mut pause_applied,
            &mut notice_sent,
            || {
                pause_calls += 1;
                Ok(())
            },
            || {
                notice_calls += 1;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(pause_calls, 1);
        assert_eq!(notice_calls, 1);
    }

    #[test]
    fn hall_expiry_notice_retries_without_repeating_successful_pause() {
        let mut pause_applied = false;
        let mut notice_sent = false;
        let mut pause_calls = 0;
        let mut notice_calls = 0;

        let result = apply_hall_expiry_pause(
            &mut pause_applied,
            &mut notice_sent,
            || {
                pause_calls += 1;
                Ok(())
            },
            || {
                notice_calls += 1;
                anyhow::bail!("queue unavailable")
            },
        );
        assert!(result.is_err());
        assert!(pause_applied);
        assert!(!notice_sent);

        apply_hall_expiry_pause(
            &mut pause_applied,
            &mut notice_sent,
            || {
                pause_calls += 1;
                Ok(())
            },
            || {
                notice_calls += 1;
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(pause_calls, 1);
        assert_eq!(notice_calls, 2);
    }

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
