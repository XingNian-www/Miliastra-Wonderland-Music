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
mod song_request;
mod startup;
mod tasks;
mod workers;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, sleep};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use self::formal_task::{FormalTaskClient, FormalTaskExecutionRuntime};
use crate::adapters::feeluown::FeelUOwnClient;
use crate::adapters::windows::{WindowsUiDevice, parse_key};
use crate::config::{AppConfig, PointConfig};
use crate::features::administration::AdministrationCommand;
#[cfg(test)]
use crate::features::card_games::LandlordConfig;
use crate::features::card_games::{
    CardGameCommandStart, CardGameDeliveryPort, CardGameEffect, CardGameEffectClaim,
    CardGameEffectLane, CardGameEffectRequest, CardGameEffectResult, CardGameResume,
    CardGameService, LandlordCommand, LandlordPrivateDelivery,
};
use crate::features::chat_text::split_numbered_chat_message;
use crate::features::custom_workflow::{CustomWorkflowService, service_from_config_parts};
use crate::features::friend_delivery::{FriendBatchOutcome, FriendMessage};
use crate::features::hall::HallCommand;
use crate::features::idiom_chain;
use crate::features::idiom_chain::IdiomChainService;
use crate::features::invite::{InviteRequest, InviteService, InviteStart};
use crate::features::moderation::{ModerationPolicy, ModerationResultTask, ModerationService};
use crate::features::playback::{
    HALL_EXPIRING_WARNING_MINUTES, MismatchDecision, PersistentQueue, PersistentRuntimeState,
    PersistentSongDedupHistory, PlaybackAttempt, PlaybackCommand, PlaybackOutcome, PlaybackRequest,
    PlaybackService, PlaybackSnapshot, PlaybackVerification, PlayerController,
    PlayerRuntimeBackend, PlayerStatus, QueueAdvanceContext, QueueAdvanceDecision, QueueItem,
    QueueRemoval, estimated_player_status, format_lyrics, format_play_message, format_status,
    is_playing, song_title,
};
use crate::features::song_request::{
    AiClient, SongCommand, SongReviewCandidate, SongReviewClient, split_candidate_title_artist,
};
use crate::features::startup::{StartupService, StartupSource, StartupTask};
use crate::features::turtle_soup::{
    self, QuestionSubmitOutcome, SecondaryOcrObservation, SecondaryOcrStability, TurtleSoupService,
};
use crate::features::undercover::{
    UndercoverCommand, UndercoverCommandSource, UndercoverCommandStart, UndercoverDeliveryPort,
    UndercoverEffect, UndercoverEffectClaim, UndercoverEffectLane, UndercoverEffectRequest,
    UndercoverEffectResult, UndercoverResume, UndercoverRuntimeService,
};
use crate::interfaces::chat::{
    self as command, CommandLockState, CommandObservation, ParsedCommand, PendingCommand,
    from_custom_workflow_match,
};
use crate::interfaces::hotkeys;
use crate::interfaces::http::{self, WebToolRequest, WebToolTemplate};
use crate::observation::chat::{
    ChatMessage, ChatObservationDispatch, ChatObservationExclusiveGuard, ChatObservationShared,
    CompletionAdvanceSubscriber, ObservedFrame, PrimaryObservedMessage, ResolvedTemplateArgs,
    SECONDARY_TITLE_RECT, SecondaryChatIdentity, SecondaryChatObservation, SecondaryHallBubble,
    SecondaryObservedMessage, SecondaryRecognizedMessage, TemplateArgs, UnreadFriendHit,
    classify_title, count_chat_markers, find_unread_friend_hits, hall_bubble_sequence_overlap,
    latest_incoming_bubble_rect, latest_incoming_fingerprint, prepare_chat_scan,
    recognize_prepared_chat, secondary_hall_bubbles,
};
use crate::observation::decision::DecisionScreenLock;
use crate::observation::shared::ObservationRead;
use crate::privacy::redacted_chat_text;
use crate::runtime::business::{
    BusinessEvent, BusinessIntent, BusinessRuntime, BusinessRuntimeEventSink,
    BusinessRuntimeHandle, BusinessRuntimeWorker,
};
use crate::runtime::chat_listener::{ChatListenerMode, ChatListenerModeCommand};
use crate::runtime::deadline_bridge::{BusinessRuntimeGroup, BusinessRuntimeGroupBuilder};
use crate::runtime::decision::DecisionAction;
use crate::runtime::deferred_chat::{
    BatchFailureOutcome, DeferredChatItem, DeferredChatMessage, DeferredChatTarget, EnqueueOutcome,
};
use crate::runtime::identity::BusinessOperationIdAllocator;
use crate::runtime::monitor::{MonitorEvent, MonitorShared, OcrSnapshot};
use crate::runtime::ocr::{
    OcrArgs, OcrBackendProbeStatus, OcrPriority, OcrRuntime, OcrRuntimeHandle, ProductionOcrDevice,
    probe_ocr_backend_support,
};
use crate::runtime::openai::OpenAiRuntime;
use crate::runtime::player_io::{PlayerRuntime, PlayerSearchClient, PlayerSearchClientError};
use crate::runtime::scheduler::{
    DiagnosticTaskCompletion, FormalTaskCompletion, FormalTaskEnqueueOutcome,
};
use crate::runtime::ui::{
    FrameDemand, FrameDemandSubscription, FramePublication, UiRuntime, UiRuntimeHandle,
};
use crate::text::normalize_comparison_text;
use crate::ui::atoms::GameUi;
use crate::ui::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
use crate::ui::chat_output::{ChatBatchSendOutcome, ChatBatchSendStatus, ChatOutput};
use crate::ui::frame::{Canvas, Frame, from_captured_frame, load_frame};
use crate::ui::geometry::{Rect, crop_canvas};
#[cfg(test)]
use crate::ui::locator::secondary_hall_search_rect;
use crate::ui::locator::{HallInfo, format_hall_remaining_suffix};
use crate::ui::routines::{
    CustomActionUi, DetectPublicHall, DetectPublicHallEffect, EstablishResidency, FriendDeliveryUi,
    HallBatchUi, HallUi, InviteUi, ModerationUi, ProcessSecondaryUnread, ReadHallInfo,
    ReadHallInfoEffect, ResidencyUi, SecondaryUnreadEffect, SecondaryUnreadUi, StartupUi,
    ToggleMicrophone, ToggleMicrophoneEffect, UiResidencyOutcome, UiResidencyTarget,
};
use crate::ui::state::{UiTemplateArgs, detect_ui_state};
use crate::ui::template::{best_template_hit, find_template_hits};
use anyhow::{Context, Result, anyhow, bail};
use enigo::Key;
use image::DynamicImage;

