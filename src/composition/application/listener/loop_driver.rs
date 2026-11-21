//! 网络面板、全局热键、值守模式落盘与主扫描循环。
//!
//! 从 `listener.rs` 拆出：进程级入口（HTTP 面板、全局热键）、值守模式的持久化与
//! 执行，以及主扫描循环 `run_scan_loop` / `run_scan_loop_inner`。
//!
//! ## 状态归属
//!
//! 循环内部的进度状态（观察帧需求、指纹、退避、各个告警节流槽）全部是**本轮
//! 循环的局部变量**，不放进共享服务——它们描述的是「这次循环走到哪一步」，
//! 生命周期就该与循环一致。
//!
//! 跨例程的两类状态则各归其位：
//!
//! - 展示去重（[`ObservationDedup`]）由循环持有，循环结束即释放；
//! - 恢复状态（冷却、互斥、Unknown 观测）交给进程级
//!   [`GameRecoveryCoordinator`](crate::runtime::recovery::GameRecoveryCoordinator)，
//!   与启动链、驻留恢复共享同一份。

use super::super::secondary_chat::{SecondaryHallCommandTracker, SecondaryListenerRoundState};
use super::super::*;
use super::display::{ObservationDedup, residency_target_label, ui_state_display_label};
use super::recovery_guard::{
    CONFIG_RELOAD_STARTUP_RETRY_INTERVAL, ListenerReadyEvidence, RecoveryGuard,
    automatic_startup_task, game_location_probe_blocks_residency, listener_mode_matches_ui,
    listener_ready_after_recheck, loop_failure_warn_due, replacement_ready_allowed,
    replacement_startup_fallback_due, replacement_startup_retry_due, scan_frame_is_fresh,
    secondary_hall_recovery_allowed, unresolved_ui_recovery_due,
};
use crate::observation::chat::ChatIdentity;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub(in crate::composition::application) const CHAT_LISTENER_MODE_STATE_SUFFIX: &str =
    ".chat-listener-mode.json";

pub(in crate::composition::application) const CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL: Duration =
    Duration::from_secs(5);

/// 等观察帧的切片长度：让主循环能在大约 25ms 内感知共享聊天发布并提前回顶部派发。
pub(in crate::composition::application) const FRAME_WAIT_SLICE: Duration =
    Duration::from_millis(25);

/// 二级值守落在非二级界面时，重复告警的最短间隔。
///
/// 该分支每轮都会走到（`loop_idle_ms` 默认 60ms）。无条件 `warn` 会每秒刷十几条，
/// 把真正需要关注的告警挤出日志；但完全不告警又会让值守卡住时查不到线索，
/// 所以首次立即告警、之后按本间隔重申。
pub(in crate::composition::application) const SECONDARY_UI_MISMATCH_WARN_INTERVAL: Duration =
    Duration::from_secs(60);

pub(in crate::composition::application) fn chat_listener_mode_path(
    playback_state_path: &Path,
) -> PathBuf {
    let mut file_name = playback_state_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("playback-state"))
        .to_os_string();
    file_name.push(CHAT_LISTENER_MODE_STATE_SUFFIX);
    playback_state_path.with_file_name(file_name)
}

/// 尺寸不符告警开关：每次从 live 快照读，避免保存后仍用启动值。
///
/// 注意：当前唯一调用点是启动时的 `warn_if_screen_size_mismatch`（lifecycle.rs:51），
/// 因此接线后的可观察变化要到该检查再次被调用时才出现；这里只负责取值来源，
/// 不改变调用时机。
pub(in crate::composition::application) fn screen_size_warning_enabled(config: &AppConfig) -> bool {
    config.screen.warn_on_size_mismatch
}

pub(in crate::composition::application) fn load_persisted_chat_listener_mode(
    playback_state_path: &Path,
) -> Result<Option<ChatListenerMode>> {
    let path = chat_listener_mode_path(playback_state_path);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取聊天值守模式状态失败: {}", path.display()));
        }
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("解析聊天值守模式状态失败: {}", path.display()))
        .map(Some)
}

pub(in crate::composition::application) fn persist_chat_listener_mode(
    playback_state_path: &Path,
    mode: ChatListenerMode,
) -> Result<()> {
    let path = chat_listener_mode_path(playback_state_path);
    let bytes = serde_json::to_vec(&mode).context("序列化聊天值守模式状态失败")?;
    crate::adapters::file_store::write_atomic(&path, &bytes, "聊天值守模式状态")
}

pub(in crate::composition::application) struct HttpHallDetector {
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

impl AppServices {
    pub(in crate::composition::application) fn poll_playlist_import_completions(
        &self,
    ) -> Result<()> {
        for messages in self
            .business
            .playlist_application
            .poll_import_completions(Instant::now())?
        {
            let rendered: Vec<String> = messages
                .iter()
                .flat_map(|reply| self.render_playlist_reply(reply))
                .collect();
            let messages = rendered.iter().map(String::as_str).collect::<Vec<_>>();
            self.reply_batch(&messages, 0)?;
        }
        Ok(())
    }

    pub(in crate::composition::application) fn persist_listener_mode_best_effort(
        &self,
        mode: ChatListenerMode,
    ) {
        if let Err(error) =
            persist_chat_listener_mode(&self.shared.config.state.playback_state_path, mode)
        {
            log::error!(
                "保存聊天值守模式失败（步骤：写入值守模式状态文件），本次选择未落盘，进程重启后会回到上一次保存的值：{error:#}"
            );
        }
    }

    pub(in crate::composition::application) fn persist_current_chat_listener_mode(&self) {
        match self.business.business.chat_listener_snapshot() {
            Ok(snapshot) => self.persist_listener_mode_best_effort(snapshot.mode),
            Err(error) => log::error!(
                "保存聊天值守模式失败（步骤：读取当前值守模式），本次没有可保存的值，进程重启后会回到上一次保存的值：{error}"
            ),
        }
    }

    pub(in crate::composition::application) fn complete_chat_listener_mode(
        &self,
        mode: ChatListenerMode,
    ) -> Result<()> {
        self.business.business.complete_chat_listener_mode(mode)?;
        self.persist_listener_mode_best_effort(mode);
        Ok(())
    }

    pub(in crate::composition::application) fn fail_chat_listener_mode_to_primary(
        &self,
    ) -> Result<()> {
        self.business
            .business
            .fail_chat_listener_mode_to_primary()?;
        self.persist_listener_mode_best_effort(ChatListenerMode::Primary);
        Ok(())
    }

    pub(in crate::composition::application) fn warn_if_screen_size_mismatch(&self) -> Result<()> {
        let frame = match self.ui.game_ui.capture() {
            Ok(frame) => frame,
            Err(error) => {
                log::warn!(
                    "启动时截图失败（步骤：检查屏幕尺寸），本次尺寸校验跳过，扫描循环会等目标窗口恢复后继续：{error:#}"
                );
                return Ok(());
            }
        };
        // 告警开关每次读 live 快照（保存后下一次调用即生效），不缓存启动值。
        let warn_on_size_mismatch =
            screen_size_warning_enabled(&self.shared.live_configs.snapshot());
        if warn_on_size_mismatch
            && (frame.width() != self.shared.config.screen.expected_width
                || frame.height() != self.shared.config.screen.expected_height)
        {
            log::warn!(
                "截图尺寸与预期不一致：实际 {}x{}，预期 {}x{}。模板匹配可能失准，程序继续运行，建议核对游戏窗口分辨率",
                frame.width(),
                frame.height(),
                self.shared.config.screen.expected_width,
                self.shared.config.screen.expected_height
            );
        }
        Ok(())
    }

