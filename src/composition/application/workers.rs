use super::*;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::features::playback::{
    LyricTracker, PlaybackMonitorPort, PlaybackWorkload, QueueAdvanceContext, QueueAdvanceDecision,
};

pub(super) struct BackgroundCommandManager {
    inner: Arc<BackgroundCommandManagerInner>,
}

struct BackgroundCommandManagerInner {
    state: Mutex<HashMap<String, BackgroundCommandState>>,
}

struct BackgroundCommandState {
    stop: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

impl BackgroundCommandManager {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(BackgroundCommandManagerInner {
                state: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub(super) fn start<F>(&self, name: &str, worker: F) -> Result<bool>
    where
        F: FnOnce(Arc<AtomicBool>) + Send + 'static,
    {
        self.reap_finished()?;
        {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| anyhow!("后台命令状态锁已损坏"))?;
            if state.contains_key(name) {
                return Ok(false);
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_name = format!("background-command-{name}");
        let worker = thread::Builder::new()
            .name(thread_name)
            .spawn(move || worker(thread_stop))
            .map_err(|error| anyhow!("启动后台命令线程失败: {error}"))?;

        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| anyhow!("后台命令状态锁已损坏"))?;
        if state.contains_key(name) {
            stop.store(true, AtomicOrdering::SeqCst);
            drop(state);
            let _ = worker.join();
            return Ok(false);
        }
        state.insert(name.to_string(), BackgroundCommandState { stop, worker });
        log::info!("后台命令已启动: {name}");
        Ok(true)
    }

    fn reap_finished(&self) -> Result<()> {
        let finished = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| anyhow!("后台命令状态锁已损坏"))?;
            let finished_names = state
                .iter()
                .filter(|(_, command)| command.worker.is_finished())
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            finished_names
                .into_iter()
                .filter_map(|name| state.remove(&name).map(|command| (name, command)))
                .collect::<Vec<_>>()
        };
        for (name, command) in finished {
            command
                .worker
                .join()
                .map_err(|_| anyhow!("后台命令线程 panic: {name}"))?;
            log::info!("后台命令已自然结束: {name}");
        }
        Ok(())
    }

    pub(super) fn stop(&self, name: &str) -> Result<bool> {
        let Some(command) = self
            .inner
            .state
            .lock()
            .map_err(|_| anyhow!("后台命令状态锁已损坏"))?
            .remove(name)
        else {
            return Ok(false);
        };
        command.stop.store(true, AtomicOrdering::SeqCst);
        command
            .worker
            .join()
            .map_err(|_| anyhow!("后台命令线程 panic: {name}"))?;
        log::info!("后台命令已停止: {name}");
        Ok(true)
    }

