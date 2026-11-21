use super::*;
use miliastra_playback::TrackKey;

use crate::features::playback::{
    BackgroundLyricsScope, LyricTracker, PauseReason, PlaybackMonitorPort, PlaybackWorkload,
    PlayerStatus, QueueAdvanceContext, QueueAdvanceDecision,
};

/// 同一条日志在**高频循环**内的重复抑制。
///
/// 这些 warn 所在的循环每秒会跑 1 到 16 次，持续故障时一整晚能刷几万行，
/// 把其余真正需要人看的告警冲掉。策略与 `listener.rs` 的驻留告警一致：
///
/// - **首次立即 warn**：保证故障有痕迹，不会被抑制掉；
/// - 窗口内（默认 60 秒）重复出现降 `debug`：保留排障信息但不刷屏；
/// - 窗口过后仍然失败，**再 warn 一次**：说明问题没自愈，需要重新提醒。
///
/// 刻意不做成全局静态：抑制状态属于**具体那个循环**，跨实例共享会互相干扰，
/// 而且全局可变状态在测试之间会串味。
#[derive(Debug, Default)]
struct RepeatWarn {
    last_warned_at: Option<Instant>,
}

/// 重复抑制窗口：与 `listener.rs` 保持一致，便于读日志的人形成统一预期。
const REPEAT_WARN_INTERVAL: Duration = Duration::from_secs(60);

impl RepeatWarn {
    /// 这次是否应当以 `warn` 输出。
    ///
    /// 返回 `true` 时**已经把计时推进到当下**，调用方随即打 warn；
    /// 返回 `false` 时调用方应打 debug。
    fn should_warn(&mut self, now: Instant) -> bool {
        self.should_warn_within(now, REPEAT_WARN_INTERVAL)
    }

    /// 可指定窗口的版本，供单测直接驱动，不必真的等 60 秒。
    fn should_warn_within(&mut self, now: Instant, interval: Duration) -> bool {
        let due = self
            .last_warned_at
            .is_none_or(|last| now.saturating_duration_since(last) >= interval);
        if due {
            self.last_warned_at = Some(now);
        }
        due
    }
}

/// 后台歌词输出的唯一托管任务。
///
/// 进程里同时只允许一条后台歌词线程：`@歌词` 重复触发时第二次调用返回
/// `Ok(false)`，调用方据此回「已经在发送」。这个约束是**一条任务**上的约束，
/// 不是一张命令注册表——原先用 `HashMap<String, ...>` 按名字登记，实际只有
/// `"lyrics"` 一个键，字符串既没有第二个取值也没有外部来源，只会让
/// 「查表是否为空」冒充「任务是否在跑」，把状态判断绕了一圈。
///
/// 现在的形状与真实约束一一对应：一个 `Mutex<Option<RunningBackgroundLyrics>>`
/// 表达「至多一条在跑」，`stop` 与 `reap` 都只作用于这一条，线程名固定。
///
/// 并发语义与原来一致：启动前先 `reap` 回收自然结束的线程；判断与登记在
/// 同一把锁里完成（`spawn` 也在锁内），并发触发时只有一条线程会被创建。
/// `Drop` 兜底停掉仍在跑的那条，绝不留下野线程。
pub(super) struct BackgroundLyricsWorker {
    inner: Arc<BackgroundLyricsWorkerInner>,
}

struct BackgroundLyricsWorkerInner {
    /// 至多一条在跑的歌词线程。`None` 表示当前没有。
    task: Mutex<Option<RunningBackgroundLyrics>>,
}

struct RunningBackgroundLyrics {
    stop: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}

/// 后台歌词线程的名字。固定值，因为进程里只有这一条。
const BACKGROUND_LYRICS_THREAD_NAME: &str = "background-lyrics";

