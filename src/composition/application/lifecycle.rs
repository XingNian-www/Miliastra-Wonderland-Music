use super::ShutdownReason;
use super::shutdown::{
    SHUTDOWN_JOIN_BUDGET, ShutdownHandles, ShutdownReport, ShutdownStep, WorkerThread,
    join_worker_threads,
};
use super::*;
use crate::RunOutcome;

fn should_run_startup_automation(
    config_reload_child: bool,
    reload_requires_startup: bool,
    reusable_window_available: bool,
) -> bool {
    !config_reload_child || reload_requires_startup || !reusable_window_available
}

/// 收尾：把 `run()` 的并发结果映射成进程退出结论。
///
/// 4 态的**按原因分叉**在这里只查表（[`ShutdownReason::finale`]），不再手写 match：
/// 「哪种原因该吞掉收尾错误」是一个数据事实（见 `shutdown.rs` 的类型文档表），
/// 散在 match 里就会有人只改一处。
fn committed_run_result(result: Result<()>, reason: ShutdownReason) -> Result<RunOutcome> {
    let finale = reason.finale();
    match result {
        Ok(()) => Ok(finale.outcome),
        Err(error) => match finale.tail_error_prefix {
            // 收尾阶段出错：进程替代已提交、不可逆，因此保留原结论 + 记 error。
            Some(prefix) => {
                log::error!("{prefix}{error:#}");
                Ok(finale.outcome)
            }
            // `Running` 是普通运行期，收尾错误照常上抛给看门狗做错误恢复。
            None => Err(error),
        },
    }
}

