mod ai_chat_commands;
mod commands;
mod custom_workflow;
#[cfg(test)]
mod decision_tests;
mod delivery;
mod diagnostics;
pub(crate) mod formal_task;
/// Cross-platform command/login/player/AI facades, re-exported here so Windows call sites keep
/// the same `http_facade::` path while Linux test builds can use them without `composition`.
pub(crate) use crate::interfaces::http::facade as http_facade;
pub(crate) mod external_chat;
mod lifecycle;
mod listener;
mod moderation;
mod playback;
#[allow(dead_code)]
mod secondary_chat;
mod shared_observer;
mod song_request_port;
mod startup;
mod tasks;
mod workers;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, sleep};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use self::formal_task::{FormalTaskClient, FormalTaskRuntime};
use crate::adapters::input::parse_key;
use crate::adapters::login_helper::LoginHelperManager;
use crate::adapters::native_playback::NativePlaybackAdapter;
use crate::adapters::player::PlayerRuntimeBackend;
use crate::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
use crate::config::{AppConfig, LiveConfigs, PointConfig};
use crate::features::administration::{
    AdministrationApplication, AdministrationCommand, ChatListenerModeCommand,
};
use crate::features::card_games::{
    CardGameApplication, CardGameDeliveryPort, CardGameEffectLane, CardGameEffectTask,
    CardGameService, LandlordCommand, LandlordPrivateDelivery,
};
#[cfg(test)]
use crate::features::card_games::{CardGameCommandStart, LandlordConfig};
use crate::features::chat_text::command_identity;
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandObservation, CommandPrefix, ModuleCommand,
    RoutedCommand,
};
use crate::features::custom_workflow::{CustomWorkflowService, WorkflowDefaults};
use crate::features::friend_delivery::{FriendBatchOutcome, FriendMessage};
use crate::features::hall::{HallApplication, HallCommand, HallStateService};
use crate::features::identity::{IdentityAccess, IdentityRole};
use crate::features::idiom_chain;
use crate::features::idiom_chain::{IdiomChainApplication, IdiomChainService};
use crate::features::invite::{InviteRequest, InviteService, InviteStart};
use crate::features::moderation::{ModerationPolicy, ModerationResultTask, ModerationService};
use crate::features::playback::{
    ActivePlaybackIdentity, ConfirmedPlaybackState, ExternalPlaybackObservation,
    PlaybackApplication, PlaybackApplicationConfig, PlaybackCommand, PlaybackObservation,
    PlaybackRequest, PlaybackRuntimeState, PlaybackService, PlaybackStatePort, PlaybackStateUpdate,
    PlaybackTimePorts, PlayerController, PlayerStatus, QueueAdvanceContext, QueueAdvanceDecision,
    QueueItem, SongDedupCandidate, has_restorable_playback_progress,
};
use crate::features::song_request::{
    AiClient, ResolvedSongRequest, SongCommand, SongRequestApplication, SongRequestContext,
    SongRequestDecision, SongReviewClient,
};
use crate::features::startup::{StartupService, StartupSource, StartupTask, StartupTaskKind};
use crate::features::turtle_soup::{
    self, TurtleSoupApplication, TurtleSoupConfig, TurtleSoupService,
};
use crate::features::undercover::{
    UndercoverApplication, UndercoverCommand, UndercoverCommandSource, UndercoverDeliveryPort,
    UndercoverEffectTask, UndercoverRuntimeService,
};
use crate::geometry::{Rect, crop_canvas};
use crate::interfaces::chat::{self as command, ChatCommandRouter, PendingCommand};
use crate::interfaces::hotkeys;
use crate::interfaces::http::{self, WebToolRequest, WebToolTemplate};
use crate::observation::chat::{
    ChatMessage, ChatObservationDispatch, ChatObservationExclusiveGuard, ChatObservationShared,
    ChatScanTelemetry, ChatScanTelemetrySink, CompletionAdvanceSubscriber, ObservedFrame,
    PrimaryObservedMessage, ResolvedTemplateArgs, SecondaryChatIdentity, SecondaryChatObservation,
    SecondaryHallBubble, SecondaryObservedMessage, TemplateArgs, UnreadFriendHit,
    count_chat_markers, find_unread_friend_hits, hall_bubble_sequence_is_retained_prefix,
    hall_bubble_sequence_overlap, latest_incoming_fingerprint, prepare_chat_scan,
    prepare_chat_scan_with_markers, recognize_prepared_chat,
};
use crate::observation::decision::DecisionScreenLock;
use crate::observation::shared::ObservationRead;
use crate::privacy::redacted_chat_text;
use crate::runtime::business::{
    BusinessEvent, BusinessRuntime, BusinessRuntimeEventSink, BusinessRuntimeHandle,
    BusinessRuntimeWorker,
};
use crate::runtime::chat_listener::{ChatListenerMode, ChatListenerSnapshot};
use crate::runtime::deadline_bridge::{BusinessRuntimeGroup, BusinessRuntimeGroupBuilder};
use crate::runtime::decision::DecisionAction;
use crate::runtime::deferred_chat::{
    BatchFailureOutcome, DeferredChatItem, DeferredChatMessage, DeferredChatTarget, EnqueueOutcome,
};
use crate::runtime::monitor::{MonitorEvent, MonitorShared, OcrSnapshot};
use crate::runtime::ocr::{
    OcrArgs, OcrBackendProbeStatus, OcrPriority, OcrRuntime, OcrRuntimeConfig, OcrRuntimeHandle,
    ProductionOcrDevice, ResolvedOcrArgs, probe_ocr_backend_support,
};
use crate::runtime::player_io::{
    PlayerRuntime, PlayerRuntimeConfig, PlayerSearchClient, PlayerSearchClientError,
};
use crate::runtime::scheduler::FormalTaskEnqueueOutcome;
use crate::runtime::task_engine::TaskEngineHandle;
use crate::runtime::ui::{
    FrameDemand, FrameDemandSubscription, FramePublication, UiCoordinator, UiRuntime, UiStateKind,
    UiStateObservation,
};
use crate::ui::atoms::GameUi;
use crate::ui::chat_output::{ChatBatchSendOutcome, ChatBatchSendStatus, ChatOutput};
use crate::ui::frame::{Canvas, Frame, LatestFrameCache, from_captured_frame};
#[cfg(test)]
use crate::ui::locator::secondary_hall_search_rect;
use crate::ui::plan::{WorkflowOperation, WorkflowResidency};
use crate::ui::routines::{
    CustomActionUi, DetectPublicHall, DetectPublicHallEffect, EstablishResidency,
    FriendDeliveryRoutineConfig, FriendDeliveryRoutineConfigSource, FriendDeliveryUi, HallBatchUi,
    HallRoutineConfig, HallUi, InviteRoutineConfig, InviteRoutineConfigSource, InviteUi,
    ModerationRoutineConfig, ModerationRoutineConfigSource, ModerationUi, ProcessSecondaryUnread,
    ReadHallInfo, ReadHallInfoEffect, ResidencyUi, SecondaryUnreadEffect,
    SecondaryUnreadRoutineConfig, SecondaryUnreadUi, StartupRoutineConfig, StartupUi,
    StartupUiConfig, StartupUiTemplates, ToggleMicrophone, ToggleMicrophoneEffect,
    UiResidencyOutcome, UiResidencyTarget,
};
use crate::ui::state::{
    PanelRegions, ResolvedUiTemplateArgs, TemplateUiStateClassifier, UiTemplateArgs,
};
use crate::ui::template::{best_template_hit, find_template_hits};
use anyhow::{Context, Result, anyhow};
use enigo::Key;
use image::DynamicImage;
use miliastra_kernel::ai::OpenAiRuntime;
use miliastra_kernel::clock::SystemClock;
use miliastra_kernel::identity::BusinessOperationIdAllocator;
use miliastra_playback::{
    PlayableTrack, PlaybackHandle, PlaybackRuntime as NativePlaybackRuntime, TrackKey,
};

const TARGET_MISSING_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const TARGET_MISSING_BACKOFF_MAX: Duration = Duration::from_secs(60);
const CONFIG_RELOAD_IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const UI_RUNTIME_QUEUE_CAPACITY: usize = 32;
const OCR_RUNTIME_QUEUE_CAPACITY: usize = 64;
const BUSINESS_RUNTIME_QUEUE_CAPACITY: usize = 64;
const DEADLINE_RUNTIME_QUEUE_CAPACITY: usize = 64;

impl ChatScanTelemetrySink for MonitorShared {
    fn publish_chat_scan(&self, telemetry: ChatScanTelemetry) {
        self.publish(MonitorEvent::Ocr(OcrSnapshot::new(
            telemetry.marker_count,
            telemetry.lines,
            telemetry.marker_ms,
            telemetry.ocr_ms,
            telemetry.total_ms,
            telemetry.scope,
        )));
    }
}

pub(crate) struct ResolvedApplicationConfig {
    app: AppConfig,
    /// 热更新共享句柄集合（阶段 7）：resolve 时从最终配置创建，
    /// 由 ApplicationRuntime::new 分发到各消费方并随生命周期持有。
    live_configs: LiveConfigs,
    player_runtime: PlayerRuntimeConfig,
    ocr: ResolvedOcrArgs,
    chat_templates: ResolvedTemplateArgs,
    ui_templates: ResolvedUiTemplateArgs,
    friend_delivery: FriendDeliveryRoutineConfig,
    hall: HallRoutineConfig,
    moderation: ModerationRoutineConfig,
    startup: StartupRoutineConfig,
    secondary_unread: SecondaryUnreadRoutineConfig,
    invite: InviteRoutineConfig,
    turtle_soup: TurtleSoupConfig,
    ai_request_timeout: Duration,
}

