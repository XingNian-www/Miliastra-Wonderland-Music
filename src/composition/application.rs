mod commands;
mod custom_workflow;
mod delivery;
mod diagnostics;
pub(crate) mod formal_task;
mod lifecycle;
mod listener;
mod moderation;
mod playback;
mod secondary_chat;
mod song_request_port;
mod startup;
mod tasks;
mod workers;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, sleep};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use self::formal_task::{FormalTaskClient, FormalTaskExecutionRuntime};
use crate::adapters::feeluown::FeelUOwnClient;
use crate::adapters::player::PlayerRuntimeBackend;
use crate::adapters::windows::{WindowsUiDevice, parse_key};
use crate::config::{AppConfig, PointConfig};
use crate::features::administration::{
    AdministrationApplication, AdministrationCommand, ChatListenerModeCommand,
};
use crate::features::card_games::{
    CardGameApplication, CardGameDeliveryPort, CardGameEffectLane, CardGameEffectTask,
    CardGameService, LandlordCommand, LandlordPrivateDelivery,
};
#[cfg(test)]
use crate::features::card_games::{CardGameCommandStart, LandlordConfig};
use crate::features::command::{
    CommandEnvelope, CommandObservation, CommandPrefix, ModuleCommand, RoutedCommand,
};
use crate::features::custom_workflow::{CustomWorkflowService, WorkflowDefaults};
use crate::features::friend_delivery::{FriendBatchOutcome, FriendMessage};
use crate::features::hall::{HallApplication, HallCommand, HallStateService};
use crate::features::idiom_chain;
use crate::features::idiom_chain::{IdiomChainApplication, IdiomChainService};
use crate::features::invite::{InviteRequest, InviteService, InviteStart};
use crate::features::moderation::{ModerationPolicy, ModerationResultTask, ModerationService};
use crate::features::playback::{
    ExternalPlaybackObservation, PlaybackApplication, PlaybackApplicationConfig, PlaybackCommand,
    PlaybackRequest, PlaybackRuntimeState, PlaybackService, PlaybackStatePort, PlaybackStateUpdate,
    PlaybackTimePorts, PlayerController, PlayerStatus, QueueItem, SongDedupCandidate,
};
use crate::features::song_request::{
    AiClient, ResolvedSongRequest, SongRequestApplication, SongRequestContext, SongRequestDecision,
    SongReviewClient,
};
use crate::features::startup::{StartupService, StartupSource, StartupTask};
use crate::features::turtle_soup::{
    self, SecondaryOcrObservation, SecondaryOcrStability, TurtleSoupApplication, TurtleSoupConfig,
    TurtleSoupService,
};
use crate::features::undercover::{
    UndercoverApplication, UndercoverCommand, UndercoverCommandSource, UndercoverDeliveryPort,
    UndercoverEffectTask, UndercoverRuntimeService,
};
use crate::interfaces::chat::{
    self as command, ChatCommandRouter, CommandLockState, PendingCommand,
};
use crate::interfaces::hotkeys;
use crate::interfaces::http::{self, WebToolRequest, WebToolTemplate};
use crate::interfaces::ui_plan::{WorkflowOperation, WorkflowResidency};
use crate::observation::chat::{
    ChatMessage, ChatObservationDispatch, ChatObservationExclusiveGuard, ChatObservationShared,
    ChatScanTelemetry, ChatScanTelemetrySink, CompletionAdvanceSubscriber, ObservedFrame,
    PrimaryObservedMessage, ResolvedTemplateArgs, SECONDARY_TITLE_RECT, SecondaryChatIdentity,
    SecondaryChatObservation, SecondaryHallBubble, SecondaryObservedMessage,
    SecondaryRecognizedMessage, TemplateArgs, UnreadFriendHit, classify_title, count_chat_markers,
    find_unread_friend_hits, hall_bubble_sequence_is_retained_prefix, hall_bubble_sequence_overlap,
    hall_bubble_sequences_stable, latest_incoming_bubble_rect, latest_incoming_fingerprint,
    prepare_chat_scan, recognize_prepared_chat, secondary_hall_bubbles,
};
use crate::observation::decision::DecisionScreenLock;
use crate::observation::shared::ObservationRead;
use crate::privacy::redacted_chat_text;
use crate::runtime::business::{
    BusinessEvent, BusinessRuntime, BusinessRuntimeEventSink, BusinessRuntimeHandle,
    BusinessRuntimeWorker,
};
use crate::runtime::chat_listener::ChatListenerMode;
use crate::runtime::clock::SystemClock;
use crate::runtime::deadline_bridge::{BusinessRuntimeGroup, BusinessRuntimeGroupBuilder};
use crate::runtime::decision::DecisionAction;
use crate::runtime::deferred_chat::{
    BatchFailureOutcome, DeferredChatItem, DeferredChatMessage, DeferredChatTarget, EnqueueOutcome,
};
use crate::runtime::identity::BusinessOperationIdAllocator;
use crate::runtime::monitor::{MonitorEvent, MonitorShared, OcrSnapshot};
use crate::runtime::ocr::{
    OcrArgs, OcrBackendProbeStatus, OcrPriority, OcrRuntime, OcrRuntimeHandle, ProductionOcrDevice,
    ResolvedOcrArgs, probe_ocr_backend_support,
};
use crate::runtime::openai::OpenAiRuntime;
use crate::runtime::player_io::{
    PlayerRuntime, PlayerRuntimeConfig, PlayerSearchClient, PlayerSearchClientError,
};
use crate::runtime::scheduler::{
    DiagnosticTaskCompletion, FormalTaskCompletion, FormalTaskEnqueueOutcome,
};
use crate::runtime::ui::{
    FrameDemand, FrameDemandSubscription, FramePublication, UiRuntime, UiStateKind,
    UiStateObservation,
};
use crate::ui::atoms::GameUi;
use crate::ui::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
use crate::ui::chat_output::{ChatBatchSendOutcome, ChatBatchSendStatus, ChatOutput};
use crate::ui::frame::{Canvas, Frame, from_captured_frame, load_frame};
use crate::ui::geometry::{Rect, crop_canvas};
#[cfg(test)]
use crate::ui::locator::secondary_hall_search_rect;
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
use crate::ui::state::{ResolvedUiTemplateArgs, TemplateUiStateClassifier, UiTemplateArgs};
use crate::ui::template::{best_template_hit, find_template_hits};
use anyhow::{Context, Result, anyhow};
use enigo::Key;
use image::DynamicImage;

