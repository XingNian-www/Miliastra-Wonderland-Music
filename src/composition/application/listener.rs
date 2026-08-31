use super::secondary_chat::{SecondaryHallCommandTracker, SecondaryListenerRoundState};
use super::*;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

const CHAT_LISTENER_MODE_STATE_SUFFIX: &str = ".chat-listener-mode.json";
const CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CHAT_LISTENER_UNRESOLVED_UI_GRACE: Duration = Duration::from_secs(5);
const CONFIG_RELOAD_STARTUP_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CONFIG_RELOAD_UI_FALLBACK_GRACE: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListenerReadyEvidence {
    PrimaryScan,
    SecondaryCurrentHall,
}

fn chat_listener_mode_path(playback_state_path: &Path) -> PathBuf {
    let mut file_name = playback_state_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("playback-state"))
        .to_os_string();
    file_name.push(CHAT_LISTENER_MODE_STATE_SUFFIX);
    playback_state_path.with_file_name(file_name)
}

fn listener_mode_matches_ui(
    mode: ChatListenerMode,
    temporary_primary: bool,
    ui_kind: UiStateKind,
) -> bool {
    matches!(
        (listener_residency(mode, temporary_primary), ui_kind),
        (UiResidency::Primary, UiStateKind::Primary)
            | (UiResidency::SecondaryCurrentHall, UiStateKind::Secondary)
    )
}

fn primary_rescan_can_publish(stable_kind: Option<UiStateKind>) -> bool {
    stable_kind == Some(UiStateKind::Primary)
}

fn listener_ready_after_recheck(
    evidence: Option<ListenerReadyEvidence>,
    snapshot: &ChatListenerSnapshot,
    task_busy: bool,
    paused: bool,
    running: bool,
) -> bool {
    if !running
        || paused
        || task_busy
        || snapshot.pending_mode.is_some()
        || snapshot.unread_task_pending
    {
        return false;
    }
    match evidence {
        Some(ListenerReadyEvidence::PrimaryScan) => {
            snapshot.mode == ChatListenerMode::Primary
                || (snapshot.mode == ChatListenerMode::Secondary && snapshot.temporary_primary)
        }
        Some(ListenerReadyEvidence::SecondaryCurrentHall) => {
            snapshot.mode == ChatListenerMode::Secondary
                && !snapshot.temporary_primary
                && !snapshot.initial_unread_clear
                && !snapshot.hall_round_required
        }
        None => false,
    }
}

fn replacement_ready_allowed(listener_ready: bool, startup_ready: bool) -> bool {
    listener_ready && startup_ready
}

fn unresolved_ui_recovery_due(
    unresolved_since: Option<Instant>,
    now: Instant,
    retry_after: Instant,
    pending_mode: bool,
    task_busy: bool,
) -> bool {
    !pending_mode
        && !task_busy
        && now >= retry_after
        && unresolved_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= CHAT_LISTENER_UNRESOLVED_UI_GRACE
        })
}

fn secondary_hall_recovery_allowed(now: Instant, retry_after: Instant) -> bool {
    // A pending reload can remain blocked indefinitely by external or uncertain playback.
    // Recovery itself becomes a formal task, so it safely delays shutdown without leaving a
    // secondary listener stranded outside its current hall.
    now >= retry_after
}

// A pending reload may remain blocked indefinitely by external or uncertain playback. It is
// deliberately not a gate for replacement startup recovery: the child must finish startup and
// publish readiness before the watchdog can treat it as a normal process.
fn replacement_startup_retry_due(
    config_reload_child: bool,
    child_ready: bool,
    startup_actions_missing: bool,
    task_engine_idle: bool,
    now: Instant,
    retry_after: Instant,
) -> bool {
    config_reload_child
        && !child_ready
        && startup_actions_missing
        && task_engine_idle
        && now >= retry_after
}

#[allow(clippy::too_many_arguments)]
fn replacement_startup_fallback_due(
    config_reload_child: bool,
    child_ready: bool,
    startup_gate_active: bool,
    startup_actions_available: bool,
    task_engine_idle: bool,
    paused: bool,
    now: Instant,
    unverified_since: Option<Instant>,
) -> bool {
    config_reload_child
        && !child_ready
        && !startup_gate_active
        && startup_actions_available
        && task_engine_idle
        && !paused
        && unverified_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= CONFIG_RELOAD_UI_FALLBACK_GRACE
        })
}

pub(super) fn load_persisted_chat_listener_mode(
    playback_state_path: &Path,
) -> Result<Option<ChatListenerMode>> {
    let path = chat_listener_mode_path(playback_state_path);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取聊天监听模式状态失败: {}", path.display()));
        }
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("解析聊天监听模式状态失败: {}", path.display()))
        .map(Some)
}

pub(super) fn persist_chat_listener_mode(
    playback_state_path: &Path,
    mode: ChatListenerMode,
) -> Result<()> {
    let path = chat_listener_mode_path(playback_state_path);
    let bytes = serde_json::to_vec(&mode).context("序列化聊天监听模式状态失败")?;
    crate::adapters::file_store::write_atomic(&path, &bytes, "聊天监听模式状态")
}

use crate::features::administration::{
    AdministrationCommandContext, AdministrationDispatch, ImmediateAdministrationOutcome,
};

enum ObservedInput<C, Q> {
    Command(C),
    Question(Q),
}

struct HttpHallDetector {
    hall_ui: HallUi,
}

impl http::HttpHallPort for HttpHallDetector {
    fn capture_hall_screenshot(&self) -> Result<Arc<DynamicImage>> {
        let outcome = self
            .hall_ui
            .submit_read(ReadHallInfo)
            .context("提交大厅截图检测 UI 事务")?
            .wait()
            .context("等待大厅截图检测 UI 事务")?;
        if let ReadHallInfoEffect::Failed(failure) = outcome.effect() {
            return Err(anyhow!("大厅截图检测失败：{failure}"));
        }
        if let UiResidencyOutcome::Failed(failure) = outcome.residency() {
            return Err(anyhow!("大厅截图已生成，但一级驻留恢复失败：{failure}"));
        }
        outcome
            .screenshot()
            .ok_or_else(|| anyhow!("大厅检测未返回截图"))
    }
}

fn merge_observed_inputs<C, Q>(
    commands: Vec<(usize, C)>,
    questions: Vec<(usize, Q)>,
) -> Vec<ObservedInput<C, Q>> {
    let mut inputs = commands
        .into_iter()
        .map(|(order, command)| (order, ObservedInput::Command(command)))
        .chain(
            questions
                .into_iter()
                .map(|(order, question)| (order, ObservedInput::Question(question))),
        )
        .collect::<Vec<_>>();
    inputs.sort_by_key(|(order, _)| *order);
    inputs.into_iter().map(|(_, input)| input).collect()
}

fn attach_question_orders<Q: PartialEq>(
    mut visible: Vec<(usize, Q)>,
    accepted: Vec<Q>,
) -> Result<Vec<(usize, Q)>> {
    accepted
        .into_iter()
        .map(|question| {
            let order = visible
                .iter()
                .position(|(_, candidate)| candidate == &question)
                .map(|index| visible.remove(index).0)
                .ok_or_else(|| anyhow!("稳定海龟汤提问无法映射回当前观察帧"))?;
            Ok((order, question))
        })
        .collect()
}

fn primary_command_candidates(
    messages: &[PrimaryObservedMessage],
) -> impl Iterator<Item = (usize, &PrimaryObservedMessage)> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.is_new)
}