    /// 组装并启动 Web 面板。
    ///
    /// 面板只拿共享服务里的句柄（正式任务客户端、UI 句柄、OCR、播放器后端），
    /// 因此这一步不需要 `&self`；启动成功后把面板真实的排空闸门登记回
    /// 共享服务，配置空闲重载据此判断「是否还有 HTTP 写操作在途」。
    pub(in crate::composition::application) fn start_http_server(
        &self,
    ) -> Result<http::HttpServer> {
        if self.shared.http_reload.is_attached() {
            return Err(anyhow!("HTTP/Web 面板已经启动"));
        }
        let formal_tasks = Arc::new(self.formal_tasks()?.clone());
        // 调试沙盒复用与公开 HTTP 完全相同的播放控制入队路径，不另造旁路：
        // 这样"沙盒里能做的"和"API 能做的"永远一致，不会各自漂移。
        // 模拟聊天注入需要真身的链路句柄：timeline 决定消息进哪条队列，
        // identity 决定按什么角色授权，external 决定决策窗口走快路径还是 OCR。
        let simulation_external = self.shared.external_chat.clone();
        let simulation = http_facade::ChatSimulation {
            timeline: self.ui.chat_timeline.clone(),
            identity: self.shared.live_configs.identity.clone(),
            external_active: Arc::new(move || simulation_external.supplies_hall_messages()),
        };
        // 网页端“解析全部页签”：用监听运行时的 UI 句柄与 OCR 直接驱动一次扫描。
        let emoji_catalog_ui = crate::ui::routines::EmojiCatalogUi::new(
            self.ui.ui_handle.clone(),
            self.ui.ocr.clone(),
            friend_delivery_routine_config(&self.shared.config, &self.shared.live_configs),
        );
        let emoji_scan: Arc<dyn Fn() -> Result<String, http::HttpCommandError> + Send + Sync> =
            Arc::new(move || {
                let operation = emoji_catalog_ui.submit().map_err(|error| {
                    http::HttpCommandError::internal(format!("表情目录扫描提交失败：{error}"))
                })?;
                let outcome = operation.wait().map_err(|error| {
                    http::HttpCommandError::internal(format!("表情目录扫描等待失败：{error}"))
                })?;
                if let Some(failure) = outcome.failure {
                    return Err(http::HttpCommandError::internal(format!(
                        "表情目录扫描失败：{failure}"
                    )));
                }
                let scanned_at_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis())
                    .unwrap_or(0);
                let tabs = outcome.tabs;
                let entries: Vec<String> = tabs
                    .iter()
                    .flat_map(|tab| {
                        tab.labels
                            .iter()
                            .map(move |label| format!("{}|{}", tab.pack, label))
                    })
                    .collect();
                let payload = serde_json::json!({
                    "ok": true,
                    "scannedAtMs": scanned_at_ms,
                    "tabs": tabs,
                    "entries": entries,
                });
                let text = serde_json::to_string_pretty(&payload).map_err(|error| {
                    http::HttpCommandError::internal(format!("序列化表情目录失败：{error}"))
                })?;
                let path = std::path::Path::new("deps/data/emoji-catalog.json");
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        http::HttpCommandError::internal(format!("创建表情目录目录失败：{error}"))
                    })?;
                }
                std::fs::write(path, &text).map_err(|error| {
                    http::HttpCommandError::internal(format!("写入表情目录失败：{error}"))
                })?;
                Ok(text)
            });
        let debug_command_port: Arc<dyn http::HttpCommandPort> = Arc::new(
            http_facade::ApplicationHttpCommandFacade::with_simulation(
                formal_tasks.clone(),
                self.business.custom_workflow.clone(),
                simulation,
            )
            .with_emoji_scan(emoji_scan.clone()),
        );
        let config_store = self.shared.config_store.clone();
        let http_config = self.shared.config.http.clone();
        let mut http_state = http::HttpSharedState::new(
            http::HttpInterfaceConfig::new(
                http_config,
                self.shared.config.screen.clone(),
                self.shared.config.templates.clone(),
                self.shared.config.moderation.clone(),
                self.shared.config.startup.clone(),
                self.shared.config.invite.clone(),
                self.shared.config.timing.clone(),
                self.shared.config.custom_workflows.clone(),
            ),
            self.shared.monitor.clone(),
            self.ui.latest_frame.clone(),
            config_store,
            self.shared.live_configs.clone(),
            http::HttpApplicationPorts::new(
                Arc::new(
                    http_facade::ApplicationHttpCommandFacade::with_simulation(
                        formal_tasks.clone(),
                        self.business.custom_workflow.clone(),
                        http_facade::ChatSimulation {
                            timeline: self.ui.chat_timeline.clone(),
                            identity: self.shared.live_configs.identity.clone(),
                            external_active: {
                                let external = self.shared.external_chat.clone();
                                Arc::new(move || external.supplies_hall_messages())
                            },
                        },
                    )
                    .with_emoji_scan(emoji_scan.clone()),
                ),
                formal_tasks.clone(),
                formal_tasks,
                Arc::new(HttpHallDetector {
                    hall_ui: self.ui.hall_ui.clone(),
                }),
                Arc::new(http_facade::ApplicationHttpPlayerFacade::new(
                    PlayerRuntimeBackend::new(self.playback.player_runtime.clone()),
                    self.playback.player_search.clone(),
                    self.playback.native_playback.clone(),
                    self.playback.playback_application.play_mode_handle(),
                )),
                Arc::new(http_facade::ApplicationHttpLoginFacade::new(
                    self.playback.login_helper.clone(),
                )),
                // AI 点歌与 AI 聊天记忆是两个正交端口：
                // 点歌只需要模型客户端，记忆需要记忆库 + 身份映射 + 维护代际。
                // 这里分别装配，任何一个漏接都是编译错误而不是运行期静默失效。
                Arc::new(http_facade::ApplicationHttpAiMusicFacade::new(
                    self.business.ai.clone(),
                )),
                Arc::new(http_facade::ApplicationHttpAiMemoryFacade::with_memory(
                    self.business.ai_chat_memory.clone(),
                    {
                        // 重跑清洗：闭包捕获可克隆的句柄，模型配置走实时快照，
                        // 这样面板请求时用的是当前生效的模型而不是装配时那份。
                        let memory = self.business.ai_chat_memory.clone();
                        let openai = self.business.ai_chat_openai.clone();
                        let connection = self.business.ai_connection.clone();
                        let timeout = self.business.ai_request_timeout;
                        let live_configs = self.shared.live_configs.clone();
                        let generation = self.business.ai_chat_memory_generation.clone();
                        Arc::new(move |actor: &str| {
                            // 重建是权威结果：先推进代际作废在途的抽取/压缩提交，
                            // 再重放历史；结束后再推一次，防止重建期间新入队的
                            // 任务用旧结果覆盖重建结果。
                            generation.fetch_add(1, AtomicOrdering::SeqCst);
                            let config = live_configs.snapshot().ai_chat.clone();
                            let display = live_configs.identity.player_name_for_key(actor);
                            let result =
                                crate::composition::application::AppServices::rebuild_facts_with(
                                    &memory,
                                    &openai,
                                    &connection,
                                    timeout,
                                    &config,
                                    actor,
                                    &display,
                                );
                            generation.fetch_add(1, AtomicOrdering::SeqCst);
                            result
                        })
                    },
                    {
                        // 记忆键是 UUID，面板要显示成人能认的名字。
                        let identity = self.shared.live_configs.identity.clone();
                        Arc::new(move |key: &str| identity.player_name_for_key(key))
                    },
                    {
                        // 反向：面板/bookmark 传的是名字，查库要的是键。
                        let identity = self.shared.live_configs.identity.clone();
                        Arc::new(move |name: &str| identity.memory_key(name))
                    },
                    // 面板删改记忆要能作废在途的抽取/压缩，必须共享同一份代际。
                    self.business.ai_chat_memory_generation.clone(),
                )),
            ),
        );
        http_state.set_debug_port(Arc::new(http_facade::ApplicationHttpDebugFacade::new(
            self.ui.ui_handle.clone(),
            self.ui.ocr.clone(),
            &self.shared.config.state.playback_state_path,
            Arc::new({
                let business = self.business.business.clone();
                move || {
                    business
                        .playback_queue_snapshot()
                        .map(|queue| queue.len())
                        .map_err(|e| e.to_string())
                }
            }),
            Arc::new({
                let business = self.business.business.clone();
                move || {
                    business
                        .clear_playback_queue()
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            }),
            Arc::new({
                let business = self.business.business.clone();
                move || {
                    business
                        .remove_playback_queue(crate::features::playback::QueueRemoval::Front)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            }),
            Arc::new({
                let task_engine = self.business.task_engine.clone();
                move |task_id| {
                    task_engine
                        .cancel_formal(task_id)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            }),
            debug_command_port,
        )?));
        let reload_blockers = http_state.active_reload_blockers.clone();
        let reload_draining = http_state.reload_draining.clone();
        let server = http::start(http_state)?;
        self.shared
            .adopt_http_reload_gates(reload_blockers, reload_draining);
        Ok(server)
    }

    pub(in crate::composition::application) fn start_hotkeys(
        &self,
    ) -> Result<hotkeys::HotkeyRuntime> {
        let task_engine = self.business.task_engine.clone();
        let shutdown = self.shared.shutdown.clone();
        hotkeys::start(
            &self.shared.config.hotkeys,
            Arc::clone(&self.shared.running),
            Arc::clone(&self.shared.paused),
            Arc::new(move || shutdown.request_user_exit()),
            Arc::new(move || {
                if let Err(error) = task_engine.wake() {
                    log::debug!(
                        "热键切换暂停状态后唤醒任务调度失败，暂停或恢复要等下一轮扫描才生效：{error}"
                    );
                }
            }),
        )
    }

    pub(in crate::composition::application) fn activate_reload_startup_fallback(
        &self,
        reason: &str,
    ) -> Result<bool> {
        if std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_none()
            || crate::config_reload_child_ready()
        {
            return Ok(false);
        }
        if !self.shared.reload_startup.required_actions().is_empty() {
            return Ok(false);
        }
        if !self.require_reload_startup_automation()? {
            return Ok(false);
        }
        log::warn!(
            "配置重载的替代进程无法复用业务界面（原因：{}），改为重新执行启动自动化进入游戏",
            reason
        );
        Ok(true)
    }

    pub(in crate::composition::application) fn retry_reload_startup_if_needed(
        &self,
        config_reload_child: bool,
        retry_after: &mut Instant,
    ) -> Result<()> {
        let missing = !self.shared.reload_startup.missing_actions().is_empty();
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
            Ok(true) => log::warn!(
                "配置重载的替代进程尚未完成启动自动化，已按冷却间隔重新排队缺失的启动动作"
            ),
            Ok(false) => {}
            Err(error) => log::error!(
                "配置重载的替代进程重排启动自动化失败（步骤：补排缺失的启动动作），该进程暂时无法完成交接，会按冷却间隔重试：{error:#}"
            ),
        }
        *retry_after = now + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
        Ok(())
    }

    pub(in crate::composition::application) fn maybe_activate_reload_startup_after_ui_grace(
        &self,
        config_reload_child: bool,
        task_engine_idle: bool,
        unverified_since: &mut Option<Instant>,
        retry_after: &mut Instant,
        reason: &str,
    ) -> Result<()> {
        let now = Instant::now();
        let paused = self.shared.paused.load(AtomicOrdering::SeqCst);
        let startup_gate_active = !self.shared.reload_startup.required_actions().is_empty();
        let startup_actions_available =
            !ReloadStartupActions::from_startup_config(&self.shared.config).is_empty();
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

    pub(in crate::composition::application) fn run_scan_loop(
        &self,
        shared_subscriber: crate::observation::chat::ChatTimelineSubscriber,
    ) -> Result<()> {
        let mut completion_subscriber = self
            .ui
            .chat_observations
            .subscribe_completion_advances()
            .context("订阅聊天观察完成推进")?;
        let scan_result = self.run_scan_loop_inner(&mut completion_subscriber, &shared_subscriber);
        let final_forward_result = self.forward_completion_advances(&mut completion_subscriber);
        match scan_result {
            Err(error) => {
                if let Err(forward_error) = final_forward_result {
                    log::error!(
                        "扫描循环退出时转发观察完成推进失败（步骤：把已完成的观察推进提交给业务运行时），业务可能因此收不到最后一批完成事件：{forward_error:#}"
                    );
                }
                Err(error)
            }
            Ok(()) => final_forward_result,
        }
    }

    #[allow(unused_variables, unused_mut, unused_assignments)]
    pub(in crate::composition::application) fn run_scan_loop_inner(
        &self,
        completion_subscriber: &mut CompletionAdvanceSubscriber,
        shared_subscriber: &crate::observation::chat::ChatTimelineSubscriber,
    ) -> Result<()> {
        let template_args = self.ui.chat_templates.clone();
        let canvas = Canvas {
            width: self.shared.config.screen.expected_width,
            height: self.shared.config.screen.expected_height,
            resize: true,
        };
        let ui_handle = self.ui.ui_handle.clone();
        let initial_live_config = self.shared.live_configs.snapshot();
        let mut frame_demand_ms = initial_live_config.timing.loop_idle_ms.max(1);
        let mut frame_demand = FrameDemand::new(Duration::from_millis(frame_demand_ms))
            .context("创建聊天观察帧需求")?;
        let mut frame_subscription: Option<FrameDemandSubscription> = None;
        let mut last_scan_boundary: Option<Instant> = None;
        let mut last_fingerprint: Option<ChangeFingerprint> = None;
        let mut last_ocr_at = Instant::now()
            - Duration::from_millis(initial_live_config.timing.chat_scan.fallback_ms);
        let mut last_change_ocr_at = Instant::now()
            - Duration::from_millis(initial_live_config.timing.chat_scan.change_cooldown_ms);
        let mut suppress_change_until = Instant::now();
        let mut force_scan_after: Option<Instant> = None;
        let mut force_scan_reason: Option<&'static str> = None;
        let mut primary_visible = false;
        // 二级值守时 primary_visible 恒为 false（进入二级分支就先置 false）。
        // 大厅到期提醒只应要求"当前聊天界面能看到本大厅"，因此单独记一个
        // 「二级大厅聊天界面已确认」标志，否则二级值守下永远不提醒。
        let mut secondary_hall_visible = false;
        let mut game_location_baseline_reset = false;
        let mut secondary_friend_bubble_fingerprint: Option<ChangeFingerprint> = None;
        let mut secondary_hall_bubble_sequence: Option<Vec<SecondaryHallBubble>> = None;
        let mut secondary_hall_command_tracker = SecondaryHallCommandTracker::default();
        let mut secondary_identity: Option<SecondaryChatIdentity> = None;
        let mut listener_residency_retry_after = Instant::now();
        let mut automatic_startup_pending = false;
        let mut automatic_startup_retry_after = Instant::now();
        let config_reload_child = std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_some();
        let mut reload_startup_retry_after = Instant::now() + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
        let mut replacement_ui_unverified_since = config_reload_child.then(Instant::now);
        let mut unresolved_ui_since: Option<Instant> = None;
        // 二级值守落在非二级界面时的告警节流：该分支每轮都会走到
        // （loop_idle_ms 默认 60ms），无条件 warn 会每秒刷十几条把其余告警冲掉。
        // 首次立即告警留痕，之后每 SECONDARY_UI_MISMATCH_WARN_INTERVAL 才重申一次。
        let mut secondary_ui_mismatch_warned_at: Option<Instant> = None;
        // 以下四个闸门针对主循环里**持续失败**的 warn：loop_idle_ms 默认 60ms，
        // 无闸门时每秒十几条，会把偶发但重要的告警冲掉。
        // 规则统一：首次立即 warn 留痕，之后窗口内降 debug，窗口后再 warn 一次。
        // 与 secondary_ui_mismatch_warned_at 同一模式。
        let mut listener_residency_mismatch_warned_at: Option<Instant> = None;
        let mut unresolved_ui_warned_at: Option<Instant> = None;
        let mut ui_classify_failed_warned_at: Option<Instant> = None;
        let mut capture_failed_warned_at: Option<Instant> = None;
        let mut target_missing_backoff = TARGET_MISSING_BACKOFF_INITIAL;
        let mut target_missing = false;
        let mut hall_expiry_pause_applied = false;
        let mut hall_expiry_notice_sent = false;
        let mut hall_expiry_check_after = Instant::now();
        let mut shared_backlog = std::collections::VecDeque::new();
        // 配置空闲重载的检查节流时钟：属于本循环的进度状态，不放共享服务。
        let mut reload_check_after = Instant::now();
        // 自动重启的进度状态在 `recovery_guard` 里：
        // - 冷却与「最近一次重启是谁发起的」交给进程级协调器，三处触发源共享；
        // - 「已在等待桥重新拉起」的起点是**本轮循环**的进度，跟循环一起结束。
        // 原先的 `last_auto_restart_at` / `auto_restart_no_relaunch_warned_at`
        // 两个局部槽已经并入协调器与守卫，这里不再各存一份。
        // 展示去重：界面状态迁移与调试事件气泡。原先各是一个进程级 `static Mutex`，
        // 所有单测共享同一份“上一次推送过什么”，只能串行跑；改成循环局部实例后，
        // 谁持有谁负责，测试之间不再互相污染。
        let mut dedup = ObservationDedup::default();
        // 恢复守卫：冷却、互斥与 Unknown 观测都经进程级协调器，与启动链、
        // 驻留恢复共享同一份状态；本结构只保留本轮循环的进度（等待桥拉起的起点）。
        let mut recovery_guard = RecoveryGuard::default();
        log::info!("自动化扫描已启动，开始按界面状态轮询");
        while self.shared.running.load(AtomicOrdering::SeqCst) {
            self.poll_hall_expiry(
                &mut hall_expiry_pause_applied,
                &mut hall_expiry_notice_sent,
                &mut hall_expiry_check_after,
            );
            let live_config = self.shared.live_configs.snapshot();
            if automatic_startup_pending
                && self.ui.coordinator.scan_may_run()
                && self.business.task_engine.is_idle()?
            {
                automatic_startup_pending = false;
                automatic_startup_retry_after =
                    Instant::now() + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
            }
            self.retry_reload_startup_if_needed(
                config_reload_child,
                &mut reload_startup_retry_after,
            )?;
            let next_frame_demand_ms = live_config.timing.loop_idle_ms.max(1);
            if next_frame_demand_ms != frame_demand_ms {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!(
                        "撤销旧观察帧需求失败（触发原因：主循环间隔改变），旧订阅可能多存活一轮，下一轮会重新申请：{error}"
                    );
                }
                frame_demand = FrameDemand::new(Duration::from_millis(next_frame_demand_ms))
                    .context("更新聊天观察帧需求")?;
                frame_demand_ms = next_frame_demand_ms;
            }
            self.consume_shared_chat_snapshots(shared_subscriber, &mut shared_backlog, &mut dedup)?;
            self.forward_completion_advances(completion_subscriber)?;
            let loop_started = Instant::now();
            secondary_hall_visible = false;
            self.update_monitor_operational_state();
            self.tick_entertainment();
            let Some(scan_lease) = self.ui.coordinator.try_acquire_scan() else {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!(
                        "撤销观察帧需求失败（触发原因：正式任务占用界面），旧订阅可能多存活一轮，下一轮会重新申请：{error}"
                    );
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
                self.maybe_reload_config_when_idle(&mut reload_check_after);
                if !self.shared.running.load(AtomicOrdering::SeqCst) {
                    continue;
                }
                sleep(
                    self.reload_aware_wait(Duration::from_millis(live_config.timing.loop_idle_ms)),
                );
                continue;
            };
            // Keep the frame, OCR and observation dispatch in one registered scan.
            // Direct UI routines stay protected; release before housekeeping or idle waits.
            let scan_boundary = scan_lease.not_before();
            if scan_boundary != last_scan_boundary {
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!(
                        "撤销旧观察帧需求失败（触发原因：扫描新鲜度边界变化），旧订阅可能多存活一轮，下一轮会重新申请：{error}"
                    );
                }
                self.invalidate_latest_frame();
                primary_visible = false;
                game_location_baseline_reset = false;
                last_fingerprint = None;
                force_scan_after = None;
                force_scan_reason = None;
                secondary_friend_bubble_fingerprint = None;
                secondary_hall_bubble_sequence = None;
                secondary_hall_command_tracker.reset();
                secondary_identity = None;
                unresolved_ui_since = None;
                suppress_change_until = Instant::now();
                last_scan_boundary = scan_boundary;
            }
            let mut scan_lease = Some(scan_lease);
            if self.shared.paused.load(AtomicOrdering::SeqCst) {
                drop(scan_lease.take());
                if let Some(subscription) = frame_subscription.take()
                    && let Err(error) = subscription.cancel()
                {
                    log::warn!(
                        "撤销观察帧需求失败（触发原因：暂停值守），旧订阅可能多存活一轮，恢复后会重新申请：{error}"
                    );
                }
                unresolved_ui_since = None;
                self.maybe_idle_exit()?;
                self.maybe_reload_config_when_idle(&mut reload_check_after);
                if !self.shared.running.load(AtomicOrdering::SeqCst) {
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
            // 等观察帧的同时，每 FRAME_WAIT_SLICE 检查共享聊天是否已发布新快照：
            // 有就提前结束等待、回循环顶部派发，避免把 OCR→派发拖满一整轮。
            let frame_deadline = frame_started + CONFIG_RELOAD_IDLE_CHECK_INTERVAL;
            let mut woken_by_snapshot = false;
            let mut frame_outcome: Result<Option<Frame>> = Ok(None);
            loop {
                match shared_subscriber.has_pending() {
                    Ok(true) => {
                        woken_by_snapshot = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        frame_outcome = Err(error);
                        break;
                    }
                }
                let now = Instant::now();
                if now >= frame_deadline {
                    break;
                }
                let slice = (frame_deadline - now).min(FRAME_WAIT_SLICE);
                match receive_observation_frame(
                    frame_subscription
                        .as_ref()
                        .expect("frame subscription initialized above"),
                    &canvas,
                    slice,
                ) {
                    Ok(None) => {}
                    other => {
                        frame_outcome = other;
                        break;
                    }
                }
            }
            match frame_outcome {
                Ok(Some(frame)) => {
                    if !scan_frame_is_fresh(frame.captured_at, scan_boundary) {
                        log::debug!("丢弃扫描新鲜度边界之前捕获的旧观察帧");
                        continue;
                    }
                    if let Ok(mut latest_frame) = self.ui.latest_frame.lock() {
                        latest_frame.store(Arc::clone(&frame.image));
                    } else {
                        log::error!(
                            "主扫描画面缓存已损坏（步骤：写入最新观察帧），本次截图不会进入缓存，依赖该画面的下游步骤本轮拿不到新画面"
                        );
                    }
                    let frame_ms = elapsed_ms(frame_started);
                    log::debug!(target: "timing",
                        "观察帧交付: wait={}ms age={}ms",
                        frame_ms,
                        frame.captured_at.elapsed().as_millis()
                    );
                    if target_missing {
                        log::debug!("目标窗口已恢复，截图退避重置为初始值");
                        self.reset_chat_observation_baseline("目标窗口恢复")?;
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
                        Ok((ui_state, kind)) => {
                            // 界面状态迁移推送到事件流：迁移图应当能被"听"出来，
                            // 而不是靠轮询 /monitor 再逐个比对。
                            if dedup.ui_transitions.take_transition(*kind) {
                                crate::debug_events::bus().publish(
                                    "ui.state",
                                    serde_json::json!({
                                        "kind": kind.map(|kind| kind.name()),
                                        "stable": kind.is_some(),
                                        "label": ui_state,
                                    }),
                                );
                            }
                            self.shared.monitor.publish(MonitorEvent::UiState(
                                ui_state_display_label(ui_state.as_str()),
                            ))
                        }
                        Err(_) => self
                            .shared
                            .monitor
                            .publish(MonitorEvent::UiState("界面检测失败".to_string())),
                    }
                    // 统一观测「界面持续无法识别」：只记录与汇报，不触发任何自动动作。
                    // 原先主循环对 Unknown 只打一条 debug 就跳过，这里保持行为等价。
                    recovery_guard.observe_ui_kind(
                        &self.shared.recovery,
                        Instant::now(),
                        matches!(ui_state_result, Ok((_, Some(UiStateKind::Unknown)))),
                    );
                    let ui_ms = elapsed_ms(ui_started);
                    // 值守切换自愈：pending_mode 置位后必须有人在正式队列里执行
                    // SetChatListenerMode，否则它会永久悬挂——既切不到目标模式，
                    // 又因为 pending_mode.is_some() 挡住后续一切驻留恢复与再次切换。
                    // 外部聊天源、HTTP 面板都会先 request 再入队；这里兜住任何
                    // "只置位未入队"的路径（包括入队失败后没撤销的情况）。
                    self.reconcile_pending_listener_mode()?;
                    // 外部聊天源的默认值守：等**第一次真正停在一级聊天界面**时
                    // 切一次二级。放在这里而不是握手时，是因为握手发生在游戏还没
                    // 进界面的时候，那时按界面门控必然被跳过。
                    if let Ok((state, Some(UiStateKind::Primary))) = &ui_state_result {
                        self.maybe_switch_to_secondary(state.as_str())?;
                    }
                    let listener_snapshot = self.business.business.chat_listener_snapshot()?;
                    let command_executing = !self.business.task_engine.is_idle()?;
                    let mut residency_recovery_attempted = false;
                    let unresolved_ui_reason = match &ui_state_result {
                        Ok((ui_state, None)) => Some(ui_state.clone()),
                        Err(error) => Some(format!("状态分类失败: {error:#}")),
                        Ok((_, Some(_))) => None,
                    };
                    match &ui_state_result {
                        Ok((_, Some(kind @ (UiStateKind::GameGate | UiStateKind::Overworld)))) => {
                            unresolved_ui_since = None;
                            if !game_location_baseline_reset {
                                self.reset_chat_observation_baseline("已确认处于大门或主世界")?;
                                game_location_baseline_reset = true;
                            }
                            if !command_executing
                                && listener_snapshot.pending_mode.is_none()
                                && Instant::now() >= automatic_startup_retry_after
                                && let Some(task) = automatic_startup_task(&live_config, *kind)
                            {
                                log::debug!("界面已稳定，排队执行自动进入任务：{}", task.label());
                                self.push_pending_task(PendingTask::Startup(task))?;
                                automatic_startup_pending = true;
                                residency_recovery_attempted = true;
                            } else if automatic_startup_task(&live_config, *kind).is_none() {
                                listener_ready_evidence = Some(ListenerReadyEvidence::GameLocation);
                            }
                        }
                        Ok((ui_state, Some(ui_kind))) => {
                            unresolved_ui_since = None;
                            game_location_baseline_reset = false;
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
                                if loop_failure_warn_due(
                                    &mut listener_residency_mismatch_warned_at,
                                    Instant::now(),
                                ) {
                                    log::warn!(
                                        "聊天值守模式与当前界面不一致（逻辑模式：{}，实际界面：{}），尝试把界面切回{}；本条在问题持续期间最多每分钟重复一次",
                                        listener_snapshot.mode.label(),
                                        ui_state,
                                        residency_target_label(target)
                                    );
                                } else {
                                    log::debug!(
                                        "聊天值守模式仍与界面不一致（实际界面：{}），继续尝试切回{}",
                                        ui_state,
                                        residency_target_label(target)
                                    );
                                }
                                match self.establish_ui_residency(
                                    target,
                                    ResidencyPurpose::IndependentRecovery("恢复持久化聊天值守驻留"),
                                ) {
                                    Ok(()) => {
                                        log::debug!(
                                            "聊天值守驻留恢复完成，界面已切回{}",
                                            residency_target_label(target)
                                        )
                                    }
                                    Err(error) => log::warn!(
                                        "聊天值守驻留恢复失败（步骤：把界面切回{}），逻辑模式保持不变，稍后重试；本条在问题持续期间最多每分钟重复一次：{error:#}",
                                        residency_target_label(target)
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
                            if !game_location_probe_blocks_residency(frame.ui_state.as_ref())
                                && unresolved_ui_recovery_due(
                                    Some(unresolved_since),
                                    now,
                                    listener_residency_retry_after,
                                    listener_snapshot.pending_mode.is_some(),
                                    command_executing,
                                )
                            {
                                let target = listener_residency(
                                    listener_snapshot.mode,
                                    listener_snapshot.temporary_primary,
                                );
                                if loop_failure_warn_due(
                                    &mut unresolved_ui_warned_at,
                                    Instant::now(),
                                ) {
                                    log::warn!(
                                        "界面持续无法验证（逻辑模式：{}，最后识别结果：{}），尝试把界面切回{}；本条在问题持续期间最多每分钟重复一次",
                                        listener_snapshot.mode.label(),
                                        ui_state,
                                        residency_target_label(target)
                                    );
                                } else {
                                    log::debug!(
                                        "界面仍无法验证（最后识别结果：{}），继续尝试切回{}",
                                        ui_state,
                                        residency_target_label(target)
                                    );
                                }
                                match self.establish_ui_residency(
                                    target,
                                    ResidencyPurpose::IndependentRecovery(
                                        "恢复持续未知界面的聊天值守驻留",
                                    ),
                                ) {
                                    Ok(()) => log::debug!(
                                        "持续未知界面的聊天值守驻留恢复完成，界面已切回{}",
                                        residency_target_label(target)
                                    ),
                                    Err(error) => log::warn!(
                                        "持续未知界面的聊天值守驻留恢复失败（步骤：把界面切回{}），稍后重试；本条在问题持续期间最多每分钟重复一次：{error:#}",
                                        residency_target_label(target)
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
                            // 主菜单虽然是叠在大厅上的浮层，但它会盖住聊天区，
                            // 所以和 GameGate/Overworld 一样按"不在聊天界面"处理，
                            // 顺手丢弃旧的残留观察，菜单关闭后重新验证（§44.10）。
                            // 加载界面同理：此时根本没有聊天区，旧的观察必须作废（§55）。
                            Ok((
                                _,
                                Some(
                                    UiStateKind::GameGate
                                    | UiStateKind::Overworld
                                    | UiStateKind::MainMenu
                                    | UiStateKind::Loading
                                    | UiStateKind::Panel,
                                ),
                            )) => {
                                primary_visible = false;
                                last_fingerprint = None;
                                secondary_friend_bubble_fingerprint = None;
                                secondary_hall_bubble_sequence = None;
                                secondary_hall_command_tracker.reset();
                                secondary_identity = None;
                            }
                            Ok((ui_state, None)) => {
                                log::debug!(
                                    "界面仍在过渡中（当前判定：{}），本轮暂停聊天扫描",
                                    ui_state
                                );
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
                                        "二级值守任务临时离开二级界面（当前界面：{}），等待任务状态机恢复",
                                        ui_state
                                    );
                                    None
                                } else {
                                    // 该分支每轮都会走到，必须节流：否则 60ms 一轮时
                                    // 每秒刷十几条 warn，把其余告警全冲掉。
                                    let warn_due =
                                        secondary_ui_mismatch_warned_at.is_none_or(|last| {
                                            last.elapsed() >= SECONDARY_UI_MISMATCH_WARN_INTERVAL
                                        });
                                    if warn_due {
                                        secondary_ui_mismatch_warned_at = Some(Instant::now());
                                        log::warn!(
                                            "二级值守当前不在二级聊天界面（当前界面：{}），保留逻辑模式并等待驻留恢复；本条在界面切换回来前最多每分钟重复一次",
                                            ui_state
                                        );
                                    } else {
                                        log::debug!(
                                            "二级值守仍不在二级聊天界面（当前界面：{}），继续等待驻留恢复",
                                            ui_state
                                        );
                                    }
                                    secondary_friend_bubble_fingerprint = None;
                                    secondary_hall_bubble_sequence = None;
                                    secondary_hall_command_tracker.reset();
                                    secondary_identity = None;
                                    None
                                };
                                let scanned = outcome.is_some_and(|outcome| outcome.scanned);
                                secondary_hall_visible =
                                    outcome.is_some_and(|outcome| outcome.verified);
                                if secondary_hall_visible {
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
                                primary_visible = true;
                                if listener_snapshot.mode == ChatListenerMode::Primary {
                                    secondary_friend_bubble_fingerprint = None;
                                    secondary_hall_bubble_sequence = None;
                                    secondary_hall_command_tracker.reset();
                                    secondary_identity = None;
                                }
                                // Recognition is supplied independently, even while this navigation loop is blocked.
                                if self.ui.chat_timeline.latest()?.is_some_and(|snapshot| {
                                    snapshot.chat.as_ref() == Some(&ChatIdentity::PrimaryHall)
                                }) {
                                    listener_ready_evidence =
                                        Some(ListenerReadyEvidence::PrimaryScan);
                                }
                            }
                            Ok((ui_state, Some(UiStateKind::Secondary))) => {
                                primary_visible = false;
                                secondary_friend_bubble_fingerprint = None;
                                secondary_hall_bubble_sequence = None;
                                secondary_hall_command_tracker.reset();
                                secondary_identity = None;
                                log::debug!(
                                    "当前是一级值守但不在聊天界面（当前界面：{}），本轮跳过聊天扫描",
                                    ui_state
                                );
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
                                log::debug!(
                                    "界面状态仍在过渡中（当前判定：{}），本轮暂停聊天扫描",
                                    ui_state
                                );
                            }
                            Err(error) => {
                                primary_visible = false;
                                if loop_failure_warn_due(
                                    &mut ui_classify_failed_warned_at,
                                    Instant::now(),
                                ) {
                                    log::warn!(
                                        "界面状态检测失败（步骤：识别当前界面），本轮不派发任何命令，下一轮重新识别；本条在问题持续期间最多每分钟重复一次：{error:#}"
                                    );
                                } else {
                                    log::debug!("界面状态检测再次失败，继续下一轮识别：{error:#}");
                                }
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
                    // A receive timeout is a normal polling tick, not a capture failure.
                    drop(scan_lease.take());
                    self.maybe_reload_config_when_idle(&mut reload_check_after);
                    if !woken_by_snapshot && self.shared.running.load(AtomicOrdering::SeqCst) {
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
                    drop(scan_lease.take());
                    self.invalidate_latest_frame();
                    unresolved_ui_since = None;
                    if let Some(subscription) = frame_subscription.take()
                        && let Err(cancel_error) = subscription.cancel()
                    {
                        log::warn!(
                            "撤销观察帧需求失败（触发原因：截图失败），旧订阅可能多存活一轮，重试时会重新申请：{cancel_error}"
                        );
                    }
                    let frame_ms = elapsed_ms(frame_started);
                    if !target_missing {
                        self.abort_entertainment_for_context_loss("目标游戏窗口已关闭或不可用");
                    }
                    self.shared
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
                    if loop_failure_warn_due(&mut capture_failed_warned_at, Instant::now()) {
                        log::warn!(
                            "截图失败（步骤：抓取游戏画面），本轮无法识别界面与聊天，{} 秒后重试；本条在问题持续期间最多每分钟重复一次：{error:#}",
                            target_missing_wait.as_secs()
                        );
                    } else {
                        log::debug!(
                            "截图再次失败，{} 秒后重试：{error:#}",
                            target_missing_wait.as_secs()
                        );
                    }
                    log::info!(target: "timing",
                        "主循环阶段耗时: total={}ms frame={}ms state=capture_error retry={}ms",
                        elapsed_ms(loop_started),
                        frame_ms,
                        target_missing_wait.as_millis()
                    );
                    target_missing = true;
                    self.maybe_idle_exit()?;
                    self.maybe_reload_config_when_idle(&mut reload_check_after);
                    if !self.shared.running.load(AtomicOrdering::SeqCst) {
                        continue;
                    }
                    if self.activate_reload_startup_fallback("目标窗口不可用")? {
                        replacement_ui_unverified_since = None;
                        reload_startup_retry_after =
                            Instant::now() + CONFIG_RELOAD_STARTUP_RETRY_INTERVAL;
                    }
                    let window_retry_at = Instant::now() + target_missing_wait;
                    // 保持窗口捕获退避，同时继续检查大厅截止时间。
                    let window_reset = loop {
                        self.poll_hall_expiry(
                            &mut hall_expiry_pause_applied,
                            &mut hall_expiry_notice_sent,
                            &mut hall_expiry_check_after,
                        );
                        let now = Instant::now();
                        if !self.shared.running.load(AtomicOrdering::SeqCst)
                            || now >= window_retry_at
                        {
                            break false;
                        }
                        let wait = window_retry_at
                            .saturating_duration_since(now)
                            .min(hall_expiry_check_after.saturating_duration_since(now));
                        if self
                            .ui
                            .window_detection_signal
                            .wait_for_change(observed_window_detection_generation, wait)?
                        {
                            break true;
                        }
                    };
                    if window_reset {
                        log::debug!("收到窗口检测重置请求，立即重试截图并重置退避");
                        target_missing_backoff = TARGET_MISSING_BACKOFF_INITIAL;
                    } else {
                        target_missing_backoff =
                            next_target_missing_backoff(target_missing_backoff);
                    }
                    // **必须在抓帧失败分支也判一次**：游戏崩溃 / 窗口消失是
                    // 「失联后需要重启游戏」最常见的形态，而这时抓帧会持续失败，
                    // 只在成功分支判等于把这个场景整个漏掉。
                    // 这里读的是外部聊天源的失联计时（与界面无关），
                    // 玩家主动退出游戏由闲置退出抑制位挡住（见 maybe_idle_exit）。
                    self.poll_auto_game_restart(&live_config, &mut recovery_guard)?;
                    continue;
                }
            }
            // 一级模式看 primary_visible；二级值守停在"本大厅聊天界面"时
            // secondary_hall_visible 才为真——两者都代表"现在能往大厅说话"。
            if (primary_visible || secondary_hall_visible)
                && self.business.task_engine.is_idle()?
                && self.maybe_warn_hall_expiring()?
            {
                suppress_change_until = Instant::now()
                    + Duration::from_millis(live_config.timing.command.post_settle_ms);
                force_scan_after = Some(suppress_change_until);
                force_scan_reason = Some("hall-expiring");
                last_fingerprint = None;
                last_ocr_at = Instant::now();
            }
            drop(scan_lease.take());
            self.forward_completion_advances(completion_subscriber)?;
            // 自动恢复守卫：读本轮刚判出的界面状态与外部聊天源状态，不发输入、
            // 不抢扫描租约（此时已 drop），真正重启动作走正式任务队列。
            self.poll_auto_game_restart(&live_config, &mut recovery_guard)?;
            self.maybe_idle_exit()?;
            self.maybe_reload_config_when_idle(&mut reload_check_after);
            self.poll_playlist_import_completions()?;
            if !self.shared.running.load(AtomicOrdering::SeqCst) {
                continue;
            }
            let ready_snapshot = self.business.business.chat_listener_snapshot()?;
            let ready_task_busy = !self.business.task_engine.is_idle()?;
            let listener_ready = listener_ready_after_recheck(
                listener_ready_evidence,
                &ready_snapshot,
                ready_task_busy,
                self.shared.paused.load(AtomicOrdering::SeqCst),
                self.shared.running.load(AtomicOrdering::SeqCst),
            );
            let startup_ready = self.shared.reload_startup.is_satisfied();
            if replacement_ready_allowed(listener_ready, startup_ready)
                && crate::mark_config_reload_child_ready()?
            {
                if std::env::var_os(crate::CONFIG_RELOAD_CHILD_ENV).is_some() {
                    log::info!("配置重载的替代进程已完成交接并进入就绪状态");
                }
            } else if listener_ready {
                // 必需的启动工作仍在运行或已经失败。仅验证屏幕不能确认退出码 77 的交接已就绪。
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
            log::warn!(
                "扫描循环结束时撤销观察帧需求失败（步骤：退出前释放观察帧订阅），旧订阅随进程退出一起回收：{error}"
            );
        }

        Ok(())
    }

    pub(in crate::composition::application) fn tick_entertainment(&self) {
        let scheduler_idle = match self.business.task_engine.snapshot() {
            Ok(snapshot) => snapshot.is_idle(),
            Err(error) => {
                log::error!("无法读取业务调度状态，娱乐计时保持暂停：{error}");
                false
            }
        };
        let clock_active = !self.shared.paused.load(AtomicOrdering::SeqCst) && scheduler_idle;
        if let Err(error) = self
            .business
            .business
            .refresh_turtle_soup_deadline(Instant::now(), clock_active)
        {
            log::error!("无法同步海龟汤期限：{error:#}");
        }
        let card_game_outcome = match self
            .business
            .card_games
            .poll_timed_outcome(Instant::now(), clock_active)
        {
            Ok(outcome) => outcome,
            Err(error) => {
                log::error!("无法推进牌局回合计时：{error:#}");
                None
            }
        };
        if let Some(outcome) = card_game_outcome {
            let key = outcome.key();
            let effect = self.business.card_games.timed_effect(outcome);
            if let Err(error) = self.push_pending_task(PendingTask::CardGameEffect(effect)) {
                // 真实后果分两种：入队失败后立即回滚取消（下一轮计时会重新判定），
                // 只有回滚也失败才会真的卡住——那种情况由下面那条 error 单独报。
                log::error!(
                    "牌局计时结果入队失败（步骤：把超时效果加入待处理队列）：本次超时判定的出牌或结束动作没有执行，牌局停在当前回合；已在回滚该效果，若回滚成功下一轮计时会重新判定并可继续，回滚失败则牌局计时不再推进：{error:#}"
                );
                if let Err(cancel_error) = self.business.card_games.cancel_effect(key) {
                    log::error!("牌局计时结果入队失败后无法清理牌局：{cancel_error:#}");
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
                log::error!("无法推进谁是卧底计时：{error:#}");
                None
            }
        };
        if let Some(outcome) = undercover_outcome {
            let key = outcome.key();
            let effect = self.business.undercover_game.timed_effect(outcome);
            if let Err(error) = self.push_pending_task(PendingTask::UndercoverEffect(effect)) {
                log::error!(
                    "谁是卧底计时消息入队失败（步骤：把超时效果加入待处理队列）：本次超时判定的淘汰或揭示动作没有执行，本局停在当前阶段；已在回滚该效果，若回滚成功下一轮计时会重新判定并可继续，回滚失败则本局计时不再推进：{error:#}"
                );
                if let Err(cancel_error) = self.business.undercover_game.cancel_effect(key) {
                    log::error!("谁是卧底计时消息入队失败后无法清理牌局：{cancel_error:#}");
                }
            }
        }
        // 数字炸弹的超时推进在业务线程内完成，这里只把已经产生的文案送入
        // 延迟聊天队列；不需要 claim/resume，因为超时没有需要回滚的 UI 效果。
        let number_bomb_outcome = match self
            .business
            .business
            .poll_number_bomb_timeout(Instant::now(), clock_active)
        {
            Ok(outcome) => outcome,
            Err(error) => {
                log::error!("无法推进数字炸弹计时：{error:#}");
                None
            }
        };
        if let Some(outcome) = number_bomb_outcome {
            for reply in &outcome.replies {
                let Some(text) = self.render_template_text(reply.spec, &reply.vars) else {
                    continue;
                };
                if let Err(error) = enqueue_current_hall_reply(&self.business.task_engine, &text) {
                    log::error!(
                        "数字炸弹超时提示入队失败（步骤：把自动猜测结果加入延迟聊天队列），本次提示不会发送给玩家，玩局状态已经推进：{error:#}"
                    );
                }
            }
        }
    }

    /// 值守切换自愈：pending_mode 有值但正式队列里没有对应任务时补入队。
    ///
    /// 正常情况下 request + enqueue 由同一个生产者完成；但只要中途入队失败、
    /// 队列被撤销，或某个调用点只 request 不入队，pending_mode 就会永久悬挂：
    /// 目标模式切不过去，且 `pending_mode.is_some()` 会挡住后续所有驻留恢复，
    /// 表现为「卡在一级值守，怎么都进不了二级」。
    ///
    /// 去重键保证这里重复调用是安全的：队列里已有同模式任务时直接返回。
    /// 外部聊天源启用时，第一次真正停在一级聊天界面就切到二级。
    ///
    /// 只做一次：用 `claim_initial_secondary_switch` 原子认领，之后用户手动
    /// 切回一级不会再被顶回二级。
    ///
    /// 准入条件除了「一级界面」，还要求外部源已接管、当前不是二级、没有正在
    /// 进行的切换，避免与启动自动化抢 UI 事务。
    pub(in crate::composition::application) fn maybe_switch_to_secondary(
        &self,
        ui_state: &str,
    ) -> Result<()> {
        if !self.shared.external_chat.supplies_hall_messages() {
            return Ok(());
        }
        let snapshot = self.business.business.chat_listener_snapshot()?;
        if snapshot.mode == ChatListenerMode::Secondary || snapshot.pending_mode.is_some() {
            return Ok(());
        }
        if !self.shared.external_chat.claim_initial_secondary_switch() {
            return Ok(());
        }
        log::info!(
            "外部聊天源首次遇到一级聊天界面，自动切换到二级值守（当前界面：{}）",
            ui_state
        );
        if let Err(error) = self
            .business
            .business
            .request_chat_listener_mode(ChatListenerMode::Secondary)
        {
            log::warn!(
                "外部聊天源自动切换到二级值守失败（步骤：请求切换值守模式），当前保持一级值守，不再自动重试：{error}"
            );
        }
        Ok(())
    }

    pub(in crate::composition::application) fn reconcile_pending_listener_mode(
        &self,
    ) -> Result<()> {
        let snapshot = self.business.business.chat_listener_snapshot()?;
        let Some(target) = snapshot.pending_mode else {
            return Ok(());
        };
        if snapshot.mode == target {
            // 已经切到了目标模式，pending 只是残留：直接收尾。
            self.business.business.complete_chat_listener_mode(target)?;
            log::info!(
                "值守模式已经是{}，清理残留的切换请求，避免它继续挡住驻留恢复",
                target.label()
            );
            return Ok(());
        }
        let key = crate::runtime::scheduler::FormalTaskDedupKey::new(format!(
            "chat_listener_mode_{}",
            target.label()
        ));
        if self.business.task_engine.contains_formal_dedup_key(&key)? {
            return Ok(());
        }
        log::warn!(
            "值守切换请求没有对应的排队任务（目标模式：{}），已补入队，否则该请求会一直悬挂",
            target.label()
        );
        if let Err(error) = self.push_pending_task(PendingTask::SetChatListenerMode { target }) {
            // 入队失败就撤销请求，避免下个周期继续空转。
            if let Err(cancel_error) = self
                .business
                .business
                .cancel_chat_listener_mode_request(target)
            {
                log::error!(
                    "撤销值守切换请求失败（步骤：入队失败后回滚切换请求），该请求可能残留在待处理状态并挡住后续驻留恢复，需要人工核对：{cancel_error}"
                );
            }
            return Err(error);
        }
        Ok(())
    }

    pub(in crate::composition::application) fn execute_set_chat_listener_mode(
        &self,
        target: ChatListenerMode,
    ) -> Result<()> {
        self.abort_entertainment_for_context_loss("聊天值守模式即将切换");
        let residency = match target {
            ChatListenerMode::Primary => UiResidency::Primary,
            ChatListenerMode::Secondary => UiResidency::SecondaryCurrentHall,
        };
        if self
            .establish_ui_residency(residency, ResidencyPurpose::ListenerModeSwitch)
            .is_ok()
        {
            self.complete_chat_listener_mode(target)?;
            log::info!("聊天值守模式已切换为{}并生效", target.label());
            return Ok(());
        }

        self.fail_chat_listener_mode_to_primary()?;
        let _ = self.establish_ui_residency(
            UiResidency::Primary,
            ResidencyPurpose::IndependentRecovery("值守切换失败回退一级"),
        );
        Err(anyhow!("切换{}失败，已回退一级值守", target.label()))
    }
}