const TARGET_MISSING_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const TARGET_MISSING_BACKOFF_MAX: Duration = Duration::from_secs(60);
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
                delivery: &app.friend_delivery,
                friend_list_region: app.invite.friend_list_region.into(),
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
            search_input: crate::ui::geometry::Point::new(
                app.moderation.search_input_point.x,
                app.moderation.search_input_point.y,
            ),
            search_button: crate::ui::geometry::Point::new(
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
                enter_game: app.startup.enter_game,
                exe_path: app.startup.exe_path.clone(),
                game_args: app.startup.game_args.clone(),
                launch_wait_ms: app.startup.launch_wait_ms,
                launch_retries: app.startup.launch_retries,
                enter_game_timeout_ms: app.startup.enter_game_timeout_ms,
                enter_wonderland_timeout_ms: app.startup.enter_wonderland_timeout_ms,
                wonderland_home_retries: app.startup.wonderland_home_retries,
                wonderland_home_retry_ms: app.startup.wonderland_home_retry_ms,
                wonderland_card_retries: app.startup.wonderland_card_retries,
                wonderland_card_retry_ms: app.startup.wonderland_card_retry_ms,
                wonderland_confirm_absent_timeout_ms: app
                    .startup
                    .wonderland_confirm_absent_timeout_ms,
                wonderland_confirm_stable_timeout_ms: app
                    .startup
                    .wonderland_confirm_stable_timeout_ms,
                final_primary_timeout_ms: app.startup.final_primary_timeout_ms,
                poll_ms: app.startup.poll_ms,
                stable_mean_threshold: app.startup.stable_mean_threshold,
                stable_changed_ratio_threshold: app.startup.stable_changed_ratio_threshold,
                template_threshold: app.startup.template_threshold,
                wonderland_enter_button_threshold: app.startup.wonderland_enter_button_threshold,
                templates: StartupUiTemplates {
                    wonderland_enter_button: app.startup.templates.wonderland_enter_button.clone(),
                    paimon_menu: app.startup.templates.paimon_menu.clone(),
                    wonderland_close: app.startup.templates.wonderland_close.clone(),
                },
                enter_game_text_region: app.startup.enter_game_text_region.into(),
                wonderland_enter_button_region: app.startup.wonderland_enter_button_region.into(),
                main_ui_region: app.startup.main_ui_region.into(),
                wonderland_close_region: app.startup.wonderland_close_region.into(),
                wonderland_card_point: crate::ui::geometry::Point::new(
                    app.startup.wonderland_card_point.x,
                    app.startup.wonderland_card_point.y,
                ),
            },
            friend_delivery.clone(),
            app.window.target_process.clone(),
        );
        let secondary_unread = SecondaryUnreadRoutineConfig::resolve(
            friend_delivery.clone(),
            app.ocr.same_line_y_tolerance,
            app.timing.chat_scan.change_debounce_ms,
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
) -> Result<Frame> {
    match subscription.recv().context("等待 UI runtime 发布观察帧")? {
        FramePublication::Captured(published) => Ok(from_captured_frame(&published, canvas)),
        FramePublication::Failed(failure) => Err(anyhow!(
            "UI runtime 观察帧截图失败 at {:?}: {}",
            failure.failed_at(),
            failure.reason()
        )),
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SecondaryBubbleProcessOutcome {
    processed: bool,
    confirmation_pending: bool,
}

pub(crate) struct ApplicationRuntime {
    config: AppConfig,
    ocr_args: ResolvedOcrArgs,
    chat_templates: ResolvedTemplateArgs,
    http_server: Option<http::HttpServer>,
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
    business: BusinessRuntimeHandle,
    business_events: BusinessRuntimeEventSink,
    business_runtime: Option<BusinessRuntimeGroup>,
    formal_task_execution: Option<FormalTaskExecutionRuntime>,
    formal_tasks: Option<FormalTaskClient>,
    player: PlayerController<PlayerRuntimeBackend, BusinessPlaybackStateAdapter>,
    playback_application: PlaybackApplication,
    player_search: PlayerSearchClient,
    player_runtime: Option<PlayerRuntime>,
    openai_runtime: Option<OpenAiRuntime>,
    ai: AiClient,
    song_requests: SongRequestApplication,
    chat_output: ChatOutput,
    ocr: OcrRuntimeHandle,
    ocr_runtime: Option<OcrRuntime>,
    latest_frame: Arc<Mutex<Option<Arc<DynamicImage>>>>,
    locks: CommandLockState,
    window_detection_signal: WindowDetectionSignal,
    screen_lock_primed: Arc<AtomicBool>,
    reset_locks_requested: Arc<AtomicBool>,
    card_games: CardGameApplication,
    administration_application: AdministrationApplication,
    hall_application: HallApplication,
    idiom_chain_application: IdiomChainApplication,
    turtle_soup_application: TurtleSoupApplication,
    undercover_game: UndercoverApplication,
    moderation: ModerationService,
    moderation_workers: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    startup: StartupService,
    custom_workflow: CustomWorkflowService,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    console_reply_context: Arc<AtomicBool>,
    chat_observations: ChatObservationShared,
    monitor: MonitorShared,
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

    fn observe_external_playback(
        &self,
        identity: String,
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
                    log::error!("取消监听模式切换失败: {error}");
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
                    "二级监听初始未读清场".to_string()
                } else {
                    "二级监听好友未读".to_string()
                }
            }
            Self::RestoreSecondaryHall => "二级监听恢复当前大厅".to_string(),
            Self::TurtleSoupQuestion { .. } => "海龟汤提问".to_string(),
            Self::CardGameEffect(effect) => effect.label(),
            Self::UndercoverEffect(effect) => effect.label(),
        }
    }

    fn dedup_key(&self) -> Option<String> {
        match self {
            Self::Command(pending) => Some(pending.lock_key.clone()),
            Self::ModerationResult(task) => Some(task.dedup_key()),
            Self::AdvanceQueue { .. }
            | Self::ConsoleChat { .. }
            | Self::Startup(_)
            | Self::ClearIdleExit
            | Self::SetChatListenerMode { .. }
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
            Self::ListenerModeSwitch => "切换聊天监听模式",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatDecisionScope {
    CurrentHall,
    MultipleConversations,
}

enum ChatDecisionReaderKind {
    Primary,
    SecondaryCurrentHall { previous: Vec<SecondaryHallBubble> },
}

struct ChatDecisionReader {
    kind: ChatDecisionReaderKind,
    screen_lock: DecisionScreenLock,
    _observation_session: ChatObservationExclusiveGuard,
}

impl ChatDecisionReader {
    fn accept_once(&mut self, message: &ChatMessage) -> bool {
        self.screen_lock.accept_once(message)
    }

    fn poll_interval_ms(&self, configured_ms: u64) -> u64 {
        match &self.kind {
            ChatDecisionReaderKind::Primary => configured_ms.max(50),
            ChatDecisionReaderKind::SecondaryCurrentHall { .. } => configured_ms.clamp(100, 500),
        }
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
                log::error!("释放临时一级监听保留失败: {error}");
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
    pub(crate) fn new(config: ResolvedApplicationConfig, monitor: MonitorShared) -> Result<Self> {
        let ResolvedApplicationConfig {
            app: config,
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
        let ocr_device = ProductionOcrDevice::new(ocr_args.clone())?;
        let feeluown = FeelUOwnClient::new(&config.feeluown, &config.timing);
        let idiom_chain = IdiomChainService::load(config.idiom_chain.clone())?;
        if config.idiom_chain.enabled {
            log::info!("已加载成语接龙词库: {} 条", idiom_chain.lexicon_len());
        }
        let landlord = CardGameService::new(config.landlord.clone());
        let undercover = UndercoverRuntimeService::new(config.undercover.clone());
        let hall = HallStateService::load(
            config.state.hall_state_path.clone(),
            system_clock.clone(),
            system_clock.clone(),
        )?;
        let playback = PlaybackService::load(
            config.state.queue_path.clone(),
            config.state.playback_state_path.clone(),
            config.song_dedup.history_path.clone(),
            config.queue.max_size,
            config.song_dedup.clone(),
            system_clock.clone(),
        )?;
        let moderation_policy = ModerationPolicy::new(
            Duration::from_millis(config.timing.moderation.vote_timeout_ms),
            Duration::from_millis(config.timing.moderation.vote_poll_ms),
            config.moderation.stable_vote_samples,
            config.moderation.required_vote_margin,
        );
        let custom_workflow = CustomWorkflowService::new(
            config.custom_workflows.clone(),
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
        let chat_observations = ChatObservationShared::new(
            config.ocr.change_mean_threshold,
            config.ocr.change_pixel_threshold,
        );
        let running = Arc::new(AtomicBool::new(true));
        let business_runtime_builder =
            BusinessRuntimeGroupBuilder::start(DEADLINE_RUNTIME_QUEUE_CAPACITY)?;
        let ocr_runtime = OcrRuntime::start(ocr_device, OCR_RUNTIME_QUEUE_CAPACITY)?;
        let ocr = ocr_runtime.handle();
        let player_runtime = PlayerRuntime::start(
            feeluown.clone(),
            feeluown.clone(),
            feeluown.clone(),
            player_runtime_config,
        )
        .context("启动播放器运行时")?;
        let player_runtime_handle = player_runtime.handle();
        let player_search = PlayerSearchClient::new(
            player_runtime_handle.clone(),
            BusinessOperationIdAllocator::new(),
        );
        let ui_state_stable_count = config.resolve_stability_count(config.stability.ui_state_count);
        let ui_state_classifier =
            TemplateUiStateClassifier::new(ui_templates.clone(), config.screen.clone());
        let ui_runtime = UiRuntime::start_with_progress_and_state_classifier(
            WindowsUiDevice::new(config.window.clone()),
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
        let secondary_unread_ui =
            SecondaryUnreadUi::new(ui_handle.clone(), ocr.clone(), secondary_unread_config);
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
        let ai = AiClient::new(&config.ai, ai_request_timeout, openai.clone());
        let song_review = SongReviewClient::new(
            &config.song_review,
            ai_request_timeout,
            openai.clone(),
            system_clock.clone(),
        );
        let song_requests = SongRequestApplication::new(
            ai.clone(),
            song_review,
            config.queue.max_size,
            config.song_dedup.console_bypass,
        );
        let chat_output = ChatOutput::new(&config.output, hall_batch_ui);
        let turtle_soup = TurtleSoupService::new(
            turtle_soup_config,
            openai,
            system_clock.clone(),
            system_clock.clone(),
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
                    business_timer,
                    Arc::new(monitor.clone()),
                    system_clock.clone(),
                ),
            )
        })?;
        let business = business_runtime.business_handle();
        let business_events = business_runtime.event_sink();
        let card_games = CardGameApplication::new(Arc::new(business.clone()));
        let undercover_game = UndercoverApplication::new(Arc::new(business.clone()));
        let player = PlayerController::new(
            PlayerRuntimeBackend::new(player_runtime_handle),
            BusinessPlaybackStateAdapter::new(business.clone()),
            &config.timing.playback,
            &config.queue,
            &config.matching,
            PlaybackTimePorts::new(system_clock.clone(), system_clock.clone(), system_clock),
        );
        let playback_application = PlaybackApplication::new(PlaybackApplicationConfig {
            console_bypass_dedup: config.song_dedup.console_bypass,
            queue_max_size: config.queue.max_size,
            skip_status_initial_ms: config.timing.playback.skip_status_initial_ms,
            skip_status_poll_ms: config.timing.playback.skip_status_poll_ms,
            skip_status_retries: config.timing.playback.skip_status_retries,
            monitor_tick_ms: config.timing.playback.monitor_tick_ms,
            monitor_status_ms: config.timing.playback.monitor_status_ms,
        });
        let administration_application =
            AdministrationApplication::new(config.timing.command.help_batch_ms);
        let idiom_chain_application =
            IdiomChainApplication::new(config.timing.command.help_batch_ms);
        let moderation = ModerationService::new(moderation_policy, Arc::new(business.clone()));
        Ok(Self {
            config,
            ocr_args,
            chat_templates,
            http_server: None,
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
            business,
            business_events,
            business_runtime: Some(business_runtime),
            formal_task_execution: None,
            formal_tasks: None,
            player,
            playback_application,
            player_search,
            player_runtime: Some(player_runtime),
            openai_runtime: Some(openai_runtime),
            ai,
            song_requests,
            chat_output,
            ocr,
            ocr_runtime: Some(ocr_runtime),
            latest_frame: Arc::new(Mutex::new(None)),
            locks: CommandLockState::default(),
            window_detection_signal: WindowDetectionSignal::new(),
            screen_lock_primed: Arc::new(AtomicBool::new(false)),
            reset_locks_requested: Arc::new(AtomicBool::new(false)),
            card_games,
            administration_application,
            hall_application: HallApplication,
            idiom_chain_application,
            turtle_soup_application: TurtleSoupApplication,
            undercover_game,
            moderation,
            moderation_workers: Arc::new(Mutex::new(Vec::new())),
            startup: StartupService::new(),
            custom_workflow,
            running,
            paused: Arc::new(AtomicBool::new(false)),
            console_reply_context: Arc::new(AtomicBool::new(false)),
            chat_observations,
            monitor,
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

fn secondary_fingerprint_changed(
    previous: &ChangeFingerprint,
    current: &ChangeFingerprint,
) -> bool {
    let stats = change_stats(previous, current);
    stats.mean_abs_diff >= 0.8 || stats.changed_ratio >= 0.01
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SecondaryHallSequenceDelta {
    EstablishBaseline,
    RetainedPrefix,
    NoChange,
    LostOverlap,
    NewFrom(usize),
}

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

const SECONDARY_HALL_FALLBACK_SENDER: &str = "二级大厅";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SecondaryHallMessageKind {
    Ignored,
    Command,
    TurtleQuestion,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SecondaryHallMessageClassification {
    kind: SecondaryHallMessageKind,
    requires_sender: bool,
}

fn secondary_hall_command_text(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if trimmed.starts_with('#') || trimmed.starts_with('＃') {
        return Some(trimmed);
    }
    text.find('@').map(|index| text[index..].trim())
}

fn classify_secondary_hall_message(
    text: &str,
    command: Option<&ModuleCommand>,
    accepts_turtle_questions: bool,
) -> SecondaryHallMessageClassification {
    if let Some(command) = command {
        return SecondaryHallMessageClassification {
            kind: SecondaryHallMessageKind::Command,
            requires_sender: command.requires_hall_sender(),
        };
    }
    if accepts_turtle_questions
        && turtle_soup::parse_question_message(text, Some(SECONDARY_HALL_FALLBACK_SENDER)).is_some()
    {
        return SecondaryHallMessageClassification {
            kind: SecondaryHallMessageKind::TurtleQuestion,
            requires_sender: true,
        };
    }
    SecondaryHallMessageClassification {
        kind: SecondaryHallMessageKind::Ignored,
        requires_sender: false,
    }
}

fn normalize_secondary_sender_name(value: &str) -> String {
    let value = value.trim();
    let mut rightmost_pair = None;
    for (open, close) in [('(', ')'), ('（', '）')] {
        let Some(close_index) = value.rfind(close) else {
            continue;
        };
        let Some(open_index) = value[..close_index].rfind(open) else {
            continue;
        };
        if rightmost_pair
            .as_ref()
            .is_none_or(|(best_close, _, _)| close_index > *best_close)
        {
            rightmost_pair = Some((close_index, open_index, open.len_utf8()));
        }
    }
    if let Some((close_index, open_index, open_len)) = rightmost_pair {
        let remark = value[open_index + open_len..close_index].trim();
        if !remark.is_empty() {
            return remark.to_string();
        }
    }
    value.to_string()
}

fn secondary_optional_fingerprint_changed(
    previous: Option<&ChangeFingerprint>,
    current: Option<&ChangeFingerprint>,
) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => secondary_fingerprint_changed(previous, current),
        (None, None) => false,
        _ => true,
    }
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

    #[test]
    fn secondary_sender_uses_the_rightmost_parenthesized_name() {
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
        assert_eq!(secondary_hall_command_text("普通聊天"), None);
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
            assert!(!classification.requires_sender, "command={command:?}");
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
            assert!(classification.requires_sender, "command={command:?}");
        }

        let question = classify_secondary_hall_message("#男人是管理员吗", None, true);
        assert_eq!(question.kind, SecondaryHallMessageKind::TurtleQuestion);
        assert!(question.requires_sender);

        let ignored = classify_secondary_hall_message("普通聊天", None, true);
        assert_eq!(ignored.kind, SecondaryHallMessageKind::Ignored);
        assert!(!ignored.requires_sender);
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

        assert!(!classify("@点歌 测试", None).requires_sender);
        assert!(!classify("@帮助", None).requires_sender);
        assert!(classify("#斗地主", None).requires_sender);
        assert!(!classify("#状态", Some(EntertainmentKind::Landlord)).requires_sender);
        assert!(classify("#出3", Some(EntertainmentKind::Landlord)).requires_sender);
        assert!(!classify("#提示", Some(EntertainmentKind::IdiomChain)).requires_sender);
        assert!(classify("#画蛇添足", Some(EntertainmentKind::IdiomChain)).requires_sender);
        assert!(classify("#状态", Some(EntertainmentKind::Undercover)).requires_sender);
        assert!(!classify("#重试", Some(EntertainmentKind::Undercover)).requires_sender);
        assert!(!classify("#状态", Some(EntertainmentKind::TurtleSoup)).requires_sender);
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
