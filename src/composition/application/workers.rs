use super::*;

impl ApplicationRuntime {
    pub(super) fn start_command_executor(&self) -> thread::JoinHandle<()> {
        let mut executor = self.clone_for_background_task();
        thread::spawn(move || {
            log::info!("命令执行线程已启动");
            if let Err(error) = executor.run_pending_command_loop() {
                log::error!("命令执行线程异常退出: {error:#}");
            }
        })
    }

    pub(super) fn start_formal_task_execution_runtime(&mut self) -> Result<()> {
        if self.formal_task_execution.is_some() {
            return Ok(());
        }
        let runtime = FormalTaskExecutionRuntime::start(|handle| {
            let mut app = self.clone_for_background_task();
            app.formal_tasks = Some(FormalTaskClient::new(handle, app.business.clone()));
            app
        })?;
        self.formal_tasks = Some(FormalTaskClient::new(
            runtime.handle(),
            self.business.clone(),
        ));
        self.formal_task_execution = Some(runtime);
        Ok(())
    }

    pub(super) fn start_deferred_chat_sender(&self) -> thread::JoinHandle<()> {
        let mut sender = self.clone_for_background_task();
        thread::spawn(move || {
            log::info!("延迟聊天发送线程已启动");
            if let Err(error) = sender.run_deferred_chat_sender_loop() {
                log::error!("延迟聊天发送线程异常退出: {error:#}");
            }
        })
    }

