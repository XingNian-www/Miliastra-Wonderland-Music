use super::*;

use crate::features::administration::{
    AdministrationCommandContext, AdministrationDispatch, ImmediateAdministrationOutcome,
};
use crate::features::turtle_soup::{
    QuestionSubmitOutcome, TurtleSoupApplicationPort, TurtleSoupCommandOutcome,
};

struct TurtleSoupCommandPort {
    business: BusinessRuntimeHandle,
}

impl TurtleSoupApplicationPort for TurtleSoupCommandPort {
    fn handle_hall_command(
        &mut self,
        player: &str,
        command: &turtle_soup::TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome> {
        self.business
            .handle_turtle_soup_hall_command(player, command)
            .map_err(anyhow::Error::from)
    }

    fn handle_friend_command(
        &mut self,
        player: &str,
        command: &turtle_soup::TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome> {
        self.business
            .handle_turtle_soup_friend_command(player, command)
            .map_err(anyhow::Error::from)
    }

    fn submit_question(
        &mut self,
        question: turtle_soup::TurtleSoupQuestion,
    ) -> Result<QuestionSubmitOutcome> {
        self.business
            .submit_turtle_soup_question(question)
            .map_err(anyhow::Error::from)
    }

    fn send_current_hall(&mut self, message: &str) -> Result<()> {
        enqueue_current_hall_reply(&self.business, message)
    }
}

impl ApplicationRuntime {
    fn turtle_soup_command_port(&self) -> TurtleSoupCommandPort {
        TurtleSoupCommandPort {
            business: self.business.clone(),
        }
    }

    pub(super) fn clear_hall_countdown_cache_for_new_visual_session(
        &self,
        reason: &str,
    ) -> Result<bool> {
        let cleared = self.business.clear_hall_countdown_cache()?;
        let visual_session = self.chat_observations.begin_visual_session()?;
        if cleared {
            log::info!("{reason}，已清理大厅倒计时缓存，等待本次大厅检测重新确认");
        }
        log::info!("{reason}，聊天观察进入新视觉会话: {}", visual_session.get());
        Ok(cleared)
    }

    pub(super) fn scan_chat_with_shared_ocr(
        &self,
        image: &DynamicImage,
        templates: &ResolvedTemplateArgs,
    ) -> Result<Vec<ChatMessage>> {
        scan_chat_with_shared_ocr(
            &self.ocr,
            &self.monitor,
            self.config.screen.chat_rect.into(),
            image,
            templates,
        )
    }

    pub(super) fn warn_if_screen_size_mismatch(&self) -> Result<()> {
        let frame = match self.game_ui.capture() {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!("启动时未能截图，扫描循环将等待目标窗口恢复: {error:#}");
                return Ok(());
            }
        };
        if self.config.screen.warn_on_size_mismatch
            && (frame.width() != self.config.screen.expected_width
                || frame.height() != self.config.screen.expected_height)
        {
            log::warn!(
                "截图尺寸为 {}x{}，预期 {}x{}，程序继续运行",
                frame.width(),
                frame.height(),
                self.config.screen.expected_width,
                self.config.screen.expected_height
            );
        }
        Ok(())
    }

    pub(super) fn start_http_server(&mut self) -> Result<()> {
        if !self.config.http.enabled {
            return Ok(());
        }
        if self.http_server.is_some() {
            return Err(anyhow!("HTTP/Web 面板已经启动"));
        }
        let player_runtime = self
            .player_runtime
            .as_ref()
            .ok_or_else(|| anyhow!("播放器运行时尚未启动"))?
            .handle();
        let formal_tasks = Arc::new(
            self.formal_tasks
                .clone()
                .ok_or_else(|| anyhow!("正式任务执行运行时尚未启动"))?,
        );
        let server = http::start(http::HttpSharedState::new(
            http::HttpInterfaceConfig::new(
                self.config.http.clone(),
                self.config.screen.clone(),
                self.config.templates.clone(),
                self.config.moderation.clone(),
                self.config.startup.clone(),
                self.config.invite.clone(),
                self.config.timing.clone(),
                self.config.custom_workflows.clone(),
            ),
            self.custom_workflow.clone(),
            formal_tasks.clone(),
            formal_tasks,
            self.monitor.clone(),
            self.latest_frame.clone(),
            self.player_search.clone(),
            player_runtime,
            self.ai.clone(),
        ))?;
        self.http_server = Some(server);
        Ok(())
    }

    pub(super) fn start_hotkeys(&self) -> Result<hotkeys::HotkeyRuntime> {
        hotkeys::start(
            &self.config.hotkeys,
            Arc::clone(&self.running),
            Arc::clone(&self.paused),
        )
    }

    pub(super) fn run_scan_loop(&mut self) -> Result<()> {
        let mut completion_subscriber = self
            .chat_observations
            .subscribe_completion_advances()
            .context("订阅聊天观察完成推进")?;
        let scan_result = self.run_scan_loop_inner(&mut completion_subscriber);
        let final_forward_result = self.forward_completion_advances(&mut completion_subscriber);
        match scan_result {
            Err(error) => {
                if let Err(forward_error) = final_forward_result {
                    log::error!("扫描循环退出时转发观察完成推进失败: {forward_error:#}");
                }
                Err(error)
            }
            Ok(()) => final_forward_result,
        }
    }

    fn run_scan_loop_inner(
        &mut self,
        completion_subscriber: &mut CompletionAdvanceSubscriber,
    ) -> Result<()> {
        let template_args = self.chat_templates.clone();
        let ui_template_args = self.ui_templates.clone();
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let ui_handle = self
            .ui_runtime
            .as_ref()
            .context("UI runtime 在扫描循环启动前已停止")?
            .handle();
        let frame_demand = FrameDemand::new(Duration::from_millis(
            self.config.timing.loop_idle_ms.max(1),
        ))
        .context("创建聊天观察帧需求")?;
        let mut frame_subscription: Option<FrameDemandSubscription> = None;
        let mut last_fingerprint: Option<ChangeFingerprint> = None;
        let mut last_ocr_at =
            Instant::now() - Duration::from_millis(self.config.timing.chat_scan.fallback_ms);
        let mut last_change_ocr_at =
            Instant::now() - Duration::from_millis(self.config.timing.chat_scan.change_cooldown_ms);
        let mut suppress_change_until = Instant::now();
        let mut force_scan_after: Option<Instant> = None;
        let mut force_scan_reason: Option<&'static str> = None;
        let mut primary_visible = false;
        let mut secondary_friend_bubble_fingerprint: Option<ChangeFingerprint> = None;
        let mut secondary_hall_bubble_sequence: Vec<SecondaryHallBubble> = Vec::new();
        let mut secondary_title_fingerprint: Option<ChangeFingerprint> = None;
        let mut secondary_identity: Option<SecondaryChatIdentity> = None;
        let mut target_missing_backoff = TARGET_MISSING_BACKOFF_INITIAL;
        let mut target_missing = false;

        log::info!("自动化扫描已启动");
        while self.running.load(AtomicOrdering::SeqCst) {
            self.forward_completion_advances(completion_subscriber)?;
            let loop_started = Instant::now();
            self.update_monitor_operational_state();
            self.tick_entertainment();
            if self.paused.load(AtomicOrdering::SeqCst) {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!("暂停监听时撤销观察帧需求失败: {error}");
                }
                self.maybe_idle_exit()?;
                sleep(Duration::from_millis(self.config.timing.loop_idle_ms));
                continue;
            }

            if frame_subscription.is_none() {
                frame_subscription = Some(
                    ui_handle
                        .declare_frame_demand(frame_demand)
                        .context("向 UI runtime 声明聊天观察帧需求")?,
                );
            }

            let frame_started = Instant::now();
            match receive_observation_frame(
                frame_subscription
                    .as_ref()
                    .expect("frame subscription initialized above"),
                &ui_handle,
                &canvas,
            ) {
                Ok(frame) => {
                    if let Ok(mut latest_frame) = self.latest_frame.lock() {
                        *latest_frame = Some(Arc::clone(&frame.image));
                    } else {
                        log::error!("主扫描画面缓存锁已损坏");
                    }
                    let frame_ms = elapsed_ms(frame_started);
                    log::debug!(target: "timing",
                        "观察帧交付: wait={}ms age={}ms",
                        frame_ms,
                        frame.captured_at.elapsed().as_millis()
                    );
                    if target_missing {
                        log::info!("目标窗口已恢复，重置截图退避");
                        self.clear_hall_countdown_cache_for_new_visual_session("目标窗口恢复")?;
                        target_missing = false;
                    }
                    target_missing_backoff = TARGET_MISSING_BACKOFF_INITIAL;
                    let ui_started = Instant::now();
                    let ui_state_result =
                        detect_ui_state(&frame.image, &ui_template_args, &self.config.screen);
                    match &ui_state_result {
                        Ok(ui_state) => self
                            .monitor
                            .publish(MonitorEvent::UiState(ui_state.to_string())),
                        Err(_) => self
                            .monitor
                            .publish(MonitorEvent::UiState("界面检测失败".to_string())),
                    }
                    let ui_ms = elapsed_ms(ui_started);
                    let listener_snapshot = self.business.chat_listener_snapshot()?;
                    let command_executing = self.business.scheduler_snapshot()?.is_busy();
                    match ui_state_result {
                        Ok(ui_state)
                            if listener_snapshot.mode == ChatListenerMode::Secondary
                                && !listener_snapshot.temporary_primary =>
                        {
                            primary_visible = false;
                            last_fingerprint = None;
                            let secondary_started = Instant::now();
                            let scanned = if ui_state.is_secondary() {
                                self.run_secondary_listener_round(
                                    &frame.image,
                                    &mut secondary_friend_bubble_fingerprint,
                                    &mut secondary_hall_bubble_sequence,
                                    &mut secondary_title_fingerprint,
                                    &mut secondary_identity,
                                )?
                            } else if command_executing {
                                log::debug!(
                                    "二级监听任务临时离开二级界面，等待任务状态机恢复: {}",
                                    ui_state
                                );
                                false
                            } else {
                                log::warn!(
                                    "二级监听当前不在二级聊天界面: {}，回退一级监听",
                                    ui_state
                                );
                                self.business.fail_chat_listener_mode_to_primary()?;
                                secondary_friend_bubble_fingerprint = None;
                                secondary_hall_bubble_sequence.clear();
                                secondary_title_fingerprint = None;
                                secondary_identity = None;
                                false
                            };
                            log::info!(target: "timing",
                                "主循环阶段耗时: total={}ms frame={}ms ui={}ms secondary={}ms state={} scanned={}",
                                elapsed_ms(loop_started),
                                frame_ms,
                                ui_ms,
                                elapsed_ms(secondary_started),
                                ui_state,
                                scanned
                            );
                        }
                        Ok(ui_state) if ui_state.is_primary() => {
                            if listener_snapshot.mode == ChatListenerMode::Primary {
                                secondary_friend_bubble_fingerprint = None;
                                secondary_hall_bubble_sequence.clear();
                                secondary_title_fingerprint = None;
                                secondary_identity = None;
                            }
                            let primary_started = Instant::now();
                            let entered_primary = !primary_visible;
                            primary_visible = true;
                            let fingerprint = match rect_chat_change_fingerprint(
                                &frame.image,
                                self.config.screen.chat_rect.into(),
                            ) {
                                Ok(fingerprint) => Some(fingerprint),
                                Err(error) => {
                                    log::error!("聊天区变化指纹失败: {error:#}");
                                    None
                                }
                            };
                            let now = Instant::now();
                            if entered_primary && let Some(fingerprint) = fingerprint.clone() {
                                last_fingerprint = Some(fingerprint);
                                let scan_after = now
                                    + Duration::from_millis(
                                        self.config.timing.chat_scan.change_debounce_ms,
                                    );
                                if force_scan_after.is_none_or(|time| scan_after < time) {
                                    force_scan_after = Some(scan_after);
                                    force_scan_reason = Some("enter-primary");
                                }
                                log::info!(target: "timing",
                                    "进入一级界面，已建立聊天区对比基线，快速扫描延迟={}ms",
                                    self.config.timing.chat_scan.change_debounce_ms
                                );
                            }
                            let change_suppressed = now < suppress_change_until;
                            let forced_scan_due = force_scan_after.is_some_and(|time| now >= time);
                            let cooldown_until = last_change_ocr_at
                                + Duration::from_millis(
                                    self.config.timing.chat_scan.change_cooldown_ms,
                                );
                            let change_stats = fingerprint.as_ref().and_then(|current| {
                                last_fingerprint
                                    .as_ref()
                                    .map(|previous| change_stats(previous, current))
                            });
                            let change_over_threshold = change_stats.is_some_and(|stats| {
                                stats.mean_abs_diff >= self.config.ocr.change_mean_threshold
                                    || stats.changed_ratio >= self.config.ocr.change_pixel_threshold
                            });
                            let change_ready = !change_suppressed && now >= cooldown_until;
                            let mut keep_previous_fingerprint = false;
                            if change_over_threshold && !change_ready && !forced_scan_due {
                                let scan_after = if change_suppressed {
                                    suppress_change_until
                                } else {
                                    cooldown_until
                                };
                                if force_scan_after.is_none_or(|time| scan_after < time) {
                                    force_scan_after = Some(scan_after);
                                    force_scan_reason = Some("delayed-change");
                                }
                                keep_previous_fingerprint = true;
                            }
                            let fallback_due = !change_suppressed
                                && (forced_scan_due
                                    || now.duration_since(last_ocr_at)
                                        >= Duration::from_millis(
                                            self.config.timing.chat_scan.fallback_ms,
                                        ));
                            let change_due = change_over_threshold && change_ready;

                            let mut scanned_this_round = false;
                            if change_due {
                                let stats = change_stats.expect("change_due requires stats");
                                log::info!(target: "timing",
                                    "触发聊天扫描: reason=change mean={:.3} ratio={:.5} debounce={}ms",
                                    stats.mean_abs_diff,
                                    stats.changed_ratio,
                                    self.config.timing.chat_scan.change_debounce_ms
                                );
                                sleep(Duration::from_millis(
                                    self.config.timing.chat_scan.change_debounce_ms,
                                ));
                                let rescan_frame_started = Instant::now();
                                match receive_observation_frame(
                                    frame_subscription
                                        .as_ref()
                                        .expect("frame subscription initialized above"),
                                    &ui_handle,
                                    &canvas,
                                ) {
                                    Ok(frame) => {
                                        let rescan_frame_ms = elapsed_ms(rescan_frame_started);
                                        let scan_started = Instant::now();
                                        let observation_frame = self
                                            .chat_observations
                                            .begin_frame(frame.captured_at)?;
                                        let messages = self.scan_chat_with_shared_ocr(
                                            &frame.image,
                                            &template_args,
                                        );
                                        let scan_ms = elapsed_ms(scan_started);
                                        log::info!(target: "timing",
                                            "变化扫描阶段耗时: rescan_frame={}ms scan={}ms",
                                            rescan_frame_ms,
                                            scan_ms
                                        );
                                        match messages {
                                            Ok(messages) => self.publish_primary_chat_observation(
                                                observation_frame,
                                                messages,
                                            )?,
                                            Err(error) => {
                                                log::error!("聊天扫描失败: {error:#}");
                                                if let Err(record_error) =
                                                    self.chat_observations.record_terminal_failure(
                                                        observation_frame,
                                                        format!("{error:#}"),
                                                    )
                                                {
                                                    log::error!(
                                                        "记录聊天观察终止失败异常: {record_error:#}"
                                                    );
                                                }
                                            }
                                        }
                                        last_ocr_at = Instant::now();
                                        last_change_ocr_at = last_ocr_at;
                                        force_scan_after = None;
                                        force_scan_reason = None;
                                        last_fingerprint = rect_chat_change_fingerprint(
                                            &frame.image,
                                            self.config.screen.chat_rect.into(),
                                        )
                                        .ok();
                                        scanned_this_round = true;
                                    }
                                    Err(error) => log::error!("变化后截图失败: {error:#}"),
                                }
                            } else if fallback_due {
                                let reason = if forced_scan_due {
                                    force_scan_reason.unwrap_or("forced")
                                } else {
                                    "poll"
                                };
                                log::info!(target: "timing",
                                    "触发聊天扫描: reason={} since_last={}ms",
                                    reason,
                                    now.duration_since(last_ocr_at).as_millis()
                                );
                                let observation_frame =
                                    self.chat_observations.begin_frame(frame.captured_at)?;
                                let messages =
                                    self.scan_chat_with_shared_ocr(&frame.image, &template_args);
                                match messages {
                                    Ok(messages) => self.publish_primary_chat_observation(
                                        observation_frame,
                                        messages,
                                    )?,
                                    Err(error) => {
                                        log::error!("聊天扫描失败: {error:#}");
                                        if let Err(record_error) =
                                            self.chat_observations.record_terminal_failure(
                                                observation_frame,
                                                format!("{error:#}"),
                                            )
                                        {
                                            log::error!(
                                                "记录聊天观察终止失败异常: {record_error:#}"
                                            );
                                        }
                                    }
                                }
                                last_ocr_at = now;
                                force_scan_after = None;
                                force_scan_reason = None;
                                last_fingerprint = fingerprint.clone();
                                scanned_this_round = true;
                            }
                            let primary_ms = elapsed_ms(primary_started);
                            let loop_ms = elapsed_ms(loop_started);
                            if scanned_this_round || loop_ms >= 80 {
                                log::info!(target: "timing",
                                    "主循环阶段耗时: total={}ms frame={}ms ui={}ms primary={}ms state=primary scanned={}",
                                    loop_ms,
                                    frame_ms,
                                    ui_ms,
                                    primary_ms,
                                    scanned_this_round
                                );
                            } else {
                                log::info!(target: "timing",
                                    "主循环阶段耗时: total={}ms frame={}ms ui={}ms primary={}ms state=primary scanned=false",
                                    loop_ms,
                                    frame_ms,
                                    ui_ms,
                                    primary_ms
                                );
                            }

                            if change_suppressed {
                                last_fingerprint = None;
                            } else if !scanned_this_round
                                && !keep_previous_fingerprint
                                && last_fingerprint.is_none()
                            {
                                // 不要每帧滚动更新基线，慢速聊天动画会在超过阈值前被吃掉。
                                if let Some(fingerprint) = fingerprint {
                                    last_fingerprint = Some(fingerprint);
                                }
                            }
                        }
                        Ok(ui_state) => {
                            primary_visible = false;
                            secondary_friend_bubble_fingerprint = None;
                            secondary_hall_bubble_sequence.clear();
                            secondary_title_fingerprint = None;
                            secondary_identity = None;
                            log::debug!("当前不是一级聊天界面，跳过聊天扫描: {}", ui_state);
                            log::info!(target: "timing",
                                "主循环阶段耗时: total={}ms frame={}ms ui={}ms state={} scanned=false",
                                elapsed_ms(loop_started),
                                frame_ms,
                                ui_ms,
                                ui_state
                            );
                            last_fingerprint = None;
                        }
                        Err(error) => {
                            primary_visible = false;
                            log::error!("界面状态检测失败: {error:#}");
                            log::info!(target: "timing",
                                "主循环阶段耗时: total={}ms frame={}ms ui={}ms state=ui_error scanned=false",
                                elapsed_ms(loop_started),
                                frame_ms,
                                ui_ms
                            );
                        }
                    }
                }
                Err(error) => {
                    if let Some(subscription) = frame_subscription.take()
                        && let Err(cancel_error) = subscription.cancel()
                    {
                        log::warn!("截图失败后撤销观察帧需求失败: {cancel_error}");
                    }
                    let frame_ms = elapsed_ms(frame_started);
                    if !target_missing {
                        self.abort_entertainment_for_context_loss("目标游戏窗口已关闭或不可用");
                    }
                    self.monitor
                        .publish(MonitorEvent::UiState("目标窗口不可用".to_string()));
                    primary_visible = false;
                    last_fingerprint = None;
                    secondary_friend_bubble_fingerprint = None;
                    secondary_hall_bubble_sequence.clear();
                    secondary_title_fingerprint = None;
                    secondary_identity = None;
                    let observed_window_detection_generation =
                        self.window_detection_signal.generation()?;
                    log::warn!(
                        "截图失败，{}秒后重试: {error:#}",
                        target_missing_backoff.as_secs()
                    );
                    log::info!(target: "timing",
                        "主循环阶段耗时: total={}ms frame={}ms state=capture_error retry={}ms",
                        elapsed_ms(loop_started),
                        frame_ms,
                        target_missing_backoff.as_millis()
                    );
                    target_missing = true;
                    self.maybe_idle_exit()?;
                    if self.window_detection_signal.wait_for_change(
                        observed_window_detection_generation,
                        target_missing_backoff,
                    )? {
                        log::info!("收到窗口检测重置请求，立即重试并重置截图退避");
                        target_missing_backoff = TARGET_MISSING_BACKOFF_INITIAL;
                    } else {
                        target_missing_backoff =
                            next_target_missing_backoff(target_missing_backoff);
                    }
                    continue;
                }
            }
            if primary_visible && self.maybe_warn_hall_expiring()? {
                suppress_change_until = Instant::now()
                    + Duration::from_millis(self.config.timing.command.post_settle_ms);
                force_scan_after = Some(suppress_change_until);
                force_scan_reason = Some("hall-expiring");
                last_fingerprint = None;
                last_ocr_at = Instant::now();
            }
            self.forward_completion_advances(completion_subscriber)?;
            self.maybe_idle_exit()?;
            sleep(Duration::from_millis(self.config.timing.loop_idle_ms));
        }

        if let Some(subscription) = frame_subscription
            && let Err(error) = subscription.cancel()
        {
            log::warn!("扫描循环结束时撤销观察帧需求失败: {error}");
        }

        Ok(())
    }