impl ApplicationRuntime {
    pub(crate) fn run(&mut self) -> Result<RunOutcome> {
        let mut handles = ShutdownHandles::default();
        let mut shared_chat_subscriber = None;
        let result = (|| -> Result<()> {
            self.start_formal_task_runtime()?;
            shared_chat_subscriber = Some(self.ui.chat_timeline.subscribe_business()?);
            handles.shared_chat_observer = Some(self.start_shared_chat_observer()?);
            self.shared
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
                            "配置重载替代进程未找到可复用目标窗口，回退启动自动化：{error:#}"
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
                log::info!("配置重载的替代进程确认可以复用目标窗口，跳过冷启动自动化");
            }
            self.owned.http_server = Some(self.start_http_server()?);
            match self.start_hotkeys() {
                Ok(runtime) => self.owned.hotkeys = Some(runtime),
                // 热键被占用时降级继续运行:退出热键失效,但主流程不受影响。
                Err(error) => {
                    log::error!(
                        "全局热键注册失败（步骤：注册热键），程序继续运行，但暂停与退出热键本次不可用：{error:#}"
                    )
                }
            }
            handles.deferred_chat_sender = Some(WorkerThread::new(
                "deferred-chat-sender",
                self.start_deferred_chat_sender(),
                "延迟聊天发送线程崩溃（步骤：关停时等待线程退出），排队中的回复没能发出，\
本次关停无法正常收尾：",
            ));
            handles.playback_monitor = Some(WorkerThread::new(
                "playback-monitor",
                self.start_playback_monitor(),
                "播放监控线程崩溃（步骤：关停时等待线程退出），本次关停可能不完整，\
播放状态可能停在最后已知值：",
            ));
            self.run_scan_loop(
                shared_chat_subscriber
                    .take()
                    .context("shared chat subscriber unavailable")?,
            )
        })();
        let reason = self.shared.shutdown.reason();
        let report = self.shutdown_all(reason, SHUTDOWN_JOIN_BUDGET, &mut handles);
        let finale = reason.finale();
        self.shared
            .monitor
            .publish(MonitorEvent::Status(finale.status_label.to_string()));
        // 预算内全都退出是**常态**，只在 debug 留一行；超期才值得人看（`shutdown_all` 已逐条 warn）。
        if report.timed_out() {
            log::warn!(
                "本次关停有 {} 条线程未在预算内退出（{}），进程不再等待它们",
                report.detached().len(),
                report.detached().join("、")
            );
        } else {
            log::debug!(
                "关停序列已完成：{} 步 / {} ms",
                report.steps(),
                report.elapsed().as_millis()
            );
        }
        committed_run_result(result, reason)
    }

    /// **唯一**的关停入口：19 步按 [`super::shutdown::SHUTDOWN_STEPS`] 的固定顺序执行。
    ///
    /// 行为契约（W1a 前置，尚未进 supervisor）：
    ///
    /// - **顺序**逐项等于 golden 数据（`lifecycle_tests::shutdown_order_matches_the_golden_sequence`）。
    /// - **默认路径完全等价**于拆分前的 19 段手写关停：每一步的动作、日志文案、
    ///   返回类型处理逐字保留。
    /// - **唯一新增语义**：两处曾经无界的 `join()` 现在有预算；超期只 warn + 记账，
    ///   **绝不**提前 `return`（超期线程排在序列中段，提前返回会把业务/UI/OCR/播放器
    ///   全部丢下不关）。
    /// - `reason` 目前**不改变**序列（4 态共用同一序列）；它只被记录与转交，
    ///   收尾分叉在 [`ShutdownReason::finale`]。这是 W1 要重新审视的那条边界，
    ///   眼下先显式化参数，避免 supervisor 落地时再改一次调用点。
    fn shutdown_all(
        &mut self,
        reason: ShutdownReason,
        budget: Duration,
        handles: &mut ShutdownHandles,
    ) -> ShutdownReport {
        let mut report = ShutdownReport::new(budget);
        let exit = reason.finale().outcome;
        log::debug!(
            "开始关停（原因：{reason:?}，退出码：{}，预算：{} ms）",
            exit.exit_code(),
            budget.as_millis()
        );

        // 1. 共享聊天观察线程：靠 `Drop` 停 + join 自己的线程。
        drop(handles.shared_chat_observer.take());
        report.enter(ShutdownStep::SharedChatObserver);

        // 2. 外部聊天源必须在观察线程**停稳之后**才关：外部源线程退出路径会
        // `set_active(false)`，此时若观察线程还在跑，它会在关停那一瞬间判定
        // 「外部源不再接管」而回落到 OCR 扫描——关停期平白多一次截图与识别，
        // 日志里还会出现误导性的 OCR 噪音。先 join 观察线程，外部源状态就一直
        // 保持到没有读侧为止。顺序理由全表见 `shutdown.rs` 的 [`SHUTDOWN_STEPS`]。
        if let Some(external_chat_workers) = self.owned.external_chat_workers.take() {
            let started = external_chat_workers.started();
            external_chat_workers.shutdown();
            if started > 0 {
                log::info!("外部聊天源后台线程已全部退出（共 {started} 条）");
            }
        }
        report.enter(ShutdownStep::ExternalChatWorkers);

        // 3. 全局运行标志：先让各循环在等待时看见「该走了」。
        self.shared.running.store(false, AtomicOrdering::SeqCst);
        report.enter(ShutdownStep::RunningFlag);

        // 4. 唤醒任务引擎等待线程（失败只影响唤醒延迟，不阻断关停）。
        if let Err(error) = self.business.task_engine.wake() {
            log::debug!("关停时唤醒任务引擎等待线程失败：{error}");
        }
        report.enter(ShutdownStep::TaskEngineWake);

        // 5. 后台歌词线程。
        if let Err(error) = self.business.background_lyrics.stop_all() {
            log::error!(
                "后台歌词线程关闭失败（步骤：关停后台歌词），可能有线程残留，进程退出时会被系统强制回收：{error:#}"
            );
        }
        report.enter(ShutdownStep::BackgroundLyrics);

        // 6. 全局热键。
        if let Some(hotkeys) = self.owned.hotkeys.take()
            && let Err(error) = hotkeys.shutdown()
        {
            log::error!(
                "全局热键线程关闭失败（步骤：关停热键监听），热键可能占用到进程结束：{error:#}"
            );
        }
        report.enter(ShutdownStep::Hotkeys);

        // 7. HTTP 面板（自带 5 秒宽限，超期 warn 后返回 Ok）。
        if let Some(http_server) = self.owned.http_server.take()
            && let Err(error) = http_server.shutdown()
        {
            log::error!("HTTP 面板关闭失败（步骤：关停 Web 服务），端口可能仍被占用：{error:#}");
        }
        report.enter(ShutdownStep::HttpServer);

        // 8–9. 两处曾经**无界**的裸线程 join。
        //
        // 拆出来合成一次有界等待：预算均分给两条线程，超期只 warn + 记账，
        // 且**继续**执行第 10 步起的所有关停（这是本波唯一的行为变更）。
        let workers = [
            handles.deferred_chat_sender.take(),
            handles.playback_monitor.take(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        report.enter(ShutdownStep::DeferredChatSender);
        join_worker_threads(workers, budget, &mut report);
        report.enter(ShutdownStep::PlaybackMonitor);

        // 10. 管理投票线程（自带逐条 join，失败逐条记账）。
        self.join_moderation_workers();
        report.enter(ShutdownStep::ModerationWorkers);

        // 11. 正式任务运行时（超期 warn，继续关停其余运行时）。
        if let Some(runtime) = self.owned.formal_task_runtime.take() {
            match runtime.shutdown() {
                Ok(report) if report.timed_out() => log::error!(
                    "正式任务运行时未在关闭宽限期内结束（步骤：关停正式任务），本次停止等待并继续关闭其他运行时，该任务线程会被强制断开，可能残留未完成的写操作"
                ),
                Ok(_) => log::info!("正式任务运行时已正常关闭"),
                Err(error) => log::error!(
                    "正式任务运行时关闭失败（步骤：关停正式任务），可能有任务线程残留：{error:#}"
                ),
            }
        }
        report.enter(ShutdownStep::FormalTaskRuntime);

        // 12. 落在业务运行时**之前**：值守模式要读业务运行时的快照。
        self.persist_current_chat_listener_mode();
        report.enter(ShutdownStep::ListenerModePersist);

        // 13. 业务运行时（丢了期限是 warn，其它错误是 error）。
        if let Some(business_runtime) = self.owned.business_runtime.take() {
            match business_runtime.shutdown() {
                Ok(snapshot) => log::info!(
                    "业务运行时组已正常关闭，其中 {} 条期限已交给业务运行时处理",
                    snapshot.deadlines().forwarded_count()
                ),
                Err(error) if error.deadline_error().is_some() => log::warn!(
                    "业务运行时组关闭时丢弃了部分期限（步骤：先停业务运行时再停计时器），这些截止时间不会再生效：{error:#}"
                ),
                Err(error) => log::error!(
                    "业务运行时组关闭失败（步骤：依次关停准备阶段与期限处理），可能有业务线程残留、未提交的期限不会再生效：{error:#}"
                ),
            }
        }
        report.enter(ShutdownStep::BusinessRuntime);

        // 14. UI 运行时（自带 2 秒宽限 + detach 记账）。
        if let Some(ui_runtime) = self.owned.ui_runtime.take() {
            match ui_runtime.shutdown() {
                Ok(report) if report.timed_out() => log::warn!(
                    "UI 运行时未在关闭宽限期内结束（步骤：关停界面运行时），阻塞中的界面任务无法强制取消，其中 {} 个线程已脱离管理，进程退出时会被系统回收",
                    report.detached()
                ),
                Ok(_) => log::info!("UI 运行时已正常关闭"),
                Err(error) => log::error!(
                    "UI 运行时关闭失败（步骤：关停界面运行时），窗口或后台线程可能残留：{error}"
                ),
            }
        }
        report.enter(ShutdownStep::UiRuntime);

        // 15. OCR 运行时（自带宽限，超期 `timed_out`）。
        if let Some(ocr_runtime) = self.owned.ocr_runtime.take() {
            match ocr_runtime.shutdown() {
                Ok(report) if report.timed_out => {
                    log::warn!(
                        "OCR 运行时关闭等待超时（步骤：等待推理线程退出），底层推理线程已脱离管理，进程退出时会被强制回收"
                    );
                }
                Err(error) => log::error!(
                    "OCR 运行时关闭失败（步骤：关停文字识别），推理线程可能残留：{error:#}"
                ),
                _ => {}
            }
        }
        report.enter(ShutdownStep::OcrRuntime);

        // 16. 播放器运行时。
        if let Some(player_runtime) = self.owned.player_runtime.take()
            && let Err(error) = player_runtime.shutdown()
        {
            log::error!("播放器运行时关闭失败（步骤：关停播放器），播放状态可能没有保存：{error}");
        }
        report.enter(ShutdownStep::PlayerRuntime);

        // 17. 登录助手（子进程 + 管道排空）。
        if let Err(error) = self.playback.login_helper.shutdown() {
            log::error!("登录助手关闭失败（步骤：关停登录助手），助手线程可能残留：{error}");
        }
        report.enter(ShutdownStep::LoginHelper);

        // 18. 原生播放器（音频设备）。
        if let Some(native_playback) = self.owned.native_playback_runtime.take()
            && let Err(error) = native_playback.shutdown()
        {
            log::error!(
                "原生播放器关闭失败（步骤：关停原生播放器），音频设备可能仍被占用：{error:#}"
            );
        }
        report.enter(ShutdownStep::NativePlayback);

        // 19. OpenAI 运行时（最后：不改其它组件状态，失败仅 warn 级）。
        if let Some(openai_runtime) = self.owned.openai_runtime.take() {
            openai_runtime.shutdown();
            log::info!("OpenAI 运行时已正常关闭");
        }
        report.enter(ShutdownStep::OpenAiRuntime);

        report.finish();
        report
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod lifecycle_tests;