    fn run_deferred_chat_sender_loop(&mut self) -> Result<()> {
        let retry_delay = Duration::from_millis(self.config.timing.loop_idle_ms.max(50));
        while self.running.load(AtomicOrdering::SeqCst) {
            if self.paused.load(AtomicOrdering::SeqCst) {
                sleep(retry_delay);
                continue;
            }
            let Some((item, sending)) = self.business.take_next_deferred_chat()? else {
                sleep(retry_delay);
                continue;
            };
            if !self.running.load(AtomicOrdering::SeqCst) {
                drop(sending);
                break;
            }
            if self.paused.load(AtomicOrdering::SeqCst) {
                drop(sending);
                let _ = self.business.requeue_deferred_chat_front(item)?;
                sleep(retry_delay);
                continue;
            }

            if let DeferredChatItem::Batch(batch) = &item
                && !self
                    .business
                    .turtle_soup_delivery_is_current(batch.turtle_soup)
            {
                log::debug!(
                    "延迟聊天分段批次所属海龟汤会话已失效，跳过: {:?}",
                    batch.turtle_soup
                );
                drop(sending);
                continue;
            }

            let target = item.target();

            if !self.deferred_chat_target_is_active(target)? {
                drop(sending);
                match self.business.requeue_deferred_chat_back(item)? {
                    EnqueueOutcome::DroppedMessage => {
                        log::warn!("延迟聊天发送队列已满，已丢弃一条较早的普通回复")
                    }
                    EnqueueOutcome::Rejected => {
                        log::warn!("延迟聊天目标未激活且队列已满，当前回复已丢弃")
                    }
                    EnqueueOutcome::Added => {}
                }
                sleep(retry_delay);
                continue;
            }

            match item {
                DeferredChatItem::Message(message) => {
                    let result = match target {
                        DeferredChatTarget::Primary => self.chat_output.send(&message.text),
                        DeferredChatTarget::SecondaryCurrentHall => {
                            self.chat_output.send_current_chat(&message.text)
                        }
                        DeferredChatTarget::CurrentHall => {
                            let residency = self.active_ui_residency()?;
                            match residency {
                                UiResidency::Primary => self.chat_output.send(&message.text),
                                UiResidency::SecondaryCurrentHall => {
                                    self.chat_output.send_current_chat(&message.text)
                                }
                            }
                        }
                    };
                    drop(sending);
                    if let Err(error) = result {
                        log::error!("延迟聊天普通回复发送失败，已丢弃: {error:#}");
                    }
                }
                DeferredChatItem::Batch(mut batch) => {
                    let delivery = batch.turtle_soup;
                    let residency = match target {
                        DeferredChatTarget::Primary => UiResidency::Primary,
                        DeferredChatTarget::SecondaryCurrentHall => {
                            UiResidency::SecondaryCurrentHall
                        }
                        DeferredChatTarget::CurrentHall => self.active_ui_residency()?,
                    };
                    let messages = batch.remaining_texts();
                    let outcome = match residency {
                        UiResidency::Primary => self.chat_output.send_batch_outcome(&messages, 0),
                        UiResidency::SecondaryCurrentHall => self
                            .chat_output
                            .send_current_chat_batch_outcome(&messages, 0),
                    };
                    drop(sending);

                    let ChatBatchSendOutcome { sent, status } = outcome;
                    let all_sent = match batch.mark_sent(sent) {
                        Ok(all_sent) => all_sent,
                        Err(error) => {
                            log::error!("海龟汤批量发送进度无效: {error:#}");
                            self.business.turtle_soup_delivery_failure(delivery, &error);
                            continue;
                        }
                    };
                    if !self.running.load(AtomicOrdering::SeqCst)
                        || !self.business.turtle_soup_delivery_is_current(delivery)
                    {
                        continue;
                    }
                    if all_sent {
                        if let ChatBatchSendStatus::Failed(error) = &status {
                            log::warn!(
                                "海龟汤批次内容已完整发送，但聊天界面收尾失败，不重发内容: {error:#}"
                            );
                        }
                        self.business.turtle_soup_delivery_success(delivery);
                        continue;
                    }

                    match status {
                        ChatBatchSendStatus::Complete => {
                            let error = anyhow!(
                                "海龟汤批量发送提前完成: sent={} remaining={}",
                                sent,
                                batch.remaining_texts().len()
                            );
                            log::error!("{error:#}");
                            self.business.turtle_soup_delivery_failure(delivery, &error);
                        }
                        ChatBatchSendStatus::Failed(error) => {
                            let attempt = batch.current_attempt();
                            let max_attempts = batch.max_attempts();
                            match batch.mark_current_failed() {
                                BatchFailureOutcome::Retry => {
                                    log::warn!(
                                        "海龟汤批量发送失败，准备从首条未发送消息重试: purpose={:?} attempt={}/{} sent={} error={:#}",
                                        delivery.purpose,
                                        attempt,
                                        max_attempts,
                                        sent,
                                        error
                                    );
                                    match self.business.requeue_deferred_chat_front(
                                        DeferredChatItem::Batch(batch),
                                    )? {
                                        EnqueueOutcome::Added => {}
                                        EnqueueOutcome::DroppedMessage => {
                                            log::warn!("海龟汤批量重试入队时淘汰了一条普通回复")
                                        }
                                        EnqueueOutcome::Rejected => {
                                            let requeue_error =
                                                anyhow!("海龟汤批量重试无法重新进入延迟队列");
                                            log::error!("{requeue_error:#}");
                                            self.business.turtle_soup_delivery_failure(
                                                delivery,
                                                &requeue_error,
                                            );
                                        }
                                    }
                                    sleep(retry_delay);
                                }
                                BatchFailureOutcome::Exhausted => {
                                    log::error!(
                                        "海龟汤批量发送已耗尽当前消息重试: purpose={:?} attempts={} sent={} error={:#}",
                                        delivery.purpose,
                                        max_attempts,
                                        sent,
                                        error
                                    );
                                    self.business.turtle_soup_delivery_failure(delivery, &error);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn start_web_tool_executor(&self) -> thread::JoinHandle<()> {
        let mut worker = self.clone_for_background_task();
        thread::spawn(move || {
            log::info!("Web 工具执行线程已启动");
            worker.run_web_tool_loop();
        })
    }

    pub(super) fn start_playback_monitor(&self) -> thread::JoinHandle<()> {
        let mut monitor = self.clone_for_background_task();
        thread::spawn(move || {
            log::info!("播放监控线程已启动");
            monitor.run_playback_monitor_loop();
        })
    }

    pub(super) fn clone_for_background_task(&self) -> Self {
        Self {
            config: self.config.clone(),
            http_server: None,
            hotkeys: None,
            game_ui: self.game_ui.clone(),
            residency_ui: self.residency_ui.clone(),
            hall_ui: self.hall_ui.clone(),
            moderation_ui: self.moderation_ui.clone(),
            startup_ui: self.startup_ui.clone(),
            secondary_unread_ui: self.secondary_unread_ui.clone(),
            friend_delivery_ui: self.friend_delivery_ui.clone(),
            invite_ui: self.invite_ui.clone(),
            custom_action_ui: self.custom_action_ui.clone(),
            ui_runtime: None,
            business: self.business.clone(),
            business_events: self.business_events.clone(),
            business_runtime: None,
            formal_task_execution: None,
            formal_tasks: self.formal_tasks.clone(),
            player: self.player.clone(),
            player_search: self.player_search.clone(),
            player_runtime: None,
            openai_runtime: None,
            ai: self.ai.clone(),
            song_review: self.song_review.clone(),
            chat_output: self.chat_output.clone(),
            ocr: self.ocr.clone(),
            ocr_runtime: None,
            latest_frame: self.latest_frame.clone(),
            locks: CommandLockState::default(),
            window_detection_signal: self.window_detection_signal.clone(),
            screen_lock_primed: self.screen_lock_primed.clone(),
            reset_locks_requested: self.reset_locks_requested.clone(),
            moderation: self.moderation.clone(),
            startup: self.startup,
            custom_workflow: self.custom_workflow.clone(),
            running: self.running.clone(),
            paused: self.paused.clone(),
            console_reply_context: self.console_reply_context.clone(),
            chat_observations: self.chat_observations.clone(),
            monitor: self.monitor.clone(),
        }
    }

    pub(super) fn playback_queue(&self) -> Result<Vec<QueueItem>> {
        self.business
            .playback_queue_snapshot()
            .map_err(anyhow::Error::from)
    }

    pub(super) fn latest_frame(&self) -> Result<Arc<DynamicImage>> {
        self.latest_frame
            .lock()
            .map_err(|_| anyhow!("主扫描画面缓存锁已损坏"))?
            .clone()
            .ok_or_else(|| anyhow!("尚未获取主扫描画面，请稍后重试"))
    }

    fn run_web_tool_loop(&mut self) {
        while self.running.load(AtomicOrdering::SeqCst) {
            match self.business.take_next_diagnostic_task() {
                Ok(Some(task)) => {
                    let id = task.task_id();
                    let label = task.label().to_string();
                    let completion = match task.execute() {
                        Ok(result) => DiagnosticTaskCompletion::Succeeded(result),
                        Err(error) => {
                            log::error!("Web 工具执行失败 {label}: {error:#}");
                            DiagnosticTaskCompletion::Failed(format!("{error:#}"))
                        }
                    };
                    if let Err(error) = self.business.complete_diagnostic_task(id, completion) {
                        log::error!("Web 工具任务收尾异常: {error}");
                    }
                }
                Ok(None) => sleep(Duration::from_millis(100)),
                Err(error) => {
                    log::error!("Web 工具任务调度异常: {error:#}");
                    sleep(Duration::from_millis(250));
                }
            }
        }
    }
}