    pub(super) fn stop_all(&self) -> Result<()> {
        let commands = self
            .inner
            .state
            .lock()
            .map_err(|_| anyhow!("后台命令状态锁已损坏"))?
            .drain()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for (name, command) in commands {
            command.stop.store(true, AtomicOrdering::SeqCst);
            if command.worker.join().is_err() && first_error.is_none() {
                first_error = Some(anyhow!("后台命令线程 panic: {name}"));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Clone for BackgroundCommandManager {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for BackgroundCommandManagerInner {
    fn drop(&mut self) {
        let state = match self.state.get_mut() {
            Ok(state) => state,
            Err(error) => {
                log::error!("后台命令状态锁已损坏，仍尝试关闭后台线程");
                error.into_inner()
            }
        };
        for (name, command) in state.drain() {
            command.stop.store(true, AtomicOrdering::SeqCst);
            if command.worker.join().is_err() {
                log::error!("后台命令线程关闭时 panic: {name}");
            }
        }
    }
}

pub(super) fn start_background_lyrics(
    manager: &BackgroundCommandManager,
    player: PlayerController<PlayerRuntimeBackend, BusinessPlaybackStateAdapter>,
    business: BusinessRuntimeHandle,
    running: Arc<AtomicBool>,
    poll: Duration,
    duration: Option<Duration>,
) -> Result<bool> {
    manager.start("lyrics", move |stop| {
        run_background_lyrics(player, business, running, stop, poll, duration);
    })
}

fn run_background_lyrics(
    player: PlayerController<PlayerRuntimeBackend, BusinessPlaybackStateAdapter>,
    business: BusinessRuntimeHandle,
    running: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    poll: Duration,
    duration: Option<Duration>,
) {
    let poll = poll.max(Duration::from_millis(1));
    let deadline = duration.map(|duration| Instant::now() + duration);
    let mut lyric_tracker = LyricTracker::default();

    while running.load(AtomicOrdering::SeqCst)
        && !stop.load(AtomicOrdering::SeqCst)
        && deadline.is_none_or(|deadline| Instant::now() < deadline)
    {
        let scheduler = match business.scheduler_snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                log::warn!("后台歌词查询正式任务状态失败，暂缓发送: {error}");
                if !sleep_background_lyrics_poll(poll, deadline) {
                    break;
                }
                continue;
            }
        };
        if scheduler.formal_busy() {
            if !sleep_background_lyrics_poll(poll, deadline) {
                break;
            }
            continue;
        }

        match player.status() {
            Ok(status) => {
                if lyric_tracker.observe(&status) {
                    let message = DeferredChatMessage {
                        text: crate::features::playback::format_lyrics(&status),
                        target: DeferredChatTarget::CurrentHall,
                        background_key: Some("lyrics".to_string()),
                        formal_epoch: Some(scheduler.formal_epoch()),
                    };
                    match business.enqueue_deferred_chat(message) {
                        Ok(EnqueueOutcome::Added) => {}
                        Ok(EnqueueOutcome::DroppedMessage) => {
                            log::debug!("后台歌词输出淘汰了一条较早的普通回复");
                        }
                        Ok(EnqueueOutcome::Rejected) => {
                            log::debug!("后台歌词输出因延迟回复队列受保护而跳过");
                        }
                        Err(error) => {
                            log::warn!("后台歌词输出入队失败: {error}");
                        }
                    }
                }
            }
            Err(error) => log::debug!("后台歌词读取播放器状态失败: {error:#}"),
        }

        if !sleep_background_lyrics_poll(poll, deadline) {
            break;
        }
    }
    log::info!("后台歌词监听线程已退出");
}

fn sleep_background_lyrics_poll(poll: Duration, deadline: Option<Instant>) -> bool {
    let sleep_for = deadline.map_or(poll, |deadline| {
        poll.min(deadline.saturating_duration_since(Instant::now()))
    });
    if sleep_for.is_zero() {
        return false;
    }
    sleep(sleep_for);
    true
}

struct FormalTaskDispatcher {
    business: BusinessRuntimeHandle,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    loop_idle: Duration,
    post_settle: Duration,
}

impl FormalTaskDispatcher {
    fn run(self) -> Result<()> {
        while self.running.load(AtomicOrdering::SeqCst) {
            if self.paused.load(AtomicOrdering::SeqCst) {
                sleep(self.loop_idle);
                continue;
            }
            let Some(task) = self.business.take_next_formal_task()? else {
                sleep(self.loop_idle.max(Duration::from_millis(20)));
                continue;
            };
            if self.paused.load(AtomicOrdering::SeqCst) {
                self.business.restore_formal_task(task)?;
                sleep(self.loop_idle);
                continue;
            }
            let task_id = task.task_id();
            let task_label = task.label().to_string();
            log::info!("待处理任务开始: {}", task_label);
            let result = match catch_unwind(AssertUnwindSafe(|| task.execute())) {
                Ok(result) => result,
                Err(_) => Err(anyhow!("待处理任务执行发生未捕获异常")),
            };
            match result {
                Ok(result) => {
                    self.business
                        .complete_formal_task(task_id, FormalTaskCompletion::Succeeded(result))?;
                    log::info!("待处理任务完成: {}", task_label);
                    sleep(self.post_settle);
                }
                Err(error) => {
                    self.business.complete_formal_task(
                        task_id,
                        FormalTaskCompletion::Failed(format!("错误: {error:#}")),
                    )?;
                    log::error!("待处理任务执行异常: {error:#}");
                }
            }
        }
        Ok(())
    }
}

impl ApplicationRuntime {
    pub(super) fn start_formal_task_dispatcher(&self) -> thread::JoinHandle<()> {
        let dispatcher = FormalTaskDispatcher {
            business: self.business.clone(),
            running: Arc::clone(&self.running),
            paused: Arc::clone(&self.paused),
            loop_idle: Duration::from_millis(self.config.timing.loop_idle_ms),
            post_settle: Duration::from_millis(self.config.timing.command.post_settle_ms),
        };
        thread::spawn(move || {
            log::info!("正式任务调度线程已启动");
            if let Err(error) = dispatcher.run() {
                log::error!("正式任务调度线程异常退出: {error:#}");
            }
        })
    }

    pub(super) fn start_formal_task_execution_runtime(&mut self) -> Result<()> {
        if self.formal_task_execution.is_some() {
            return Ok(());
        }
        let runtime = FormalTaskExecutionRuntime::start(|handle| {
            let mut app = self.clone_for_formal_task_execution();
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
        let sender = DeferredChatSender {
            retry_delay: Duration::from_millis(self.config.timing.loop_idle_ms.max(50)),
            running: Arc::clone(&self.running),
            paused: Arc::clone(&self.paused),
            business: self.business.clone(),
            chat_output: self.chat_output.clone(),
        };
        thread::spawn(move || {
            log::info!("延迟聊天发送线程已启动");
            if let Err(error) = sender.run() {
                log::error!("延迟聊天发送线程异常退出: {error:#}");
            }
        })
    }
}

struct DeferredChatSender {
    retry_delay: Duration,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    business: BusinessRuntimeHandle,
    chat_output: ChatOutput,
}

impl DeferredChatSender {
    fn run(self) -> Result<()> {
        let retry_delay = self.retry_delay;
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

            if let DeferredChatItem::Message(message) = &item
                && let Some(epoch) = message.formal_epoch
            {
                match self.business.scheduler_snapshot() {
                    Ok(snapshot) if snapshot.formal_busy() || snapshot.formal_epoch() != epoch => {
                        log::debug!("后台歌词已跨越正式任务，丢弃过期输出");
                        drop(sending);
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log::warn!("后台歌词校验正式任务代际失败，暂缓发送: {error}");
                        drop(sending);
                        if let Err(requeue_error) = self.business.requeue_deferred_chat_front(item)
                        {
                            log::warn!("后台歌词重新入队失败: {requeue_error}");
                        }
                        sleep(retry_delay);
                        continue;
                    }
                }
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

    fn active_ui_residency(&self) -> Result<UiResidency> {
        let snapshot = self.business.chat_listener_snapshot()?;
        Ok(listener_residency(
            snapshot.mode,
            snapshot.temporary_primary,
        ))
    }

    fn deferred_chat_target_is_active(&self, target: DeferredChatTarget) -> Result<bool> {
        Ok(matches!(
            (target, self.active_ui_residency()?),
            (DeferredChatTarget::Primary, UiResidency::Primary)
                | (
                    DeferredChatTarget::SecondaryCurrentHall,
                    UiResidency::SecondaryCurrentHall
                )
                | (DeferredChatTarget::CurrentHall, _)
        ))
    }
}

impl ApplicationRuntime {
    pub(super) fn start_web_tool_executor(&self) -> thread::JoinHandle<()> {
        let worker = DiagnosticExecutor {
            business: self.business.clone(),
            running: Arc::clone(&self.running),
        };
        thread::spawn(move || {
            log::info!("Web 工具执行线程已启动");
            worker.run();
        })
    }

    pub(super) fn start_playback_monitor(&self) -> thread::JoinHandle<()> {
        let monitor = PlaybackMonitorWorker {
            application: self.playback_application.clone(),
            player: self.player.clone(),
            business: self.business.clone(),
            formal_tasks: self.formal_tasks.clone(),
            running: Arc::clone(&self.running),
            paused: Arc::clone(&self.paused),
            monitor: self.monitor.clone(),
        };
        thread::spawn(move || {
            log::info!("播放监控线程已启动");
            monitor.run();
        })
    }

    // The formal executor is the only worker that may dispatch every vertical module.
    // All long-lived background workers use the narrow contexts defined in this file.
    fn clone_for_formal_task_execution(&self) -> Self {
        Self {
            config: self.config.clone(),
            ocr_args: self.ocr_args.clone(),
            chat_templates: self.chat_templates.clone(),
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
            background_commands: self.background_commands.clone(),
            player: self.player.clone(),
            playback_application: self.playback_application.clone(),
            player_search: self.player_search.clone(),
            player_runtime: None,
            openai_runtime: None,
            ai: self.ai.clone(),
            song_requests: self.song_requests.clone(),
            chat_output: self.chat_output.clone(),
            ocr: self.ocr.clone(),
            ocr_runtime: None,
            latest_frame: self.latest_frame.clone(),
            locks: CommandLockState::default(),
            window_detection_signal: self.window_detection_signal.clone(),
            screen_lock_primed: self.screen_lock_primed.clone(),
            reset_locks_requested: self.reset_locks_requested.clone(),
            card_games: self.card_games.clone(),
            administration_application: self.administration_application,
            hall_application: self.hall_application,
            idiom_chain_application: self.idiom_chain_application,
            turtle_soup_application: self.turtle_soup_application,
            undercover_game: self.undercover_game.clone(),
            moderation: self.moderation.clone(),
            moderation_workers: self.moderation_workers.clone(),
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
            .image()
            .ok_or_else(|| anyhow!("尚未获取主扫描画面，请稍后重试"))
    }

    pub(super) fn invalidate_latest_frame(&self) {
        if let Ok(mut latest_frame) = self.latest_frame.lock() {
            latest_frame.invalidate();
        } else {
            log::error!("主扫描画面缓存锁已损坏");
        }
    }
}

struct PlaybackMonitorWorker {
    application: PlaybackApplication,
    player: PlayerController<PlayerRuntimeBackend, BusinessPlaybackStateAdapter>,
    business: BusinessRuntimeHandle,
    formal_tasks: Option<FormalTaskClient>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    monitor: MonitorShared,
}

impl PlaybackMonitorWorker {
    fn run(mut self) {
        self.application.clone().run_monitor_loop(&mut self);
    }
}

impl PlaybackMonitorPort for PlaybackMonitorWorker {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn is_running(&self) -> bool {
        self.running.load(AtomicOrdering::SeqCst)
    }

    fn is_paused(&self) -> bool {
        self.paused.load(AtomicOrdering::SeqCst)
    }

    fn wait(&mut self, duration: Duration) {
        sleep(duration);
    }

    fn player_status(&mut self) -> Result<PlayerStatus> {
        self.player.status()
    }

    fn playback_queue(&mut self) -> Result<Vec<QueueItem>> {
        self.business
            .playback_queue_snapshot()
            .map_err(anyhow::Error::from)
    }

    fn workload(&mut self) -> Result<PlaybackWorkload> {
        let scheduler = self.business.scheduler_snapshot()?;
        Ok(PlaybackWorkload {
            has_pending_playback_task: scheduler.pending_playback_related(),
            command_executing: scheduler.is_busy(),
            song_command_executing: scheduler.active_playback_related(),
        })
    }

    fn maybe_advance_queue(
        &mut self,
        status: PlayerStatus,
        context: QueueAdvanceContext,
    ) -> Result<QueueAdvanceDecision> {
        self.player.maybe_advance_queue(status, context)
    }

    fn enqueue_advance_queue(&mut self, reason: &'static str) -> Result<()> {
        let tasks = self
            .formal_tasks
            .clone()
            .ok_or_else(|| anyhow!("正式任务执行运行时尚未启动"))?;
        match tasks.enqueue(PendingTask::AdvanceQueue { reason })? {
            FormalTaskEnqueueOutcome::Queued(_) => Ok(()),
            FormalTaskEnqueueOutcome::Duplicate => {
                log::info!("播放队列推进任务已在待执行范围内，跳过重复入队");
                Ok(())
            }
        }
    }

    fn update_monitor(&mut self) {
        self.monitor
            .publish(MonitorEvent::PlaybackController(self.player.snapshot()));
    }
}

struct DiagnosticExecutor {
    business: BusinessRuntimeHandle,
    running: Arc<AtomicBool>,
}

impl DiagnosticExecutor {
    fn run(self) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_background_workers_are_reaped_before_restarting_same_name() {
        let manager = BackgroundCommandManager::new();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);

        assert!(
            manager
                .start("timed-lyrics", move |_| {
                    worker_finished.store(true, AtomicOrdering::SeqCst);
                })
                .expect("start timed worker")
        );

        while !finished.load(AtomicOrdering::SeqCst) {
            thread::yield_now();
        }

        let restarted = (0..100).find_map(|_| {
            let started = manager
                .start("timed-lyrics", |_| {})
                .expect("restart after timed worker");
            if started {
                Some(())
            } else {
                thread::yield_now();
                None
            }
        });
        assert!(restarted.is_some());
        manager.stop("timed-lyrics").expect("stop restarted worker");
    }
}