    fn forward_completion_advances(
        &self,
        subscriber: &mut CompletionAdvanceSubscriber,
    ) -> Result<()> {
        loop {
            match self.chat_observations.read_completion_advance(subscriber)? {
                Some(ObservationRead::Item { value, .. }) => self
                    .business_events
                    .submit(BusinessEvent::CompletionAdvance(Arc::unwrap_or_clone(
                        value,
                    )))
                    .context("向业务运行时提交观察完成推进")?,
                Some(ObservationRead::Gap(gap)) => self
                    .business_events
                    .submit(BusinessEvent::CompletionGap(gap))
                    .context("向业务运行时提交观察完成流缺口")?,
                None => return Ok(()),
            }
        }
    }

    fn publish_primary_chat_observation(
        &mut self,
        frame: ObservedFrame,
        messages: Vec<ChatMessage>,
    ) -> Result<()> {
        let dispatches = self.chat_observations.publish_primary(frame, messages)?;
        self.dispatch_chat_observations(dispatches)?;
        Ok(())
    }

    pub(super) fn dispatch_chat_observations(
        &mut self,
        dispatches: Vec<ChatObservationDispatch>,
    ) -> Result<bool> {
        let mut processed_secondary = false;
        for dispatch in dispatches {
            match dispatch {
                ChatObservationDispatch::Primary { frame, messages } => {
                    let messages = messages.into_iter().collect::<Vec<_>>();
                    self.handle_scan_messages(frame, messages)?;
                }
                ChatObservationDispatch::Secondary { frame, observation } => {
                    processed_secondary |=
                        self.process_secondary_chat_observation(frame, observation)?;
                }
                ChatObservationDispatch::Gap(gap) => {
                    self.locks = CommandLockState::default();
                    self.screen_lock_primed.store(false, AtomicOrdering::SeqCst);
                    log::warn!(
                        "一级聊天观察出现缺口，下一屏仅重建命令基线: kind={:?} missing={:?}..={:?}",
                        gap.kind,
                        gap.missing_from,
                        gap.missing_through
                    );
                }
            }
        }
        Ok(processed_secondary)
    }

