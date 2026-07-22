use super::*;

impl ApplicationRuntime {
    pub(super) fn record_command_activity(&self, observed_at: Instant) -> Result<()> {
        self.business
            .record_command_activity(observed_at)
            .map_err(anyhow::Error::from)
    }

    pub(super) fn maybe_idle_exit(&self) -> Result<()> {
        let Some(timeout) = self.business.claim_idle_exit(Instant::now())? else {
            return Ok(());
        };
        log::info!(
            "闲置退出触发: {}分钟无新命令，自动暂停播放器并关闭目标游戏进程，保留软件进程",
            timeout.as_secs() / 60
        );
        self.abort_entertainment_for_context_loss("闲置退出即将关闭游戏");
        if let Err(error) = self.player.pause_for_idle_exit() {
            log::error!("闲置退出自动暂停播放器失败: {error:#}");
        } else {
            log::info!("闲置退出已自动暂停播放器，防止退出后自动恢复或出队");
        }
        self.update_monitor_playback_controller();
        match self.game_ui.close_window() {
            Ok(()) => self.invalidate_latest_frame(),
            Err(error) => {
                log::error!("关闭目标窗口失败: {error:#}");
            }
        }
        Ok(())
    }

    pub(super) fn clear_idle_exit_timer(&self) -> Result<()> {
        self.business.clear_idle_exit().map_err(anyhow::Error::from)
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
            PendingTask::Startup(task) => self
                .startup
                .execute(task, self)
                .map(|_| PendingTaskExecution::Completed),
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
                &self.console_reply_context,
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
        write_executed_command_fields(
            &self.monitor,
            &self.config.state.executed_commands_log_path,
            message_type,
            username,
            user_command,
            final_command,
        )
    }

    pub(super) fn pending_contains_command(&self, parsed: &RoutedCommand) -> Result<bool> {
        self.business
            .formal_task_contains_dedup_key(crate::runtime::scheduler::FormalTaskDedupKey::new(
                command::lock_key(parsed),
            ))
            .map_err(anyhow::Error::from)
    }

    pub(super) fn executor_is_idle(&self) -> Result<bool> {
        Ok(self.business.scheduler_snapshot()?.is_idle())
    }

    pub(super) fn push_pending_task(&self, task: PendingTask) -> Result<()> {
        let tasks = self
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
        if !self.config.startup.enabled {
            return Ok(());
        }
        if self.config.startup.launch_game || self.config.startup.enter_game {
            self.push_pending_task(PendingTask::Startup(StartupTask::start_game(
                StartupSource::STARTUP_CONFIG,
            )))?;
        }
        if self.config.startup.enter_wonderland {
            self.push_pending_task(PendingTask::Startup(StartupTask::enter_wonderland(
                StartupSource::STARTUP_CONFIG,
            )))?;
        }
        Ok(())
    }

    pub(super) fn active_ui_residency(&self) -> Result<UiResidency> {
        let snapshot = self.business.chat_listener_snapshot()?;
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
