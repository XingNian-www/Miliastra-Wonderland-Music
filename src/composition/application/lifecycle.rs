use super::*;

impl ApplicationRuntime {
    pub(crate) fn run(&mut self) -> Result<()> {
        self.start_formal_task_runtime()?;
        self.monitor
            .publish(MonitorEvent::Status("运行中".to_string()));
        self.update_monitor_playback_controller();
        self.update_monitor_operational_state();
        self.warn_if_screen_size_mismatch()?;
        // No fallible setup may follow start_hotkeys: later workers require the shared teardown.
        self.enqueue_startup_task_if_enabled()?;
        self.start_http_server()?;
        self.hotkeys = Some(self.start_hotkeys()?);
        let deferred_chat_sender = self.start_deferred_chat_sender();
        let playback_monitor = self.start_playback_monitor();
        let result = self.run_scan_loop();
        self.running.store(false, AtomicOrdering::SeqCst);
        if let Err(error) = self.background_commands.stop_all() {
            log::error!("后台命令线程关闭失败: {error:#}");
        }
        if let Some(hotkeys) = self.hotkeys.take()
            && let Err(error) = hotkeys.shutdown()
        {
            log::error!("全局热键线程关闭失败: {error:#}");
        }
        if let Some(http_server) = self.http_server.take()
            && let Err(error) = http_server.shutdown()
        {
            log::error!("HTTP/Web 面板关闭失败: {error:#}");
        }
        if let Some(runtime) = self.formal_task_runtime.take() {
            match runtime.shutdown() {
                Ok(report) if report.timed_out() => log::error!(
                    "正式任务未在关闭宽限期内结束，停止等待并继续关闭其他运行时: active_task_id={:?}",
                    report.active_task_id()
                ),
                Ok(_) => log::info!("正式任务运行时已关闭"),
                Err(error) => log::error!("正式任务运行时关闭失败: {error:#}"),
            }
        }
        if let Err(error) = deferred_chat_sender.join() {
            log::error!("延迟聊天发送线程 panic: {error:?}");
        }
        if let Err(error) = playback_monitor.join() {
            log::error!("播放监控线程 panic: {error:?}");
        }
        self.join_moderation_workers();
        if let Some(business_runtime) = self.business_runtime.take() {
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
        if let Some(ui_runtime) = self.ui_runtime.take()
            && let Err(error) = ui_runtime.shutdown()
        {
            log::error!("UI 运行时关闭失败: {error}");
        }
        if let Some(ocr_runtime) = self.ocr_runtime.take()
            && let Err(error) = ocr_runtime.shutdown()
        {
            log::error!("OCR 运行时关闭失败: {error:#}");
        }
        if let Some(player_runtime) = self.player_runtime.take()
            && let Err(error) = player_runtime.shutdown()
        {
            log::error!("播放器运行时关闭失败: {error}");
        }
        if let Some(openai_runtime) = self.openai_runtime.take() {
            openai_runtime.shutdown();
            log::info!("OpenAI runtime 已关闭");
        }
        self.monitor
            .publish(MonitorEvent::Status("已退出".to_string()));
        result
    }
}