impl ResolvedApplicationConfig {
    pub(crate) fn resolve(app: AppConfig) -> Result<Self> {
        app.validate().context("启动前校验组合配置")?;
        // 热更新共享值以最终启动配置为准（与运行态消费方同一份值）。
        let live_configs = LiveConfigs::from_config(&app);

        let player_runtime = app
            .player_runtime_config()
            .context("校验播放器运行时配置")?;
        let ocr = OcrArgs::default().resolve(&app.ocr);
        let chat_templates = TemplateArgs::default().resolve(&app.templates, &app.ocr);
        let ui_templates = UiTemplateArgs::default().resolve(&app.templates, &app.ocr);
        let friend_delivery =
            FriendDeliveryRoutineConfig::resolve(FriendDeliveryRoutineConfigSource {
                screen: &app.screen,
                templates: &app.templates,
                ocr: &app.ocr,
                output: &app.output,
                input_timing: &app.timing.input,
                auto_retry_count: live_configs.friend_delivery_auto_retry_count.clone(),
                send_enabled: live_configs.output_send_enabled.clone(),
                friend_list_region: app.invite.friend_list_region.into(),
                fuzzy_friend_match: app.invite.fuzzy_friend_match,
                friend_step_ms: app.timing.invite.step_ms,
                timeout_ms: app.timing.workflow.default_timeout_ms,
                poll_ms: app.timing.workflow.default_poll_ms,
                stable_count: app.resolve_stability_count(app.invite.friend_name_stable_count),
            });
        let hall = HallRoutineConfig::resolve(
            friend_delivery.clone(),
            &app.screen,
            &app.timing.hall,
            &app.ocr,
        );
        let moderation = ModerationRoutineConfig::resolve(ModerationRoutineConfigSource {
            residency: friend_delivery.clone(),
            friend_panel_template: app.templates.friend_panel.clone(),
            search_panel_template: app.templates.friend_search_panel.clone(),
            more_settings_template: app.templates.friend_more_settings.clone(),
            blacklist_template: app.templates.friend_blacklist.clone(),
            block_chat_template: app.templates.friend_block_chat.clone(),
            confirm_template: app.templates.friend_confirm.clone(),
            friend_panel_region: app.moderation.friend_panel_region.into(),
            search_panel_region: app.moderation.search_panel_region.into(),
            more_settings_region: app.moderation.more_settings_region.into(),
            blacklist_region: app.moderation.blacklist_region.into(),
            block_chat_region: app.moderation.block_chat_region.into(),
            confirm_region: app.moderation.confirm_region.into(),
            search_input: crate::geometry::Point::new(
                app.moderation.search_input_point.x,
                app.moderation.search_input_point.y,
            ),
            search_button: crate::geometry::Point::new(
                app.moderation.search_button_point.x,
                app.moderation.search_button_point.y,
            ),
            marker_threshold: app.templates.marker_threshold,
            ui_timeout_ms: app.timing.command.ui_timeout_ms,
            search_timeout_ms: app.timing.moderation.search_result_timeout_ms,
            confirm_wait_ms: app.timing.moderation.confirm_wait_ms,
            step_ms: app.timing.invite.step_ms,
            text_ms: app.timing.input.text_ms,
            return_retry_ms: app.timing.command.return_retry_ms,
        });
        let startup = StartupRoutineConfig::resolve(
            StartupUiConfig {
                launch_game: app.startup.launch_game,
                // 启用外部聊天源时，游戏由 Miliastra-Wonderland-ChatBridge 拉起（只有它能注入）。
                external_launcher: (app.chat_source.enabled && app.chat_source.auto_start)
                    .then(|| app.chat_source.clone()),
                enter_game: app.startup.enter_game,
                exe_path: app.startup.exe_path.clone(),
                game_args: app.startup.game_args.clone(),
                launch_wait_ms: app.startup.launch_wait_ms,
                launch_retries: app.startup.launch_retries,
                enter_game_timeout_ms: app.startup.enter_game_timeout_ms,
                enter_wonderland_timeout_ms: app.startup.enter_wonderland_timeout_ms,
                wonderland_map_star_retries: app.startup.wonderland_map_star_retries,
                wonderland_map_star_retry_ms: app.startup.wonderland_map_star_retry_ms,
                wonderland_hall_retries: app.startup.wonderland_hall_retries,
                wonderland_hall_retry_ms: app.startup.wonderland_hall_retry_ms,
                wonderland_transition_timeout_ms: app.startup.wonderland_transition_timeout_ms,
                wonderland_confirm_stable_timeout_ms: app
                    .startup
                    .wonderland_confirm_stable_timeout_ms,
                final_primary_timeout_ms: app.startup.final_primary_timeout_ms,
                poll_ms: app.startup.poll_ms,
                stable_mean_threshold: app.startup.stable_mean_threshold,
                stable_changed_ratio_threshold: app.startup.stable_changed_ratio_threshold,
                template_threshold: app.startup.template_threshold,
                wonderland_confirm_threshold: app.startup.wonderland_confirm_threshold,
                world_wish_threshold: app.templates.marker_threshold,
                templates: StartupUiTemplates {
                    wonderland_map_star: app.startup.templates.wonderland_map_star.clone(),
                    wonderland_confirm: app.startup.templates.wonderland_confirm.clone(),
                    paimon_menu: app.startup.templates.paimon_menu.clone(),
                    world_wish: app.templates.world_wish.clone(),
                },
                enter_game_text_region: app.startup.enter_game_text_region.into(),
                world_wish_region: app.screen.world_wish_rect.into(),
                wonderland_hall_ocr_region: app.startup.wonderland_hall_ocr_region.into(),
                wonderland_confirm_region: app.startup.wonderland_confirm_region.into(),
                main_ui_region: app.startup.main_ui_region.into(),
                wonderland_map_star_region: app.startup.wonderland_map_star_region.into(),
            },
            friend_delivery.clone(),
            app.window.target_process.clone(),
        );
        let secondary_unread = SecondaryUnreadRoutineConfig::resolve(
            friend_delivery.clone(),
            app.ocr.same_line_y_tolerance,
            live_configs.change_debounce_ms.clone(),
        );
        let invite = InviteRoutineConfig::resolve(InviteRoutineConfigSource {
            friend: friend_delivery.clone(),
            view_star_template: app.templates.invite_view_star.clone(),
            view_star_region: app.invite.view_star_region.into(),
            goto_hall_template: app.templates.invite_goto_hall.clone(),
            goto_hall_region: app.invite.goto_hall_region.into(),
            enter_hall_template: app.templates.invite_enter_hall.clone(),
            enter_hall_region: app.invite.enter_hall_region.into(),
            template_threshold: app.templates.marker_threshold,
            button_timeout_ms: app.timing.workflow.default_timeout_ms,
            completion_timeout_ms: app.timing.command.ui_timeout_ms,
            poll_ms: app.timing.workflow.default_poll_ms,
            stable_count: app.resolve_stability_count(app.invite.friend_name_stable_count),
            click_ms: app.timing.input.click_ms,
            password_step_ms: app.timing.invite.step_ms,
            password_digit_ms: app.timing.input.text_ms,
        });
        let mut turtle_soup = app.turtle_soup.clone();
        turtle_soup.nickname_stable_count =
            app.resolve_stability_count_usize(turtle_soup.nickname_stable_count);
        turtle_soup.content_stable_count =
            app.resolve_stability_count_usize(turtle_soup.content_stable_count);
        let ai_request_timeout = Duration::from_millis(app.timing.external.ai_request_timeout_ms);

        Ok(Self {
            app,
            live_configs,
            player_runtime,
            ocr,
            chat_templates,
            ui_templates,
            friend_delivery,
            hall,
            moderation,
            startup,
            secondary_unread,
            invite,
            turtle_soup,
            ai_request_timeout,
        })
    }

    pub(crate) const fn app(&self) -> &AppConfig {
        &self.app
    }
}

