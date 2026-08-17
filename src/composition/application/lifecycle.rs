use super::*;
use crate::RunOutcome;

fn should_run_startup_automation(
    config_reload_child: bool,
    reload_requires_startup: bool,
    reusable_window_available: bool,
) -> bool {
    !config_reload_child || reload_requires_startup || !reusable_window_available
}

fn committed_run_result(result: Result<()>, reason: ShutdownReason) -> Result<RunOutcome> {
    match (result, reason) {
        (Err(error), ShutdownReason::ConfigReload | ShutdownReason::ConfigReloadWithStartup) => {
            // Once the task engine has stopped accepting work, process replacement is
            // irreversible. Preserve the watchdog handoff even if a final forwarding step fails.
            log::error!("配置重载已提交，关停尾部错误不再取消进程替换: {error:#}");
            Ok(match reason {
                ShutdownReason::ConfigReload => RunOutcome::Reload,
                ShutdownReason::ConfigReloadWithStartup => RunOutcome::ReloadWithStartup,
                _ => unreachable!(),
            })
        }
        (Err(error), ShutdownReason::UserExit) => {
            // A concurrent tail failure must not turn an explicit user exit into watchdog error
            // recovery and restart the process the user just stopped.
            log::error!("用户退出已提交，关停尾部错误不再触发看门狗重启: {error:#}");
            Ok(RunOutcome::Stopped)
        }
        (result, ShutdownReason::Running) => result.map(|()| RunOutcome::Stopped),
        (Ok(()), ShutdownReason::ConfigReload) => Ok(RunOutcome::Reload),
        (Ok(()), ShutdownReason::ConfigReloadWithStartup) => Ok(RunOutcome::ReloadWithStartup),
        (Ok(()), ShutdownReason::UserExit) => Ok(RunOutcome::Stopped),
    }
}