const IDLE_EXIT_MIN_MINUTES: u32 = 15;
const TARGET_MISSING_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const TARGET_MISSING_BACKOFF_MAX: Duration = Duration::from_secs(60);
const UI_RUNTIME_QUEUE_CAPACITY: usize = 32;
const OCR_RUNTIME_QUEUE_CAPACITY: usize = 64;
const BUSINESS_RUNTIME_QUEUE_CAPACITY: usize = 64;
const DEADLINE_RUNTIME_QUEUE_CAPACITY: usize = 64;

fn receive_observation_frame(
    subscription: &FrameDemandSubscription,
    ui: &UiRuntimeHandle,
    canvas: &Canvas,
) -> Result<Frame> {
    match subscription.recv().context("等待 UI runtime 发布观察帧")? {
        FramePublication::Captured(published) => {
            let frame = ui
                .latest_frame()
                .filter(|latest| latest.captured_at() >= published.captured_at())
                .unwrap_or(published);
            Ok(from_captured_frame(&frame, canvas))
        }
        FramePublication::Failed(failure) => {
            if let Some(latest) = ui
                .latest_frame()
                .filter(|latest| latest.captured_at() >= failure.failed_at())
            {
                return Ok(from_captured_frame(&latest, canvas));
            }
            Err(anyhow!(
                "UI runtime 观察帧截图失败 at {:?}: {}",
                failure.failed_at(),
                failure.reason()
            ))
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SecondaryBubbleProcessOutcome {
    processed: bool,
    ocr_pending: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueuePushOutcome {
    Added(usize),
    Full,
    DedupLimited,
}

#[derive(Clone, Copy)]
struct QueuePushFeedback {
    queued_action: &'static str,
    full_action: &'static str,
    queued_prefix: &'static str,
    full_reply: &'static str,
}

const QUEUE_PUSH_FEEDBACK: QueuePushFeedback = QueuePushFeedback {
    queued_action: "queue",
    full_action: "queue-full",
    queued_prefix: "队列已加入",
    full_reply: "队列已满，请稍后再试",
};

const UNKNOWN_STATUS_QUEUE_PUSH_FEEDBACK: QueuePushFeedback = QueuePushFeedback {
    queued_action: "queue-status-unknown",
    full_action: "queue-full-status-unknown",
    queued_prefix: "状态未知，队列已加入",
    full_reply: "状态未知且队列已满，请稍后再试",
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserDecision {
    Confirm,
    Skip,
    SwitchSource,
    Ai,
    Timeout,
    Stopped,
}

#[derive(Debug, PartialEq, Eq)]
enum PlayerSearchResolution<T> {
    Found(T),
    NoSource,
    Failed(PlayerSearchClientError),
}

fn classify_player_search<T>(
    result: std::result::Result<Option<T>, PlayerSearchClientError>,
) -> PlayerSearchResolution<T> {
    match result {
        Ok(Some(value)) => PlayerSearchResolution::Found(value),
        Ok(None) => PlayerSearchResolution::NoSource,
        Err(error) => PlayerSearchResolution::Failed(error),
    }
}

fn player_search_failure_reply(error: &PlayerSearchClientError) -> &'static str {
    match error {
        PlayerSearchClientError::QueueFull => "歌曲搜索繁忙，请稍后再试",
        PlayerSearchClientError::RuntimeStopped
        | PlayerSearchClientError::OperationIdExhausted
        | PlayerSearchClientError::NotRun { .. } => "歌曲搜索服务暂不可用，请稍后再试",
        PlayerSearchClientError::Failed(_) => "歌曲搜索后端失败，请稍后再试",
        PlayerSearchClientError::UnexpectedOutcome(_) => "歌曲搜索后端返回异常，请稍后再试",
    }
}

pub(crate) struct ApplicationRuntime {
    config: AppConfig,
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
    player: PlayerController<PlayerRuntimeBackend>,
    player_search: PlayerSearchClient,
    player_runtime: Option<PlayerRuntime>,
    openai_runtime: Option<OpenAiRuntime>,
    ai: AiClient,
    song_review: SongReviewClient,
    chat_output: ChatOutput,
    ocr: OcrRuntimeHandle,
    ocr_runtime: Option<OcrRuntime>,
    latest_frame: Arc<Mutex<Option<Arc<DynamicImage>>>>,
    locks: CommandLockState,
    window_detection_signal: WindowDetectionSignal,
    screen_lock_primed: Arc<AtomicBool>,
    reset_locks_requested: Arc<AtomicBool>,
    moderation: ModerationService,
    startup: StartupService,
    custom_workflow: CustomWorkflowService,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    console_reply_context: Arc<AtomicBool>,
    chat_observations: ChatObservationShared,
    monitor: MonitorShared,
}

struct DeferredCardGamePort<'a> {
    app: &'a ApplicationRuntime,
}

pub(crate) struct QueuedCardGameEffect {
    business: BusinessRuntimeHandle,
    action: &'static str,
    request: CardGameEffectRequest,
}

impl QueuedCardGameEffect {
    pub(crate) fn new(
        business: BusinessRuntimeHandle,
        action: &'static str,
        request: CardGameEffectRequest,
    ) -> Self {
        Self {
            business,
            action,
            request,
        }
    }

    fn label(&self) -> String {
        format!("发送牌局计时结果({})", self.action)
    }

    fn execute(self, port: &dyn CardGameDeliveryPort) -> Result<()> {
        drive_card_game_effect_chain(
            &self.business,
            self.request,
            CardGameEffectLane::Formal,
            CardGameLatePolicy::Ignore,
            port,
        )
    }

    fn cancel(&self) -> Result<()> {
        let _ = self.business.cancel_card_game_effect(self.request.key)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum CardGameLatePolicy {
    Error,
    Ignore,
}

fn drive_card_game_start(
    business: &BusinessRuntimeHandle,
    start: CardGameCommandStart,
    expected_lane: CardGameEffectLane,
    port: &dyn CardGameDeliveryPort,
) -> Result<()> {
    match start {
        CardGameCommandStart::Completed(_) => Ok(()),
        CardGameCommandStart::Suspended(request) => drive_card_game_effect_chain(
            business,
            request,
            expected_lane,
            CardGameLatePolicy::Error,
            port,
        ),
    }
}

fn drive_card_game_effect_chain(
    business: &BusinessRuntimeHandle,
    mut request: CardGameEffectRequest,
    expected_lane: CardGameEffectLane,
    late_policy: CardGameLatePolicy,
    port: &dyn CardGameDeliveryPort,
) -> Result<()> {
    loop {
        if request.lane != expected_lane {
            let _ = business.cancel_card_game_effect(request.key);
            bail!(
                "牌局效果通道不一致: expected={expected_lane:?} actual={:?}",
                request.lane
            );
        }
        match business.claim_card_game_effect(request.key)? {
            CardGameEffectClaim::Claimed => {}
            CardGameEffectClaim::Late(_) => return handle_late_card_game_effect(late_policy),
        }
        let key = request.key;
        let result = match request.effect {
            CardGameEffect::FriendVerify { player, message } => {
                CardGameEffectResult::FriendVerify(run_card_game_delivery("好友验证", || {
                    port.verify_friend(&player, &message)
                }))
            }
            CardGameEffect::PrivateDelivery { player, message } => {
                CardGameEffectResult::PrivateDelivery(run_card_game_delivery(
                    "好友消息发送",
                    || port.send_friend(&player, &message),
                ))
            }
            CardGameEffect::PrivateBatch { deliveries } => CardGameEffectResult::PrivateBatch(
                run_card_game_delivery("好友批量消息发送", || {
                    port.send_friend_batch(&deliveries)
                }),
            ),
            CardGameEffect::HallDelivery { message } => CardGameEffectResult::HallDelivery(
                run_card_game_delivery("大厅消息发送", || port.send_hall(&message)),
            ),
        };
        match business.resume_card_game(key, result)? {
            CardGameResume::Completed(_) => return Ok(()),
            CardGameResume::Suspended(next) => request = next,
            CardGameResume::Late(_) => return handle_late_card_game_effect(late_policy),
        }
    }
}

fn run_card_game_delivery<T>(label: &str, delivery: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(delivery)) {
        Ok(result) => result,
        Err(_) => Err(anyhow!("牌局{label}发生未捕获异常")),
    }
}

fn handle_late_card_game_effect(policy: CardGameLatePolicy) -> Result<()> {
    match policy {
        CardGameLatePolicy::Ignore => Ok(()),
        CardGameLatePolicy::Error => bail!("牌局命令在效果链完成前已失效"),
    }
}

pub(crate) struct QueuedUndercoverEffect {
    business: BusinessRuntimeHandle,
    action: &'static str,
    request: UndercoverEffectRequest,
}

impl QueuedUndercoverEffect {
    fn new(
        business: BusinessRuntimeHandle,
        action: &'static str,
        request: UndercoverEffectRequest,
    ) -> Self {
        Self {
            business,
            action,
            request,
        }
    }

    fn label(&self) -> String {
        format!("发送谁是卧底效果({})", self.action)
    }

    fn execute(self, port: &dyn UndercoverDeliveryPort) -> Result<()> {
        drive_undercover_effect_chain(
            &self.business,
            self.request,
            UndercoverEffectLane::Deferred,
            UndercoverLatePolicy::Ignore,
            port,
        )
    }

    fn cancel(&self) -> Result<()> {
        Ok(self.business.cancel_undercover_effect(self.request.key)?)
    }
}

#[derive(Clone, Copy)]
enum UndercoverLatePolicy {
    Error,
    Ignore,
}

fn drive_undercover_start(
    business: &BusinessRuntimeHandle,
    start: UndercoverCommandStart,
    port: &dyn UndercoverDeliveryPort,
) -> Result<()> {
    match start {
        UndercoverCommandStart::Completed(_) => Ok(()),
        UndercoverCommandStart::Suspended(request) => drive_undercover_effect_chain(
            business,
            request,
            UndercoverEffectLane::Formal,
            UndercoverLatePolicy::Error,
            port,
        ),
    }
}

fn drive_undercover_effect_chain(
    business: &BusinessRuntimeHandle,
    mut request: UndercoverEffectRequest,
    expected_lane: UndercoverEffectLane,
    late_policy: UndercoverLatePolicy,
    port: &dyn UndercoverDeliveryPort,
) -> Result<()> {
    loop {
        if request.lane != expected_lane {
            let _ = business.cancel_undercover_effect(request.key);
            bail!(
                "谁是卧底效果通道不一致: expected={expected_lane:?} actual={:?}",
                request.lane
            );
        }
        match business.claim_undercover_effect(request.key)? {
            UndercoverEffectClaim::Claimed => {}
            UndercoverEffectClaim::Late(_) => return handle_late_undercover_effect(late_policy),
        }
        let key = request.key;
        let result = match request.effect {
            UndercoverEffect::FriendVerify { player, message } => {
                UndercoverEffectResult::FriendVerify(run_undercover_delivery(
                    "好友验证",
                    || port.verify_friend(&player, &message),
                ))
            }
            UndercoverEffect::FriendBatch { deliveries } => UndercoverEffectResult::FriendBatch(
                run_undercover_delivery("好友私密批次发送", || {
                    port.send_friend_batch(&deliveries)
                }),
            ),
            UndercoverEffect::Hall { message } => {
                UndercoverEffectResult::Hall(run_undercover_delivery("大厅消息发送", || {
                    port.send_hall(&message)
                }))
            }
            UndercoverEffect::HallBatch { messages } => UndercoverEffectResult::HallBatch(
                run_undercover_delivery("大厅批量消息发送", || {
                    port.send_hall_batch(&messages)
                }),
            ),
        };
        match business.resume_undercover(key, result)? {
            UndercoverResume::Completed(_) => return Ok(()),
            UndercoverResume::Suspended(next) => request = next,
            UndercoverResume::Late(_) => return handle_late_undercover_effect(late_policy),
        }
    }
}

fn run_undercover_delivery<T>(label: &str, delivery: impl FnOnce() -> Result<T>) -> Result<T> {
    match catch_unwind(AssertUnwindSafe(delivery)) {
        Ok(result) => result,
        Err(_) => Err(anyhow!("谁是卧底{label}发生未捕获异常")),
    }
}

fn handle_late_undercover_effect(policy: UndercoverLatePolicy) -> Result<()> {
    match policy {
        UndercoverLatePolicy::Ignore => Ok(()),
        UndercoverLatePolicy::Error => bail!("谁是卧底命令在效果链完成前已失效"),
    }
}

impl CardGameDeliveryPort for DeferredCardGamePort<'_> {
    fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Err(anyhow!("延迟牌类端口不能执行好友验证"))
    }

    fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Err(anyhow!("延迟牌类端口不能发送好友消息"))
    }

    fn send_friend_batch(
        &self,
        _deliveries: &[LandlordPrivateDelivery],
    ) -> Result<FriendBatchOutcome> {
        Err(anyhow!("延迟牌类端口不能发送好友批次"))
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        self.app.enqueue_current_hall_reply(message)
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

#[derive(Clone, Debug)]
struct ResolvedSongRequest {
    keyword: String,
    source: String,
    prefer_accompaniment: bool,
    ai_original_text: String,
    uri: String,
    friend_username: String,
    console_bypass_dedup: bool,
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
    CardGameEffect(QueuedCardGameEffect),
    UndercoverEffect(QueuedUndercoverEffect),
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
            Self::Command(pending) if pending.parsed.message_type == "控制台" => {
                format!("控制台命令: {}", pending.parsed.raw)
            }
            Self::Command(pending) => pending.parsed.raw.clone(),
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
            | Self::CardGameEffect(_)
            | Self::UndercoverEffect(_) => None,
        }
    }

    fn is_playback_task(&self) -> bool {
        match self {
            Self::AdvanceQueue { .. } => true,
            Self::Command(pending) => matches!(
                &pending.parsed.command,
                BusinessIntent::SongRequest(_)
                    | BusinessIntent::Playback(
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
    DecisionObservation(&'static str),
    CustomWorkflowStep,
}

impl ResidencyPurpose {
    const fn label(self) -> &'static str {
        match self {
            Self::ListenerModeSwitch => "切换聊天监听模式",
            Self::IndependentRecovery(context) | Self::DecisionObservation(context) => context,
            Self::CustomWorkflowStep => "自定义流程显式驻留步骤",
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

fn idiom_command_requires_executor(command: &idiom_chain::IdiomChainCommand) -> bool {
    matches!(command, idiom_chain::IdiomChainCommand::Explain(_))
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
    pub(crate) fn new(
        config: AppConfig,
        runtime_state: PersistentRuntimeState,
        queue: PersistentQueue,
        song_dedup_history: PersistentSongDedupHistory,
        monitor: MonitorShared,
    ) -> Result<Self> {
        let player_runtime_config = config
            .player_runtime_config()
            .context("校验播放器运行时配置")?;
        let business_runtime_builder =
            BusinessRuntimeGroupBuilder::start(DEADLINE_RUNTIME_QUEUE_CAPACITY)?;
        let ocr_args = OcrArgs::default().resolve(&config);
        let ocr_runtime = OcrRuntime::start(
            ProductionOcrDevice::new(ocr_args)?,
            OCR_RUNTIME_QUEUE_CAPACITY,
        )?;
        let ocr = ocr_runtime.handle();
        let feeluown = FeelUOwnClient::new(&config.feeluown, &config.timing);
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
        let running = Arc::new(AtomicBool::new(true));
        let ui_runtime = UiRuntime::start_with_progress(
            WindowsUiDevice::new(config.window.clone()),
            UI_RUNTIME_QUEUE_CAPACITY,
            Arc::new(monitor.clone()),
        )?;
        let ui_handle = ui_runtime.handle();
        let game_ui = GameUi::runtime(ui_handle.clone());
        let residency_ui = ResidencyUi::new(ui_handle.clone(), ocr.clone(), &config);
        let hall_ui = HallUi::new(ui_handle.clone(), ocr.clone(), &config);
        let moderation_ui = ModerationUi::new(ui_handle.clone(), &config);
        let startup_ui = StartupUi::new(ui_handle.clone(), ocr.clone(), &config);
        let secondary_unread_ui = SecondaryUnreadUi::new(ui_handle.clone(), ocr.clone(), &config);
        let friend_delivery_ui = FriendDeliveryUi::new(ui_handle.clone(), ocr.clone(), &config);
        let hall_batch_ui = HallBatchUi::new(ui_handle.clone(), ocr.clone(), &config);
        let invite_ui = InviteUi::new(ui_handle.clone(), ocr.clone(), &config);
        let custom_action_ui =
            CustomActionUi::new(ui_handle, ocr.clone(), running.clone(), &config);
        let openai_runtime = OpenAiRuntime::start().context("启动 OpenAI runtime")?;
        let openai = openai_runtime.handle();
        let ai = AiClient::new(&config.ai, &config.timing, openai.clone());
        let song_review =
            SongReviewClient::new(&config.song_review, &config.timing, openai.clone());
        let chat_output = ChatOutput::new(&config.output, hall_batch_ui);
        let idiom_chain = IdiomChainService::load(config.idiom_chain.clone())?;
        if config.idiom_chain.enabled {
            log::info!("已加载成语接龙词库: {} 条", idiom_chain.lexicon_len());
        }
        let landlord = CardGameService::new(config.landlord.clone());
        let undercover = UndercoverRuntimeService::new(config.undercover.clone());
        let mut turtle_soup_config = config.turtle_soup.clone();
        turtle_soup_config.nickname_stable_count =
            config.resolve_stability_count_usize(turtle_soup_config.nickname_stable_count);
        turtle_soup_config.content_stable_count =
            config.resolve_stability_count_usize(turtle_soup_config.content_stable_count);
        let turtle_soup = TurtleSoupService::new(turtle_soup_config, openai);
        let chat_observations = ChatObservationShared::new(
            config.ocr.change_mean_threshold,
            config.ocr.change_pixel_threshold,
        );
        let business_timer = business_runtime_builder.handle();
        let playback = PlaybackService::new(
            queue,
            runtime_state,
            song_dedup_history,
            config.matching.clone(),
            config.song_dedup.clone(),
        );
        let business_runtime = business_runtime_builder.build_with(|| {
            BusinessRuntime::start_with_timer_and_modules_and_state_sink(
                BUSINESS_RUNTIME_QUEUE_CAPACITY,
                BusinessRuntimeWorker::from_parts(
                    idiom_chain,
                    landlord,
                    undercover,
                    turtle_soup,
                    playback,
                    InviteService::new(),
                    business_timer,
                    Arc::new(monitor.clone()),
                ),
            )
        })?;
        let business = business_runtime.business_handle();
        let business_events = business_runtime.event_sink();
        let player = PlayerController::new(
            PlayerRuntimeBackend::new(player_runtime_handle),
            business.clone(),
            &config.timing.playback,
            &config.queue,
            &config.matching,
        );
        let moderation = ModerationService::new(
            ModerationPolicy::new(
                Duration::from_millis(config.timing.moderation.vote_timeout_ms),
                Duration::from_millis(config.timing.moderation.vote_poll_ms),
                config.moderation.stable_vote_samples,
                config.moderation.required_vote_margin,
            ),
            Arc::new(business.clone()),
        );
        let custom_workflow = service_from_config_parts(
            &config.custom_workflows,
            &config.timing.workflow,
            &config.timing.decision,
            &config.timing.input,
            &config.ocr,
        );
        Ok(Self {
            config,
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
            player_search,
            player_runtime: Some(player_runtime),
            openai_runtime: Some(openai_runtime),
            ai,
            song_review,
            chat_output,
            ocr,
            ocr_runtime: Some(ocr_runtime),
            latest_frame: Arc::new(Mutex::new(None)),
            locks: CommandLockState::default(),
            window_detection_signal: WindowDetectionSignal::new(),
            screen_lock_primed: Arc::new(AtomicBool::new(false)),
            reset_locks_requested: Arc::new(AtomicBool::new(false)),
            moderation,
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

fn ai_candidate_source(song: &SongCommand) -> &'static str {
    if song.friend_username.trim().is_empty() {
        "qqmusic,netease"
    } else {
        song.source.as_str()
    }
}

fn song_label(song: &SongCommand) -> String {
    source_label(&song.friend_username)
}

fn request_label(request: &ResolvedSongRequest) -> String {
    source_label(&request.friend_username)
}

fn source_label(username: &str) -> String {
    let username = username.trim();
    if username.is_empty() {
        String::new()
    } else {
        format!("好友{}:", username)
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

fn command_username(parsed: &ParsedCommand) -> &str {
    match &parsed.command {
        BusinessIntent::SongRequest(song) if !song.friend_username.trim().is_empty() => {
            &song.friend_username
        }
        _ => &parsed.username,
    }
}

fn is_private_undercover_input(parsed: &ParsedCommand) -> bool {
    matches!(
        &parsed.command,
        BusinessIntent::Undercover(UndercoverCommand::Vote(_))
    )
}

fn private_safe_command_log(parsed: &ParsedCommand) -> &str {
    match &parsed.command {
        BusinessIntent::Undercover(UndercoverCommand::Vote(_)) => "谁是卧底投票",
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

fn song_review_level_text(level: Option<u8>) -> String {
    level
        .map(|level| level.to_string())
        .unwrap_or_else(|| "无".to_string())
}

fn normalized_review_reason(reason: &str) -> String {
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if reason.trim().is_empty() {
        "审核服务未给出原因".to_string()
    } else {
        reason
    }
}

fn review_reject_reply(reason: &str, max_chars: usize) -> String {
    let reason = normalized_review_reason(reason);
    let max_chars = max_chars.max(1);
    let shortened = if reason.chars().count() > max_chars {
        format!("{}...", reason.chars().take(max_chars).collect::<String>())
    } else {
        reason
    };
    format!("点歌未通过审核: {shortened}")
}

fn final_song_command_text(request: &ResolvedSongRequest, action: &str) -> String {
    let source = if request.source.trim().is_empty() {
        "all"
    } else {
        request.source.trim()
    };
    format!(
        "{} keyword={} source={} uri={} aiOriginal={}",
        action, request.keyword, source, request.uri, request.ai_original_text,
    )
}

fn parse_decision_command(text: &str) -> Option<UserDecision> {
    let raw = text.trim();
    let command_text = if let Some(index) = raw.find(['：', ':', ']', '】']) {
        let sep_len = raw[index..].chars().next().map(char::len_utf8).unwrap_or(1);
        &raw[index + sep_len..]
    } else {
        raw
    }
    .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    if command::strip_ascii_case_prefix(command_text, "@确认")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(UserDecision::Confirm)
    } else if command::strip_ascii_case_prefix(command_text, "@跳过")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(UserDecision::Skip)
    } else if command::strip_ascii_case_prefix(command_text, "@换源")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(UserDecision::SwitchSource)
    } else if command::strip_ascii_case_prefix(command_text, "@AI")
        .is_some_and(|rest| decision_boundary(rest.chars().next()))
    {
        Some(UserDecision::Ai)
    } else {
        None
    }
}

fn is_decision_feedback_text(text: &str) -> bool {
    [
        "匹配失败",
        "AI自动匹配",
        "换源结果",
        "换源到",
        "换源后仍无音源",
        "下次可以尝试",
        "如非预期",
        "命令已超时",
        "搜索到:",
        "AI匹配:",
        "AI匹配中",
        "AI点歌未启用",
        "AI点歌识别失败",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

fn decision_boundary(ch: Option<char>) -> bool {
    match ch {
        None => true,
        Some(ch) => {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '，' | ',' | '。' | '.' | '!' | '！' | '?' | '？' | ']' | '】'
                )
        }
    }
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

fn secondary_new_bubble_start(
    previous: &[SecondaryHallBubble],
    current: &[SecondaryHallBubble],
) -> Option<usize> {
    if previous.is_empty() {
        return Some(0);
    }
    let overlap = hall_bubble_sequence_overlap(previous, current);
    (overlap > 0).then_some(overlap)
}

fn secondary_message_sender_rect(image: &DynamicImage, bubble: Rect) -> Rect {
    let left = (bubble.x - 20).max(0);
    let top = (bubble.y - 48).max(0);
    let right = (bubble.right() + 20).min(image.width() as i32);
    let bottom = bubble.y.min(image.height() as i32);
    Rect::new(
        left,
        top,
        (right - left).max(1) as u32,
        (bottom - top).max(1) as u32,
    )
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
    fn player_search_queue_full_aborts_without_no_source_follow_up() {
        let resolution: PlayerSearchResolution<()> =
            classify_player_search(Err(PlayerSearchClientError::QueueFull));

        assert!(matches!(
            resolution,
            PlayerSearchResolution::Failed(PlayerSearchClientError::QueueFull)
        ));
        let reply = player_search_failure_reply(&PlayerSearchClientError::QueueFull);
        assert_eq!(reply, "歌曲搜索繁忙，请稍后再试");
        assert!(!reply.contains("无音源"));
        assert!(!reply.contains("换源"));
        assert!(!reply.contains("AI"));
    }

    #[test]
    fn player_search_only_classifies_successful_empty_results_as_no_source() {
        assert_eq!(
            classify_player_search::<()>(Ok(None)),
            PlayerSearchResolution::NoSource
        );
        assert_eq!(
            classify_player_search(Ok(Some("candidate"))),
            PlayerSearchResolution::Found("candidate")
        );
        let empty_candidates = Ok::<_, PlayerSearchClientError>(Vec::<u8>::new())
            .map(|candidates| (!candidates.is_empty()).then_some(candidates));
        assert_eq!(
            classify_player_search(empty_candidates),
            PlayerSearchResolution::NoSource
        );
    }

    #[test]
    fn player_search_failures_have_explicit_user_facing_categories() {
        use crate::runtime::player_io::PlayerSearchError;

        let cases = [
            (
                PlayerSearchClientError::QueueFull,
                "歌曲搜索繁忙，请稍后再试",
            ),
            (
                PlayerSearchClientError::RuntimeStopped,
                "歌曲搜索服务暂不可用，请稍后再试",
            ),
            (
                PlayerSearchClientError::OperationIdExhausted,
                "歌曲搜索服务暂不可用，请稍后再试",
            ),
            (
                PlayerSearchClientError::NotRun {
                    reason: "shutdown".to_string(),
                },
                "歌曲搜索服务暂不可用，请稍后再试",
            ),
            (
                PlayerSearchClientError::Failed(PlayerSearchError::new("backend failed")),
                "歌曲搜索后端失败，请稍后再试",
            ),
            (
                PlayerSearchClientError::UnexpectedOutcome("pick"),
                "歌曲搜索后端返回异常，请稍后再试",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(player_search_failure_reply(&error), expected);
            assert!(matches!(
                classify_player_search::<()>(Err(error)),
                PlayerSearchResolution::Failed(_)
            ));
        }
    }

    #[test]
    fn parses_ai_decision_case_insensitive() {
        assert_eq!(parse_decision_command("用户：@ai"), Some(UserDecision::Ai));
    }

    #[test]
    fn secondary_listener_resides_in_current_hall() {
        assert_eq!(
            listener_residency(ChatListenerMode::Secondary, false),
            UiResidency::SecondaryCurrentHall
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
    fn idiom_explanation_uses_the_exclusive_command_executor() {
        assert!(idiom_command_requires_executor(
            &idiom_chain::IdiomChainCommand::Explain(Some("画蛇添足".to_string()))
        ));
        assert!(!idiom_command_requires_executor(
            &idiom_chain::IdiomChainCommand::Hint
        ));
        assert!(!idiom_command_requires_executor(
            &idiom_chain::IdiomChainCommand::Submit("足智多谋".to_string())
        ));
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

        let error = drive_card_game_start(
            &business,
            start,
            CardGameEffectLane::Formal,
            &FailingCardGamePort,
        )
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
    fn formal_card_game_ui_panics_are_resumed_before_being_reported() {
        let runtime = card_game_runtime_for_test();
        let business = runtime.handle();
        let start = business
            .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
            .unwrap();

        let error = drive_card_game_start(
            &business,
            start,
            CardGameEffectLane::Formal,
            &PanickingCardGamePort,
        )
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

        let formal_error = drive_card_game_start(
            &business,
            CardGameCommandStart::Suspended(request.clone()),
            CardGameEffectLane::Formal,
            &NeverCalledCardGamePort,
        )
        .unwrap_err();
        assert!(formal_error.to_string().contains("已失效"));

        QueuedCardGameEffect::new(business, "test-timeout", request)
            .execute(&NeverCalledCardGamePort)
            .unwrap();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn secondary_decision_reader_accepts_first_bubble_after_empty_baseline() {
        let mut image = image::RgbaImage::new(1920, 1080);
        for y in 300..354 {
            for x in 415..700 {
                image.put_pixel(x, y, image::Rgba([62, 71, 89, 255]));
            }
        }
        let current =
            secondary_hall_bubbles(&DynamicImage::ImageRgba8(image)).expect("secondary bubbles");

        assert!(!current.is_empty());
        assert_eq!(secondary_new_bubble_start(&[], &current), Some(0));
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