impl ApplicationRuntime {
    fn persist_listener_mode_best_effort(&self, mode: ChatListenerMode) {
        if let Err(error) =
            persist_chat_listener_mode(&self.lifecycle.config.state.playback_state_path, mode)
        {
            log::error!("持久化聊天监听模式失败: {error:#}");
        }
    }

    pub(super) fn persist_current_chat_listener_mode(&self) {
        match self.business.business.chat_listener_snapshot() {
            Ok(snapshot) => self.persist_listener_mode_best_effort(snapshot.mode),
            Err(error) => log::error!("读取聊天监听模式用于持久化失败: {error}"),
        }
    }

    fn complete_chat_listener_mode(&self, mode: ChatListenerMode) -> Result<()> {
        self.business.business.complete_chat_listener_mode(mode)?;
        self.persist_listener_mode_best_effort(mode);
        Ok(())
    }

    pub(super) fn fail_chat_listener_mode_to_primary(&self) -> Result<()> {
        self.business
            .business
            .fail_chat_listener_mode_to_primary()?;
        self.persist_listener_mode_best_effort(ChatListenerMode::Primary);
        Ok(())
    }

    pub(super) fn clear_hall_countdown_cache_for_new_visual_session(
        &self,
        reason: &str,
    ) -> Result<bool> {
        let cleared = self.business.business.clear_hall_countdown_cache()?;
        let visual_session = self.ui.chat_observations.begin_visual_session()?;
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
        marker_hits: Option<Vec<crate::ui::template::TemplateHit>>,
    ) -> Result<Vec<ChatMessage>> {
        scan_chat_with_shared_ocr(
            &self.ui.ocr,
            &self.lifecycle.monitor,
            self.lifecycle.config.screen.chat_rect.into(),
            image,
            templates,
            marker_hits,
        )
    }