impl BackgroundLyricsWorker {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(BackgroundLyricsWorkerInner {
                task: Mutex::new(None),
            }),
        }
    }

    /// 启动后台歌词线程；已经有一条在跑时返回 `Ok(false)`。
    pub(super) fn start(&self, request: BackgroundLyricsRequest) -> Result<bool> {
        self.start_spawn(move |stop| request.run(stop))
    }

    /// 启动线程的公共骨架：只关心「至多一条」与停止信号，不关心任务是什么。
    ///
    /// 单测用它注入不需要整条播放器链路的任务体；生产路径经 [`Self::start`]。
    ///
    /// **判断与插入在同一把锁里**，`spawn` 也在锁内完成。这样「是否已有任务」
    /// 与「登记新任务」之间没有空档，并发触发时只会有一条线程被真正创建；
    /// 若先把锁放掉再 spawn，两个调用者会同时看到「空闲」而各起一条，后到的那条
    /// 只能原地停掉——它已经跑起来了，日志里会留下一次并非真实的启动记录。
    /// `spawn` 只花几微秒，被锁挡住的是同一时刻的其他启动请求，不是歌词线程本身。
    fn start_spawn(&self, run: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Result<bool> {
        self.reap()?;
        let mut task = self
            .inner
            .task
            .lock()
            .map_err(|_| anyhow!("后台歌词任务状态锁已损坏"))?;
        if task.is_some() {
            return Ok(false);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let spawned = thread::Builder::new()
            .name(BACKGROUND_LYRICS_THREAD_NAME.to_string())
            .spawn(move || run(thread_stop))
            .map_err(|error| anyhow!("启动后台歌词线程失败: {error}"))?;
        *task = Some(RunningBackgroundLyrics {
            stop,
            worker: spawned,
        });
        drop(task);
        log::info!("后台歌词监听线程已启动");
        Ok(true)
    }

    /// 回收已经自然结束的线程，并把退出信息记进日志。
    ///
    /// 有单次时长（`@歌词 N秒`）的任务会自己退出；不回收就会一直占着
    /// 「在跑」的位置，下一次 `@歌词` 永远起不来。
    fn reap(&self) -> Result<()> {
        let finished = {
            let mut task = self
                .inner
                .task
                .lock()
                .map_err(|_| anyhow!("后台歌词任务状态锁已损坏"))?;
            match task.as_ref() {
                Some(running) if running.worker.is_finished() => task.take(),
                _ => None,
            }
        };
        if let Some(finished) = finished {
            finished
                .worker
                .join()
                .map_err(|_| anyhow!("后台歌词线程 panic"))?;
            log::info!("后台歌词监听线程已自然结束");
        }
        Ok(())
    }

    /// 请求停止并等待线程退出；当前没有在跑的任务时返回 `Ok(false)`。
    pub(super) fn stop(&self) -> Result<bool> {
        let Some(running) = self
            .inner
            .task
            .lock()
            .map_err(|_| anyhow!("后台歌词任务状态锁已损坏"))?
            .take()
        else {
            return Ok(false);
        };
        running.stop.store(true, AtomicOrdering::SeqCst);
        running
            .worker
            .join()
            .map_err(|_| anyhow!("后台歌词线程 panic"))?;
        log::info!("后台歌词监听线程已停止");
        Ok(true)
    }

    /// 停机路径：确保线程已退出。没有任务时同样返回 `Ok(())`。
    pub(super) fn stop_all(&self) -> Result<()> {
        self.stop().map(|_| ())
    }
}

impl Clone for BackgroundLyricsWorker {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for BackgroundLyricsWorkerInner {
    fn drop(&mut self) {
        let task = match self.task.get_mut() {
            Ok(task) => task,
            Err(error) => {
                log::error!(
                    "后台歌词任务状态已损坏（步骤：关闭时读取任务状态），无法确认线程是否已退出，仍尝试关闭，可能残留线程"
                );
                error.into_inner()
            }
        };
        let Some(running) = task.take() else {
            return;
        };
        running.stop.store(true, AtomicOrdering::SeqCst);
        if running.worker.join().is_err() {
            log::error!("后台歌词线程关闭时崩溃（步骤：等待线程退出），该次歌词输出未能正常收尾");
        }
    }
}

/// 一次后台歌词请求的全部依赖。
///
/// 字段刻意保持私有：调用方只能经 [`start_background_lyrics`] 构造它，
/// 因此「哪些东西属于后台歌词任务」这件事只有一处定义。
pub(super) struct BackgroundLyricsRequest {
    player: PlayerController<PlayerRuntimeBackend, BusinessPlaybackStateAdapter>,
    task_engine: TaskEngineHandle,
    running: Arc<AtomicBool>,
    poll: Duration,
    duration: Option<Duration>,
    scope: BackgroundLyricsScope,
}

impl BackgroundLyricsRequest {
    fn run(self, stop: Arc<AtomicBool>) {
        run_background_lyrics(
            self.player,
            self.task_engine,
            self.running,
            stop,
            self.poll,
            self.duration,
            self.scope,
        );
    }
}

pub(super) fn start_background_lyrics(
    worker: &BackgroundLyricsWorker,
    player: PlayerController<PlayerRuntimeBackend, BusinessPlaybackStateAdapter>,
    task_engine: TaskEngineHandle,
    running: Arc<AtomicBool>,
    poll: Duration,
    duration: Option<Duration>,
    scope: BackgroundLyricsScope,
) -> Result<bool> {
    worker.start(BackgroundLyricsRequest {
        player,
        task_engine,
        running,
        poll,
        duration,
        scope,
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
    // 两个失败点各自计时：任务引擎查询失败与入队失败是不同故障，
    // 共用一个计时器会让后者的首次告警被前者压掉。
    let mut snapshot_fail = RepeatWarn::default();
    let mut enqueue_fail = RepeatWarn::default();

    while running.load(AtomicOrdering::SeqCst)
        && !stop.load(AtomicOrdering::SeqCst)
        && deadline.is_none_or(|deadline| Instant::now() < deadline)
    {
        let scheduler = match task_engine.snapshot() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if snapshot_fail.should_warn(Instant::now()) {
                    log::warn!(
                        "后台歌词暂缓发送（步骤：查询正式任务状态），本轮不发送歌词，等下一次刷新再试：{error:#}；本条在问题持续期间最多每分钟重复一次"
                    );
                } else {
                    log::debug!(
                        "后台歌词仍在暂缓发送（步骤：查询正式任务状态），等待任务调度恢复：{error:#}"
                    );
                }
                if !sleep_background_lyrics_poll(poll, deadline, &running, &stop) {
                    break;
                }
                continue;
            }
        };
        if scheduler.formal_busy() {
            if !sleep_background_lyrics_poll(poll, deadline, &running, &stop) {
                break;
            }
            continue;
        }

        match player.status() {
            Ok(status) => {
                if matches!(scope, BackgroundLyricsScope::CurrentSong)
                    && current_song_has_switched(&mut first_key, &status)
                {
                    log::info!("单曲后台歌词已结束：当前歌曲已切换");
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
                            if enqueue_fail.should_warn(Instant::now()) {
                                log::warn!(
                                    "后台歌词入队失败（步骤：把歌词加入发送队列），这段歌词不会显示：{error:#}；本条在问题持续期间最多每分钟重复一次"
                                );
                            } else {
                                log::debug!(
                                    "后台歌词仍无法入队（步骤：把歌词加入发送队列），继续跳过本段歌词：{error:#}"
                                );
                            }
                        }
                    }
                }
            }
            Err(error) => log::debug!("后台歌词读取播放器状态失败：{error:#}"),
        }

        if !sleep_background_lyrics_poll(poll, deadline, &running, &stop) {
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

fn sleep_background_lyrics_poll(
    poll: Duration,
    deadline: Option<Instant>,
    running: &AtomicBool,
    stop: &AtomicBool,
) -> bool {
    let sleep_for = deadline.map_or(poll, |deadline| {
        poll.min(deadline.saturating_duration_since(Instant::now()))
    });
    if sleep_for.is_zero() {
        return false;
    }
    interruptible_sleep(sleep_for, || {
        running.load(AtomicOrdering::SeqCst) && !stop.load(AtomicOrdering::SeqCst)
    }) && deadline.is_none_or(|deadline| Instant::now() < deadline)
}

fn interruptible_sleep(duration: Duration, mut keep_running: impl FnMut() -> bool) -> bool {
    const MAX_SLICE: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + duration;
    while keep_running() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        sleep(remaining.min(MAX_SLICE));
    }
    false
}

impl ApplicationRuntime {
    /// 启动正式任务工作线程，并把它的客户端接线回共享服务。
    ///
    /// 这一步是**唯一**同时需要「拥有的运行时」和「共享服务」的地方：
    /// 线程句柄归 [`OwnedRuntimes`] 关停，客户端进入 [`AppServices::formal_tasks`]。
    pub(super) fn start_formal_task_runtime(&mut self) -> Result<()> {
        if self.owned.formal_task_runtime.is_some() {
            return Ok(());
        }
        let services = Arc::clone(&self.services);
        // 客户端必须在**工作线程启动之前**接线：`FormalTaskRuntime::start` 会在
        // 返回前就 spawn 任务循环，队列里已有的任务可能立刻开跑。若等到返回后再
        // `attach`，那一小段窗口里 `AppServices::formal_tasks()` 会报「尚未启动」。
        let runtime = FormalTaskRuntime::start(
            self.business.business.clone(),
            self.business.task_engine.clone(),
            Arc::clone(&self.shared.running),
            Arc::clone(&self.shared.paused),
            self.shared.live_configs.clone(),
            move |client| {
                services.attach_formal_tasks(client);
                FormalTaskExecutionContext::new(services)
            },
        )?;
        self.owned.formal_task_runtime = Some(runtime);
        Ok(())
    }
}

impl AppServices {
    pub(super) fn start_deferred_chat_sender(&self) -> thread::JoinHandle<()> {
        let sender = DeferredChatSender {
            config_store: self.shared.config_store.clone(),
            live_configs: self.shared.live_configs.clone(),
            running: Arc::clone(&self.shared.running),
            paused: Arc::clone(&self.shared.paused),
            task_engine: self.business.task_engine.clone(),
            business: self.business.business.clone(),
            chat_output: self.ui.chat_output.clone(),
            epoch_check_fail: RepeatWarn::default(),
            requeue_fail: RepeatWarn::default(),
            queue_full_dropped: RepeatWarn::default(),
            target_inactive_dropped: RepeatWarn::default(),
        };
        thread::spawn(move || {
            log::info!("延迟聊天发送线程已启动");
            if let Err(error) = sender.run() {
                log::error!(
                    "延迟聊天发送线程异常退出（步骤：发送队列循环），后续排队中的回复不会再发出，需要重启程序：{error:#}"
                );
            }
        })
    }
}

struct DeferredChatSender {
    /// 配置中心共享句柄：延迟发送也要读命令面板的覆盖与静音。
    config_store: crate::config::SharedConfigStore,
    live_configs: LiveConfigs,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    task_engine: TaskEngineHandle,
    business: BusinessRuntimeHandle,
    chat_output: ChatOutput,
    /// 四个高频失败点各自的重复抑制计时。
    ///
    /// 分开而不是共用一个：它们代表四种不同的故障（代际校验失败、重新入队失败、
    /// 队列满丢弃、目标未激活丢弃），合并计时会让后来者的首次告警被前者吞掉，
    /// 而这正是排查时最需要看到的信息。
    epoch_check_fail: RepeatWarn,
    requeue_fail: RepeatWarn,
    queue_full_dropped: RepeatWarn,
    target_inactive_dropped: RepeatWarn,
}

/// 读取命令面板保存的覆盖文案；与立即发送路径共用同一查询语义。
fn command_reply_override(
    config_store: &crate::config::SharedConfigStore,
    key: &str,
) -> Option<String> {
    let store = config_store.lock().ok()?;
    let replies = store.reply_templates().ok()?;
    crate::features::reply_template::stored_override(&replies, key)
}

/// 该 key 是否被命令面板静音；与立即发送路径共用同一判定。
fn command_reply_is_muted(config_store: &crate::config::SharedConfigStore, key: &str) -> bool {
    let Ok(store) = config_store.lock() else {
        return false;
    };
    let Ok(replies) = store.reply_templates() else {
        return false;
    };
    crate::features::reply_template::is_muted(&replies, key)
}

impl DeferredChatSender {
    /// 按模板渲染一条文案；被静音时返回 None。
    ///
    /// 与立即发送路径同一契约：静音判定在渲染入口完成，调用方必须处理 None。
    fn render_template_text(
        &self,
        spec: &crate::features::reply_template::ReplyTemplate,
        vars: &crate::features::reply_template::ReplyVars,
    ) -> Option<String> {
        if command_reply_is_muted(&self.config_store, spec.key) {
            return None;
        }
        // 覆盖只来自命令面板保存的行；配置中心不再提供回复文案入口。
        let override_text = command_reply_override(&self.config_store, spec.key);
        let rendered =
            crate::features::reply_template::render(spec, override_text.as_deref(), vars);
        if !rendered.missing.is_empty() {
            log::warn!(
                "回复模板「{}」缺少变量 {}，这些位置已按空串渲染，玩家会看到内容缺一段",
                spec.key,
                super::delivery::missing_vars_label(&rendered.missing)
            );
        }
        Some(rendered.text)
    }

    fn render_reply_text(
        &self,
        reply: &crate::features::turtle_soup::TurtleSoupReplyText,
    ) -> Option<String> {
        use crate::features::turtle_soup::TurtleSoupReplyText;
        match reply {
            TurtleSoupReplyText::Raw(text) => Some(text.clone()),
            TurtleSoupReplyText::Keyed { spec, vars } => self.render_template_text(spec, vars),
            TurtleSoupReplyText::NumberedComposed {
                label,
                index,
                total,
                content,
            } => {
                let label = self.render_template_text(
                    label,
                    &crate::features::reply_template::ReplyVars::new(),
                )?;
                let content = self.render_reply_text(content)?;
                Some(format!("{label}{index}/{total}：{content}"))
            }
            TurtleSoupReplyText::Numbered {
                label,
                index,
                total,
                content,
            } => {
                let rendered = self.render_template_text(
                    label,
                    &crate::features::reply_template::ReplyVars::new(),
                )?;
                Some(format!("{rendered}{index}/{total}：{content}"))
            }
            TurtleSoupReplyText::Composed { spec, vars, nested } => {
                let mut resolved = Vec::with_capacity(nested.len());
                for (name, reply) in nested {
                    resolved.push((*name, self.render_reply_text(reply)?));
                }
                self.render_template_text(spec, &vars.clone().with_nested(resolved))
            }
        }
    }

    fn run(mut self) -> Result<()> {
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
                        if self.epoch_check_fail.should_warn(Instant::now()) {
                            log::warn!(
                                "后台歌词暂缓发送（步骤：校验正式任务代际），本轮不发送，等下一次刷新再试：{error:#}；本条在问题持续期间最多每分钟重复一次"
                            );
                        } else {
                            log::debug!(
                                "后台歌词仍在暂缓发送（步骤：校验正式任务代际），继续等待任务调度恢复：{error:#}"
                            );
                        }
                        drop(sending);
                        if let Err(requeue_error) = self.task_engine.requeue_deferred_front(item) {
                            if self.requeue_fail.should_warn(Instant::now()) {
                                log::warn!(
                                    "后台歌词重新入队失败（步骤：暂缓后重新排队），这段歌词不会显示：{requeue_error:#}；本条在问题持续期间最多每分钟重复一次"
                                );
                            } else {
                                log::debug!(
                                    "后台歌词仍无法重新入队（步骤：暂缓后重新排队），继续丢弃这段歌词：{requeue_error:#}"
                                );
                            }
                        }
                        self.wait_for_retry(&mut generation)?;
                        continue;
                    }
                }
            }

            if let DeferredChatItem::Message(message) = &item
                && message.background_key.as_deref() == Some("lyrics")
                && (self.business.playback_state_snapshot()?.pause_reason == PauseReason::User
                    || self.business.hall_state_snapshot()?.remaining_minutes_now() == Some(0))
            {
                log::debug!("播放已暂停或大厅已到期，丢弃待发送歌词");
                drop(sending);
                continue;
            }

            if let DeferredChatItem::Batch(batch) = &item
                && !self
                    .business
                    .turtle_soup_delivery_is_current(batch.turtle_soup)
            {
                log::debug!("延迟聊天分段批次所属的海龟汤会话已失效，跳过这一批消息的发送");
                drop(sending);
                continue;
            }

            let target = item.target();

            if !self.deferred_chat_target_is_active(target)? {
                drop(sending);
                match self.task_engine.requeue_deferred_back(item)? {
                    EnqueueOutcome::DroppedMessage => {
                        if self.queue_full_dropped.should_warn(Instant::now()) {
                            log::warn!(
                                "延迟聊天发送队列已满，已丢弃一条较早的普通回复，玩家会少收到一句；本条在问题持续期间最多每分钟重复一次"
                            );
                        } else {
                            log::debug!("延迟聊天发送队列仍然已满，继续丢弃较早的普通回复");
                        }
                    }
                    EnqueueOutcome::Rejected => {
                        if self.target_inactive_dropped.should_warn(Instant::now()) {
                            log::warn!(
                                "延迟聊天目标未激活且队列已满，当前回复已丢弃，玩家收不到这句；本条在问题持续期间最多每分钟重复一次"
                            );
                        } else {
                            log::debug!("延迟聊天目标仍未激活且队列已满，继续丢弃当前回复");
                        }
                    }
                    EnqueueOutcome::Added => {}
                }
                self.wait_for_retry(&mut generation)?;
                continue;
            }

            match item {
                DeferredChatItem::Message(message) => {
                    let text = self
                        .live_configs
                        .identity
                        .replace_display_names(&message.text);
                    // 普通回复同样可能超过聊天单条显示宽度（超过会被游戏截断）：
                    // 走 ChatOutput 的自动换行入口——超宽才按 80 宽切段并批量发送，
                    // 短文本仍是单条。段间间隔与批量发送同源，不另造硬编码节奏。
                    let batch_interval_ms =
                        self.live_configs.snapshot().timing.command.help_batch_ms;
                    let result = match target {
                        DeferredChatTarget::Primary => {
                            self.chat_output.send_wrapped(&text, batch_interval_ms)
                        }
                        DeferredChatTarget::SecondaryCurrentHall => self
                            .chat_output
                            .send_current_chat_wrapped(&text, batch_interval_ms),
                        DeferredChatTarget::CurrentHall => {
                            let residency = self.active_ui_residency()?;
                            match residency {
                                UiResidency::Primary => {
                                    self.chat_output.send_wrapped(&text, batch_interval_ms)
                                }
                                UiResidency::SecondaryCurrentHall => self
                                    .chat_output
                                    .send_current_chat_wrapped(&text, batch_interval_ms),
                            }
                        }
                    };
                    drop(sending);
                    if let Err(error) = result {
                        log::error!(
                            "延迟聊天回复发送失败（步骤：发送普通回复），本条已丢弃不会重试，玩家收不到这句：{error:#}"
                        );
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
                    // 静音条目在此处被剔除：它们不应进入待发送队列。
                    let mapped_messages = if batch.is_rendered() {
                        batch
                            .remaining_replies()
                            .filter_map(|reply| match reply {
                                crate::features::turtle_soup::TurtleSoupReplyText::Raw(text) => {
                                    Some(text.clone())
                                }
                                other => self.render_reply_text(other),
                            })
                            .collect::<Vec<_>>()
                    } else {
                        let mut rows = Vec::new();
                        for reply in batch.remaining_replies() {
                            if let crate::features::turtle_soup::TurtleSoupReplyText::NumberedComposed {
                                label, content, ..
                            } = reply {
                                // 任一段被静音，整条编号消息就不发。
                                let (Some(label_text), Some(content_text)) = (
                                    self.render_template_text(
                                        label,
                                        &crate::features::reply_template::ReplyVars::new(),
                                    ),
                                    self.render_reply_text(content),
                                ) else {
                                    continue;
                                };
                                let rendered = crate::features::chat_text::split_numbered_chat_message(
                                    &label_text, &content_text,
                                );
                                rows.extend(rendered.into_iter().map(|row| {
                                    self.live_configs.identity.replace_display_names(&row)
                                }));
                            } else if let Some(rendered) = self.render_reply_text(reply) {
                                rows.push(self.live_configs.identity.replace_display_names(&rendered));
                            }
                        }
                        batch.replace_remaining(rows.clone());
                        rows
                    };
                    let messages = mapped_messages
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>();
                    let batch_interval_ms =
                        self.live_configs.snapshot().timing.command.help_batch_ms;
                    let outcome = match residency {
                        UiResidency::Primary => self
                            .chat_output
                            .send_batch_outcome(&messages, batch_interval_ms),
                        UiResidency::SecondaryCurrentHall => self
                            .chat_output
                            .send_current_chat_batch_outcome(&messages, batch_interval_ms),
                    };
                    drop(sending);

                    let ChatBatchSendOutcome { sent, status, .. } = outcome;
                    let all_sent = match batch.mark_sent(sent) {
                        Ok(all_sent) => all_sent,
                        Err(error) => {
                            log::error!(
                                "海龟汤批量发送进度无效（步骤：读取批量发送进度），该批消息可能没有全部送达：{error:#}"
                            );
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
                                "海龟汤批次内容已完整发送，但聊天界面收尾失败，不重发内容：{error:#}"
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
                                batch.remaining_replies().count()
                            );
                            log::error!(
                                "海龟汤批量发送中断（步骤：等待剩余消息发送完成），已发送 {} 条、剩余 {} 条未发出，本轮投递判为失败并通知等待方：{error:#}",
                                sent,
                                batch.remaining_replies().count()
                            );
                            self.business.turtle_soup_delivery_failure(delivery, &error);
                        }
                        ChatBatchSendStatus::Failed(error) => {
                            let attempt = batch.current_attempt();
                            let max_attempts = batch.max_attempts();
                            match batch.mark_current_failed() {
                                BatchFailureOutcome::Retry => {
                                    log::warn!(
                                        "海龟汤批量发送失败，准备从首条未发送消息重试：第 {} 次重试（最多 {} 次），已发出 {} 条，原因 {:#}",
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
                                            log::error!(
                                                "海龟汤批量重试无法重新入队（步骤：把失败的批量消息放回延迟队列队首），剩余消息不会再自动重试，本轮投递判为失败：{requeue_error:#}"
                                            );
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
                                        "海龟汤批量消息发送失败且已用尽重试（步骤：逐条发送批量回复，已发出 {} 条、最多重试 {} 次），剩余消息不会自动补发，本轮投递判为失败：{error:#}",
                                        sent,
                                        max_attempts
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
        let retry_delay =
            Duration::from_millis(self.live_configs.snapshot().timing.loop_idle_ms.max(50));
        *generation = self.task_engine.generation()?;
        *generation = self
            .task_engine
            .wait_for_change_timeout(*generation, retry_delay)?;
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

impl AppServices {
    pub(super) fn start_playback_monitor(&self) -> thread::JoinHandle<()> {
        let monitor = PlaybackMonitorWorker {
            application: self.playback.playback_application.clone(),
            player: self.playback.player.clone(),
            business: self.business.business.clone(),
            task_engine: self.business.task_engine.clone(),
            formal_tasks: self.formal_tasks.get().cloned(),
            live_configs: self.shared.live_configs.clone(),
            running: Arc::clone(&self.shared.running),
            paused: Arc::clone(&self.shared.paused),
            monitor: self.shared.monitor.clone(),
        };
        thread::spawn(move || {
            log::info!("播放监控线程已启动");
            monitor.run();
        })
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
    live_configs: LiveConfigs,
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
        interruptible_sleep(duration, || self.running.load(AtomicOrdering::SeqCst));
    }

    fn player_status(&mut self) -> Result<PlayerStatus> {
        // 控制器按进度/时间阈值节流持久化，保证重启能从最新可靠进度续播。
        self.player.monitor_status()
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
        })
    }

    fn maybe_advance_queue(
        &mut self,
        status: PlayerStatus,
        context: QueueAdvanceContext,
    ) -> Result<QueueAdvanceDecision> {
        // 开播回执已经立即发过了；这里只在**失败确认、要放弃当前曲目**时补一条错误回报。
        // 失败信息要在把 status 交给控制器前先取出（状态会被消费）。
        let failure = (!status.failure_code.trim().is_empty())
            .then(|| (status.failure_code.clone(), status.failure_message.clone()));
        let decision = self.player.try_maybe_advance_queue(status, context)?;
        let pending_reload = self.live_configs.has_pending_reload();
        let decision = suppress_automatic_advance_for_reload(decision, pending_reload);
        if let QueueAdvanceDecision::AdvanceQueue { reason } = &decision
            && *reason == "播放失败"
            && let Some((code, message)) = failure
        {
            let detail = if message.trim().is_empty() {
                code
            } else {
                format!("{code}: {}", compact_failure_detail(&message))
            };
            // 监控线程没有聊天发送能力：走延迟聊天队列，由 DeferredChatSender 统一发。
            if let Err(error) = self.task_engine.enqueue_deferred(DeferredChatMessage {
                text: format!("播放失败，已跳过当前歌曲（{detail}）"),
                target: DeferredChatTarget::CurrentHall,
                background_key: None,
                formal_epoch: None,
            }) {
                log::warn!(
                    "回报播放失败状态失败（步骤：把播放失败同步给界面与监控），面板可能仍显示播放中：{error:#}"
                );
            }
        }
        Ok(decision)
    }

    fn enqueue_advance_queue(&mut self, reason: &'static str) -> Result<()> {
        let tasks = self
            .formal_tasks
            .clone()
            .ok_or_else(|| anyhow!("正式任务执行运行时尚未启动"))?;
        match tasks.enqueue(PendingTask::AdvanceQueue { reason })? {
            FormalTaskEnqueueOutcome::Queued(_) => Ok(()),
            FormalTaskEnqueueOutcome::Duplicate => {
                log::debug!("播放队列推进任务已在待执行范围内，跳过重复入队");
                Ok(())
            }
        }
    }

    fn update_monitor(&mut self) {
        self.monitor
            .publish(MonitorEvent::PlaybackController(self.player.snapshot()));
    }
}

/// 失败回报里的引擎详情：折叠空白并截断，避免把整段错误/URI 塞进聊天。
fn compact_failure_detail(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut truncated: String = compact.chars().take(120).collect();
    if compact.chars().count() > 120 {
        truncated.push('…');
    }
    truncated
}

/// 让播放控制器完成终态记录，但子进程等待重载期间不要将该状态转换为新的自动播放请求。
/// 持久化队列会跨越拆除过程，由替代进程继续消费。
fn suppress_automatic_advance_for_reload(
    decision: QueueAdvanceDecision,
    pending_reload: bool,
) -> QueueAdvanceDecision {
    if pending_reload && matches!(decision, QueueAdvanceDecision::AdvanceQueue { .. }) {
        QueueAdvanceDecision::PlaybackStateChanged
    } else {
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    /// 首次必须 warn：抑制不能把故障的第一条痕迹也吞掉。
    #[test]
    fn repeat_warn_warns_immediately_on_first_failure() {
        let mut gate = RepeatWarn::default();
        let now = Instant::now();
        assert!(gate.should_warn(now), "首次失败必须 warn，不能一上来就静默");
    }

    /// 窗口内重复出现降为 debug（返回 false），这是抑制的核心。
    #[test]
    fn repeat_warn_suppresses_repeats_inside_the_window() {
        let mut gate = RepeatWarn::default();
        let start = Instant::now();
        assert!(gate.should_warn(start), "首次 warn");
        // 窗口内的每一次重复都不该再 warn。
        for offset_ms in [1u64, 500, 1_000, 16_000, 59_999] {
            let later = start + Duration::from_millis(offset_ms);
            assert!(
                !gate.should_warn(later),
                "窗口内（{offset_ms}ms）重复不应 warn，否则高频循环会刷屏"
            );
        }
    }

    /// 窗口过后仍失败要再 warn 一次：问题没自愈，必须重新提醒。
    #[test]
    fn repeat_warn_warns_again_after_the_window() {
        let mut gate = RepeatWarn::default();
        let start = Instant::now();
        assert!(gate.should_warn(start));
        assert!(!gate.should_warn(start + Duration::from_millis(59_999)));
        assert!(
            gate.should_warn(start + REPEAT_WARN_INTERVAL),
            "窗口过后仍在失败必须再 warn，否则日志里只剩一次、看不出持续故障"
        );
        // 重新 warn 之后窗口重新计时。
        assert!(!gate.should_warn(start + REPEAT_WARN_INTERVAL + Duration::from_millis(1)));
        assert!(gate.should_warn(start + REPEAT_WARN_INTERVAL * 2));
    }

    /// 模拟真实轮询节奏：60ms 一轮跑 10 秒，只应产生 1 条 warn。
    ///
    /// 这条直接对应任务里的量化依据：`wait_for_retry` 每秒约 16 次。
    #[test]
    fn repeat_warn_caps_a_sixty_millisecond_loop_to_one_warn_per_window() {
        let mut gate = RepeatWarn::default();
        let start = Instant::now();
        let mut warns = 0usize;
        let mut ticks = 0usize;
        for tick in 0..167u64 {
            // 167 × 60ms ≈ 10 秒
            let now = start + Duration::from_millis(tick * 60);
            if gate.should_warn(now) {
                warns += 1;
            }
            ticks += 1;
        }
        assert_eq!(ticks, 167);
        assert_eq!(
            warns, 1,
            "10 秒内 167 次失败只该留 1 条 warn，实际 {warns} 条"
        );
    }

    /// 不同故障点各自计时：一个点刚 warn 过，另一个点的首次仍要 warn。
    #[test]
    fn separate_gates_do_not_suppress_each_other() {
        let mut queue_full = RepeatWarn::default();
        let mut target_inactive = RepeatWarn::default();
        let now = Instant::now();
        assert!(queue_full.should_warn(now));
        assert!(
            target_inactive.should_warn(now + Duration::from_millis(60)),
            "不同故障点的首次告警不能被另一个点压掉"
        );
    }

    #[test]
    fn deferred_turtle_reply_uses_independent_command_store_override() {
        use crate::config::{BootstrapConfig, HttpConfig, LoggingConfig};
        use crate::config::{ConfigStore, StoredReply};
        use std::collections::BTreeMap;

        let root = std::env::temp_dir().join(format!("deferred-reply-{}", uuid::Uuid::new_v4()));
        let database_path = root.join("deps/data/playback.sqlite3");
        std::fs::create_dir_all(database_path.parent().unwrap()).unwrap();
        let bootstrap = BootstrapConfig {
            database_path: database_path.clone(),
            http: HttpConfig::default(),
            logging: LoggingConfig::default(),
        };
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        let updates = BTreeMap::from([(
            "#海龟汤".to_string(),
            vec![StoredReply {
                key: "turtle_soup.status".to_string(),
                text: "状态={阶段}/{秒数}/{人数}/{提问数}".to_string(),
            }],
        )]);
        store.save_reply_templates(0, &updates).unwrap();
        let store = Arc::new(Mutex::new(store));
        let spec = crate::features::reply_template::turtle_soup::STATUS;
        let vars = crate::features::reply_template::ReplyVars::new()
            .with("阶段", "进行中")
            .with("秒数", "12")
            .with("人数", "3")
            .with("提问数", "2");
        let override_text = command_reply_override(&store, spec.key).unwrap();
        assert_eq!(
            crate::features::reply_template::render(&spec, Some(&override_text), &vars).text,
            "状态=进行中/12/3/2"
        );
        assert!(!command_reply_is_muted(&store, spec.key));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn long_worker_wait_stops_promptly_after_shutdown_signal() {
        let running = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::clone(&running);
        let worker = thread::spawn(move || {
            interruptible_sleep(Duration::from_secs(60), || {
                shutdown.load(AtomicOrdering::SeqCst)
            })
        });
        thread::sleep(Duration::from_millis(20));
        running.store(false, AtomicOrdering::SeqCst);

        assert!(!worker.join().expect("interruptible wait worker"));
    }

    #[test]
    fn shutdown_signal_wakes_a_waiter_that_reentered_after_engine_seal() {
        let running = Arc::new(AtomicBool::new(true));
        let task_engine = TaskEngineHandle::new(None);
        let worker_running = Arc::clone(&running);
        let worker_engine = task_engine.clone();
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut generation = worker_engine.generation().expect("initial generation");
            while worker_running.load(AtomicOrdering::SeqCst) {
                waiting_tx.send(()).expect("report wait boundary");
                generation = worker_engine
                    .wait_for_change(generation)
                    .expect("wait for task-engine change");
            }
        });

        waiting_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker reached initial wait");
        assert!(task_engine.begin_shutdown_if_idle().unwrap());
        waiting_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker reentered wait after engine seal");

        running.store(false, AtomicOrdering::SeqCst);
        task_engine.wake().unwrap();
        worker
            .join()
            .expect("waiting worker exits after final wake");
    }

    /// 只为测试准备的后台歌词任务体：不碰播放器，只按控制信号跑完。
    ///
    /// 真实 [`BackgroundLyricsRequest`] 需要整条播放器链路，这里验证的是
    /// 「至多一条线程」这层生命周期，两者互不干扰。
    /// 等到条件成立；超时直接失败，避免用例在坏实现上挂死。
    fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(Instant::now() < deadline, "{what}（等待超时）");
            thread::yield_now();
        }
    }

    /// 等到后台线程真正结束，而不只是任务体置了标志。
    fn wait_until_worker_exits(worker: &BackgroundLyricsWorker, finished: &AtomicBool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            worker.reap().expect("reap finished worker");
            let running = worker
                .inner
                .task
                .lock()
                .expect("task state")
                .as_ref()
                .is_some_and(|task| !task.worker.is_finished());
            if !running {
                assert!(finished.load(AtomicOrdering::SeqCst), "任务体应当已经跑完");
                return;
            }
            assert!(Instant::now() < deadline, "后台歌词线程未在预期时间内退出");
            thread::yield_now();
        }
    }

    struct TestLyricsTask {
        started: Arc<AtomicUsize>,
        finished: Arc<AtomicBool>,
        hold: bool,
    }

    impl BackgroundLyricsWorker {
        fn start_test_task(&self, task: TestLyricsTask) -> Result<bool> {
            self.start_spawn(move |stop| {
                task.started.fetch_add(1, AtomicOrdering::SeqCst);
                while task.hold && !stop.load(AtomicOrdering::SeqCst) {
                    thread::yield_now();
                }
                task.finished.store(true, AtomicOrdering::SeqCst);
            })
        }
    }

    #[test]
    fn finished_background_lyrics_worker_is_reaped_before_restart() {
        let worker = BackgroundLyricsWorker::new();
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicBool::new(false));

        assert!(
            worker
                .start_test_task(TestLyricsTask {
                    started: Arc::clone(&started),
                    finished: Arc::clone(&finished),
                    hold: false,
                })
                .expect("start one-shot worker")
        );

        // 任务体置位 `finished` 与线程真正退出之间有一小段窗口，`reap` 认的是
        // 后者（`JoinHandle::is_finished`）。这里等到线程确实结束再重启，
        // 与生产路径「下一次 `@歌词` 先 reap 再启动」的时序一致。
        wait_until_worker_exits(&worker, &finished);

        // 一次性任务自己退出后必须能被回收，否则下次 `@歌词` 永远起不来。
        assert!(
            worker
                .start_test_task(TestLyricsTask {
                    started: Arc::clone(&started),
                    finished: Arc::clone(&finished),
                    hold: true,
                })
                .expect("restart after one-shot worker")
        );
        // 线程是异步调度的：`start` 返回只代表已登记，任务体要等被调度才计数。
        wait_until(
            || started.load(AtomicOrdering::SeqCst) == 2,
            "重启的线程应当跑起来",
        );
        assert!(worker.stop().expect("stop restarted worker"));
    }

    #[test]
    fn concurrent_start_of_background_lyrics_keeps_exactly_one_thread() {
        let worker = BackgroundLyricsWorker::new();
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let requests = 8;
        let mut handles = Vec::with_capacity(requests);
        for _ in 0..requests {
            let worker = worker.clone();
            let started = Arc::clone(&started);
            let finished = Arc::clone(&finished);
            handles.push(thread::spawn(move || {
                worker
                    .start_test_task(TestLyricsTask {
                        started,
                        finished,
                        hold: true,
                    })
                    .expect("start background lyrics")
            }));
        }
        let accepted = handles
            .into_iter()
            .map(|handle| handle.join().expect("start thread"))
            .filter(|started| *started)
            .count();

        assert_eq!(
            accepted, 1,
            "并发触发只应成功启动一条后台歌词线程，实际 {accepted} 条"
        );
        assert_eq!(started.load(AtomicOrdering::SeqCst), 1);
        assert!(worker.stop().expect("stop the single worker"));
        assert!(!worker.stop().expect("second stop is a no-op"));
    }

    #[test]
    fn dropping_background_lyrics_worker_stops_the_running_thread() {
        let worker = BackgroundLyricsWorker::new();
        let started = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let other = worker.clone();

        assert!(
            worker
                .start_test_task(TestLyricsTask {
                    started: Arc::clone(&started),
                    finished: Arc::clone(&finished),
                    hold: true,
                })
                .expect("start held worker")
        );
        while started.load(AtomicOrdering::SeqCst) == 0 {
            thread::yield_now();
        }

        drop(worker);
        drop(other);
        // 最后一个句柄消失时 `Drop` 必须兜底停掉线程，而不是把它留成野线程。
        assert!(finished.load(AtomicOrdering::SeqCst));
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

    #[test]
    fn pending_reload_suppresses_automatic_advance_but_keeps_state_updates() {
        assert_eq!(
            suppress_automatic_advance_for_reload(
                QueueAdvanceDecision::AdvanceQueue {
                    reason: "自然结束"
                },
                true,
            ),
            QueueAdvanceDecision::PlaybackStateChanged
        );
        assert_eq!(
            suppress_automatic_advance_for_reload(QueueAdvanceDecision::None, true),
            QueueAdvanceDecision::None
        );
        assert_eq!(
            suppress_automatic_advance_for_reload(
                QueueAdvanceDecision::AdvanceQueue {
                    reason: "自然结束"
                },
                false,
            ),
            QueueAdvanceDecision::AdvanceQueue {
                reason: "自然结束"
            }
        );
    }
}