impl ApplicationRuntime {
    pub(crate) fn run(&mut self) -> Result<RunOutcome> {
        let mut deferred_chat_sender = None;
        let mut playback_monitor = None;
        let result = (|| -> Result<()> {
            self.start_formal_task_runtime()?;
            self.lifecycle
                .monitor
                .publish(MonitorEvent::Status("运行中".to_string()));
            self.update_monitor_playback_controller();
            self.update_monitor_operational_state();
            self.warn_if_screen_size_mismatch()?;
            let config_reload_child = std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_some();
            let reload_requires_startup =
                std::env::var_os(crate::CONFIG_RELOAD_RUN_STARTUP_ENV).is_some();
            let reusable_window_available = if config_reload_child {
                match self.ui.game_ui.capture() {
                    Ok(_) => true,
                    Err(error) => {
                        log::warn!(
                            "配置重载替代进程未找到可复用目标窗口，回退启动自动化: {error:#}"
                        );
                        false
                    }
                }
            } else {
                false
            };
            if should_run_startup_automation(
                config_reload_child,
                reload_requires_startup,
                reusable_window_available,
            ) {
                if config_reload_child {
                    self.require_reload_startup_automation()?;
                } else {
                    self.enqueue_startup_task_if_enabled()?;
                }
            } else {
                log::info!("配置重载替代进程已确认目标窗口可复用，跳过冷启动自动化");
            }
            self.start_http_server()?;
            match self.start_hotkeys() {
                Ok(runtime) => self.ui.hotkeys = Some(runtime),
                // 热键被占用时降级继续运行:退出热键失效,但主流程不受影响。
                Err(error) => {
                    log::error!("全局热键不可用(继续运行,退出热键失效): {error:#}")
                }
            }
            deferred_chat_sender = Some(self.start_deferred_chat_sender());
            playback_monitor = Some(self.start_playback_monitor());
            self.run_scan_loop()
        })();
        self.lifecycle.running.store(false, AtomicOrdering::SeqCst);
        if let Err(error) = self.business.task_engine.wake() {
            log::debug!("关停时唤醒任务引擎等待线程失败: {error}");
        }
        if let Err(error) = self.business.background_commands.stop_all() {
            log::error!("后台命令线程关闭失败: {error:#}");
        }
        if let Some(hotkeys) = self.ui.hotkeys.take()
            && let Err(error) = hotkeys.shutdown()
        {
            log::error!("全局热键线程关闭失败: {error:#}");
        }
        if let Some(http_server) = self.lifecycle.http_server.take()
            && let Err(error) = http_server.shutdown()
        {
            log::error!("HTTP/Web 面板关闭失败: {error:#}");
        }
        if let Some(deferred_chat_sender) = deferred_chat_sender
            && let Err(error) = deferred_chat_sender.join()
        {
            log::error!("延迟聊天发送线程 panic: {error:?}");
        }
        if let Some(playback_monitor) = playback_monitor
            && let Err(error) = playback_monitor.join()
        {
            log::error!("播放监控线程 panic: {error:?}");
        }
        self.join_moderation_workers();
        if let Some(runtime) = self.business.formal_task_runtime.take() {
            match runtime.shutdown() {
                Ok(report) if report.timed_out() => log::error!(
                    "正式任务未在关闭宽限期内结束，停止等待并继续关闭其他运行时: active_task_id={:?}",
                    report.active_task_id()
                ),
                Ok(_) => log::info!("正式任务运行时已关闭"),
                Err(error) => log::error!("正式任务运行时关闭失败: {error:#}"),
            }
        }
        self.persist_current_chat_listener_mode();
        if let Some(business_runtime) = self.business.business_runtime.take() {
            match business_runtime.shutdown() {
                Ok(snapshot) => log::info!(
                    "业务运行时组已关闭: deadline_forwarded={} business={:?}",
                    snapshot.deadlines().forwarded_count(),
                    snapshot.business()
                ),
                Err(error) => log::error!(
                    "业务运行时组关闭失败: {error}; prepare={:?} deadlines={:?} finish={:?}",
                    error.prepare_error(),
                    error.deadline_error(),
                    error.finish_error()
                ),
            }
        }
        if let Some(ui_runtime) = self.ui.ui_runtime.take() {
            match ui_runtime.shutdown() {
                Ok(report) if report.timed_out() => log::warn!(
                    "UI 运行时未在关闭宽限期内结束，阻塞任务无法强制取消，线程已脱离: detached={}",
                    report.detached()
                ),
                Ok(_) => log::info!("UI 运行时已关闭"),
                Err(error) => log::error!("UI 运行时关闭失败: {error}"),
            }
        }
        if let Some(ocr_runtime) = self.ui.ocr_runtime.take() {
            match ocr_runtime.shutdown() {
                Ok(report) if report.timed_out => {
                    log::warn!("OCR 运行时关闭等待超时，底层推理线程已脱离");
                }
                Err(error) => log::error!("OCR 运行时关闭失败: {error:#}"),
                _ => {}
            }
        }
        if let Some(player_runtime) = self.playback.player_runtime.take()
            && let Err(error) = player_runtime.shutdown()
        {
            log::error!("播放器运行时关闭失败: {error}");
        }
        if let Err(error) = self.playback.login_helper.shutdown() {
            log::error!("登录助手关闭失败: {error}");
        }
        if let Some(native_playback) = self.playback.native_playback_runtime.take()
            && let Err(error) = native_playback.shutdown()
        {
            log::error!("原生播放器关闭失败: {error:#}");
        }
        if let Some(mut kugou_api) = self.playback.kugou_api_sidecar.take()
            && let Err(error) = kugou_api.shutdown()
        {
            log::error!("酷狗 API sidecar 关闭失败: {error:#}");
        }
        if let Some(openai_runtime) = self.lifecycle.openai_runtime.take() {
            openai_runtime.shutdown();
            log::info!("OpenAI runtime 已关闭");
        }
        let shutdown_reason = self.lifecycle.shutdown.reason();
        let outcome = match shutdown_reason {
            ShutdownReason::ConfigReload => RunOutcome::Reload,
            ShutdownReason::ConfigReloadWithStartup => RunOutcome::ReloadWithStartup,
            ShutdownReason::Running | ShutdownReason::UserExit => RunOutcome::Stopped,
        };
        let status = match outcome {
            RunOutcome::Stopped => "已退出",
            RunOutcome::Reload | RunOutcome::ReloadWithStartup => "配置重载关停完成",
        };
        self.lifecycle
            .monitor
            .publish(MonitorEvent::Status(status.to_string()));
        committed_run_result(result, shutdown_reason)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::{
        ReloadStartupActions, ReloadStartupGate, ShutdownReason, ShutdownState,
        committed_run_result, should_run_startup_automation,
    };
    use crate::RunOutcome;
    use crate::features::startup::StartupTaskKind;

    #[test]
    fn config_reload_child_skips_cold_start_only_with_a_reusable_window() {
        assert!(should_run_startup_automation(false, false, false));
        assert!(should_run_startup_automation(false, false, true));
        assert!(should_run_startup_automation(true, false, false));
        assert!(!should_run_startup_automation(true, false, true));
        assert!(should_run_startup_automation(true, true, true));
    }

    #[test]
    fn reload_startup_gate_requires_every_requested_action_to_succeed() {
        let gate = ReloadStartupGate::default();
        let required = ReloadStartupActions::from_flags(true, true, true);
        gate.require(required);

        assert_eq!(gate.required_actions(), required);
        assert_eq!(gate.missing_actions(), required);
        assert!(!gate.is_satisfied());

        gate.record_success(StartupTaskKind::StartGame);
        assert!(gate.missing_actions().includes_enter_wonderland());
        assert!(!gate.missing_actions().includes_start_game());
        assert!(!gate.is_satisfied());

        gate.record_success(StartupTaskKind::EnterWonderland);
        assert!(gate.is_satisfied());
    }

    #[test]
    fn disabled_startup_has_no_reload_ready_gate() {
        let gate = ReloadStartupGate::default();
        let actions = ReloadStartupActions::from_flags(false, true, true);
        assert!(actions.is_empty());
        gate.require(actions);
        assert!(gate.is_satisfied());
    }

    #[test]
    fn startup_success_before_a_reload_requirement_does_not_acknowledge_it() {
        let gate = ReloadStartupGate::default();
        gate.record_success(StartupTaskKind::StartGame);
        gate.require(ReloadStartupActions::from_flags(true, true, false));

        assert!(!gate.is_satisfied());
        assert!(gate.missing_actions().includes_start_game());
    }

    #[test]
    fn user_exit_wins_before_or_after_a_reload_claim() {
        let exit_first = ShutdownState::new();
        exit_first.request_user_exit();
        assert!(!exit_first.try_claim_config_reload());
        assert_eq!(exit_first.reason(), ShutdownReason::UserExit);

        let reload_first = ShutdownState::new();
        assert!(reload_first.try_claim_config_reload());
        reload_first.request_user_exit();
        reload_first.release_config_reload_claim();
        assert_eq!(reload_first.reason(), ShutdownReason::UserExit);
    }

    #[test]
    fn an_abandoned_reload_claim_can_be_retried() {
        let shutdown = ShutdownState::new();
        assert!(shutdown.try_claim_config_reload());
        shutdown.release_config_reload_claim();
        assert_eq!(shutdown.reason(), ShutdownReason::Running);
        assert!(shutdown.try_claim_config_reload());
        assert_eq!(shutdown.reason(), ShutdownReason::ConfigReload);
    }

    #[test]
    fn committed_reload_can_require_startup_without_overriding_user_exit() {
        let shutdown = ShutdownState::new();
        assert!(shutdown.try_claim_config_reload());
        shutdown.require_startup_after_reload();
        assert_eq!(shutdown.reason(), ShutdownReason::ConfigReloadWithStartup);
        assert_eq!(
            committed_run_result(Ok(()), shutdown.reason()).unwrap(),
            RunOutcome::ReloadWithStartup
        );

        shutdown.request_user_exit();
        assert_eq!(shutdown.reason(), ShutdownReason::UserExit);
    }

    #[test]
    fn committed_reload_keeps_its_exit_outcome_after_a_tail_error() {
        assert_eq!(
            committed_run_result(
                Err(anyhow!("final forward failed")),
                ShutdownReason::ConfigReload,
            )
            .unwrap(),
            RunOutcome::Reload
        );
        assert!(
            committed_run_result(Err(anyhow!("ordinary failure")), ShutdownReason::Running)
                .is_err()
        );
    }

    #[test]
    fn committed_user_exit_ignores_a_concurrent_tail_error() {
        assert_eq!(
            committed_run_result(
                Err(anyhow!("final forward failed")),
                ShutdownReason::UserExit,
            )
            .unwrap(),
            RunOutcome::Stopped
        );
    }
}