    pub(super) fn warn_if_screen_size_mismatch(&self) -> Result<()> {
        let frame = match self.ui.game_ui.capture() {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!("启动时未能截图，扫描循环将等待目标窗口恢复: {error:#}");
                return Ok(());
            }
        };
        if self.lifecycle.config.screen.warn_on_size_mismatch
            && (frame.width() != self.lifecycle.config.screen.expected_width
                || frame.height() != self.lifecycle.config.screen.expected_height)
        {
            log::warn!(
                "截图尺寸为 {}x{}，预期 {}x{}，程序继续运行",
                frame.width(),
                frame.height(),
                self.lifecycle.config.screen.expected_width,
                self.lifecycle.config.screen.expected_height
            );
        }
        Ok(())
    }

    pub(super) fn start_http_server(&mut self) -> Result<()> {
        if !self.lifecycle.config.http.enabled {
            return Ok(());
        }
        if self.lifecycle.http_server.is_some() {
            return Err(anyhow!("HTTP/Web 面板已经启动"));
        }
        let player_runtime = self
            .playback
            .player_runtime
            .as_ref()
            .ok_or_else(|| anyhow!("播放器运行时尚未启动"))?
            .handle();
        let formal_tasks = Arc::new(
            self.business
                .formal_tasks
                .clone()
                .ok_or_else(|| anyhow!("正式任务执行运行时尚未启动"))?,
        );
        let config_store = self
            .lifecycle
            .config_store
            .clone()
            .ok_or_else(|| anyhow!("配置中心存储尚未初始化"))?;
        let http_config = self.lifecycle.config.http.clone();
        let http_state = http::HttpSharedState::new(
            http::HttpInterfaceConfig::new(
                http_config,
                self.lifecycle.config.screen.clone(),
                self.lifecycle.config.templates.clone(),
                self.lifecycle.config.moderation.clone(),
                self.lifecycle.config.startup.clone(),
                self.lifecycle.config.invite.clone(),
                self.lifecycle.config.timing.clone(),
                self.lifecycle.config.custom_workflows.clone(),
            ),
            self.lifecycle.monitor.clone(),
            self.ui.latest_frame.clone(),
            config_store,
            self.lifecycle.live_configs.clone(),
            http::HttpApplicationPorts::new(
                Arc::new(http_facade::ApplicationHttpCommandFacade::new(
                    formal_tasks.clone(),
                    self.business.custom_workflow.clone(),
                )),
                formal_tasks.clone(),
                formal_tasks,
                Arc::new(HttpHallDetector {
                    hall_ui: self.ui.hall_ui.clone(),
                }),
                Arc::new(http_facade::ApplicationHttpPlayerFacade::new(
                    PlayerRuntimeBackend::new(player_runtime),
                    self.playback.player_search.clone(),
                    self.playback.native_playback.clone(),
                    self.playback.playback_application.play_mode_handle(),
                )),
                Arc::new(http_facade::ApplicationHttpLoginFacade::new(
                    self.playback.login_helper.clone(),
                )),
                Arc::new(http_facade::ApplicationHttpAiFacade::new(
                    self.business.ai.clone(),
                )),
            ),
        );
        self.lifecycle.http_reload_blockers = http_state.active_reload_blockers.clone();
        self.lifecycle.http_reload_draining = http_state.reload_draining.clone();
        let server = http::start(http_state)?;
        self.lifecycle.http_server = Some(server);
        Ok(())
    }

    pub(super) fn start_hotkeys(&self) -> Result<hotkeys::HotkeyRuntime> {
        let task_engine = self.business.task_engine.clone();
        let shutdown = self.lifecycle.shutdown.clone();
        hotkeys::start(
            &self.lifecycle.config.hotkeys,
            Arc::clone(&self.lifecycle.running),
            Arc::clone(&self.lifecycle.paused),
            Arc::new(move || shutdown.request_user_exit()),
            Arc::new(move || {
                if let Err(error) = task_engine.wake() {
                    log::debug!("热键切换暂停状态后唤醒任务引擎失败: {error}");
                }
            }),
        )
    }

    fn activate_reload_startup_fallback(&self, reason: &str) -> Result<bool> {
        if std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_none()
            || crate::config_reload_child_ready()
        {
            return Ok(false);
        }
        if !self.lifecycle.reload_startup.required_actions().is_empty() {
            return Ok(false);
        }
        if !self.require_reload_startup_automation()? {
            return Ok(false);
        }
        log::warn!("配置重载替代进程无法复用业务界面，启用启动自动化回退: {reason}");
        Ok(true)
    }

    fn retry_reload_startup_if_needed(
        &self,
        config_reload_child: bool,
        retry_after: &mut Instant,
    ) -> Result<()> {
        let missing = !self.lifecycle.reload_startup.missing_actions().is_empty();
        let now = Instant::now();
        if !replacement_startup_retry_due(
            config_reload_child,
            crate::config_reload_child_ready(),
            missing,
            self.business.task_engine.is_idle()?,
            now,
            *retry_after,
        ) {
            return Ok(());
        }
        match self.enqueue_reload_startup_tasks_if_needed() {
            Ok(true) => log::warn!("配置重载替代进程 startup 尚未完成，已按冷却重新排队缺失动作"),
            Ok(false) => {}
            Err(error) => log::error!("配置重载替代进程重排启动自动化失败: {error:#}"),
        }
        *retry_after = now + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
        Ok(())
    }

    fn maybe_activate_reload_startup_after_ui_grace(
        &self,
        config_reload_child: bool,
        task_engine_idle: bool,
        unverified_since: &mut Option<Instant>,
        retry_after: &mut Instant,
        reason: &str,
    ) -> Result<()> {
        let now = Instant::now();
        let paused = self.lifecycle.paused.load(AtomicOrdering::SeqCst);
        let startup_gate_active = !self.lifecycle.reload_startup.required_actions().is_empty();
        let startup_actions_available =
            !ReloadStartupActions::from_startup_config(&self.lifecycle.config).is_empty();
        if config_reload_child
            && !crate::config_reload_child_ready()
            && !startup_gate_active
            && startup_actions_available
            && task_engine_idle
            && !paused
        {
            let since = *unverified_since.get_or_insert(now);
            if replacement_startup_fallback_due(
                config_reload_child,
                crate::config_reload_child_ready(),
                startup_gate_active,
                startup_actions_available,
                task_engine_idle,
                paused,
                now,
                Some(since),
            ) && self.activate_reload_startup_fallback(reason)?
            {
                *unverified_since = None;
                *retry_after = now + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
            }
        } else {
            *unverified_since = None;
        }
        Ok(())
    }

    pub(super) fn run_scan_loop(&mut self) -> Result<()> {
        let mut completion_subscriber = self
            .ui
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
        let template_args = self.ui.chat_templates.clone();
        let canvas = Canvas {
            width: self.lifecycle.config.screen.expected_width,
            height: self.lifecycle.config.screen.expected_height,
            resize: true,
        };
        let ui_handle = self
            .ui
            .ui_runtime
            .as_ref()
            .context("UI runtime 在扫描循环启动前已停止")?
            .handle();
        let initial_live_config = self.lifecycle.live_configs.snapshot();
        let mut frame_demand_ms = initial_live_config.timing.loop_idle_ms.max(1);
        let mut frame_demand = FrameDemand::new(Duration::from_millis(frame_demand_ms))
            .context("创建聊天观察帧需求")?;
        let mut frame_subscription: Option<FrameDemandSubscription> = None;
        let mut last_fingerprint: Option<ChangeFingerprint> = None;
        let mut last_ocr_at = Instant::now()
            - Duration::from_millis(initial_live_config.timing.chat_scan.fallback_ms);
        let mut last_change_ocr_at = Instant::now()
            - Duration::from_millis(initial_live_config.timing.chat_scan.change_cooldown_ms);
        let mut suppress_change_until = Instant::now();
        let mut force_scan_after: Option<Instant> = None;
        let mut force_scan_reason: Option<&'static str> = None;
        let mut primary_visible = false;
        let mut secondary_friend_bubble_fingerprint: Option<ChangeFingerprint> = None;
        let mut secondary_hall_bubble_sequence: Option<Vec<SecondaryHallBubble>> = None;
        let mut secondary_hall_command_tracker = SecondaryHallCommandTracker::default();
        let mut secondary_identity: Option<SecondaryChatIdentity> = None;
        let mut listener_residency_retry_after = Instant::now();
        let config_reload_child = std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_some();
        let mut reload_startup_retry_after = Instant::now() + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
        let mut replacement_ui_unverified_since = config_reload_child.then(Instant::now);
        let mut unresolved_ui_since: Option<Instant> = None;
        let mut target_missing_backoff = TARGET_MISSING_BACKOFF_INITIAL;
        let mut target_missing = false;
        log::info!("自动化扫描已启动");
        while self.lifecycle.running.load(AtomicOrdering::SeqCst) {
            let live_config = self.lifecycle.live_configs.snapshot();
            self.retry_reload_startup_if_needed(
                config_reload_child,
                &mut reload_startup_retry_after,
            )?;
            let next_frame_demand_ms = live_config.timing.loop_idle_ms.max(1);
            if next_frame_demand_ms != frame_demand_ms {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!("主循环间隔变化时撤销旧观察帧需求失败: {error}");
                }
                frame_demand = FrameDemand::new(Duration::from_millis(next_frame_demand_ms))
                    .context("更新聊天观察帧需求")?;
                frame_demand_ms = next_frame_demand_ms;
            }
            self.forward_completion_advances(completion_subscriber)?;
            let loop_started = Instant::now();
            self.update_monitor_operational_state();
            self.tick_entertainment();
            if !self.ui.coordinator.scan_may_run() {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!("正式任务占用 UI 时撤销观察帧需求失败: {error}");
                }
                self.invalidate_latest_frame();
                primary_visible = false;
                last_fingerprint = None;
                force_scan_after = None;
                force_scan_reason = None;
                secondary_friend_bubble_fingerprint = None;
                secondary_hall_bubble_sequence = None;
                secondary_hall_command_tracker.reset();
                secondary_identity = None;
                unresolved_ui_since = None;
                self.maybe_idle_exit()?;
                self.maybe_reload_config_when_idle();
                if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                    continue;
                }
                sleep(
                    self.reload_aware_wait(Duration::from_millis(live_config.timing.loop_idle_ms)),
                );
                continue;
            }
            if self.lifecycle.paused.load(AtomicOrdering::SeqCst) {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!("暂停监听时撤销观察帧需求失败: {error}");
                }
                unresolved_ui_since = None;
                self.maybe_idle_exit()?;
                self.maybe_reload_config_when_idle();
                if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                    continue;
                }
                sleep(
                    self.reload_aware_wait(Duration::from_millis(live_config.timing.loop_idle_ms)),
                );
                continue;
            }

            if frame_subscription.is_none() {
                frame_subscription = Some(
                    ui_handle
                        .declare_frame_demand(frame_demand)
                        .context("向 UI runtime 声明聊天观察帧需求")?,
                );
            }

            let mut listener_ready_evidence = None;
            let frame_started = Instant::now();
            match receive_observation_frame(
                frame_subscription
                    .as_ref()
                    .expect("frame subscription initialized above"),
                &canvas,
            ) {
                Ok(Some(frame)) => {
                    if let Ok(mut latest_frame) = self.ui.latest_frame.lock() {
                        latest_frame.store(Arc::clone(&frame.image));
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
                    let ui_state_result: Result<(String, Option<UiStateKind>)> =
                        match frame.ui_state.clone() {
                            Some(UiStateObservation::Classified(state)) => {
                                Ok((state.to_string(), state.stable_kind()))
                            }
                            Some(UiStateObservation::Failed { reason, .. }) => {
                                Err(anyhow!(reason.to_string()))
                            }
                            None => Err(anyhow!(
                                "UI runtime 未附带统一模板状态观察，拒绝局部重复判定"
                            )),
                        };
                    match &ui_state_result {
                        Ok((ui_state, _)) => self
                            .lifecycle
                            .monitor
                            .publish(MonitorEvent::UiState(ui_state.clone())),
                        Err(_) => self
                            .lifecycle
                            .monitor
                            .publish(MonitorEvent::UiState("界面检测失败".to_string())),
                    }
                    let ui_ms = elapsed_ms(ui_started);
                    let listener_snapshot = self.business.business.chat_listener_snapshot()?;
                    let command_executing = !self.business.task_engine.is_idle()?;
                    let mut residency_recovery_attempted = false;
                    let unresolved_ui_reason = match &ui_state_result {
                        Ok((ui_state, None)) => Some(ui_state.clone()),
                        Err(error) => Some(format!("状态分类失败: {error:#}")),
                        Ok((_, Some(_))) => None,
                    };
                    match &ui_state_result {
                        Ok((ui_state, Some(ui_kind))) => {
                            unresolved_ui_since = None;
                            let mode_matches = listener_mode_matches_ui(
                                listener_snapshot.mode,
                                listener_snapshot.temporary_primary,
                                *ui_kind,
                            );
                            let can_recover_residency = !mode_matches
                                && listener_snapshot.pending_mode.is_none()
                                && !command_executing
                                && *ui_kind != UiStateKind::Unknown
                                && Instant::now() >= listener_residency_retry_after;
                            if can_recover_residency {
                                let target = listener_residency(
                                    listener_snapshot.mode,
                                    listener_snapshot.temporary_primary,
                                );
                                log::warn!(
                                    "聊天监听逻辑模式与当前界面不一致，尝试恢复驻留: mode={} ui={} target={target:?}",
                                    listener_snapshot.mode.label(),
                                    ui_state
                                );
                                match self.establish_ui_residency(
                                    target,
                                    ResidencyPurpose::IndependentRecovery("恢复持久化聊天监听驻留"),
                                ) {
                                    Ok(()) => {
                                        log::info!("聊天监听驻留恢复完成: target={target:?}")
                                    }
                                    Err(error) => log::warn!(
                                        "聊天监听驻留恢复失败，保留逻辑模式并稍后重试: {error:#}"
                                    ),
                                }
                                listener_residency_retry_after =
                                    Instant::now() + CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL;
                                residency_recovery_attempted = true;
                            }
                        }
                        _ if unresolved_ui_reason.is_some() => {
                            let ui_state = unresolved_ui_reason
                                .as_deref()
                                .expect("unresolved UI reason is present");
                            let now = Instant::now();
                            let unresolved_since = *unresolved_ui_since.get_or_insert(now);
                            if unresolved_ui_recovery_due(
                                Some(unresolved_since),
                                now,
                                listener_residency_retry_after,
                                listener_snapshot.pending_mode.is_some(),
                                command_executing,
                            ) {
                                let target = listener_residency(
                                    listener_snapshot.mode,
                                    listener_snapshot.temporary_primary,
                                );
                                log::warn!(
                                    "界面持续无法验证，尝试恢复聊天监听驻留: mode={} ui={} target={target:?}",
                                    listener_snapshot.mode.label(),
                                    ui_state
                                );
                                match self.establish_ui_residency(
                                    target,
                                    ResidencyPurpose::IndependentRecovery(
                                        "恢复持续未知界面的聊天监听驻留",
                                    ),
                                ) {
                                    Ok(()) => log::info!(
                                        "持续未知界面的聊天监听驻留恢复完成: target={target:?}"
                                    ),
                                    Err(error) => log::warn!(
                                        "持续未知界面的聊天监听驻留恢复失败，稍后重试: {error:#}"
                                    ),
                                }
                                listener_residency_retry_after =
                                    Instant::now() + CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL;
                                unresolved_ui_since = Some(Instant::now());
                                residency_recovery_attempted = true;
                            }
                        }
                        _ => {
                            unreachable!("unresolved UI reason must cover every non-stable result")
                        }
                    }
                    if residency_recovery_attempted {
                        primary_visible = false;
                        last_fingerprint = None;
                        secondary_friend_bubble_fingerprint = None;
                        secondary_hall_bubble_sequence = None;
                        secondary_hall_command_tracker.reset();
                        secondary_identity = None;
                        log::debug!("驻留恢复后废弃当前旧观察帧，下一轮重新验证");
                    } else {
                        match ui_state_result {
                            Ok((ui_state, None)) => {
                                log::debug!("界面仍在过渡，暂停聊天扫描: {}", ui_state);
                                log::info!(target: "timing",
                                    "主循环阶段耗时: total={}ms frame={}ms ui={}ms state={} scanned=false",
                                    elapsed_ms(loop_started),
                                    frame_ms,
                                    ui_ms,
                                    ui_state
                                );
                            }
                            Ok((ui_state, Some(ui_kind)))
                                if listener_snapshot.mode == ChatListenerMode::Secondary
                                    && !listener_snapshot.temporary_primary =>
                            {
                                primary_visible = false;
                                last_fingerprint = None;
                                let secondary_started = Instant::now();
                                let allow_hall_recovery = secondary_hall_recovery_allowed(
                                    Instant::now(),
                                    listener_residency_retry_after,
                                );
                                let outcome = if ui_kind == UiStateKind::Secondary {
                                    Some(self.run_secondary_listener_round(
                                        &frame.image,
                                        SecondaryListenerRoundState {
                                            last_friend_bubble:
                                                &mut secondary_friend_bubble_fingerprint,
                                            hall_bubble_sequence:
                                                &mut secondary_hall_bubble_sequence,
                                            hall_command_tracker:
                                                &mut secondary_hall_command_tracker,
                                            identity: &mut secondary_identity,
                                        },
                                        allow_hall_recovery,
                                    )?)
                                } else if command_executing {
                                    log::debug!(
                                        "二级监听任务临时离开二级界面，等待任务状态机恢复: {}",
                                        ui_state
                                    );
                                    None
                                } else {
                                    log::warn!(
                                        "二级监听当前不在二级聊天界面: {}，保留逻辑模式并等待驻留恢复",
                                        ui_state
                                    );
                                    secondary_friend_bubble_fingerprint = None;
                                    secondary_hall_bubble_sequence = None;
                                    secondary_hall_command_tracker.reset();
                                    secondary_identity = None;
                                    None
                                };
                                let scanned = outcome.is_some_and(|outcome| outcome.scanned);
                                if outcome.is_some_and(|outcome| outcome.verified) {
                                    listener_ready_evidence =
                                        Some(ListenerReadyEvidence::SecondaryCurrentHall);
                                }
                                if outcome.is_some_and(|outcome| outcome.recovery_requested) {
                                    listener_residency_retry_after =
                                        Instant::now() + CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL;
                                }
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
                            Ok((_ui_state, Some(UiStateKind::Primary))) => {
                                if listener_snapshot.mode == ChatListenerMode::Primary {
                                    secondary_friend_bubble_fingerprint = None;
                                    secondary_hall_bubble_sequence = None;
                                    secondary_hall_command_tracker.reset();
                                    secondary_identity = None;
                                }
                                let primary_started = Instant::now();
                                let entered_primary = !primary_visible;
                                primary_visible = true;
                                let fingerprint = match rect_chat_change_fingerprint(
                                    &frame.image,
                                    self.lifecycle.config.screen.chat_rect.into(),
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
                                            live_config.timing.chat_scan.change_debounce_ms,
                                        );
                                    if force_scan_after.is_none_or(|time| scan_after < time) {
                                        force_scan_after = Some(scan_after);
                                        force_scan_reason = Some("enter-primary");
                                    }
                                    log::info!(target: "timing",
                                        "进入一级界面，已建立聊天区对比基线，快速扫描延迟={}ms",
                                        live_config.timing.chat_scan.change_debounce_ms
                                    );
                                }
                                let change_suppressed = now < suppress_change_until;
                                let forced_scan_due =
                                    force_scan_after.is_some_and(|time| now >= time);
                                let cooldown_until = last_change_ocr_at
                                    + Duration::from_millis(
                                        live_config.timing.chat_scan.change_cooldown_ms,
                                    );
                                let change_stats = fingerprint.as_ref().and_then(|current| {
                                    last_fingerprint
                                        .as_ref()
                                        .map(|previous| change_stats(previous, current))
                                });
                                let change_over_threshold = change_stats.is_some_and(|stats| {
                                    stats.mean_abs_diff
                                        >= self.lifecycle.config.ocr.change_mean_threshold
                                        || stats.changed_ratio
                                            >= self.lifecycle.config.ocr.change_pixel_threshold
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
                                                live_config.timing.chat_scan.fallback_ms,
                                            ));
                                let change_due = change_over_threshold && change_ready;

                                let mut scanned_this_round = false;
                                let mut primary_scan_succeeded = false;
                                if change_due {
                                    let stats = change_stats.expect("change_due requires stats");
                                    log::info!(target: "timing",
                                        "触发聊天扫描: reason=change mean={:.3} ratio={:.5} debounce={}ms",
                                        stats.mean_abs_diff,
                                        stats.changed_ratio,
                                        live_config.timing.chat_scan.change_debounce_ms
                                    );
                                    sleep(Duration::from_millis(
                                        live_config.timing.chat_scan.change_debounce_ms,
                                    ));
                                    let rescan_frame_started = Instant::now();
                                    match receive_observation_frame(
                                        frame_subscription
                                            .as_ref()
                                            .expect("frame subscription initialized above"),
                                        &canvas,
                                    ) {
                                        Ok(Some(frame)) => {
                                            let rescan_stable_kind = match frame.ui_state.as_ref() {
                                                Some(UiStateObservation::Classified(state)) => {
                                                    state.stable_kind()
                                                }
                                                Some(UiStateObservation::Failed { .. }) | None => {
                                                    None
                                                }
                                            };
                                            if !primary_rescan_can_publish(rescan_stable_kind) {
                                                log::warn!(
                                                    "变化防抖后的新观察帧已不再是稳定一级界面，废弃本轮 OCR"
                                                );
                                                primary_visible = false;
                                                last_fingerprint = None;
                                                continue;
                                            }
                                            let rescan_frame_ms = elapsed_ms(rescan_frame_started);
                                            let scan_started = Instant::now();
                                            let observation_frame = self
                                                .ui
                                                .chat_observations
                                                .begin_frame(frame.captured_at)?;
                                            let marker_hits = frame.marker_hits_for_image();
                                            let messages = self.scan_chat_with_shared_ocr(
                                                &frame.image,
                                                &template_args,
                                                marker_hits,
                                            );
                                            let scan_ms = elapsed_ms(scan_started);
                                            log::info!(target: "timing",
                                                "变化扫描阶段耗时: rescan_frame={}ms scan={}ms",
                                                rescan_frame_ms,
                                                scan_ms
                                            );
                                            match messages {
                                                Ok(messages) => {
                                                    self.publish_primary_chat_observation(
                                                        observation_frame,
                                                        messages,
                                                    )?;
                                                    primary_scan_succeeded = true;
                                                }
                                                Err(error) => {
                                                    log::error!("聊天扫描失败: {error:#}");
                                                    if let Err(record_error) = self
                                                        .ui
                                                        .chat_observations
                                                        .record_terminal_failure(
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
                                                self.lifecycle.config.screen.chat_rect.into(),
                                            )
                                            .ok();
                                            scanned_this_round = true;
                                        }
                                        Ok(None) => {
                                            log::debug!("变化后等待新观察帧超时，本轮稍后重试")
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
                                        self.ui.chat_observations.begin_frame(frame.captured_at)?;
                                    let marker_hits = frame.marker_hits_for_image();
                                    let messages = self.scan_chat_with_shared_ocr(
                                        &frame.image,
                                        &template_args,
                                        marker_hits,
                                    );
                                    match messages {
                                        Ok(messages) => {
                                            self.publish_primary_chat_observation(
                                                observation_frame,
                                                messages,
                                            )?;
                                            primary_scan_succeeded = true;
                                        }
                                        Err(error) => {
                                            log::error!("聊天扫描失败: {error:#}");
                                            if let Err(record_error) =
                                                self.ui.chat_observations.record_terminal_failure(
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

                                if primary_scan_succeeded {
                                    listener_ready_evidence =
                                        Some(ListenerReadyEvidence::PrimaryScan);
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
                            Ok((ui_state, Some(UiStateKind::Secondary))) => {
                                primary_visible = false;
                                secondary_friend_bubble_fingerprint = None;
                                secondary_hall_bubble_sequence = None;
                                secondary_hall_command_tracker.reset();
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
                            Ok((ui_state, Some(UiStateKind::Unknown))) => {
                                primary_visible = false;
                                last_fingerprint = None;
                                log::debug!("界面状态仍为过渡态，暂停聊天扫描: {}", ui_state);
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
                }
                Ok(None) => {
                    self.maybe_reload_config_when_idle();
                    if self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                        self.maybe_activate_reload_startup_after_ui_grace(
                            config_reload_child,
                            self.business.task_engine.is_idle()?,
                            &mut replacement_ui_unverified_since,
                            &mut reload_startup_retry_after,
                            "观察帧长期未发布",
                        )?;
                    }
                    continue;
                }
                Err(error) => {
                    self.invalidate_latest_frame();
                    unresolved_ui_since = None;
                    if let Some(subscription) = frame_subscription.take()
                        && let Err(cancel_error) = subscription.cancel()
                    {
                        log::warn!("截图失败后撤销观察帧需求失败: {cancel_error}");
                    }
                    let frame_ms = elapsed_ms(frame_started);
                    if !target_missing {
                        self.abort_entertainment_for_context_loss("目标游戏窗口已关闭或不可用");
                    }
                    self.lifecycle
                        .monitor
                        .publish(MonitorEvent::UiState("目标窗口不可用".to_string()));
                    primary_visible = false;
                    last_fingerprint = None;
                    secondary_friend_bubble_fingerprint = None;
                    secondary_hall_bubble_sequence = None;
                    secondary_hall_command_tracker.reset();
                    secondary_identity = None;
                    let observed_window_detection_generation =
                        self.ui.window_detection_signal.generation()?;
                    let target_missing_wait = self.reload_aware_wait(target_missing_backoff);
                    log::warn!(
                        "截图失败，{}秒后重试: {error:#}",
                        target_missing_wait.as_secs()
                    );
                    log::info!(target: "timing",
                        "主循环阶段耗时: total={}ms frame={}ms state=capture_error retry={}ms",
                        elapsed_ms(loop_started),
                        frame_ms,
                        target_missing_wait.as_millis()
                    );
                    target_missing = true;
                    self.maybe_idle_exit()?;
                    self.maybe_reload_config_when_idle();
                    if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                        continue;
                    }
                    if self.activate_reload_startup_fallback("目标窗口不可用")? {
                        replacement_ui_unverified_since = None;
                        reload_startup_retry_after =
                            Instant::now() + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
                    }
                    if self.ui.window_detection_signal.wait_for_change(
                        observed_window_detection_generation,
                        target_missing_wait,
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
                    + Duration::from_millis(live_config.timing.command.post_settle_ms);
                force_scan_after = Some(suppress_change_until);
                force_scan_reason = Some("hall-expiring");
                last_fingerprint = None;
                last_ocr_at = Instant::now();
            }
            self.forward_completion_advances(completion_subscriber)?;
            self.maybe_idle_exit()?;
            self.maybe_reload_config_when_idle();
            if !self.lifecycle.running.load(AtomicOrdering::SeqCst) {
                continue;
            }
            let ready_snapshot = self.business.business.chat_listener_snapshot()?;
            let ready_task_busy = !self.business.task_engine.is_idle()?;
            let listener_ready = listener_ready_after_recheck(
                listener_ready_evidence,
                &ready_snapshot,
                ready_task_busy,
                self.lifecycle.paused.load(AtomicOrdering::SeqCst),
                self.lifecycle.running.load(AtomicOrdering::SeqCst),
            );
            let startup_ready = self.lifecycle.reload_startup.is_satisfied();
            if replacement_ready_allowed(listener_ready, startup_ready)
                && crate::mark_config_reload_child_ready()?
            {
                if std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_some() {
                    log::info!("配置重载替代进程已 ready");
                }
            } else if listener_ready {
                // Required startup work is still running or has failed. A verified screen alone
                // must not acknowledge an exit-77 handoff as ready.
                replacement_ui_unverified_since = None;
            } else {
                self.maybe_activate_reload_startup_after_ui_grace(
                    config_reload_child,
                    !ready_task_busy,
                    &mut replacement_ui_unverified_since,
                    &mut reload_startup_retry_after,
                    "业务界面长期无法完成验证",
                )?;
            }
            sleep(self.reload_aware_wait(Duration::from_millis(live_config.timing.loop_idle_ms)));
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
            match self
                .ui
                .chat_observations
                .read_completion_advance(subscriber)?
            {
                Some(ObservationRead::Item { value, .. }) => self
                    .business
                    .business_events
                    .submit(BusinessEvent::CompletionAdvance(Arc::unwrap_or_clone(
                        value,
                    )))
                    .context("向业务运行时提交观察完成推进")?,
                Some(ObservationRead::Gap(gap)) => self
                    .business
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
        let dispatches = self.ui.chat_observations.publish_primary(frame, messages)?;
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
                    self.ui
                        .chat_baseline_primed
                        .store(false, AtomicOrdering::SeqCst);
                    log::warn!(
                        "聊天观察流出现缺口，下一屏仅重建命令基线: kind={:?} missing={:?}..={:?}",
                        gap.kind,
                        gap.missing_from,
                        gap.missing_through
                    );
                }
            }
        }
        Ok(processed_secondary)
    }

    pub(super) fn mapped_actor_name(&self, ocr_name: &str) -> String {
        self.lifecycle.live_configs.identity.display_name(ocr_name)
    }

    pub(super) fn canonical_actor_name(&self, ocr_name: &str) -> String {
        self.lifecycle
            .live_configs
            .identity
            .canonical_name(ocr_name)
    }

    fn map_command_actor(&self, mut command: RoutedCommand) -> RoutedCommand {
        let actor = self.mapped_actor_name(&command.username);
        match &mut command.command {
            ModuleCommand::Invite(invite) => invite.display_name = actor,
            ModuleCommand::Moderation(moderation) => moderation.requester = actor,
            _ => {}
        }
        command
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
        let active_entertainment = self.business.business.active_entertainment()?;
        let command_router = ChatCommandRouter::with_identity(
            &self.business.custom_workflow,
            &self.lifecycle.live_configs.identity,
        );
        let visible_turtle_questions = if self.business.business.turtle_soup_accepts_questions()? {
            messages
                .iter()
                .enumerate()
                .filter(|(_, message)| message.message_type == "blue" && !message.text.is_empty())
                .filter(|(_, message)| {
                    command::parse_command_envelope(
                        &message.text,
                        &message.message_type,
                        CommandObservation::default(),
                    )
                    .filter(|envelope| envelope.prefix() == CommandPrefix::Hash)
                    .and_then(|envelope| command_router.route(&envelope, active_entertainment))
                    .is_none()
                })
                .filter_map(|(order, message)| {
                    turtle_soup::parse_question_message(&message.text, None)
                        .map(|question| (order, question))
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let suppress_new_turtle_questions =
            !self.ui.chat_baseline_primed.load(AtomicOrdering::SeqCst);
        let new_turtle_questions = self
            .business
            .business
            .filter_turtle_soup_primary_questions(
                visible_turtle_questions
                    .iter()
                    .map(|(_, question)| question.clone())
                    .collect(),
                suppress_new_turtle_questions,
            )?;
        if messages.is_empty() {
            log::debug!("没有找到聊天标志，本轮不更新命令锁");
            return Ok(());
        }

        let mut parsed = Vec::new();
        for (_, observed) in primary_command_candidates(&observed_messages) {
            let message = &observed.message;
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
            let parsed_command = self.map_command_actor(parsed_command);
            if !self.commands_enabled()?
                && parsed_command.authority == CommandAuthority::HallMember
                && parsed_command.role.is_none()
            {
                log::info!("命令识别已禁用，跳过: {}", parsed_command.raw);
                self.ui
                    .chat_observations
                    .acknowledge_primary(&observed.id)?;
                continue;
            }
            if let ModuleCommand::Invite(invite) = &parsed_command.command
                && !self.business.business.invite_should_accept(invite.seq)?
            {
                let seq = invite.seq.expect("unsequenced invites are always accepted");
                log::info!("邀请参数 {} 已执行过，跳过: {}", seq, parsed_command.raw);
                self.ui
                    .chat_observations
                    .acknowledge_primary(&observed.id)?;
                continue;
            }
            log::debug!("解析命令: {}", parsed_command.raw);
            parsed.push(parsed_command);
        }

        let accepted = parsed
            .into_iter()
            .map(|routed| PendingCommand {
                lock_key: command::lock_key(&routed),
                routed,
            })
            .collect::<Vec<_>>();
        if !self
            .ui
            .chat_baseline_primed
            .swap(true, AtomicOrdering::SeqCst)
        {
            for question in new_turtle_questions {
                log::info!(
                    "启动屏幕锁已记录当前可见海龟汤提问，不执行: nickname={}",
                    question.player
                );
            }
            for pending in accepted {
                log::info!(
                    "聊天观察基线已记录当前可见命令，不执行: {}",
                    pending.routed.raw
                );
                self.acknowledge_primary_command(&pending.routed)?;
            }
            return Ok(());
        }
        let questions = if self.commands_enabled()? {
            attach_question_orders(visible_turtle_questions, new_turtle_questions)?
        } else {
            Vec::new()
        };
        let commands = accepted
            .into_iter()
            .map(|pending| {
                let order = pending
                    .routed
                    .observation
                    .message_id
                    .as_ref()
                    .and_then(|message_id| {
                        observed_messages
                            .iter()
                            .position(|observed| &observed.id == message_id)
                    })
                    .unwrap_or(usize::MAX);
                (order, pending)
            })
            .collect();

        for input in merge_observed_inputs(commands, questions) {
            match input {
                ObservedInput::Question(question) => {
                    self.enqueue_turtle_soup_question(question, frame.captured_at())?;
                }
                ObservedInput::Command(pending) => {
                    if self.enqueue_chat_listener_command(&pending.routed)? {
                        self.acknowledge_primary_command(&pending.routed)?;
                        continue;
                    }
                    if self.apply_immediate_administration(&pending.routed, false)? {
                        self.acknowledge_primary_command(&pending.routed)?;
                        continue;
                    }
                    let routed = pending.routed.clone();
                    self.enqueue_pending_command(pending)?;
                    self.acknowledge_primary_command(&routed)?;
                }
            }
        }
        Ok(())
    }

    fn acknowledge_primary_command(&self, command: &RoutedCommand) -> Result<()> {
        let Some(message_id) = command.observation.message_id.as_ref() else {
            return Ok(());
        };
        if !self.ui.chat_observations.acknowledge_primary(message_id)? {
            log::debug!("一级命令完成处理时消息已滚出当前画面: id={message_id:?}");
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
                let snapshot = self.business.business.chat_listener_snapshot()?;
                let pending = snapshot
                    .pending_mode
                    .map(|mode| format!("，等待切换{}", mode.label()))
                    .unwrap_or_default();
                let message = format!("监听模式状态: {}{}", snapshot.mode.label(), pending);
                log::info!("{}", message);
                self.lifecycle
                    .monitor
                    .publish(MonitorEvent::Command(format!(
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
                let (queued, snapshot) =
                    self.business.business.request_chat_listener_mode(target)?;
                if !queued {
                    log::info!(
                        "监听模式切换已处于当前或等待状态，跳过: current={} pending={:?}",
                        snapshot.mode.label(),
                        snapshot.pending_mode
                    );
                    return Ok(true);
                }
                self.record_command_activity(command_observed_at(parsed))?;
                if let Err(error) =
                    self.push_pending_task(PendingTask::SetChatListenerMode { target })
                {
                    self.business
                        .business
                        .cancel_chat_listener_mode_request(target)?;
                    return Err(error);
                }
                log::info!("监听模式切换已加入待处理队列: {}", target.label());
            }
        }
        Ok(true)
    }

    pub(super) fn enqueue_turtle_soup_question(
        &self,
        mut question: turtle_soup::TurtleSoupQuestion,
        observed_at: Instant,
    ) -> Result<()> {
        question.rename_player(self.mapped_actor_name(&question.player));
        self.record_command_activity(observed_at)?;
        log::info!("海龟汤提问已加入正式输入队列: nickname={}", question.player);
        self.push_pending_task(PendingTask::TurtleSoupQuestion {
            question: Box::new(question),
            observed_at,
        })
    }

    pub(super) fn abort_entertainment_for_context_loss(&self, reason: &str) {
        if let Err(error) = self.business.business.abort_turtle_soup(reason) {
            log::error!("无法中止海龟汤会话: {error:#}");
        }
        match self.business.undercover_game.abort() {
            Ok(true) => log::warn!("谁是卧底已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧谁是卧底牌局: {error:#}"),
        }
        match self.business.card_games.abort() {
            Ok(true) => log::warn!("牌局已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧牌局: {error:#}"),
        }
        match self.business.business.abort_idiom_chain() {
            Ok(true) => log::warn!("成语接龙已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧成语接龙会话: {error:#}"),
        }
    }

    fn tick_entertainment(&self) {
        let scheduler_idle = match self.business.task_engine.snapshot() {
            Ok(snapshot) => snapshot.is_idle(),
            Err(error) => {
                log::error!("无法读取业务调度状态，娱乐计时保持暂停: {error}");
                false
            }
        };
        let clock_active = !self.lifecycle.paused.load(AtomicOrdering::SeqCst) && scheduler_idle;
        if let Err(error) = self
            .business
            .business
            .refresh_turtle_soup_deadline(Instant::now(), clock_active)
        {
            log::error!("无法同步海龟汤期限: {error:#}");
        }
        let card_game_outcome = match self
            .business
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
            let effect = self.business.card_games.timed_effect(outcome);
            if let Err(error) = self.push_pending_task(PendingTask::CardGameEffect(effect)) {
                log::error!("牌局计时结果入队失败: {error:#}");
                if let Err(cancel_error) = self.business.card_games.cancel_effect(key) {
                    log::error!("牌局计时结果入队失败后无法清理牌局: {cancel_error:#}");
                }
            }
        }
        let undercover_outcome = match self
            .business
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
            let effect = self.business.undercover_game.timed_effect(outcome);
            if let Err(error) = self.push_pending_task(PendingTask::UndercoverEffect(effect)) {
                log::error!("谁是卧底计时消息入队失败: {error:#}");
                if let Err(cancel_error) = self.business.undercover_game.cancel_effect(key) {
                    log::error!("谁是卧底计时消息入队失败后无法清理牌局: {cancel_error:#}");
                }
            }
        }
    }

    pub(super) fn submit_secondary_command(&self, parsed: RoutedCommand) -> Result<()> {
        let parsed = self.map_command_actor(parsed);
        if self.enqueue_chat_listener_command(&parsed)? {
            return Ok(());
        }
        if !self.commands_enabled()?
            && parsed.authority == CommandAuthority::HallMember
            && parsed.role.is_none()
        {
            log::info!("命令识别已禁用，跳过二级大厅命令: {}", parsed.raw);
            return Ok(());
        }
        if let ModuleCommand::Invite(invite) = &parsed.command
            && !self.business.business.invite_should_accept(invite.seq)?
        {
            let seq = invite.seq.expect("unsequenced invites are always accepted");
            log::info!("邀请参数 {} 已执行过，跳过: {}", seq, parsed.raw);
            return Ok(());
        }
        if self.apply_immediate_administration(&parsed, true)? {
            return Ok(());
        }
        self.enqueue_pending_command(PendingCommand {
            lock_key: command::lock_key(&parsed),
            routed: parsed,
        })
    }

    fn enqueue_pending_command(&self, pending: PendingCommand) -> Result<()> {
        if self.pending_contains_command(&pending.routed)? {
            log::info!("命令已在待处理队列，本轮跳过: {}", pending.routed.raw);
            return Ok(());
        }
        self.record_command_activity(command_observed_at(&pending.routed))?;
        log::info!("命令已加入待处理队列: {}", pending.routed.raw);
        self.push_pending_task(PendingTask::Command(Box::new(pending)))
    }

    fn apply_immediate_administration(
        &self,
        parsed: &RoutedCommand,
        propagate_log_error: bool,
    ) -> Result<bool> {
        if parsed.permission_required.is_some() {
            return Ok(false);
        }
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
            self.business.administration_application.apply_immediate(
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
            self.complete_chat_listener_mode(target)?;
            log::info!("聊天监听模式已切换为{}", target.label());
            return Ok(());
        }

        self.fail_chat_listener_mode_to_primary()?;
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
    marker_hits: Option<Vec<crate::ui::template::TemplateHit>>,
) -> Result<Vec<ChatMessage>> {
    let total_started = Instant::now();
    let prepared = prepare_chat_scan_with_markers(image, templates, chat_rect, marker_hits)?;
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

#[cfg(test)]
mod tests {
    use super::{
        CHAT_LISTENER_UNRESOLVED_UI_GRACE, CONFIG_RELOAD_UI_FALLBACK_GRACE, ChatListenerMode,
        ListenerReadyEvidence, ObservedInput, attach_question_orders, listener_mode_matches_ui,
        listener_ready_after_recheck, load_persisted_chat_listener_mode, merge_observed_inputs,
        persist_chat_listener_mode, primary_command_candidates, primary_rescan_can_publish,
        replacement_ready_allowed, replacement_startup_fallback_due, replacement_startup_retry_due,
        secondary_hall_recovery_allowed, unresolved_ui_recovery_due,
    };
    use crate::observation::chat::{
        BubbleSequence, ChatIdentity, ChatMessage, ObservedChatMessageId, PrimaryObservedMessage,
        VisualSessionId,
    };
    use crate::ui::geometry::Rect;
    use std::time::{Duration, Instant};

    fn listener_snapshot(
        mode: ChatListenerMode,
    ) -> crate::runtime::chat_listener::ChatListenerSnapshot {
        crate::runtime::chat_listener::ChatListenerSnapshot {
            mode,
            pending_mode: None,
            temporary_primary: false,
            initial_unread_clear: false,
            unread_task_pending: false,
            hall_round_required: false,
        }
    }

    fn labels(inputs: Vec<ObservedInput<&'static str, &'static str>>) -> Vec<&'static str> {
        inputs
            .into_iter()
            .map(|input| match input {
                ObservedInput::Command(label) | ObservedInput::Question(label) => label,
            })
            .collect()
    }

    #[test]
    fn observed_commands_and_questions_keep_screen_order() {
        let inputs = merge_observed_inputs(
            vec![(1, "control"), (3, "later-control")],
            vec![(0, "earlier-question"), (2, "question")],
        );

        assert_eq!(
            labels(inputs),
            ["earlier-question", "control", "question", "later-control"]
        );
    }

    #[test]
    fn accepted_equal_questions_retain_distinct_screen_positions() {
        let ordered = attach_question_orders(
            vec![(2, "same-question"), (5, "same-question")],
            vec!["same-question", "same-question"],
        )
        .expect("question orders");

        assert_eq!(ordered, [(2, "same-question"), (5, "same-question")]);
    }

    #[test]
    fn primary_command_parsing_receives_only_unhandled_new_message_ids() {
        let messages = vec![
            observed_message(1, "@状态", false),
            observed_message(2, "状态消息", false),
            observed_message(3, "@状态", true),
        ];

        let candidates = primary_command_candidates(&messages)
            .map(|(order, message)| {
                (
                    order,
                    message.id.bubble_sequence.get(),
                    message.message.text.as_str(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(candidates, [(2, 3, "@状态")]);
    }

    #[test]
    fn chat_listener_mode_persistence_round_trips_without_using_the_database_file() {
        let root = std::env::temp_dir().join(format!(
            "miliastra-chat-listener-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let playback_state_path = root.join("playback.sqlite3");

        persist_chat_listener_mode(&playback_state_path, ChatListenerMode::Secondary).unwrap();
        assert_eq!(
            load_persisted_chat_listener_mode(&playback_state_path).unwrap(),
            Some(ChatListenerMode::Secondary)
        );
        assert!(
            root.join("playback.sqlite3.chat-listener-mode.json")
                .is_file()
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listener_ready_requires_the_selected_logical_mode_to_match_the_ui() {
        assert!(listener_mode_matches_ui(
            ChatListenerMode::Primary,
            false,
            crate::runtime::ui::UiStateKind::Primary
        ));
        assert!(!listener_mode_matches_ui(
            ChatListenerMode::Primary,
            false,
            crate::runtime::ui::UiStateKind::Secondary
        ));
        assert!(listener_mode_matches_ui(
            ChatListenerMode::Secondary,
            false,
            crate::runtime::ui::UiStateKind::Secondary
        ));
        assert!(!listener_mode_matches_ui(
            ChatListenerMode::Secondary,
            false,
            crate::runtime::ui::UiStateKind::Primary
        ));
        assert!(listener_mode_matches_ui(
            ChatListenerMode::Secondary,
            true,
            crate::runtime::ui::UiStateKind::Primary
        ));
    }

    #[test]
    fn primary_rescan_rejects_a_fresh_frame_that_left_primary_ui() {
        assert!(primary_rescan_can_publish(Some(
            crate::runtime::ui::UiStateKind::Primary
        )));
        assert!(!primary_rescan_can_publish(Some(
            crate::runtime::ui::UiStateKind::Secondary
        )));
        assert!(!primary_rescan_can_publish(Some(
            crate::runtime::ui::UiStateKind::Unknown
        )));
        assert!(!primary_rescan_can_publish(None));
    }

    #[test]
    fn listener_ready_requires_fresh_verified_work_and_a_final_idle_recheck() {
        let primary = listener_snapshot(ChatListenerMode::Primary);
        assert!(listener_ready_after_recheck(
            Some(ListenerReadyEvidence::PrimaryScan),
            &primary,
            false,
            false,
            true,
        ));
        assert!(!listener_ready_after_recheck(
            None, &primary, false, false, true,
        ));
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::PrimaryScan),
            &primary,
            true,
            false,
            true,
        ));
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::PrimaryScan),
            &primary,
            false,
            true,
            true,
        ));
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::PrimaryScan),
            &primary,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn replacement_ready_waits_for_required_startup_success() {
        assert!(replacement_ready_allowed(true, true));
        assert!(!replacement_ready_allowed(true, false));
        assert!(!replacement_ready_allowed(false, true));
    }

    #[test]
    fn secondary_ready_requires_a_completed_current_hall_round() {
        let mut secondary = listener_snapshot(ChatListenerMode::Secondary);
        assert!(listener_ready_after_recheck(
            Some(ListenerReadyEvidence::SecondaryCurrentHall),
            &secondary,
            false,
            false,
            true,
        ));

        secondary.initial_unread_clear = true;
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::SecondaryCurrentHall),
            &secondary,
            false,
            false,
            true,
        ));
        secondary.initial_unread_clear = false;
        secondary.hall_round_required = true;
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::SecondaryCurrentHall),
            &secondary,
            false,
            false,
            true,
        ));
        secondary.hall_round_required = false;
        secondary.unread_task_pending = true;
        assert!(!listener_ready_after_recheck(
            Some(ListenerReadyEvidence::SecondaryCurrentHall),
            &secondary,
            false,
            false,
            true,
        ));
    }

    #[test]
    fn unresolved_ui_recovery_waits_for_grace_and_runtime_idle() {
        let now = Instant::now();
        let old = now.checked_sub(CHAT_LISTENER_UNRESOLVED_UI_GRACE).unwrap();
        assert!(unresolved_ui_recovery_due(
            Some(old),
            now,
            now,
            false,
            false,
        ));
        assert!(!unresolved_ui_recovery_due(
            Some(now),
            now,
            now,
            false,
            false,
        ));
        assert!(!unresolved_ui_recovery_due(
            Some(old),
            now,
            now + Duration::from_millis(1),
            false,
            false,
        ));
        assert!(!unresolved_ui_recovery_due(
            Some(old),
            now,
            now,
            true,
            false,
        ));
        assert!(!unresolved_ui_recovery_due(
            Some(old),
            now,
            now,
            false,
            true,
        ));
    }

    #[test]
    fn secondary_hall_recovery_uses_only_its_cooldown() {
        let now = Instant::now();
        assert!(secondary_hall_recovery_allowed(now, now));
        assert!(!secondary_hall_recovery_allowed(
            now,
            now + Duration::from_secs(1),
        ));
    }

    #[test]
    fn replacement_startup_retry_is_limited_to_idle_pre_ready_children() {
        let now = Instant::now();
        assert!(replacement_startup_retry_due(
            true, false, true, true, now, now,
        ));
        assert!(!replacement_startup_retry_due(
            false, false, true, true, now, now,
        ));
        assert!(!replacement_startup_retry_due(
            true, true, true, true, now, now,
        ));
        assert!(!replacement_startup_retry_due(
            true, false, false, true, now, now,
        ));
        assert!(!replacement_startup_retry_due(
            true,
            false,
            true,
            true,
            now,
            now + Duration::from_millis(1),
        ));
    }

    #[test]
    fn replacement_ui_fallback_waits_for_a_stable_pre_ready_failure() {
        let now = Instant::now();
        let old = now.checked_sub(CONFIG_RELOAD_UI_FALLBACK_GRACE).unwrap();
        assert!(replacement_startup_fallback_due(
            true,
            false,
            false,
            true,
            true,
            false,
            now,
            Some(old),
        ));
        assert!(!replacement_startup_fallback_due(
            true,
            false,
            false,
            true,
            true,
            false,
            now,
            Some(now),
        ));
        assert!(!replacement_startup_fallback_due(
            true,
            false,
            true,
            true,
            true,
            false,
            now,
            Some(old),
        ));
        assert!(!replacement_startup_fallback_due(
            true,
            false,
            false,
            true,
            false,
            false,
            now,
            Some(old),
        ));
        assert!(!replacement_startup_fallback_due(
            true,
            false,
            false,
            true,
            true,
            true,
            now,
            Some(old),
        ));
    }

    #[test]
    fn missing_chat_listener_mode_defaults_to_none() {
        let path = std::env::temp_dir()
            .join(format!(
                "miliastra-chat-listener-missing-{}",
                uuid::Uuid::new_v4()
            ))
            .join("playback.sqlite3");
        assert_eq!(load_persisted_chat_listener_mode(&path).unwrap(), None);
    }

    fn observed_message(sequence: u64, text: &str, is_new: bool) -> PrimaryObservedMessage {
        PrimaryObservedMessage {
            id: ObservedChatMessageId::new(
                VisualSessionId::new(1),
                ChatIdentity::PrimaryHall,
                BubbleSequence::new(sequence),
            ),
            message: ChatMessage {
                message_type: "blue".to_string(),
                block: Rect::new(0, sequence as i32 * 20, 10, 10),
                text: text.to_string(),
            },
            is_new,
        }
    }
}