    fn handle_scan_messages(
        &mut self,
        frame: ObservedFrame,
        observed_messages: Vec<PrimaryObservedMessage>,
    ) -> Result<()> {
        let messages = observed_messages
            .iter()
            .map(|observed| {
                log::debug!("处理一级观察消息: id={:?}", observed.id);
                &observed.message
            })
            .collect::<Vec<_>>();
        if self
            .reset_locks_requested
            .swap(false, AtomicOrdering::SeqCst)
        {
            self.locks = CommandLockState::default();
            log::info!("已重置命令屏幕锁");
        }
        let active_entertainment = self.business.active_entertainment()?;
        let command_router = ChatCommandRouter::new(&self.custom_workflow);
        let visible_turtle_questions = if self.business.turtle_soup_accepts_questions()? {
            messages
                .iter()
                .filter(|message| message.message_type == "blue" && !message.text.is_empty())
                .filter(|message| {
                    command::parse_command_envelope(
                        &message.text,
                        &message.message_type,
                        CommandObservation::default(),
                    )
                    .filter(|envelope| envelope.prefix() == CommandPrefix::Hash)
                    .and_then(|envelope| command_router.route(&envelope, active_entertainment))
                    .is_none()
                })
                .filter_map(|message| turtle_soup::parse_question_message(&message.text, None))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let suppress_new_turtle_questions = !self.screen_lock_primed.load(AtomicOrdering::SeqCst);
        let new_turtle_questions = self.business.filter_turtle_soup_primary_questions(
            visible_turtle_questions,
            suppress_new_turtle_questions,
        )?;
        if messages.is_empty() {
            log::debug!("没有找到聊天标志，本轮不更新命令锁");
            return Ok(());
        }

        let mut parsed = Vec::new();
        for (observed, message) in observed_messages.iter().zip(messages) {
            if message.text.is_empty() {
                continue;
            }
            log::debug!(
                "识别文本: [{}] {}",
                message.message_type,
                redacted_chat_text(&message.text)
            );
            let observation = CommandObservation {
                frame_id: Some(frame.id()),
                captured_at: Some(frame.captured_at()),
                message_id: Some(observed.id.clone()),
            };
            let Some(envelope) =
                command::parse_command_envelope(&message.text, &message.message_type, observation)
            else {
                continue;
            };
            let Some(parsed_command) = command_router.route(&envelope, active_entertainment) else {
                continue;
            };
            if !self.commands_enabled()? && message.message_type != "pink" {
                log::info!("命令识别已禁用，跳过: {}", parsed_command.raw);
                continue;
            }
            if let ModuleCommand::Invite(invite) = &parsed_command.command
                && !self.business.invite_should_accept(invite.seq)?
            {
                let seq = invite.seq.expect("unsequenced invites are always accepted");
                log::info!("邀请参数 {} 已执行过，跳过: {}", seq, parsed_command.raw);
                continue;
            }
            if parsed
                .iter()
                .any(|existing| command::same_lock_command(existing, &parsed_command))
            {
                log::info!("同轮重复识别命令，已合并: {}", parsed_command.raw);
                continue;
            }
            log::debug!("解析命令: {}", parsed_command.raw);
            parsed.push(parsed_command);
        }

        let update = self
            .locks
            .update(&parsed, self.business.scheduler_snapshot()?.is_busy());
        for command in update.unlocked {
            log::info!("解锁: {}", command);
        }
        for command in update.skipped {
            log::info!("命令仍在屏幕内，本轮跳过: {}", command);
        }
        if !self.screen_lock_primed.swap(true, AtomicOrdering::SeqCst) {
            for question in new_turtle_questions {
                log::info!(
                    "启动屏幕锁已记录当前可见海龟汤提问，不执行: nickname={}",
                    question.player
                );
            }
            for pending in update.accepted {
                log::info!(
                    "启动屏幕锁已记录当前可见命令，不执行: {}",
                    pending.routed.raw
                );
            }
            return Ok(());
        }
        if self.commands_enabled()? {
            for question in new_turtle_questions {
                self.handle_turtle_soup_question(question)?;
            }
        }
        for pending in update.accepted {
            if self.handle_turtle_soup_command(&pending.routed)? {
                continue;
            }
            if self.handle_idiom_chain_command(&pending.routed)? {
                continue;
            }
            if self.handle_landlord_command(&pending.routed)? {
                continue;
            }
            if self.enqueue_chat_listener_command(&pending.routed)? {
                continue;
            }
            if self.pending_contains_command(&pending.routed)? {
                log::info!("命令已在待处理队列，本轮跳过: {}", pending.routed.raw);
                continue;
            }
            if self.apply_immediate_administration(&pending.routed, false)? {
                continue;
            }
            log::info!("命令已加入待处理队列: {}", pending.routed.raw);
            self.record_command_activity()?;
            self.push_pending_task(PendingTask::Command(Box::new(pending)))?;
        }
        Ok(())
    }

    fn enqueue_chat_listener_command(&self, parsed: &RoutedCommand) -> Result<bool> {
        let ModuleCommand::Administration(command) = &parsed.command else {
            return Ok(false);
        };
        let AdministrationDispatch::ChatListenerMode(command) = command.dispatch() else {
            return Ok(false);
        };
        match command {
            ChatListenerModeCommand::Status => {
                let snapshot = self.business.chat_listener_snapshot()?;
                let pending = snapshot
                    .pending_mode
                    .map(|mode| format!("，等待切换{}", mode.label()))
                    .unwrap_or_default();
                let message = format!("监听模式状态: {}{}", snapshot.mode.label(), pending);
                log::info!("{}", message);
                self.monitor.publish(MonitorEvent::Command(format!(
                    "{} -> {}",
                    parsed.user_command, message
                )));
            }
            ChatListenerModeCommand::Primary | ChatListenerModeCommand::Secondary => {
                let target = match command {
                    ChatListenerModeCommand::Primary => ChatListenerMode::Primary,
                    ChatListenerModeCommand::Secondary => ChatListenerMode::Secondary,
                    ChatListenerModeCommand::Status => unreachable!(),
                };
                if !self.business.request_chat_listener_mode(target)? {
                    let snapshot = self.business.chat_listener_snapshot()?;
                    log::info!(
                        "监听模式切换已处于当前或等待状态，跳过: current={} pending={:?}",
                        snapshot.mode.label(),
                        snapshot.pending_mode
                    );
                    return Ok(true);
                }
                self.record_command_activity()?;
                if let Err(error) =
                    self.push_pending_task(PendingTask::SetChatListenerMode { target })
                {
                    self.business.cancel_chat_listener_mode_request(target)?;
                    return Err(error);
                }
                log::info!("监听模式切换已加入待处理队列: {}", target.label());
            }
        }
        Ok(true)
    }

    pub(super) fn handle_idiom_chain_command(&self, parsed: &RoutedCommand) -> Result<bool> {
        let ModuleCommand::IdiomChain(command) = &parsed.command else {
            return Ok(false);
        };
        if command.requires_executor() {
            return Ok(false);
        }
        let mut port = self.deferred_idiom_chain_port();
        self.idiom_chain_application.execute_deferred(
            &parsed.raw,
            &parsed.username,
            command,
            &mut port,
        )?;
        Ok(true)
    }

    fn handle_landlord_command(&self, parsed: &RoutedCommand) -> Result<bool> {
        let ModuleCommand::CardGame(command) = &parsed.command else {
            return Ok(false);
        };
        if command.requires_executor() {
            return Ok(false);
        }
        self.card_games.execute_command(
            &parsed.username,
            command,
            Instant::now(),
            CardGameEffectLane::Deferred,
            &DeferredCardGamePort { app: self },
        )?;
        log::info!(
            "牌局命令已处理: command={} user={}",
            parsed.raw,
            parsed.username
        );
        Ok(true)
    }

    pub(super) fn handle_turtle_soup_command(&self, parsed: &RoutedCommand) -> Result<bool> {
        let ModuleCommand::TurtleSoup(command) = &parsed.command else {
            return Ok(false);
        };
        let mut port = self.turtle_soup_command_port();
        self.turtle_soup_application.execute_command(
            &parsed.raw,
            &parsed.username,
            parsed.message_type == "pink",
            command,
            &mut port,
        )?;
        Ok(true)
    }

    pub(super) fn handle_turtle_soup_question(
        &self,
        question: turtle_soup::TurtleSoupQuestion,
    ) -> Result<bool> {
        let mut port = self.turtle_soup_command_port();
        self.turtle_soup_application
            .submit_question(question, &mut port)
    }

    pub(super) fn enqueue_current_hall_reply(&self, text: &str) -> Result<()> {
        enqueue_current_hall_reply(&self.business, text)
    }

    pub(super) fn abort_entertainment_for_context_loss(&self, reason: &str) {
        if let Err(error) = self.business.abort_turtle_soup(reason) {
            log::error!("无法中止海龟汤会话: {error:#}");
        }
        match self.undercover_game.abort() {
            Ok(true) => log::warn!("谁是卧底已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧谁是卧底牌局: {error:#}"),
        }
        match self.card_games.abort() {
            Ok(true) => log::warn!("牌局已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧牌局: {error:#}"),
        }
        match self.business.abort_idiom_chain() {
            Ok(true) => log::warn!("成语接龙已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧成语接龙会话: {error:#}"),
        }
    }

    fn tick_entertainment(&self) {
        let scheduler_idle = match self.business.scheduler_snapshot() {
            Ok(snapshot) => snapshot.is_idle(),
            Err(error) => {
                log::error!("无法读取业务调度状态，娱乐计时保持暂停: {error}");
                false
            }
        };
        let clock_active = !self.paused.load(AtomicOrdering::SeqCst) && scheduler_idle;
        if let Err(error) = self
            .business
            .refresh_turtle_soup_deadline(Instant::now(), clock_active)
        {
            log::error!("无法同步海龟汤期限: {error:#}");
        }
        let card_game_outcome = match self
            .card_games
            .poll_timed_outcome(Instant::now(), clock_active)
        {
            Ok(outcome) => outcome,
            Err(error) => {
                log::error!("无法推进牌局回合计时: {error:#}");
                None
            }
        };
        if let Some(outcome) = card_game_outcome {
            let key = outcome.key();
            let effect = self.card_games.timed_effect(outcome);
            if let Err(error) = self.push_pending_task(PendingTask::CardGameEffect(effect)) {
                log::error!("牌局计时结果入队失败: {error:#}");
                if let Err(cancel_error) = self.card_games.cancel_effect(key) {
                    log::error!("牌局计时结果入队失败后无法清理牌局: {cancel_error:#}");
                }
            }
        }
        let undercover_outcome = match self
            .undercover_game
            .poll_timed_outcome(Instant::now(), clock_active)
        {
            Ok(outcome) => outcome,
            Err(error) => {
                log::error!("无法推进谁是卧底计时: {error:#}");
                None
            }
        };
        if let Some(outcome) = undercover_outcome {
            let key = outcome.key();
            let effect = self.undercover_game.timed_effect(outcome);
            if let Err(error) = self.push_pending_task(PendingTask::UndercoverEffect(effect)) {
                log::error!("谁是卧底计时消息入队失败: {error:#}");
                if let Err(cancel_error) = self.undercover_game.cancel_effect(key) {
                    log::error!("谁是卧底计时消息入队失败后无法清理牌局: {cancel_error:#}");
                }
            }
        }
    }

    pub(super) fn submit_secondary_command(&self, parsed: RoutedCommand) -> Result<()> {
        if self.enqueue_chat_listener_command(&parsed)? {
            return Ok(());
        }
        if !self.commands_enabled()? && parsed.message_type != "pink" {
            log::info!("命令识别已禁用，跳过二级大厅命令: {}", parsed.raw);
            return Ok(());
        }
        if self.handle_turtle_soup_command(&parsed)? {
            return Ok(());
        }
        if self.handle_idiom_chain_command(&parsed)? {
            return Ok(());
        }
        if self.handle_landlord_command(&parsed)? {
            return Ok(());
        }
        if let ModuleCommand::Invite(invite) = &parsed.command
            && !self.business.invite_should_accept(invite.seq)?
        {
            let seq = invite.seq.expect("unsequenced invites are always accepted");
            log::info!("邀请参数 {} 已执行过，跳过: {}", seq, parsed.raw);
            return Ok(());
        }
        if self.pending_contains_command(&parsed)? {
            log::info!("二级监听命令已在待处理队列，跳过: {}", parsed.raw);
            return Ok(());
        }
        if self.apply_immediate_administration(&parsed, true)? {
            return Ok(());
        }
        self.record_command_activity()?;
        log::info!("二级监听命令已加入待处理队列: {}", parsed.raw);
        self.push_pending_task(PendingTask::Command(Box::new(PendingCommand {
            lock_key: command::lock_key(&parsed),
            routed: parsed,
        })))
    }

    fn apply_immediate_administration(
        &self,
        parsed: &RoutedCommand,
        propagate_log_error: bool,
    ) -> Result<bool> {
        let ModuleCommand::Administration(command) = &parsed.command else {
            return Ok(false);
        };
        let context = AdministrationCommandContext {
            message_type: parsed.message_type.clone(),
            username: command_username(parsed).to_string(),
            user_command: parsed.user_command.clone(),
        };
        let mut port = self.immediate_administration_port();
        Ok(matches!(
            self.administration_application.apply_immediate(
                &context,
                command,
                propagate_log_error,
                &mut port,
            )?,
            ImmediateAdministrationOutcome::Handled
        ))
    }

    pub(super) fn execute_console_chat_task(&mut self, text: String, prefix: String) -> Result<()> {
        let message = format!("{}{}", prefix, text);
        self.reply(&message)
    }

    pub(super) fn execute_set_chat_listener_mode(
        &mut self,
        target: ChatListenerMode,
    ) -> Result<()> {
        self.abort_entertainment_for_context_loss("聊天监听模式即将切换");
        let residency = match target {
            ChatListenerMode::Primary => UiResidency::Primary,
            ChatListenerMode::Secondary => UiResidency::SecondaryCurrentHall,
        };
        if self
            .establish_ui_residency(residency, ResidencyPurpose::ListenerModeSwitch)
            .is_ok()
        {
            self.business.complete_chat_listener_mode(target)?;
            log::info!("聊天监听模式已切换为{}", target.label());
            return Ok(());
        }

        self.business.fail_chat_listener_mode_to_primary()?;
        let _ = self.establish_ui_residency(
            UiResidency::Primary,
            ResidencyPurpose::IndependentRecovery("监听切换失败回退一级"),
        );
        Err(anyhow!("切换{}失败，已回退一级监听", target.label()))
    }
}

pub(super) fn scan_chat_with_shared_ocr(
    ocr: &OcrRuntimeHandle,
    monitor: &MonitorShared,
    chat_rect: Rect,
    image: &DynamicImage,
    templates: &ResolvedTemplateArgs,
) -> Result<Vec<ChatMessage>> {
    let total_started = Instant::now();
    let prepared = prepare_chat_scan(image, templates, chat_rect)?;
    let messages = recognize_prepared_chat(
        ocr,
        OcrPriority::ChatObservation,
        templates,
        prepared,
        Some(monitor),
    );
    log::info!(target: "timing",
        "聊天扫描端到端耗时: total={}ms",
        elapsed_ms(total_started)
    );
    messages
}

fn enqueue_current_hall_reply(business: &BusinessRuntimeHandle, text: &str) -> Result<()> {
    match business.enqueue_deferred_chat(DeferredChatMessage {
        text: text.to_string(),
        target: DeferredChatTarget::CurrentHall,
    })? {
        EnqueueOutcome::Added => {}
        EnqueueOutcome::DroppedMessage => {
            log::warn!("大厅延迟回复入队时淘汰了一条较早的普通回复")
        }
        EnqueueOutcome::Rejected => {
            log::warn!("大厅延迟回复队列已被受保护批次占满，当前回复已丢弃")
        }
    }
    Ok(())
}