fn receive_observation_frame(
    subscription: &FrameDemandSubscription,
    canvas: &Canvas,
    timeout: Duration,
) -> Result<Option<Frame>> {
    match subscription.recv_timeout(timeout) {
        Ok(FramePublication::Captured(published)) => {
            Ok(Some(from_captured_frame(&published, canvas)))
        }
        Ok(FramePublication::Failed(failure)) => Err(anyhow!(
            "UI runtime 观察帧截图失败 at {:?}: {}",
            failure.failed_at(),
            failure.reason()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(anyhow!("等待 UI runtime 发布观察帧时通道断开"))
        }
    }
}

struct UiBundle {
    coordinator: UiCoordinator,
    ocr_args: ResolvedOcrArgs,
    chat_templates: ResolvedTemplateArgs,
    hotkeys: Option<hotkeys::HotkeyRuntime>,
    game_ui: GameUi,
    residency_ui: ResidencyUi,
    hall_ui: HallUi,
    moderation_ui: ModerationUi,
    startup_ui: StartupUi,
    secondary_unread_ui: SecondaryUnreadUi,
    friend_delivery_ui: FriendDeliveryUi,
    invite_ui: InviteUi,
    custom_action_ui: CustomActionUi,
    ui_runtime: Option<UiRuntime>,
    chat_output: ChatOutput,
    ocr: OcrRuntimeHandle,
    ocr_runtime: Option<OcrRuntime>,
    latest_frame: Arc<Mutex<LatestFrameCache>>,
    window_detection_signal: WindowDetectionSignal,
    chat_baseline_primed: Arc<AtomicBool>,
    chat_observations: ChatObservationShared,
    chat_timeline: crate::observation::chat::ChatTimeline,
}

struct PlaybackBundle {
    player: PlayerController<PlayerRuntimeBackend, BusinessPlaybackStateAdapter>,
    playback_application: PlaybackApplication,
    player_search: PlayerSearchClient,
    player_runtime: Option<PlayerRuntime>,
    native_playback: PlaybackHandle,
    login_helper: LoginHelperManager,
    native_playback_runtime: Option<NativePlaybackRuntime>,
}

struct BusinessBundle {
    business: BusinessRuntimeHandle,
    task_engine: TaskEngineHandle,
    business_events: BusinessRuntimeEventSink,
    business_runtime: Option<BusinessRuntimeGroup>,
    formal_task_runtime: Option<FormalTaskRuntime>,
    formal_tasks: Option<FormalTaskClient>,
    background_commands: workers::BackgroundCommandManager,
    ai: AiClient,
    // 记忆与冷却门是「全局唯一一份」的进程内状态：facade 与正式执行上下文必须共享同一实例，
    // 因此用 Arc 而不是 Clone（它们本身也不该被复制出第二份冷却账本）。
    ai_chat_memory: std::sync::Arc<crate::features::ai_chat::AiChatMemory>,
    ai_chat_gate: std::sync::Arc<crate::features::ai_chat::TriggerGate>,
    ai_chat_openai: miliastra_kernel::ai::OpenAiRuntimeHandle,
    ai_connection: crate::config::ai::AiConfig,
    ai_request_timeout: std::time::Duration,
    song_requests: SongRequestApplication,
    card_games: CardGameApplication,
    administration_application: AdministrationApplication,
    hall_application: HallApplication,
    idiom_chain_application: IdiomChainApplication,
    tarot_application: crate::features::tarot::TarotApplication,
    playlist_application: crate::features::playlist::PlaylistApplication,
    turtle_soup_application: TurtleSoupApplication,
    undercover_game: UndercoverApplication,
    moderation: ModerationService,
    moderation_workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    startup: StartupService,
    custom_workflow: CustomWorkflowService,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum ShutdownReason {
    Running = 0,
    ConfigReload = 1,
    ConfigReloadWithStartup = 2,
    UserExit = 3,
}

#[derive(Clone, Debug)]
struct ShutdownState {
    reason: Arc<AtomicU8>,
}

impl ShutdownState {
    fn new() -> Self {
        Self {
            reason: Arc::new(AtomicU8::new(ShutdownReason::Running as u8)),
        }
    }

    fn reason(&self) -> ShutdownReason {
        match self.reason.load(AtomicOrdering::SeqCst) {
            value if value == ShutdownReason::Running as u8 => ShutdownReason::Running,
            value if value == ShutdownReason::ConfigReload as u8 => ShutdownReason::ConfigReload,
            value if value == ShutdownReason::ConfigReloadWithStartup as u8 => {
                ShutdownReason::ConfigReloadWithStartup
            }
            value if value == ShutdownReason::UserExit as u8 => ShutdownReason::UserExit,
            value => panic!("invalid shutdown reason value: {value}"),
        }
    }

    fn try_claim_config_reload(&self) -> bool {
        self.reason
            .compare_exchange(
                ShutdownReason::Running as u8,
                ShutdownReason::ConfigReload as u8,
                AtomicOrdering::SeqCst,
                AtomicOrdering::SeqCst,
            )
            .is_ok()
    }

    fn release_config_reload_claim(&self) {
        let _ = self.reason.compare_exchange(
            ShutdownReason::ConfigReload as u8,
            ShutdownReason::Running as u8,
            AtomicOrdering::SeqCst,
            AtomicOrdering::SeqCst,
        );
    }

    fn require_startup_after_reload(&self) {
        let _ = self.reason.compare_exchange(
            ShutdownReason::ConfigReload as u8,
            ShutdownReason::ConfigReloadWithStartup as u8,
            AtomicOrdering::SeqCst,
            AtomicOrdering::SeqCst,
        );
    }

    fn request_user_exit(&self) {
        self.reason
            .store(ShutdownReason::UserExit as u8, AtomicOrdering::SeqCst);
    }
}

/// 配置重载替代进程发布就绪前必须完成的启动动作。
///
/// 这些要求只对当前进程有效，描述的是创建本子进程的交接过程，而不是普通启动使用的自动化策略。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReloadStartupActions(u8);

impl ReloadStartupActions {
    const START_GAME: u8 = 1 << 0;
    const ENTER_WONDERLAND: u8 = 1 << 1;

    fn from_startup_config(config: &AppConfig) -> Self {
        Self::from_flags(config.startup.enabled, config.startup.launch_game, false)
    }

    const fn from_flags(enabled: bool, start_game: bool, enter_wonderland: bool) -> Self {
        if !enabled {
            return Self(0);
        }
        Self(
            (if start_game { Self::START_GAME } else { 0 })
                | (if enter_wonderland {
                    Self::ENTER_WONDERLAND
                } else {
                    0
                }),
        )
    }

    const fn is_empty(self) -> bool {
        self.0 == 0
    }

    const fn includes_start_game(self) -> bool {
        self.0 & Self::START_GAME != 0
    }

    const fn includes_enter_wonderland(self) -> bool {
        self.0 & Self::ENTER_WONDERLAND != 0
    }

    const fn for_task(kind: StartupTaskKind) -> Self {
        match kind {
            StartupTaskKind::StartGame => Self(Self::START_GAME),
            StartupTaskKind::EnterWonderland => Self(Self::ENTER_WONDERLAND),
        }
    }
}

/// 配置重载替代进程所需启动工作的共享完成闸门。
///
/// 正式任务通过重建的应用 facade 执行，因此该状态必须与所属运行时分开共享。
/// 单个子进程生命周期内成功动作只增不减；失败动作保持未完成，由监听器冷却后重试。
#[derive(Clone, Debug, Default)]
struct ReloadStartupGate {
    required: Arc<AtomicU8>,
    completed: Arc<AtomicU8>,
}

impl ReloadStartupGate {
    fn require_from_config(&self, config: &AppConfig) -> ReloadStartupActions {
        let actions = ReloadStartupActions::from_startup_config(config);
        self.require(actions);
        actions
    }

    fn require(&self, actions: ReloadStartupActions) {
        if !actions.is_empty() {
            self.required.fetch_or(actions.0, AtomicOrdering::SeqCst);
        }
    }

    fn required_actions(&self) -> ReloadStartupActions {
        ReloadStartupActions(self.required.load(AtomicOrdering::SeqCst))
    }

    fn missing_actions(&self) -> ReloadStartupActions {
        let required = self.required.load(AtomicOrdering::SeqCst);
        let completed = self.completed.load(AtomicOrdering::SeqCst);
        ReloadStartupActions(required & !completed)
    }

    fn is_satisfied(&self) -> bool {
        self.missing_actions().is_empty()
    }

    fn record_success(&self, kind: StartupTaskKind) {
        let action = ReloadStartupActions::for_task(kind);
        if self.required.load(AtomicOrdering::SeqCst) & action.0 != 0 {
            self.completed.fetch_or(action.0, AtomicOrdering::SeqCst);
        }
    }
}

struct LifecycleBundle {
    config: AppConfig,
    /// 热更新共享句柄集合（阶段 7）：HTTP 保存成功后 apply，
    /// HTTP 服务与各消费方持有同一份 Arc。
    live_configs: LiveConfigs,
    http_server: Option<http::HttpServer>,
    http_reload_blockers: Arc<AtomicUsize>,
    http_reload_draining: Arc<AtomicBool>,
    openai_runtime: Option<OpenAiRuntime>,
    running: Arc<AtomicBool>,
    shutdown: ShutdownState,
    reload_startup: ReloadStartupGate,
    reload_check_after: Instant,
    paused: Arc<AtomicBool>,
    console_reply_context: Arc<AtomicBool>,
    monitor: MonitorShared,
    /// 配置中心共享句柄；正式运行时由 ApplicationRuntime::new 注入，
    /// 正式任务执行上下文（facade 重建）中不持有（HTTP 服务不在其中）。
    config_store: Option<crate::config::SharedConfigStore>,
    /// 外部聊天源共享状态：观察线程据此决定是否跳过 OCR 聊天识别。
    external_chat: crate::composition::application::external_chat::ExternalChatState,
}

pub(crate) struct ApplicationRuntime {
    ui: UiBundle,
    playback: PlaybackBundle,
    business: BusinessBundle,
    lifecycle: LifecycleBundle,
}

/// 正式任务执行所需的窄上下文。
///
/// 这里只保留 `PendingTask` 各变体及其下游命令实际使用的可克隆端口和服务；
/// 各运行时所有权、HTTP 服务、热键等生命周期资源仍只属于 `ApplicationRuntime`。
pub(crate) struct FormalTaskExecutionContext {
    coordinator: UiCoordinator,
    ui: FormalTaskUiContext,
    playback: FormalTaskPlaybackContext,
    business: FormalTaskBusinessContext,
    lifecycle: FormalTaskLifecycleContext,
}

struct FormalTaskUiContext {
    ocr_args: ResolvedOcrArgs,
    chat_templates: ResolvedTemplateArgs,
    game_ui: GameUi,
    residency_ui: ResidencyUi,
    hall_ui: HallUi,
    moderation_ui: ModerationUi,
    startup_ui: StartupUi,
    secondary_unread_ui: SecondaryUnreadUi,
    friend_delivery_ui: FriendDeliveryUi,
    invite_ui: InviteUi,
    custom_action_ui: CustomActionUi,
    chat_output: ChatOutput,
    ocr: OcrRuntimeHandle,
    latest_frame: Arc<Mutex<LatestFrameCache>>,
    window_detection_signal: WindowDetectionSignal,
    chat_baseline_primed: Arc<AtomicBool>,
    chat_observations: ChatObservationShared,
    chat_timeline: crate::observation::chat::ChatTimeline,
}

struct FormalTaskPlaybackContext {
    player: PlayerController<PlayerRuntimeBackend, BusinessPlaybackStateAdapter>,
    playback_application: PlaybackApplication,
    player_search: PlayerSearchClient,
    native_playback: PlaybackHandle,
    login_helper: LoginHelperManager,
}

struct FormalTaskBusinessContext {
    business: BusinessRuntimeHandle,
    task_engine: TaskEngineHandle,
    business_events: BusinessRuntimeEventSink,
    formal_tasks: FormalTaskClient,
    background_commands: workers::BackgroundCommandManager,
    ai: AiClient,
    ai_chat_memory: std::sync::Arc<crate::features::ai_chat::AiChatMemory>,
    ai_chat_gate: std::sync::Arc<crate::features::ai_chat::TriggerGate>,
    ai_chat_openai: miliastra_kernel::ai::OpenAiRuntimeHandle,
    ai_connection: crate::config::ai::AiConfig,
    ai_request_timeout: std::time::Duration,
    song_requests: SongRequestApplication,
    card_games: CardGameApplication,
    administration_application: AdministrationApplication,
    hall_application: HallApplication,
    idiom_chain_application: IdiomChainApplication,
    tarot_application: crate::features::tarot::TarotApplication,
    playlist_application: crate::features::playlist::PlaylistApplication,
    turtle_soup_application: TurtleSoupApplication,
    undercover_game: UndercoverApplication,
    moderation: ModerationService,
    moderation_workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    startup: StartupService,
    custom_workflow: CustomWorkflowService,
}

struct FormalTaskLifecycleContext {
    config: AppConfig,
    config_store: crate::config::SharedConfigStore,
    /// 外部聊天源的共享状态。
    ///
    /// **必须来自真身**，不能用 ExternalChatState::default()：那是全新的空状态，
    /// active 恒为 false。决策窗口在 facade 里读不到接管状态就会走 OCR 慢路径，
    /// 而外部源接管期间观察线程不发布帧——点歌/歌单窗口必然等满 20s 后报
    /// 「等待共享聊天确认基线超时」。
    external_chat: crate::composition::application::external_chat::ExternalChatState,
    /// 热更新共享句柄集合：正式任务上下文（facade 重建）与 HTTP 服务共享同一份。
    live_configs: LiveConfigs,
    running: Arc<AtomicBool>,
    reload_startup: ReloadStartupGate,
    paused: Arc<AtomicBool>,
    console_reply_context: Arc<AtomicBool>,
    monitor: MonitorShared,
}

struct FormalTaskExecutionFacade {
    application: ApplicationRuntime,
}

impl FormalTaskExecutionContext {
    fn facade(&self) -> FormalTaskExecutionFacade {
        FormalTaskExecutionFacade {
            application: ApplicationRuntime {
                ui: UiBundle {
                    coordinator: self.coordinator.clone(),
                    ocr_args: self.ui.ocr_args.clone(),
                    chat_templates: self.ui.chat_templates.clone(),
                    hotkeys: None,
                    game_ui: self.ui.game_ui.clone(),
                    residency_ui: self.ui.residency_ui.clone(),
                    hall_ui: self.ui.hall_ui.clone(),
                    moderation_ui: self.ui.moderation_ui.clone(),
                    startup_ui: self.ui.startup_ui.clone(),
                    secondary_unread_ui: self.ui.secondary_unread_ui.clone(),
                    friend_delivery_ui: self.ui.friend_delivery_ui.clone(),
                    invite_ui: self.ui.invite_ui.clone(),
                    custom_action_ui: self.ui.custom_action_ui.clone(),
                    ui_runtime: None,
                    chat_output: self.ui.chat_output.clone(),
                    ocr: self.ui.ocr.clone(),
                    ocr_runtime: None,
                    latest_frame: self.ui.latest_frame.clone(),
                    window_detection_signal: self.ui.window_detection_signal.clone(),
                    chat_baseline_primed: self.ui.chat_baseline_primed.clone(),
                    chat_observations: self.ui.chat_observations.clone(),
                    chat_timeline: self.ui.chat_timeline.clone(),
                },
                playback: PlaybackBundle {
                    player: self.playback.player.clone(),
                    playback_application: self.playback.playback_application.clone(),
                    player_search: self.playback.player_search.clone(),
                    player_runtime: None,
                    native_playback: self.playback.native_playback.clone(),
                    login_helper: self.playback.login_helper.clone(),
                    native_playback_runtime: None,
                },
                business: BusinessBundle {
                    business: self.business.business.clone(),
                    task_engine: self.business.task_engine.clone(),
                    business_events: self.business.business_events.clone(),
                    business_runtime: None,
                    formal_task_runtime: None,
                    formal_tasks: Some(self.business.formal_tasks.clone()),
                    background_commands: self.business.background_commands.clone(),
                    ai: self.business.ai.clone(),
                    ai_chat_memory: self.business.ai_chat_memory.clone(),
                    ai_chat_gate: self.business.ai_chat_gate.clone(),
                    ai_chat_openai: self.business.ai_chat_openai.clone(),
                    ai_connection: self.business.ai_connection.clone(),
                    ai_request_timeout: self.business.ai_request_timeout,
                    song_requests: self.business.song_requests.clone(),
                    card_games: self.business.card_games.clone(),
                    administration_application: self.business.administration_application,
                    hall_application: self.business.hall_application,
                    idiom_chain_application: self.business.idiom_chain_application,
                    tarot_application: self.business.tarot_application.clone(),
                    playlist_application: self.business.playlist_application.clone(),
                    turtle_soup_application: self.business.turtle_soup_application,
                    undercover_game: self.business.undercover_game.clone(),
                    moderation: self.business.moderation.clone(),
                    moderation_workers: self.business.moderation_workers.clone(),
                    startup: self.business.startup,
                    custom_workflow: self.business.custom_workflow.clone(),
                },
                lifecycle: LifecycleBundle {
                    config: self.lifecycle.config.clone(),
                    live_configs: self.lifecycle.live_configs.clone(),
                    http_server: None,
                    http_reload_blockers: Arc::new(AtomicUsize::new(0)),
                    http_reload_draining: Arc::new(AtomicBool::new(false)),
                    openai_runtime: None,
                    running: self.lifecycle.running.clone(),
                    shutdown: ShutdownState::new(),
                    reload_startup: self.lifecycle.reload_startup.clone(),
                    reload_check_after: Instant::now(),
                    paused: self.lifecycle.paused.clone(),
                    console_reply_context: self.lifecycle.console_reply_context.clone(),
                    monitor: self.lifecycle.monitor.clone(),
                    config_store: Some(self.lifecycle.config_store.clone()),
                    // facade 不启动观察线程，但**命令执行发生在这里**，决策窗口要读
                    // 接管状态。必须是真身那份（Arc 共享），否则恒为 false。
                    external_chat: self.lifecycle.external_chat.clone(),
                },
            },
        }
    }

    fn execute_pending_task(&self, task: PendingTask) -> Result<PendingTaskExecution> {
        self.facade().application.execute_pending_task(task)
    }

    fn execute_web_tool_request(&self, request: WebToolRequest) -> Result<String> {
        self.facade().application.execute_web_tool_request(request)
    }
}

#[derive(Clone)]
struct BusinessPlaybackStateAdapter {
    business: BusinessRuntimeHandle,
}

impl BusinessPlaybackStateAdapter {
    fn new(business: BusinessRuntimeHandle) -> Self {
        Self { business }
    }
}

impl PlaybackStatePort for BusinessPlaybackStateAdapter {
    fn snapshot(&self) -> Result<PlaybackRuntimeState> {
        self.business
            .playback_state_snapshot()
            .map_err(anyhow::Error::from)
    }

    fn update(&self, update: PlaybackStateUpdate) -> Result<bool> {
        self.business
            .update_playback_state(update)
            .map_err(anyhow::Error::from)
    }

    fn record_observation_if_active(
        &self,
        expected: ActivePlaybackIdentity,
        observation: PlaybackObservation,
        immediate: bool,
    ) -> Result<bool> {
        self.business
            .record_observation_if_active(expected, observation, immediate)
            .map_err(anyhow::Error::from)
    }

    fn confirm_playback_and_dequeue(
        &self,
        update: PlaybackStateUpdate,
        queue_item_id: Option<u64>,
    ) -> Result<bool> {
        self.business
            .confirm_playback_and_dequeue(update, queue_item_id)
            .map_err(anyhow::Error::from)
    }

    fn song_dedup_limited(&self, candidate: SongDedupCandidate) -> Result<bool> {
        self.business
            .song_dedup_limited(candidate)
            .map_err(anyhow::Error::from)
    }

    fn record_song_dedup(&self, candidate: SongDedupCandidate) -> Result<()> {
        self.business
            .record_song_dedup(candidate)
            .map_err(anyhow::Error::from)
    }

    fn record_playback_pool_track(&self, track: PlayableTrack) -> Result<()> {
        self.business
            .record_playback_pool_track(track)
            .map_err(anyhow::Error::from)
    }

    fn playback_pool_available(&self) -> Result<bool> {
        self.business
            .playback_pool_available()
            .map_err(anyhow::Error::from)
    }

    fn observe_external_playback(
        &self,
        identity: TrackKey,
        now: Instant,
        protect_after: Duration,
    ) -> Result<ExternalPlaybackObservation> {
        self.business
            .observe_external_playback(identity, now, protect_after)
            .map_err(anyhow::Error::from)
    }

    fn clear_external_playback_tracker(&self) -> Result<()> {
        self.business
            .clear_external_playback_tracker()
            .map_err(anyhow::Error::from)
    }

    fn reconcile_player_session(
        &self,
        binding: Option<crate::features::playback::PlaybackSessionBinding>,
    ) -> Result<crate::features::playback::SessionReconciliation> {
        self.business
            .reconcile_player_session(binding)
            .map_err(anyhow::Error::from)
    }

    fn inspect_player_session(
        &self,
        binding: Option<crate::features::playback::PlaybackSessionBinding>,
    ) -> Result<crate::features::playback::SessionReconciliation> {
        self.business
            .inspect_player_session(binding)
            .map_err(anyhow::Error::from)
    }

    fn claim_terminal_outcome(
        &self,
        request_id: u64,
        outcome: String,
        handled_at_ms: u64,
    ) -> Result<bool> {
        self.business
            .claim_terminal_outcome(request_id, outcome, handled_at_ms)
            .map_err(anyhow::Error::from)
    }

    fn record_playback_attempt(
        &self,
        provider: String,
        locator: String,
        started_at_ms: u64,
        result: String,
    ) -> Result<()> {
        self.business
            .record_playback_attempt(provider, locator, started_at_ms, result)
            .map_err(anyhow::Error::from)
    }

    fn record_control_operation(
        &self,
        operation: String,
        requested_at_ms: u64,
        completed: bool,
    ) -> Result<()> {
        self.business
            .record_control_operation(operation, requested_at_ms, completed)
            .map_err(anyhow::Error::from)
    }
}

#[derive(Clone)]
struct WindowDetectionSignal {
    inner: Arc<(Mutex<u64>, Condvar)>,
}

impl WindowDetectionSignal {
    fn new() -> Self {
        Self {
            inner: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    fn generation(&self) -> Result<u64> {
        let (lock, _) = &*self.inner;
        let generation = lock
            .lock()
            .map_err(|_| anyhow!("window detection signal mutex poisoned"))?;
        Ok(*generation)
    }

    fn request(&self, reason: &'static str) -> Result<()> {
        let (lock, cvar) = &*self.inner;
        let mut generation = lock
            .lock()
            .map_err(|_| anyhow!("window detection signal mutex poisoned"))?;
        *generation = generation.wrapping_add(1);
        cvar.notify_all();
        log::info!("已请求重置窗口检测退避: {}", reason);
        Ok(())
    }

    fn wait_for_change(&self, observed_generation: u64, timeout: Duration) -> Result<bool> {
        let (lock, cvar) = &*self.inner;
        let generation = lock
            .lock()
            .map_err(|_| anyhow!("window detection signal mutex poisoned"))?;
        if *generation != observed_generation {
            return Ok(true);
        }
        let (generation, _) = cvar
            .wait_timeout_while(generation, timeout, |current| {
                *current == observed_generation
            })
            .map_err(|_| anyhow!("window detection signal condvar poisoned"))?;
        Ok(*generation != observed_generation)
    }
}

pub(crate) enum PendingTask {
    Command(Box<PendingCommand>),
    AdvanceQueue {
        reason: &'static str,
    },
    ConsoleChat {
        text: String,
        prefix: String,
    },
    Startup(StartupTask),
    ClearIdleExit,
    ModerationResult(ModerationResultTask),
    SetChatListenerMode {
        target: ChatListenerMode,
    },
    SecondaryUnread {
        hit: UnreadFriendHit,
        discard_only: bool,
    },
    RestoreSecondaryHall,
    TurtleSoupQuestion {
        question: Box<turtle_soup::TurtleSoupQuestion>,
        observed_at: Instant,
    },
    CardGameEffect(CardGameEffectTask),
    UndercoverEffect(UndercoverEffectTask),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingTaskExecution {
    Completed,
}

impl PendingTask {
    fn cancel(&mut self, business: &BusinessRuntimeHandle) {
        match self {
            PendingTask::SetChatListenerMode { target } => {
                if let Err(error) = business.cancel_chat_listener_mode_request(*target) {
                    log::error!("取消值守模式切换失败: {error}");
                }
            }
            PendingTask::SecondaryUnread { .. } | PendingTask::RestoreSecondaryHall => {
                if let Err(error) = business.release_chat_listener_unread_task() {
                    log::error!("释放二级未读任务失败: {error}");
                }
            }
            PendingTask::ModerationResult(task) => {
                task.cancel();
            }
            PendingTask::CardGameEffect(effect) => {
                if let Err(error) = effect.cancel() {
                    log::error!("撤销牌局计时结果后无法清理牌局: {error:#}");
                }
            }
            PendingTask::UndercoverEffect(effect) => {
                if let Err(error) = effect.cancel() {
                    log::error!("撤销谁是卧底效果后无法清理牌局: {error:#}");
                }
            }
            _ => {}
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Command(pending) if pending.routed.message_type == "控制台" => {
                format!("控制台命令: {}", pending.routed.raw)
            }
            Self::Command(pending) => pending.routed.raw.clone(),
            Self::AdvanceQueue { reason } => format!("自动出队({})", reason),
            Self::ConsoleChat { text, prefix } => {
                format!("控制台发言: {}{}", prefix, text)
            }
            Self::Startup(task) => task.label(),
            Self::ClearIdleExit => "取消闲置退出".to_string(),
            Self::ModerationResult(task) => task.label(),
            Self::SetChatListenerMode { target } => {
                format!("切换{}", target.label())
            }
            Self::SecondaryUnread { discard_only, .. } => {
                if *discard_only {
                    "二级值守初始未读清场".to_string()
                } else {
                    "二级值守好友未读".to_string()
                }
            }
            Self::RestoreSecondaryHall => "二级值守恢复当前大厅".to_string(),
            Self::TurtleSoupQuestion { .. } => "海龟汤提问".to_string(),
            Self::CardGameEffect(effect) => effect.label(),
            Self::UndercoverEffect(effect) => effect.label(),
        }
    }

    fn dedup_key(&self) -> Option<String> {
        match self {
            Self::Command(pending) => Some(pending.lock_key.clone()),
            Self::ModerationResult(task) => Some(task.dedup_key()),
            // 播放队列自动推进使用稳定去重键：同一时刻只保留一个待执行推进任务，
            // 推进决策在任务执行时重新评估，多个推进原因可以安全合并为一次执行。
            Self::AdvanceQueue { .. } => Some("playback_advance_queue".to_string()),
            // 值守切换用稳定去重键：同一时刻只需要一个"切到某模式"的任务。
            // 没有它时，自愈逻辑无法判断 pending_mode 是否已有任务在排队，
            // 重复入队会堆积多个切换任务，彼此撤销对方的 pending 状态。
            Self::SetChatListenerMode { target } => {
                Some(format!("chat_listener_mode_{}", target.label()))
            }
            Self::ConsoleChat { .. }
            | Self::Startup(_)
            | Self::ClearIdleExit
            | Self::SecondaryUnread { .. }
            | Self::RestoreSecondaryHall
            | Self::TurtleSoupQuestion { .. }
            | Self::CardGameEffect(_)
            | Self::UndercoverEffect(_) => None,
        }
    }

    fn is_playback_task(&self) -> bool {
        match self {
            Self::AdvanceQueue { .. } => true,
            Self::Command(pending) => matches!(
                &pending.routed.command,
                ModuleCommand::SongRequest(_)
                    | ModuleCommand::Playback(
                        PlaybackCommand::Pause
                            | PlaybackCommand::Resume
                            | PlaybackCommand::Play
                            | PlaybackCommand::Next
                            | PlaybackCommand::DeleteCurrentPoolTrack
                            | PlaybackCommand::Previous
                    )
            ),
            Self::ConsoleChat { .. }
            | Self::Startup(_)
            | Self::ClearIdleExit
            | Self::ModerationResult(_)
            | Self::SetChatListenerMode { .. }
            | Self::SecondaryUnread { .. }
            | Self::RestoreSecondaryHall
            | Self::TurtleSoupQuestion { .. }
            | Self::CardGameEffect(_)
            | Self::UndercoverEffect(_) => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiResidency {
    Primary,
    SecondaryCurrentHall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResidencyPurpose {
    ListenerModeSwitch,
    IndependentRecovery(&'static str),
}

impl ResidencyPurpose {
    const fn label(self) -> &'static str {
        match self {
            Self::ListenerModeSwitch => "切换聊天值守模式",
            Self::IndependentRecovery(context) => context,
        }
    }
}

fn listener_residency(mode: ChatListenerMode, temporary_primary: bool) -> UiResidency {
    if mode == ChatListenerMode::Secondary && !temporary_primary {
        UiResidency::SecondaryCurrentHall
    } else {
        UiResidency::Primary
    }
}

fn resolve_workflow_listener_residency(
    operations: Vec<WorkflowOperation>,
    target: UiResidency,
) -> Vec<WorkflowOperation> {
    let target = match target {
        UiResidency::Primary => WorkflowResidency::Primary,
        UiResidency::SecondaryCurrentHall => WorkflowResidency::SecondaryCurrentHall,
    };
    operations
        .into_iter()
        .map(|operation| match operation {
            WorkflowOperation::ReturnListenerResidency => {
                WorkflowOperation::EnsureResidency { target }
            }
            operation => operation,
        })
        .collect()
}

fn enqueue_current_hall_reply(task_engine: &TaskEngineHandle, text: &str) -> Result<()> {
    match task_engine.enqueue_deferred(DeferredChatMessage {
        text: text.to_string(),
        target: DeferredChatTarget::CurrentHall,
        background_key: None,
        formal_epoch: None,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatDecisionScope {
    CurrentHall,
    MultipleConversations,
}

struct ChatDecisionReader {
    listener_mode: ChatListenerMode,
    subscriber: crate::observation::chat::ChatTimelineSubscriber,
    chat: crate::observation::chat::ChatIdentity,
    /// 窗口期的业务容器。外部源只按它匹配消息，不看屏幕桶。
    scope: crate::observation::chat::ChatScope,
    /// 是否接收所有会话（`MultipleConversations`）。
    ///
    /// 邀请确认、自定义流程确认这类窗口既可能从大厅来，也可能从好友私聊来，
    /// 因此不绑定单一容器，由调用方再按消息类型（blue/pink）筛选。
    any_conversation: bool,
    /// 已见过首帧的会话。
    ///
    /// 只对 `any_conversation` 生效。窗口打开后**新出现**的会话，它的整屏
    /// 历史气泡都晚于 `baseline_at`，若照单全收就会把首屏历史当成新决策
    /// （MUST_READ 第 2 节的「首屏历史不执行」被绕过）。
    ///
    /// 做法：第一次见到某个会话时，只把当时已有的气泡记为已见、不交给调用方；
    /// 从第二帧起才是真正的决策输入。
    settled_conversations: std::collections::HashSet<crate::observation::chat::ChatScope>,
    baseline_at: Instant,
    /// 外部源的服务端序号水位（窗口打开时的最大已处理序号）。
    ///
    /// 为什么不能只靠 `baseline_at`：外部源的消息由 socket 推送，`captured_at`
    /// 是投递时刻。窗口打开后到达的第一条消息，其时间戳可能与窗口开启时刻同值，
    /// 用 `<=` 过滤会把它自己排除掉——用户发的第一条 `@1` 永远进不了窗口。
    /// OCR 有周期兜底帧，这个问题在那边从不暴露。
    ///
    /// 用序号水位则语义确定：小于等于水位的都发生在窗口打开之前。
    /// 水位在 `subscribe` **之前**采样，避免两步之间到达的消息被误判为历史。
    sequence_floor: Option<u64>,
    /// 屏幕气泡序号上限。外部消息的 `bubble_sequence` 是去重哈希、不单调，
    /// 拿它当游标会把约一半新消息误判成旧消息，所以外部源恒为 None。
    baseline_cursor: Option<(crate::observation::chat::VisualSessionId, u64)>,
    seen: std::collections::HashSet<(crate::observation::chat::ObservedChatMessageId, String)>,
    /// 屏幕位置去重锁。外部消息的 `block` 是哨兵坐标，全部相同，
    /// 用它判定会把同文本决策永久否决，所以外部源恒为空实现。
    screen_lock: DecisionScreenLock,
    _observation_session: ChatObservationExclusiveGuard,
}

impl ChatDecisionReader {
    /// 这份快照是否属于本窗口的会话。
    ///
    /// 归属一律按业务容器（`scope`）判，与来源无关。窗口绑了具体屏幕桶时，
    /// 额外排除「同一大厅的另一块面板」——那是同一物理会话的另一个视图，
    /// 不能拿它当基线游标。外部帧没有屏幕桶（`chat` 为 None），不受这条约束。
    ///
    /// 三条读者路径共用这一个判定，避免漂移。
    fn same_conversation(&self, snapshot: &crate::observation::chat::ChatTimelineSnapshot) -> bool {
        if self.any_conversation {
            return true;
        }
        // 归属一律按业务容器判，与来源无关。
        //
        // 不能因为窗口是「屏幕来源」就改按 `chat` 比：窗口打开后外部源才接管时，
        // 外部帧的 `chat` 恒为 None，按桶比会让整窗一条都收不到，直到超时。
        // 「窗口的游标是序号还是时间」由 `sequence_floor` 决定，与这条判断无关。
        if !snapshot.scope.same_container(&self.scope) {
            return false;
        }
        // 屏幕桶判据只在**屏幕基线**的窗口上生效。
        //
        // 目的：窗口绑在二级面板上时，一级面板的快照是同一大厅的另一视图，
        // 不能拿它当基线游标（会把别的面板的历史当成决策输入）。
        //
        // 但基线来自外部源的窗口例外——它的游标是服务端序号水位，与面板无关。
        // 若仍按开窗时猜的 `chat` 过滤，一旦外部源掉线、OCR 接管且实际驻留面板
        // 与开窗时不同，整窗会在回落瞬间开始漏帧直到超时。
        // `sequence_floor` 为 None 即「基线来自屏幕」——两个字段表达同一件事，
        // 不再单独存一个「是否屏幕来源」开关（存了就会与它分叉）。
        if self.sequence_floor.is_some() {
            return true;
        }
        match snapshot.chat.as_ref() {
            // 外部帧没有面板概念：它靠 scope 已经判过归属。
            None => true,
            Some(incoming) => incoming == &self.chat,
        }
    }

    /// 多会话窗口的首帧防护：首次见到某会话时只登记、不交付。
    ///
    /// 该会话的整屏气泡都是「窗口打开后才渲染出来的历史」，时间戳晚于基线，
    /// 照单全收会把首屏历史当成新决策（MUST_READ 第 2 节被绕过）。
    ///
    /// 抽成方法是为了让三条读取路径共用同一条判据——它们各有自己的循环，
    /// 若某一处漏掉，那条路径就会静默绕开首屏历史抑制。
    fn skips_first_frame_of_new_conversation(
        &mut self,
        snapshot: &crate::observation::chat::ChatTimelineSnapshot,
    ) -> bool {
        if !self.any_conversation {
            return false;
        }
        // 只对**屏幕帧**做首帧防护。
        //
        // 防护的目的是挡住「窗口打开后才被渲染出来的整屏历史」。外部聊天源的消息
        // 每条只投递一次、且是真实事件而非屏幕重绘，首帧就是决策输入本身——把它
        // 跳过等于永久吞掉这一次确认，窗口只能等到超时。
        if !snapshot.source.is_ocr() {
            return false;
        }
        // 键一律用业务容器：屏幕桶只是面板的别名，混用会让同一会话各记一次。
        // `insert` 返回 true 表示「这次才第一次见到」——那正是要跳过的一帧。
        self.settled_conversations.insert(snapshot.scope.clone())
    }

    /// 这条消息是否晚于窗口打开时的水位。
    ///
    /// 屏幕消息没有服务端序号，退回按时间比较（与解耦前一致）；
    /// 外部消息按单调序号比较，避免把「让基线成立的那条」误判成历史。
    fn is_after_baseline_at(
        &self,
        snapshot_at: Instant,
        message: &crate::observation::chat::ChatTimelineMessage,
    ) -> bool {
        // 判据跟着**这条消息自己的来源**走：外部源开窗后若掉线、OCR 接管，
        // 窗口会先后收到两种消息。外部消息有服务端序号，OCR 消息没有
        // （`source_sequence` 恒 0）。若按窗口来源判，OCR 消息会拿 0 去比
        // 序号水位，`0 > floor` 恒假，整窗漏帧直到超时。
        match self.sequence_floor {
            Some(floor) if !message.id.is_from_screen() => message.source_sequence > floor,
            _ => snapshot_at > self.baseline_at,
        }
    }

    fn read_messages(&mut self) -> Result<Vec<ChatMessage>> {
        let mut messages = Vec::new();
        for snapshot in self.subscriber.read()? {
            if !self.same_conversation(&snapshot) {
                continue;
            }
            if self.skips_first_frame_of_new_conversation(&snapshot) {
                continue;
            }
            let snapshot_at = snapshot.captured_at;
            for message in snapshot.messages {
                // 消息级时间/序号判定：外部源按单调序号，避免把让基线成立的那条排除掉。
                if !self.is_after_baseline_at(snapshot_at, &message) {
                    continue;
                }
                if self.baseline_cursor.is_some_and(|(session, sequence)| {
                    message.id.visual_session == session
                        && message.id.bubble_sequence.get() <= sequence
                }) || !self.seen.insert((message.id, message.text.clone()))
                {
                    continue;
                }
                messages.push(ChatMessage {
                    message_type: message.message_type,
                    block: message.block,
                    text: message.text,
                });
            }
        }
        Ok(messages)
    }

    fn accept_once(&mut self, message: &ChatMessage) -> bool {
        self.screen_lock.accept_once(message)
    }

    fn wait_for_input(&self, _configured_ms: u64, remaining: Duration) -> Result<()> {
        // The short cap keeps Web decisions, stop, and reload responsive.
        self.subscriber
            .wait(remaining.min(Duration::from_millis(50)))?;
        Ok(())
    }
}

pub(crate) struct TemporaryPrimaryHold {
    business: BusinessRuntimeHandle,
    active: bool,
}

impl TemporaryPrimaryHold {
    pub(crate) fn new(business: BusinessRuntimeHandle) -> Result<Self> {
        let active = business.chat_listener_snapshot()?.mode == ChatListenerMode::Secondary;
        if active {
            business.begin_chat_listener_temporary_primary()?;
        }
        Ok(Self { business, active })
    }

    fn release(&mut self) {
        if self.active {
            if let Err(error) = self.business.end_chat_listener_temporary_primary() {
                log::error!("释放临时一级值守保留失败: {error}");
            }
            self.active = false;
        }
    }
}

impl Drop for TemporaryPrimaryHold {
    fn drop(&mut self) {
        self.release();
    }
}

struct ConsoleReplyContextGuard {
    flag: Arc<AtomicBool>,
    previous: bool,
}

impl ConsoleReplyContextGuard {
    fn new(flag: Arc<AtomicBool>) -> Self {
        let previous = flag.swap(true, AtomicOrdering::SeqCst);
        Self { flag, previous }
    }
}

impl Drop for ConsoleReplyContextGuard {
    fn drop(&mut self) {
        self.flag.store(self.previous, AtomicOrdering::SeqCst);
    }
}

impl ApplicationRuntime {
    pub(crate) fn new(
        config: ResolvedApplicationConfig,
        monitor: MonitorShared,
        config_store: crate::config::SharedConfigStore,
    ) -> Result<Self> {
        let ResolvedApplicationConfig {
            app: config,
            live_configs,
            player_runtime: player_runtime_config,
            ocr: ocr_args,
            chat_templates,
            ui_templates,
            friend_delivery: friend_delivery_config,
            hall: hall_config,
            moderation: moderation_config,
            startup: startup_config,
            secondary_unread: secondary_unread_config,
            invite: invite_config,
            turtle_soup: turtle_soup_config,
            ai_request_timeout,
        } = config;
        let system_clock = Arc::new(SystemClock);
        let state_store: Arc<dyn miliastra_contracts::StateStore> =
            Arc::new(crate::adapters::file_store::FsStateStore);
        let ocr_device = ProductionOcrDevice::new(ocr_args.clone())?;
        // 酷狗设备信息由用户在设置页填写：落盘后登录辅助进程与播放侧共用同一份标识。
        if let Err(error) = crate::adapters::login_helper::sync_kugou_device_file(
            &config.kugou,
            &config.playback.credential_directory,
        ) {
            log::warn!("写入酷狗设备信息失败，将沿用已有文件: {error}");
        }
        let native_playback_runtime = NativePlaybackRuntime::start_with_lyrics_lead_and_helper(
            config.playback.credential_directory.clone(),
            // 统一数据库：缓存元数据与配置共用同一个 playback.sqlite3。
            config.playback.audio_cache_runtime_config(
                config
                    .state
                    .playback_state_path
                    .parent()
                    .expect("统一数据库路径必须有父目录"),
            ),
            live_configs.lyrics_lead_seconds.clone(),
            Some(config.playback.login_helper_executable.clone()),
        )
        .context("启动原生播放器")?;
        let native_playback = native_playback_runtime.handle();
        let login_helper = LoginHelperManager::new(
            native_playback.clone(),
            config.playback.login_helper_executable.clone(),
            config.playback.credential_directory.clone(),
            Duration::from_millis(config.playback.login_timeout_ms),
            Arc::new({
                let live_configs = live_configs.clone();
                move || live_configs.snapshot().kugou.clone()
            }),
        );
        let playback_adapter = NativePlaybackAdapter::new(
            native_playback.clone(),
            config.timing.external.volume_smooth_step_ms,
        );
        let idiom_chain = IdiomChainService::load(config.idiom_chain.clone())?;
        if config.idiom_chain.enabled {
            log::info!("已加载成语接龙词库: {} 条", idiom_chain.lexicon_len());
        }
        let landlord = CardGameService::new(config.landlord.clone());
        let undercover =
            UndercoverRuntimeService::new(config.undercover.clone(), state_store.clone());
        let hall = HallStateService::load(
            config.state.hall_state_path.clone(),
            state_store.clone(),
            system_clock.clone(),
            system_clock.clone(),
        )?;
        let playback = PlaybackService::load(
            config.state.playback_state_path.clone(),
            config.song_dedup.history_path.clone(),
            config.queue.max_size,
            config.queue.pool_max_size,
            live_configs.song_dedup.clone(),
            system_clock.clone(),
            state_store.clone(),
        )?;
        let tarot_application =
            crate::features::tarot::TarotApplication::open(&config.state.playback_state_path)?;
        let playlist_application = crate::features::playlist::PlaylistApplication::open(
            &config.state.playback_state_path,
        )?;
        // 旧版本的歌单按昵称归属；现在只认身份 UUID。
        // 这里把能在映射里找到同名昵称的行改写过去，找不到的留着等用户补配。
        {
            let mappings = config
                .identity
                .mappings
                .iter()
                .map(|mapping| (mapping.nickname.clone(), mapping.id.to_string()))
                .collect::<std::collections::HashMap<_, _>>();
            if let Err(error) = playlist_application.migrate_to_identity_keys(&mappings) {
                log::warn!("歌单归属键迁移失败（不影响启动）: {error:#}");
            }
        }
        let moderation_policy = ModerationPolicy::new(
            Duration::from_millis(config.timing.moderation.vote_timeout_ms),
            Duration::from_millis(config.timing.moderation.vote_poll_ms),
            config.moderation.stable_vote_samples,
            config.moderation.required_vote_margin,
        );
        let custom_workflow = CustomWorkflowService::with_live_config(
            live_configs.custom_workflows.clone(),
            WorkflowDefaults {
                default_timeout_ms: config.timing.workflow.default_timeout_ms,
                default_poll_ms: config.timing.workflow.default_poll_ms,
                default_step_wait_ms: config.timing.workflow.default_step_wait_ms,
                decision_timeout_ms: config.timing.decision.timeout_ms,
                decision_poll_ms: config.timing.decision.poll_ms,
                after_activate_ms: config.timing.input.after_activate_ms,
                clipboard_hold_ms: config.timing.input.text_ms,
                stability_mean_threshold: config.ocr.change_mean_threshold,
                stability_changed_ratio_threshold: config.ocr.change_pixel_threshold,
            },
        );
        let chat_observations = ChatObservationShared::new();
        let chat_timeline = crate::observation::chat::ChatTimeline::new();
        let running = Arc::new(AtomicBool::new(true));
        let task_engine = TaskEngineHandle::new(Some(Arc::new(monitor.clone())));
        let business_runtime_builder =
            BusinessRuntimeGroupBuilder::start(DEADLINE_RUNTIME_QUEUE_CAPACITY)?;
        let ocr_runtime = OcrRuntime::start_configured(
            ocr_device,
            OcrRuntimeConfig {
                queue_capacity: OCR_RUNTIME_QUEUE_CAPACITY,
                request_timeout: Duration::from_millis(config.ocr.request_timeout_ms),
                shutdown_timeout: Duration::from_millis(config.ocr.shutdown_timeout_ms),
            },
        )?;
        let ocr = ocr_runtime.handle();
        let player_runtime = PlayerRuntime::start_with_live_observation_interval(
            playback_adapter.clone(),
            playback_adapter.clone(),
            playback_adapter,
            player_runtime_config,
            live_configs.monitor_status_ms.clone(),
        )
        .context("启动播放器运行时")?;
        let player_runtime_handle = player_runtime.handle();
        let player_search = PlayerSearchClient::new(
            player_runtime_handle.clone(),
            BusinessOperationIdAllocator::new(),
        );
        let ui_state_stable_count = config.resolve_stability_count(config.stability.ui_state_count);
        let ui_state_classifier =
            TemplateUiStateClassifier::new(ui_templates.clone(), config.screen.clone())
                .with_game_entry_detection(
                    ocr.clone(),
                    config.startup.enter_game_text_region.into(),
                    ui_state_stable_count,
                )
                .with_panel_regions(PanelRegions {
                    friend_panel: config.moderation.friend_panel_region.into(),
                    friend_search_panel: config.moderation.search_panel_region.into(),
                    friend_block_chat: config.moderation.block_chat_region.into(),
                    friend_confirm: config.moderation.confirm_region.into(),
                    invite_view_star: config.invite.view_star_region.into(),
                    invite_goto_hall: config.invite.goto_hall_region.into(),
                    invite_enter_hall: config.invite.enter_hall_region.into(),
                });
        let ui_runtime = UiRuntime::start_with_progress_and_state_classifier(
            platform_ui_device(config.window.clone()),
            UI_RUNTIME_QUEUE_CAPACITY,
            Arc::new(monitor.clone()),
            ui_state_classifier,
            ui_state_stable_count,
        )?;
        let ui_handle = ui_runtime.handle();
        let game_ui = GameUi::runtime(ui_handle.clone());
        let residency_ui = ResidencyUi::new(
            ui_handle.clone(),
            ocr.clone(),
            friend_delivery_config.clone(),
        );
        let hall_ui = HallUi::new(ui_handle.clone(), ocr.clone(), hall_config);
        let moderation_ui = ModerationUi::new(ui_handle.clone(), moderation_config);
        let startup_ui = StartupUi::new(ui_handle.clone(), ocr.clone(), startup_config);
        let secondary_unread_ui = SecondaryUnreadUi::new(
            ui_handle.clone(),
            ocr.clone(),
            secondary_unread_config,
            chat_timeline.clone(),
        );
        let friend_delivery_ui = FriendDeliveryUi::new(
            ui_handle.clone(),
            ocr.clone(),
            friend_delivery_config.clone(),
        );
        let hall_batch_ui = HallBatchUi::new(
            ui_handle.clone(),
            ocr.clone(),
            friend_delivery_config.clone(),
        );
        let invite_ui = InviteUi::new(ui_handle.clone(), ocr.clone(), invite_config);
        let custom_action_ui = CustomActionUi::new(
            ui_handle,
            ocr.clone(),
            running.clone(),
            config.screen.expected_width,
            config.screen.expected_height,
            friend_delivery_config,
        );
        let openai_runtime = OpenAiRuntime::start().context("启动 OpenAI runtime")?;
        let openai = openai_runtime.handle();
        // 三个 AI 功能共用同一份连接配置与 HTTP 客户端。
        let ai_openai = openai
            .with_http_proxy(config.ai.http_proxy.as_str())
            .context("创建 AI HTTP 客户端失败")?;
        let ai = AiClient::new(
            &config.song_ai,
            &config.ai,
            ai_request_timeout,
            ai_openai.clone(),
        );
        let ai_chat_memory = std::sync::Arc::new(
            crate::features::ai_chat::AiChatMemory::open(&config.state.playback_state_path)
                .context("打开 AI 聊天记忆库")?,
        );
        let ai_chat_gate = std::sync::Arc::new(crate::features::ai_chat::TriggerGate::new(
            std::time::Duration::from_secs(config.ai_chat.cooldown_seconds),
        ));
        let song_review = SongReviewClient::new(
            &config.song_review,
            &config.ai,
            ai_request_timeout,
            ai_openai.clone(),
            system_clock.clone(),
        );
        let song_requests = SongRequestApplication::new(
            ai.clone(),
            song_review,
            config.queue.max_size,
            config.song_dedup.console_bypass,
        );
        let chat_output =
            ChatOutput::with_live_enabled(live_configs.output_send_enabled.clone(), hall_batch_ui);
        let ai_openai_for_chat = ai_openai.clone();
        let turtle_soup = TurtleSoupService::new(
            turtle_soup_config,
            config.ai.clone(),
            ai_openai,
            system_clock.clone(),
            system_clock.clone(),
            state_store,
        );
        let business_timer = business_runtime_builder.handle();
        let business_runtime = business_runtime_builder.build_with(|| {
            BusinessRuntime::start_with_timer_and_modules_and_state_sink(
                BUSINESS_RUNTIME_QUEUE_CAPACITY,
                BusinessRuntimeWorker::from_parts(
                    idiom_chain,
                    landlord,
                    undercover,
                    turtle_soup,
                    hall,
                    playback,
                    InviteService::new(),
                    task_engine.business_port(),
                    business_timer,
                    Arc::new(monitor.clone()),
                    system_clock.clone(),
                ),
            )
        })?;
        let business = business_runtime.business_handle();
        let business_events = business_runtime.event_sink();
        match listener::load_persisted_chat_listener_mode(&config.state.playback_state_path) {
            Ok(Some(mode)) => {
                business.complete_chat_listener_mode(mode)?;
                log::info!("已恢复持久化的{}", mode.label());
            }
            Ok(None) => log::info!("无持久化聊天值守模式，使用默认一级值守"),
            Err(error) => log::warn!("加载聊天值守模式失败，使用默认一级值守: {error:#}"),
        }
        let card_games = CardGameApplication::new(Arc::new(business.clone()));
        let undercover_game = UndercoverApplication::new(Arc::new(business.clone()));
        let player = PlayerController::new(
            PlayerRuntimeBackend::new(player_runtime_handle),
            BusinessPlaybackStateAdapter::new(business.clone()),
            PlaybackTimePorts::new(system_clock.clone(), system_clock.clone()),
            live_configs.clone(),
        );
        if business.hall_state_snapshot()?.remaining_minutes_now() == Some(0) {
            business.update_playback_state(PlaybackStateUpdate::UserPaused)?;
            log::info!("关闭期间大厅倒计时已结束，恢复歌曲时保持暂停");
        }
        // 重启恢复：上次会话有活动歌曲时，从最后可靠进度恢复播放（新引擎会话）。
        // 音量已持久化，恢复前由控制器重新应用；原用户暂停的歌曲恢复后保持暂停。
        match player.play_restored() {
            Ok(true) => log::info!("已恢复上次会话的活动歌曲"),
            Ok(false) => log::info!("无待恢复的活动歌曲"),
            Err(error) => log::warn!("恢复上次会话的活动歌曲失败: {error:#}"),
        }
        let playback_application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: config.song_dedup.console_bypass,
            queue_max_size: config.queue.max_size,
            // 热更新共享句柄：保存 timing.playback.monitor_tick_ms/status_poll 后立即生效。
            monitor_tick_ms: live_configs.monitor_tick_ms.clone(),
            monitor_status_ms: live_configs.monitor_status_ms.clone(),
            help_batch_ms: config.timing.command.help_batch_ms,
        });
        let administration_application =
            AdministrationApplication::new(config.timing.command.help_batch_ms);
        let idiom_chain_application =
            IdiomChainApplication::new(config.timing.command.help_batch_ms);
        let moderation = ModerationService::new(moderation_policy, Arc::new(business.clone()));
        // 外部聊天源与观察线程共用同一份时间线；UiBundle 会接管所有权，先克隆。
        let chat_timeline_for_external = chat_timeline.clone();
        // config 稍后会被移入生命周期，先把外部聊天源配置取出来。
        let chat_source_config = config.chat_source.clone();
        // 外部聊天源在好友私聊里遇到未登记的 UID 时要自动建身份，需要一个共享句柄。
        let external_identity_access = IdentityAccess::new(config.identity.clone());
        // API 模式默认走二级值守：socket 给消息，二级界面停留延迟最低。
        // 切不进去只是保持原模式，不影响消息接收。
        // 这里只置位 pending_mode；真正的界面切换由 "值守请求自愈" 在
        // 主导航循环里入队执行（见 reconcile_pending_listener_mode）。
        Ok(Self {
            ui: UiBundle {
                coordinator: UiCoordinator::new(),
                ocr_args,
                chat_templates,
                hotkeys: None,
                game_ui,
                residency_ui,
                hall_ui,
                moderation_ui,
                startup_ui,
                secondary_unread_ui,
                friend_delivery_ui,
                invite_ui,
                custom_action_ui,
                ui_runtime: Some(ui_runtime),
                chat_output,
                ocr,
                ocr_runtime: Some(ocr_runtime),
                latest_frame: Arc::new(Mutex::new(LatestFrameCache::default())),
                window_detection_signal: WindowDetectionSignal::new(),
                chat_baseline_primed: Arc::new(AtomicBool::new(false)),
                chat_observations,
                chat_timeline,
            },
            playback: PlaybackBundle {
                player,
                playback_application,
                player_search,
                player_runtime: Some(player_runtime),
                native_playback,
                login_helper,
                native_playback_runtime: Some(native_playback_runtime),
            },
            business: BusinessBundle {
                business,
                task_engine,
                business_events,
                business_runtime: Some(business_runtime),
                formal_task_runtime: None,
                formal_tasks: None,
                background_commands: workers::BackgroundCommandManager::new(),
                ai,
                ai_chat_memory,
                ai_chat_gate,
                ai_chat_openai: ai_openai_for_chat,
                ai_connection: config.ai.clone(),
                ai_request_timeout,
                song_requests,
                card_games,
                administration_application,
                hall_application: HallApplication,
                idiom_chain_application,
                tarot_application,
                playlist_application,
                turtle_soup_application: TurtleSoupApplication,
                undercover_game,
                moderation,
                moderation_workers: Arc::new(Mutex::new(Vec::new())),
                startup: StartupService::new(),
                custom_workflow,
            },
            lifecycle: LifecycleBundle {
                config,
                live_configs,
                http_server: None,
                http_reload_blockers: Arc::new(AtomicUsize::new(0)),
                http_reload_draining: Arc::new(AtomicBool::new(false)),
                openai_runtime: Some(openai_runtime),
                running,
                shutdown: ShutdownState::new(),
                reload_startup: ReloadStartupGate::default(),
                reload_check_after: Instant::now(),
                paused: Arc::new(AtomicBool::new(false)),
                console_reply_context: Arc::new(AtomicBool::new(false)),
                monitor,
                config_store: Some(config_store.clone()),
                external_chat: external_chat::spawn(
                    chat_timeline_for_external,
                    chat_source_config,
                    Some(external_identity_access),
                    Some(config_store),
                )
                .unwrap_or_else(|error| {
                    log::error!("外部聊天源启动失败，继续走 OCR: {error:#}");
                    external_chat::ExternalChatState::default()
                }),
            },
        })
    }
}

fn command_log_timestamp() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = duration.as_secs() as i64 + 8 * 3600;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3600;
    let minute = second_of_day % 3600 / 60;
    let second = second_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}-{hour:02}:{minute:02}:{second:02}")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

fn command_username(parsed: &RoutedCommand) -> &str {
    match &parsed.command {
        ModuleCommand::SongRequest(song) if !song.friend_username.trim().is_empty() => {
            &song.friend_username
        }
        _ => &parsed.username,
    }
}

fn command_observed_at(parsed: &RoutedCommand) -> Instant {
    parsed.observation.captured_at.unwrap_or_else(Instant::now)
}

fn is_private_undercover_input(parsed: &RoutedCommand) -> bool {
    matches!(
        &parsed.command,
        ModuleCommand::Undercover(
            UndercoverCommand::Vote(_) | UndercoverCommand::Abstain | UndercoverCommand::Reveal,
        )
    )
}

fn private_safe_command_log(parsed: &RoutedCommand) -> &str {
    match &parsed.command {
        ModuleCommand::Undercover(UndercoverCommand::Vote(_) | UndercoverCommand::Abstain) => {
            "谁是卧底投票"
        }
        ModuleCommand::Undercover(UndercoverCommand::Reveal) => "谁是卧底谜底查询",
        _ => &parsed.raw,
    }
}

fn command_location(message_type: &str) -> &str {
    match message_type {
        "pink" => "私聊",
        "blue" => "大厅",
        _ => message_type,
    }
}

fn command_log_field(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('-', "_")
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum SecondaryHallSequenceDelta {
    EstablishBaseline,
    RetainedPrefix,
    NoChange,
    LostOverlap,
    NewFrom(usize),
}

#[allow(dead_code)]
fn secondary_hall_sequence_delta(
    previous: Option<&[SecondaryHallBubble]>,
    current: &[SecondaryHallBubble],
) -> SecondaryHallSequenceDelta {
    let Some(previous) = previous else {
        return SecondaryHallSequenceDelta::EstablishBaseline;
    };
    if previous.is_empty() {
        return if current.is_empty() {
            SecondaryHallSequenceDelta::NoChange
        } else {
            SecondaryHallSequenceDelta::NewFrom(0)
        };
    }
    if hall_bubble_sequence_is_retained_prefix(previous, current) {
        return SecondaryHallSequenceDelta::RetainedPrefix;
    }
    let overlap = hall_bubble_sequence_overlap(previous, current);
    if overlap == 0 {
        SecondaryHallSequenceDelta::LostOverlap
    } else if overlap == current.len() {
        SecondaryHallSequenceDelta::NoChange
    } else {
        SecondaryHallSequenceDelta::NewFrom(overlap)
    }
}

/// 构造平台 UI 设备。
///
/// 这是 `composition` 对 `adapters::windows` 的**唯一**依赖点。
/// 把它收成工厂函数后，其余 17k 行组合层代码不再需要 Windows 才能编译，
/// 于是 Linux 侧可以真正运行组合层的测试（此前整块被 `#[cfg(windows)]` 挡住）。
#[cfg(target_os = "windows")]
fn platform_ui_device(window: crate::config::WindowConfig) -> impl crate::runtime::ui::UiDevice {
    crate::adapters::windows::WindowsUiDevice::new(window)
}

/// 非 Windows 构建（仅测试）没有真实设备。
///
/// 返回一个所有操作都失败的设备：组合层的测试用的是 `UiRuntime` + 测试替身，
/// 走不到这里；万一真走到，也要显式失败而不是假装成功。
#[cfg(not(target_os = "windows"))]
fn platform_ui_device(_window: crate::config::WindowConfig) -> impl crate::runtime::ui::UiDevice {
    struct UnsupportedDevice;
    impl crate::runtime::ui::UiDevice for UnsupportedDevice {
        fn capture(&mut self) -> anyhow::Result<image::DynamicImage> {
            anyhow::bail!("非 Windows 构建没有真实 UI 设备")
        }
    }
    UnsupportedDevice
}

pub(crate) use crate::observation::chat::{
    SECONDARY_HALL_FALLBACK_SENDER, SecondarySenderIdentity, parse_secondary_sender_identity,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecondaryHallMessageKind {
    Ignored,
    Command,
    TurtleQuestion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SecondaryHallMessageClassification {
    kind: SecondaryHallMessageKind,
    /// 这条消息是否需要**可确认的身份**才能执行。
    ///
    /// 语义不是「必须有昵称」：账号 UID 或 昵称任一存在就满足。外部聊天源的
    /// 昵称可能缺失而 UID 必填，只按昵称判断会让这类消息整条丢失。判定统一
    /// 走 crate::features::command::has_known_identity。
    requires_identity: bool,
}

fn secondary_hall_command_text(text: &str) -> Option<&str> {
    // 顺序与 OCR 既有语义一致：`@` 优先。
    //
    // 一级路径历史上就是「先找 @，找不到再找 #」（`CommandEnvelope` 的 At 分支
    // 先判）。这里保持同一顺序，否则同时含 `@` 与 `#` 的文本（如歌名带 `#`）
    // 会被截成后半段，两条路径派发不同结果——违反「OCR 行为不变」。
    if let Some(index) = text.find(['@', '＠']) {
        return Some(text[index..].trim());
    }
    // `#` 不要求行首：外部聊天源统一补了「昵称：」前缀，`#` 会落在行中间。
    // 屏幕上的一行本来也可能是「昵称：#状态」（二级面板的好友消息排版）。
    text.find(['#', '＃']).map(|index| text[index..].trim())
}

/// 判断一条二级大厅消息属于哪类，以及它是否依赖真实发送者身份。
///
/// requires_identity 描述的是「业务结果/授权/回合归属会随发言者变化」——
/// 具体满足条件由 crate::features::command::has_known_identity 统一裁定：
/// 账号 UID 或 昵称任一存在即可，两者皆无（Unknown）才拒绝执行。
fn classify_secondary_hall_message(
    text: &str,
    command: Option<&ModuleCommand>,
    accepts_turtle_questions: bool,
) -> SecondaryHallMessageClassification {
    if let Some(command) = command {
        return SecondaryHallMessageClassification {
            kind: SecondaryHallMessageKind::Command,
            requires_identity: command.requires_hall_sender(),
        };
    }
    if accepts_turtle_questions
        && turtle_soup::parse_question_message(text, Some(SECONDARY_HALL_FALLBACK_SENDER)).is_some()
    {
        return SecondaryHallMessageClassification {
            kind: SecondaryHallMessageKind::TurtleQuestion,
            requires_identity: true,
        };
    }
    if secondary_hall_command_text(text)
        .is_some_and(|command| command.trim_start().starts_with(['@', '＠']))
    {
        return SecondaryHallMessageClassification {
            kind: SecondaryHallMessageKind::Command,
            requires_identity: true,
        };
    }
    SecondaryHallMessageClassification {
        kind: SecondaryHallMessageKind::Ignored,
        requires_identity: false,
    }
}

#[cfg(test)]
fn normalize_secondary_sender_name(value: &str) -> String {
    parse_secondary_sender_identity(value)
        .effective_name()
        .to_string()
}

fn next_target_missing_backoff(current: Duration) -> Duration {
    current.saturating_mul(2).min(TARGET_MISSING_BACKOFF_MAX)
}

fn format_web_tool_latency_summary(values: &[u128]) -> String {
    if values.is_empty() {
        return "无有效样本".to_string();
    }
    let total = values.iter().sum::<u128>();
    let max = values.iter().copied().max().unwrap_or(0);
    format!(
        "样本={} 平均={}ms 最大={}ms",
        values.len(),
        total / values.len() as u128,
        max
    )
}

fn web_tool_panel_response_rect(config: &AppConfig) -> Rect {
    let chat = config.screen.chat_rect;
    let point = config.output.chat_click_2;
    let x = chat.x.min(point.x - 80).max(0);
    let y = chat.y.min(point.y - 80).max(0);
    let right = (chat.x + chat.width as i32).max(point.x + 360);
    let bottom = (chat.y + chat.height as i32).max(point.y + 50);
    let max_right = config.screen.expected_width as i32;
    let max_bottom = config.screen.expected_height as i32;
    Rect::new(
        x,
        y,
        (right.min(max_right) - x).max(1) as u32,
        (bottom.min(max_bottom) - y).max(1) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::chat::secondary_hall_bubbles;

    #[test]
    fn advance_queue_uses_a_stable_dedup_key() {
        // 不同推进原因必须归并为同一个调度去重键，保证多个 pending 推进任务只留一个。
        let first = PendingTask::AdvanceQueue {
            reason: "song-ended",
        }
        .dedup_key();
        let second = PendingTask::AdvanceQueue {
            reason: "manual-next",
        }
        .dedup_key();
        assert_eq!(first.as_deref(), Some("playback_advance_queue"));
        assert_eq!(second, first);
        // 与命令锁键区分，避免误合并或误拦截。
        let command = PendingTask::Command(Box::new(PendingCommand {
            lock_key: ModuleCommand::Playback(PlaybackCommand::Next).lock_key(),
            routed: RoutedCommand::console(
                "下一首",
                "下一首",
                ModuleCommand::Playback(PlaybackCommand::Next),
            ),
        }))
        .dedup_key();
        assert_ne!(command, first);
        assert_ne!(first, PendingTask::ClearIdleExit.dedup_key());
    }

    #[test]
    fn advance_queue_is_a_playback_task() {
        assert!(PendingTask::AdvanceQueue { reason: "test" }.is_playback_task());
    }

    #[test]
    fn secondary_sender_uses_the_rightmost_parenthesized_name() {
        let identity = parse_secondary_sender_identity("玩家(自带内容)(大厅备注)");
        assert_eq!(identity.nickname, "玩家(自带内容)");
        assert_eq!(identity.remark.as_deref(), Some("大厅备注"));
        assert_eq!(identity.effective_name(), "大厅备注");
        assert_eq!(
            normalize_secondary_sender_name("玩家(自带内容)(大厅备注)"),
            "大厅备注"
        );
        assert_eq!(
            normalize_secondary_sender_name("玩家（自带内容）（大厅备注）"),
            "大厅备注"
        );
        assert_eq!(normalize_secondary_sender_name("普通昵称"), "普通昵称");
        assert_eq!(
            normalize_secondary_sender_name("玩家(未闭合"),
            "玩家(未闭合"
        );
    }

    #[test]
    fn secondary_hall_extracts_only_supported_command_prefixes() {
        assert_eq!(secondary_hall_command_text(" #状态 "), Some("#状态"));
        assert_eq!(secondary_hall_command_text("＃状态"), Some("＃状态"));
        assert_eq!(
            secondary_hall_command_text("聊天内容 @点歌 测试"),
            Some("@点歌 测试")
        );
        assert_eq!(
            secondary_hall_command_text("聊天内容 ＠点歌 测试"),
            Some("＠点歌 测试")
        );
        assert_eq!(secondary_hall_command_text("普通聊天"), None);
        // 正文里出现 `#` 也会被截断出来，但**能不能执行取决于 router**：
        // `#话题` 不是注册命令，`route` 返回 None，不会被当成命令派发。
        // 这里只固定提取行为，执行与否由下游命令表决定。
        assert_eq!(secondary_hall_command_text("聊天内容 #话题"), Some("#话题"));
        assert_eq!(secondary_hall_command_text("昵称：#状态"), Some("#状态"));
    }

    #[test]
    fn secondary_hall_scans_sender_only_for_actor_dependent_inputs() {
        for command in [
            ModuleCommand::Playback(PlaybackCommand::Status),
            ModuleCommand::Administration(AdministrationCommand::Help),
            ModuleCommand::IdiomChain(idiom_chain::IdiomChainCommand::Hint),
            ModuleCommand::CardGame(LandlordCommand::Status),
            ModuleCommand::TurtleSoup(turtle_soup::TurtleSoupCommand::Status),
            ModuleCommand::Undercover(UndercoverCommand::Retry),
        ] {
            let classification = classify_secondary_hall_message("#状态", Some(&command), true);
            assert_eq!(classification.kind, SecondaryHallMessageKind::Command);
            assert!(!classification.requires_identity, "command={command:?}");
        }

        for command in [
            ModuleCommand::IdiomChain(idiom_chain::IdiomChainCommand::Submit(
                "画蛇添足".to_string(),
            )),
            ModuleCommand::CardGame(LandlordCommand::Play("3".to_string())),
            ModuleCommand::TurtleSoup(turtle_soup::TurtleSoupCommand::Start),
            ModuleCommand::Undercover(UndercoverCommand::Describe("描述".to_string())),
        ] {
            let classification = classify_secondary_hall_message("#输入", Some(&command), false);
            assert_eq!(classification.kind, SecondaryHallMessageKind::Command);
            assert!(classification.requires_identity, "command={command:?}");
        }

        let question = classify_secondary_hall_message("#男人是管理员吗", None, true);
        assert_eq!(question.kind, SecondaryHallMessageKind::TurtleQuestion);
        assert!(question.requires_identity);

        let ignored = classify_secondary_hall_message("普通聊天", None, true);
        assert_eq!(ignored.kind, SecondaryHallMessageKind::Ignored);
        assert!(!ignored.requires_identity);

        for text in ["@删除", "@邀请1"] {
            let fallback = classify_secondary_hall_message(text, None, false);
            assert_eq!(fallback.kind, SecondaryHallMessageKind::Command);
            assert!(fallback.requires_identity, "text={text}");
        }
    }

    #[test]
    fn secondary_hall_routes_common_commands_before_deciding_sender_ocr() {
        use crate::features::entertainment::EntertainmentKind;

        let classify = |text, active| {
            let command_text = secondary_hall_command_text(text).expect("command prefix");
            let envelope = CommandEnvelope::new(
                text,
                SECONDARY_HALL_FALLBACK_SENDER,
                "blue",
                command_text,
                CommandObservation::default(),
            )
            .expect("hall command envelope");
            let routed = ChatCommandRouter::without_custom_workflow()
                .route(&envelope, active)
                .expect("routed hall command");
            classify_secondary_hall_message(text, Some(&routed.command), false)
        };

        assert!(!classify("@点歌 测试", None).requires_identity);
        assert!(!classify("@帮助", None).requires_identity);
        assert!(classify("#斗地主", None).requires_identity);
        assert!(!classify("#状态", Some(EntertainmentKind::Landlord)).requires_identity);
        assert!(classify("#出3", Some(EntertainmentKind::Landlord)).requires_identity);
        assert!(!classify("#提示", Some(EntertainmentKind::IdiomChain)).requires_identity);
        assert!(classify("#画蛇添足", Some(EntertainmentKind::IdiomChain)).requires_identity);
        assert!(classify("#状态", Some(EntertainmentKind::Undercover)).requires_identity);
        assert!(!classify("#重试", Some(EntertainmentKind::Undercover)).requires_identity);
        assert!(!classify("#状态", Some(EntertainmentKind::TurtleSoup)).requires_identity);
    }

    #[test]
    fn secondary_listener_resides_in_current_hall() {
        assert_eq!(
            listener_residency(ChatListenerMode::Secondary, false),
            UiResidency::SecondaryCurrentHall
        );
    }

    #[test]
    fn return_primary_resolves_to_the_active_listener_residency() {
        let operations = vec![
            WorkflowOperation::PressKey {
                key: "F".to_string(),
            },
            WorkflowOperation::ReturnListenerResidency,
        ];

        assert_eq!(
            resolve_workflow_listener_residency(operations.clone(), UiResidency::Primary),
            vec![
                WorkflowOperation::PressKey {
                    key: "F".to_string(),
                },
                WorkflowOperation::EnsureResidency {
                    target: WorkflowResidency::Primary,
                },
            ]
        );
        assert_eq!(
            resolve_workflow_listener_residency(operations, UiResidency::SecondaryCurrentHall,),
            vec![
                WorkflowOperation::PressKey {
                    key: "F".to_string(),
                },
                WorkflowOperation::EnsureResidency {
                    target: WorkflowResidency::SecondaryCurrentHall,
                },
            ]
        );
    }

    #[test]
    fn secondary_hall_search_covers_the_scrollable_friend_list() {
        let anchor = Rect::new(10, 190, 65, 55);
        let friend_list = Rect::new(80, 280, 170, 600);

        let search = secondary_hall_search_rect(anchor, friend_list);

        assert_eq!(search.x, 10);
        assert_eq!(search.y, 190);
        assert_eq!(search.right(), 250);
        assert_eq!(search.bottom(), 880);
    }

    #[test]
    fn temporary_primary_stage_overrides_secondary_residency() {
        assert_eq!(
            listener_residency(ChatListenerMode::Secondary, true),
            UiResidency::Primary
        );
    }

    #[test]
    fn primary_listener_resides_in_primary_ui() {
        assert_eq!(
            listener_residency(ChatListenerMode::Primary, false),
            UiResidency::Primary
        );
    }

    #[test]
    fn turtle_soup_question_task_label_does_not_expose_question_text() {
        let question = turtle_soup::parse_question_message(
            "测试玩家：# 这段敏感提问不应出现在任务标签中？",
            None,
        )
        .expect("question");
        let task = PendingTask::TurtleSoupQuestion {
            question: Box::new(question),
            observed_at: Instant::now(),
        };

        assert_eq!(task.label(), "海龟汤提问");
        assert!(!task.label().contains("敏感提问"));
    }

    #[test]
    fn idiom_explanation_uses_the_exclusive_command_executor() {
        assert!(
            idiom_chain::IdiomChainCommand::Explain(Some("画蛇添足".to_string()))
                .requires_executor()
        );
        assert!(!idiom_chain::IdiomChainCommand::Hint.requires_executor());
        assert!(
            !idiom_chain::IdiomChainCommand::Submit("足智多谋".to_string()).requires_executor()
        );
    }

    #[test]
    fn landlord_uses_executor_only_for_friend_ui_operations() {
        assert!(LandlordCommand::Start.requires_executor());
        assert!(LandlordCommand::RunFastStart.requires_executor());
        assert!(LandlordCommand::Join.requires_executor());
        assert!(LandlordCommand::Hand.requires_executor());
        assert!(LandlordCommand::Retry.requires_executor());
        assert!(LandlordCommand::Rob.requires_executor());
        assert!(LandlordCommand::Decline.requires_executor());
        assert!(!LandlordCommand::Play("3".to_string()).requires_executor());
        assert!(!LandlordCommand::Pass.requires_executor());
    }

    struct FailingCardGamePort;

    impl CardGameDeliveryPort for FailingCardGamePort {
        fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
            Err(anyhow!("test verification failed"))
        }

        fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
            panic!("test should stop after verification")
        }

        fn send_friend_batch(
            &self,
            _deliveries: &[LandlordPrivateDelivery],
        ) -> Result<FriendBatchOutcome> {
            panic!("test should stop after verification")
        }

        fn send_hall(&self, _message: &str) -> Result<()> {
            panic!("test should stop after verification")
        }
    }

    struct NeverCalledCardGamePort;

    impl CardGameDeliveryPort for NeverCalledCardGamePort {
        fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
            panic!("late effect must not reach UI")
        }

        fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
            panic!("late effect must not reach UI")
        }

        fn send_friend_batch(
            &self,
            _deliveries: &[LandlordPrivateDelivery],
        ) -> Result<FriendBatchOutcome> {
            panic!("late effect must not reach UI")
        }

        fn send_hall(&self, _message: &str) -> Result<()> {
            panic!("late effect must not reach UI")
        }
    }

    struct PanickingCardGamePort;

    impl CardGameDeliveryPort for PanickingCardGamePort {
        fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
            panic!("test verification panic")
        }

        fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
            panic!("test friend delivery panic")
        }

        fn send_friend_batch(
            &self,
            _deliveries: &[LandlordPrivateDelivery],
        ) -> Result<FriendBatchOutcome> {
            panic!("test friend batch delivery panic")
        }

        fn send_hall(&self, _message: &str) -> Result<()> {
            panic!("test hall delivery panic")
        }
    }

    #[derive(Default)]
    struct RecordingCardGamePort {
        hall_messages: Mutex<Vec<String>>,
    }

    impl CardGameDeliveryPort for RecordingCardGamePort {
        fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
            Ok(true)
        }

        fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
            Ok(true)
        }

        fn send_friend_batch(
            &self,
            _deliveries: &[LandlordPrivateDelivery],
        ) -> Result<FriendBatchOutcome> {
            Ok(FriendBatchOutcome::Complete)
        }

        fn send_hall(&self, message: &str) -> Result<()> {
            self.hall_messages
                .lock()
                .expect("hall messages")
                .push(message.to_string());
            Ok(())
        }
    }

    fn card_game_runtime_for_test() -> BusinessRuntime {
        let idiom_chain = IdiomChainService::from_entries_for_test(&["画蛇添足", "足智多谋"], None);
        BusinessRuntime::start(
            8,
            idiom_chain,
            CardGameService::new(LandlordConfig::default()),
        )
        .unwrap()
    }

    #[test]
    fn formal_card_game_ui_errors_are_resumed_before_being_reported() {
        let runtime = card_game_runtime_for_test();
        let business = runtime.handle();
        let start = business
            .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
            .unwrap();

        let card_games = CardGameApplication::new(Arc::new(business.clone()));
        let error = card_games
            .drive_start(start, CardGameEffectLane::Formal, &FailingCardGamePort)
            .unwrap_err();

        assert!(error.to_string().contains("test verification failed"));
        assert_eq!(business.active_entertainment().unwrap(), None);
        let retry = business
            .begin_card_game("乙", &LandlordCommand::Start, Instant::now())
            .unwrap();
        let CardGameCommandStart::Suspended(retry) = retry else {
            panic!("failed verification should release the start reservation")
        };
        business.cancel_card_game_effect(retry.key).unwrap();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn deferred_card_game_query_drives_the_real_effect_chain_without_lane_mismatch() {
        let runtime = card_game_runtime_for_test();
        let business = runtime.handle();
        let card_games = CardGameApplication::new(Arc::new(business.clone()));
        let delivery = RecordingCardGamePort::default();

        card_games
            .execute_command(
                "甲",
                &LandlordCommand::Start,
                Instant::now(),
                CardGameEffectLane::Formal,
                &delivery,
            )
            .expect("formal start");
        card_games
            .execute_command(
                "甲",
                &LandlordCommand::Status,
                Instant::now(),
                CardGameEffectLane::Deferred,
                &delivery,
            )
            .expect("deferred status");

        assert_eq!(delivery.hall_messages.lock().unwrap().len(), 2);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn formal_card_game_ui_panics_are_resumed_before_being_reported() {
        let runtime = card_game_runtime_for_test();
        let business = runtime.handle();
        let start = business
            .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
            .unwrap();

        let card_games = CardGameApplication::new(Arc::new(business.clone()));
        let error = card_games
            .drive_start(start, CardGameEffectLane::Formal, &PanickingCardGamePort)
            .unwrap_err();

        assert!(error.to_string().contains("牌局好友验证发生未捕获异常"));
        assert_eq!(business.active_entertainment().unwrap(), None);
        let retry = business
            .begin_card_game("乙", &LandlordCommand::Start, Instant::now())
            .unwrap();
        let CardGameCommandStart::Suspended(retry) = retry else {
            panic!("panicking verification should release the start reservation")
        };
        business.cancel_card_game_effect(retry.key).unwrap();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn formal_late_effects_error_but_timed_late_effects_are_idempotent() {
        let runtime = card_game_runtime_for_test();
        let business = runtime.handle();
        let start = business
            .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
            .unwrap();
        let CardGameCommandStart::Suspended(request) = start else {
            panic!("start should wait for verification")
        };
        business.cancel_card_game_effect(request.key).unwrap();

        let card_games = CardGameApplication::new(Arc::new(business.clone()));
        let formal_error = card_games
            .drive_start(
                CardGameCommandStart::Suspended(request.clone()),
                CardGameEffectLane::Formal,
                &NeverCalledCardGamePort,
            )
            .unwrap_err();
        assert!(formal_error.to_string().contains("已失效"));

        card_games
            .effect_task("test-timeout", request)
            .execute(&NeverCalledCardGamePort)
            .unwrap();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn secondary_decision_reader_accepts_first_bubble_after_empty_baseline() {
        let mut image = image::RgbaImage::new(1920, 1080);
        for y in 260..278 {
            for x in 418..479 {
                image.put_pixel(x, y, image::Rgba([195, 193, 185, 255]));
            }
        }
        let center_x = 346_i32;
        let center_y = 308_i32;
        let radius_squared = 40_i32.pow(2);
        for y in 264_i32..352 {
            for x in 302_i32..390 {
                let dx = x - center_x;
                let dy = y - center_y;
                if dx * dx + dy * dy <= radius_squared {
                    image.put_pixel(x as u32, y as u32, image::Rgba([220, 220, 220, 255]));
                }
            }
        }
        for y in 300..354 {
            for x in 415..700 {
                image.put_pixel(x, y, image::Rgba([62, 71, 89, 255]));
            }
        }
        let current =
            secondary_hall_bubbles(&DynamicImage::ImageRgba8(image)).expect("secondary bubbles");

        assert!(!current.is_empty());
        assert_eq!(
            secondary_hall_sequence_delta(None, &current),
            SecondaryHallSequenceDelta::EstablishBaseline
        );
        assert_eq!(
            secondary_hall_sequence_delta(Some(&[]), &current),
            SecondaryHallSequenceDelta::NewFrom(0)
        );
        assert_eq!(
            secondary_hall_sequence_delta(Some(&current), &[]),
            SecondaryHallSequenceDelta::LostOverlap
        );
    }

    #[test]
    fn secondary_hall_listener_distinguishes_uninitialized_from_empty_baseline() {
        assert_eq!(
            secondary_hall_sequence_delta(None, &[]),
            SecondaryHallSequenceDelta::EstablishBaseline
        );
        assert_eq!(
            secondary_hall_sequence_delta(Some(&[]), &[]),
            SecondaryHallSequenceDelta::NoChange
        );
    }

    #[test]
    fn window_detection_signal_times_out_without_request() {
        let signal = WindowDetectionSignal::new();
        let generation = signal.generation().expect("generation");

        assert!(
            !signal
                .wait_for_change(generation, Duration::from_millis(1))
                .expect("wait")
        );
    }

    #[test]
    fn window_detection_signal_wakes_waiter() {
        let signal = WindowDetectionSignal::new();
        let generation = signal.generation().expect("generation");
        let notifier = signal.clone();
        let handle = thread::spawn(move || {
            sleep(Duration::from_millis(10));
            notifier.request("test").expect("request");
        });

        assert!(
            signal
                .wait_for_change(generation, Duration::from_secs(1))
                .expect("wait")
        );
        handle.join().expect("join notifier");
    }
}
