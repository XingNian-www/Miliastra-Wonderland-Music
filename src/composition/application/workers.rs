use super::*;
use miliastra_playback::TrackKey;
use std::collections::HashMap;

use crate::features::playback::{
    BackgroundLyricsScope, LyricTracker, PlaybackMonitorPort, PlaybackWorkload, PlayerStatus,
    QueueAdvanceContext, QueueAdvanceDecision,
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
    task_engine: TaskEngineHandle,
    running: Arc<AtomicBool>,
    poll: Duration,
    duration: Option<Duration>,
    scope: BackgroundLyricsScope,
) -> Result<bool> {
    manager.start("lyrics", move |stop| {
        run_background_lyrics(player, task_engine, running, stop, poll, duration, scope);
    })
}

fn run_background_lyrics(
    player: PlayerController<PlayerRuntimeBackend, BusinessPlaybackStateAdapter>,
    task_engine: TaskEngineHandle,
    running: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    poll: Duration,
    duration: Option<Duration>,
    scope: BackgroundLyricsScope,
) {
    let poll = poll.max(Duration::from_millis(1));
    let deadline = duration.map(|duration| Instant::now() + duration);
    let mut lyric_tracker = LyricTracker::default();
    let mut first_key = None::<TrackKey>;

    while running.load(AtomicOrdering::SeqCst)
        && !stop.load(AtomicOrdering::SeqCst)
        && deadline.is_none_or(|deadline| Instant::now() < deadline)
    {
        let scheduler = match task_engine.snapshot() {
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
                if matches!(scope, BackgroundLyricsScope::CurrentSong)
                    && current_song_has_switched(&mut first_key, &status)
                {
                    log::info!("单曲后台歌词因歌曲切换结束");
                    break;
                }
                if lyric_tracker.observe(&status) {
                    let message = DeferredChatMessage {
                        text: crate::features::playback::format_lyrics(&status),
                        target: DeferredChatTarget::CurrentHall,
                        background_key: Some("lyrics".to_string()),
                        formal_epoch: Some(scheduler.formal_epoch()),
                    };
                    match task_engine.enqueue_deferred(message) {
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

fn current_song_has_switched(first_key: &mut Option<TrackKey>, status: &PlayerStatus) -> bool {
    let Some(key) = status
        .current_track
        .as_ref()
        .map(|track| &track.track_ref.key)
    else {
        return false;
    };
    if let Some(first_key) = first_key.as_ref()
        && first_key != key
    {
        return true;
    }
    first_key.get_or_insert_with(|| key.clone());
    false
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

impl ApplicationRuntime {
    pub(super) fn start_formal_task_runtime(&mut self) -> Result<()> {
        if self.business.formal_task_runtime.is_some() {
            return Ok(());
        }
        let runtime = FormalTaskRuntime::start(
            self.business.business.clone(),
            self.business.task_engine.clone(),
            Arc::clone(&self.lifecycle.running),
            Arc::clone(&self.lifecycle.paused),
            Duration::from_millis(self.lifecycle.config.timing.command.post_settle_ms),
            |client| self.formal_task_execution_context(client),
        )?;
        self.business.formal_tasks = Some(runtime.client());
        self.business.formal_task_runtime = Some(runtime);
        Ok(())
    }

    pub(super) fn start_deferred_chat_sender(&self) -> thread::JoinHandle<()> {
        let sender = DeferredChatSender {
            retry_delay: Duration::from_millis(self.lifecycle.config.timing.loop_idle_ms.max(50)),
            running: Arc::clone(&self.lifecycle.running),
            paused: Arc::clone(&self.lifecycle.paused),
            task_engine: self.business.task_engine.clone(),
            business: self.business.business.clone(),
            chat_output: self.ui.chat_output.clone(),
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
    task_engine: TaskEngineHandle,
    business: BusinessRuntimeHandle,
    chat_output: ChatOutput,
}

impl DeferredChatSender {
    fn run(self) -> Result<()> {
        let mut generation = self.task_engine.generation()?;
        while self.running.load(AtomicOrdering::SeqCst) {
            if self.paused.load(AtomicOrdering::SeqCst) {
                generation = self.task_engine.wait_for_change(generation)?;
                continue;
            }
            let Some((item, sending)) = self.task_engine.take_next_deferred()? else {
                generation = self.task_engine.wait_for_change(generation)?;
                continue;
            };
            generation = self.task_engine.generation()?;
            if !self.running.load(AtomicOrdering::SeqCst) {
                drop(sending);
                break;
            }
            if self.paused.load(AtomicOrdering::SeqCst) {
                drop(sending);
                let _ = self.task_engine.requeue_deferred_front(item)?;
                generation = self.task_engine.generation()?;
                continue;
            }

            if let DeferredChatItem::Message(message) = &item
                && let Some(epoch) = message.formal_epoch
            {
                match self.task_engine.snapshot() {
                    Ok(snapshot) if snapshot.formal_busy() || snapshot.formal_epoch() != epoch => {
                        log::debug!("后台歌词已跨越正式任务，丢弃过期输出");
                        drop(sending);
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        log::warn!("后台歌词校验正式任务代际失败，暂缓发送: {error}");
                        drop(sending);
                        if let Err(requeue_error) = self.task_engine.requeue_deferred_front(item) {
                            log::warn!("后台歌词重新入队失败: {requeue_error}");
                        }
                        self.wait_for_retry(&mut generation)?;
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
                match self.task_engine.requeue_deferred_back(item)? {
                    EnqueueOutcome::DroppedMessage => {
                        log::warn!("延迟聊天发送队列已满，已丢弃一条较早的普通回复")
                    }
                    EnqueueOutcome::Rejected => {
                        log::warn!("延迟聊天目标未激活且队列已满，当前回复已丢弃")
                    }
                    EnqueueOutcome::Added => {}
                }
                self.wait_for_retry(&mut generation)?;
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
                                    match self
                                        .task_engine
                                        .requeue_deferred_front(DeferredChatItem::Batch(batch))?
                                    {
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
                                    self.wait_for_retry(&mut generation)?;
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

    fn wait_for_retry(&self, generation: &mut u64) -> Result<()> {
        *generation = self.task_engine.generation()?;
        *generation = self
            .task_engine
            .wait_for_change_timeout(*generation, self.retry_delay)?;
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
    pub(super) fn start_playback_monitor(&self) -> thread::JoinHandle<()> {
        let monitor = PlaybackMonitorWorker {
            application: self.playback.playback_application.clone(),
            player: self.playback.player.clone(),
            business: self.business.business.clone(),
            task_engine: self.business.task_engine.clone(),
            formal_tasks: self.business.formal_tasks.clone(),
            running: Arc::clone(&self.lifecycle.running),
            paused: Arc::clone(&self.lifecycle.paused),
            monitor: self.lifecycle.monitor.clone(),
        };
        thread::spawn(move || {
            log::info!("播放监控线程已启动");
            monitor.run();
        })
    }

    // 正式执行器是唯一可调度全部垂直模块的工作线程。
    fn formal_task_execution_context(
        &self,
        formal_tasks: FormalTaskClient,
    ) -> FormalTaskExecutionContext {
        FormalTaskExecutionContext {
            coordinator: self.ui.coordinator.clone(),
            ui: FormalTaskUiContext {
                ocr_args: self.ui.ocr_args.clone(),
                chat_templates: self.ui.chat_templates.clone(),
                game_ui: self.ui.game_ui.clone(),
                residency_ui: self.ui.residency_ui.clone(),
                hall_ui: self.ui.hall_ui.clone(),
                moderation_ui: self.ui.moderation_ui.clone(),
                startup_ui: self.ui.startup_ui.clone(),
                secondary_unread_ui: self.ui.secondary_unread_ui.clone(),
                friend_delivery_ui: self.ui.friend_delivery_ui.clone(),
                invite_ui: self.ui.invite_ui.clone(),
                custom_action_ui: self.ui.custom_action_ui.clone(),
                chat_output: self.ui.chat_output.clone(),
                ocr: self.ui.ocr.clone(),
                latest_frame: self.ui.latest_frame.clone(),
                window_detection_signal: self.ui.window_detection_signal.clone(),
                chat_baseline_primed: self.ui.chat_baseline_primed.clone(),
                chat_observations: self.ui.chat_observations.clone(),
            },
            playback: FormalTaskPlaybackContext {
                player: self.playback.player.clone(),
                playback_application: self.playback.playback_application.clone(),
                player_search: self.playback.player_search.clone(),
                native_playback: self.playback.native_playback.clone(),
                login_helper: self.playback.login_helper.clone(),
            },
            business: FormalTaskBusinessContext {
                business: self.business.business.clone(),
                task_engine: self.business.task_engine.clone(),
                business_events: self.business.business_events.clone(),
                formal_tasks,
                background_commands: self.business.background_commands.clone(),
                ai: self.business.ai.clone(),
                song_requests: self.business.song_requests.clone(),
                card_games: self.business.card_games.clone(),
                administration_application: self.business.administration_application,
                hall_application: self.business.hall_application,
                idiom_chain_application: self.business.idiom_chain_application,
                turtle_soup_application: self.business.turtle_soup_application,
                undercover_game: self.business.undercover_game.clone(),
                moderation: self.business.moderation.clone(),
                moderation_workers: self.business.moderation_workers.clone(),
                startup: self.business.startup,
                custom_workflow: self.business.custom_workflow.clone(),
            },
            lifecycle: FormalTaskLifecycleContext {
                config: self.lifecycle.config.clone(),
                running: self.lifecycle.running.clone(),
                paused: self.lifecycle.paused.clone(),
                console_reply_context: self.lifecycle.console_reply_context.clone(),
                monitor: self.lifecycle.monitor.clone(),
            },
        }
    }

    pub(super) fn playback_queue(&self) -> Result<Vec<QueueItem>> {
        self.business
            .business
            .playback_queue_snapshot()
            .map_err(anyhow::Error::from)
    }

    pub(super) fn latest_frame(&self) -> Result<Arc<DynamicImage>> {
        self.ui
            .latest_frame
            .lock()
            .map_err(|_| anyhow!("主扫描画面缓存锁已损坏"))?
            .image()
            .ok_or_else(|| anyhow!("尚未获取主扫描画面，请稍后重试"))
    }

    pub(super) fn invalidate_latest_frame(&self) {
        if let Ok(mut latest_frame) = self.ui.latest_frame.lock() {
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
    task_engine: TaskEngineHandle,
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
        let scheduler = self.task_engine.snapshot()?;
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

    #[test]
    fn current_song_scope_stops_when_the_player_track_changes() {
        let mut first_key = None;
        let status = |uri: &str| PlayerStatus {
            current_track: Some(crate::features::playback::test_track(
                uri,
                "worker test - test artist",
            )),
            current_uri: uri.to_string(),
            ..PlayerStatus::default()
        };

        assert!(!current_song_has_switched(
            &mut first_key,
            &status("miliastra://track/qqmusic/1")
        ));
        assert_eq!(
            first_key,
            Some(
                crate::features::playback::test_track(
                    "miliastra://track/qqmusic/1",
                    "worker test - test artist",
                )
                .track_ref
                .key
            )
        );
        assert!(!current_song_has_switched(
            &mut first_key,
            &status("miliastra://track/qqmusic/1")
        ));
        assert!(current_song_has_switched(
            &mut first_key,
            &status("miliastra://track/qqmusic/2")
        ));
    }
}
