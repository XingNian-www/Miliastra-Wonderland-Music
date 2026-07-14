mod ai;
mod change_detection;
mod chat_listener;
mod chat_observation;
pub(crate) mod chat_output;
mod chat_scan;
mod clipboard;
mod command;
mod custom_workflow;
mod decision_control;
mod decision_lock;
mod deferred_chat;
mod dpi;
mod feeluown;
mod frame_source;
mod game_startup;
mod game_ui;
mod geometry;
mod hall_info;
mod hotkeys;
mod http_server;
mod input_actions;
pub(crate) mod logger;
pub(crate) mod monitor;
mod ocr;
mod ocr_batch;
mod ocr_runtime;
mod playback_format;
mod player_controller;
pub(crate) mod queue;
pub(crate) mod runtime_state;
pub(crate) mod song_dedup;
mod song_matcher;
mod song_review;
mod startup_flow;
mod task_tracker;
mod template_match;
pub(crate) mod tui;
mod ui_locator;
mod ui_state;
mod web_tools;
mod window;
mod workflow_actions;

use std::collections::{HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, sleep};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use self::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
use self::chat_listener::{
    ChatListenerMode, ChatListenerShared, SecondaryChatIdentity, SecondaryHallBubble,
    UnreadFriendHit, classify_title, find_unread_friend_hits, hall_bubble_sequence_overlap,
    latest_incoming_bubble_rect, latest_incoming_fingerprint, secondary_hall_bubbles,
    unread_hit_still_visible,
};
use self::chat_observation::{
    ChatObservationDispatch, ChatObservationExclusiveGuard, ChatObservationShared,
    PrimaryObservedMessage, SecondaryChatObservation, SecondaryObservedMessage,
    SecondaryRecognizedMessage,
};
use self::chat_output::{
    ChatBatchSendOutcome, ChatBatchSendStatus, ChatOutput, redacted_chat_text,
};
use self::chat_scan::{ChatMessage, prepare_chat_scan, recognize_prepared_chat};
use self::command::{
    ChatListenerModeCommand, CommandLockState, ParsedCommand, PendingCommand, UserCommand,
};
use self::decision_control::{DecisionAction, DecisionControlShared};
use self::decision_lock::DecisionScreenLock;
use self::deferred_chat::{
    BatchFailureOutcome, DEFAULT_CAPACITY as DEFERRED_CHAT_CAPACITY, DeferredChatItem,
    DeferredChatMessage, DeferredChatQueue, DeferredChatTarget, EnqueueOutcome,
};
use self::feeluown::{FeelUOwnClient, PlayerStatus, format_lyrics, format_status};
use self::frame_source::{Canvas, from_captured_frame, load_frame};
use self::game_ui::{GameUi, WindowsUiDevice};
use self::geometry::{Rect, crop_canvas};
use self::hall_info::{
    HALL_INFO_OCR_SAMPLES, HallInfo, HallInfoSample, display_or_empty,
    format_hall_remaining_suffix, merge_hall_info_samples, parse_hall_remaining_minutes,
};
use self::input_actions::parse_key;
use self::monitor::{MonitorQueueItem, MonitorShared, OcrSnapshot};
use self::ocr::{OcrArgs, OcrBackendProbeStatus, probe_ocr_backend_support};
use self::ocr_runtime::{OcrPriority, OcrRuntime, OcrRuntimeHandle, ProductionOcrDevice};
use self::playback_format::{
    PlaybackSnapshot, estimated_player_status, format_play_message, is_playing, song_title,
};
use self::player_controller::{
    MismatchDecision, PlaybackAttempt, PlaybackOutcome, PlaybackRequest, PlaybackVerification,
    PlayerController, QueueAdvanceContext, QueueAdvanceDecision,
};
use self::queue::PersistentQueue;
use self::runtime_state::{HALL_EXPIRING_WARNING_MINUTES, PersistentRuntimeState};
use self::song_dedup::PersistentSongDedupHistory;
use self::song_review::{SongReviewCandidate, SongReviewClient};
use self::task_tracker::TaskTrackerShared;
use self::template_match::{best_template_hit, find_template_hits};
use self::ui_locator::UiLocator;
use self::ui_state::{UiState, detect_ui_state};
use self::web_tools::{WebToolRequest, WebToolShared, WebToolTask, WebToolTemplate};
use crate::config::{AppConfig, PointConfig};
use crate::features::card_games::{
    CardGameDeliveryPort, CardGameService, LandlordCommand, LandlordOutcome,
};
use crate::features::chat_text::split_numbered_chat_message;
use crate::features::entertainment::EntertainmentCoordinator;
#[cfg(test)]
use crate::features::entertainment::{AcquireOutcome, EntertainmentKind};
use crate::features::idiom_chain;
use crate::features::idiom_chain::IdiomChainService;
use crate::features::turtle_soup::{
    self, QuestionSubmitOutcome, SecondaryOcrObservation, SecondaryOcrStability, TurtleSoupService,
};
#[cfg(test)]
use crate::features::undercover;
use crate::features::undercover::{
    UndercoverCommand, UndercoverCommandContext, UndercoverCommandSource, UndercoverDelivery,
    UndercoverDeliveryPort, UndercoverService,
};
use crate::observation::chat::ObservedFrame;
use crate::runtime::ui::{
    FrameDemand, FrameDemandSubscription, FramePublication, UiRuntime, UiRuntimeHandle,
};
use anyhow::{Context, Result, anyhow};
use enigo::Key;
use image::DynamicImage;

const IDLE_EXIT_MIN_MINUTES: u32 = 15;
const TARGET_MISSING_BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const TARGET_MISSING_BACKOFF_MAX: Duration = Duration::from_secs(60);
const UI_RUNTIME_QUEUE_CAPACITY: usize = 32;
const OCR_RUNTIME_QUEUE_CAPACITY: usize = 64;

fn receive_observation_frame(
    subscription: &FrameDemandSubscription,
    ui: &UiRuntimeHandle,
    canvas: &Canvas,
) -> Result<frame_source::Frame> {
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

fn secondary_hall_search_rect(anchor: Rect, friend_list: Rect) -> Rect {
    let left = anchor.x.min(friend_list.x);
    let top = anchor.y.min(friend_list.y);
    let right = anchor.right().max(friend_list.right());
    let bottom = anchor.bottom().max(friend_list.bottom());
    Rect::new(left, top, (right - left) as u32, (bottom - top) as u32)
}
const RETURN_TO_PRIMARY_SLOW_RETRY_AFTER: u32 = 5;
const RETURN_TO_PRIMARY_SLOW_RETRY_MS: u64 = 2_000;
const PRIMARY_REGION_STABILITY_POLL_MS: u64 = 100;
const PRIMARY_REGION_STABILITY_TIMEOUT_MS: u64 = 1_000;

#[derive(Clone, Debug, Default)]
struct FrameArgs {
    image: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default)]
struct SecondaryBubbleProcessOutcome {
    processed: bool,
    ocr_pending: bool,
}

#[derive(Clone, Debug, Default)]
struct TemplateArgs {
    blue_template: Option<PathBuf>,
    yellow_template: Option<PathBuf>,
    pink_template: Option<PathBuf>,
    marker_threshold: Option<f32>,
}

#[derive(Clone, Debug)]
pub(super) struct ResolvedTemplateArgs {
    blue_template: PathBuf,
    yellow_template: PathBuf,
    pink_template: PathBuf,
    marker_threshold: f32,
    marker_dedupe_x: i32,
    marker_dedupe_y: i32,
    text_left_gap: i32,
    block_top_padding: i32,
    block_bottom_padding: i32,
    max_block_height: i32,
    next_marker_min_gap: i32,
    right_padding: i32,
    same_line_y_tolerance: i32,
    batch_recognize: bool,
}

impl TemplateArgs {
    fn resolve(&self, config: &AppConfig) -> ResolvedTemplateArgs {
        ResolvedTemplateArgs {
            blue_template: self
                .blue_template
                .clone()
                .unwrap_or_else(|| config.templates.blue_marker.clone()),
            yellow_template: self
                .yellow_template
                .clone()
                .unwrap_or_else(|| config.templates.yellow_marker.clone()),
            pink_template: self
                .pink_template
                .clone()
                .unwrap_or_else(|| config.templates.pink_marker.clone()),
            marker_threshold: self
                .marker_threshold
                .unwrap_or(config.templates.marker_threshold),
            marker_dedupe_x: config.ocr.marker_dedupe_x,
            marker_dedupe_y: config.ocr.marker_dedupe_y,
            text_left_gap: config.ocr.text_left_gap,
            block_top_padding: config.ocr.block_top_padding,
            block_bottom_padding: config.ocr.block_bottom_padding,
            max_block_height: config.ocr.max_block_height,
            next_marker_min_gap: config.ocr.next_marker_min_gap,
            right_padding: config.ocr.right_padding,
            same_line_y_tolerance: config.ocr.same_line_y_tolerance,
            batch_recognize: config.ocr.batch_recognize,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct UiTemplateArgs {
    friend_template: Option<PathBuf>,
    secondary_back_template: Option<PathBuf>,
    secondary_hall_template: Option<PathBuf>,
    chat_templates: TemplateArgs,
}

#[derive(Clone, Debug)]
struct ResolvedUiTemplateArgs {
    friend_template: PathBuf,
    secondary_back_template: PathBuf,
    secondary_hall_template: PathBuf,
    chat_templates: ResolvedTemplateArgs,
}

impl UiTemplateArgs {
    fn resolve(&self, config: &AppConfig) -> ResolvedUiTemplateArgs {
        ResolvedUiTemplateArgs {
            friend_template: self
                .friend_template
                .clone()
                .unwrap_or_else(|| config.templates.friend.clone()),
            secondary_back_template: self
                .secondary_back_template
                .clone()
                .unwrap_or_else(|| config.templates.secondary_back.clone()),
            secondary_hall_template: self
                .secondary_hall_template
                .clone()
                .unwrap_or_else(|| config.templates.secondary_hall.clone()),
            chat_templates: self.chat_templates.resolve(config),
        }
    }
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
    PromptFailed,
}

pub fn run() -> Result<()> {
    dpi::set_process_dpi_awareness();
    run_automation_with_watchdog(Path::new("config.yaml"))
}

fn run_automation_with_watchdog(config_path: &Path) -> Result<()> {
    if std::env::var_os("MILIASTRA_WATCHDOG_CHILD").is_some() {
        return crate::composition::run(config_path);
    }

    loop {
        let current_exe = std::env::current_exe().context("locate current executable")?;
        let mut child = ProcessCommand::new(&current_exe)
            .env("MILIASTRA_WATCHDOG_CHILD", "1")
            .spawn()
            .with_context(|| format!("启动监听子进程失败: {}", current_exe.display()))?;
        let status = child.wait().context("等待监听子进程退出")?;
        if status.success() {
            return Ok(());
        }

        let config = AppConfig::load_or_create(config_path)?;
        eprintln!(
            "监听子进程异常退出: status={}，{}ms 后重启",
            status, config.timing.watchdog_restart_ms
        );
        sleep(Duration::from_millis(config.timing.watchdog_restart_ms));
    }
}

pub(crate) struct AutomationApp {
    config: AppConfig,
    game_ui: GameUi,
    ui_runtime: Option<UiRuntime>,
    runtime_state: Arc<Mutex<PersistentRuntimeState>>,
    entertainment: EntertainmentCoordinator,
    idiom_chain: IdiomChainService,
    landlord: CardGameService,
    undercover: UndercoverService,
    turtle_soup: TurtleSoupService,
    deferred_chat: DeferredChatQueue,
    queue: Arc<Mutex<PersistentQueue>>,
    song_dedup_history: Arc<Mutex<PersistentSongDedupHistory>>,
    player: PlayerController<FeelUOwnClient>,
    ai: ai::AiClient,
    song_review: SongReviewClient,
    chat_output: ChatOutput,
    ocr: OcrRuntimeHandle,
    ocr_runtime: Option<OcrRuntime>,
    latest_frame: Arc<Mutex<Option<Arc<DynamicImage>>>>,
    locks: CommandLockState,
    pending: Arc<(Mutex<VecDeque<TrackedPendingTask>>, Condvar)>,
    task_tracker: TaskTrackerShared,
    decision_control: DecisionControlShared,
    web_tools: WebToolShared,
    window_detection_signal: WindowDetectionSignal,
    screen_lock_primed: Arc<AtomicBool>,
    reset_locks_requested: Arc<AtomicBool>,
    invite_executed_seqs: Arc<Mutex<HashSet<u32>>>,
    moderation_workflows: Arc<Mutex<HashSet<String>>>,
    commands_enabled: Arc<AtomicBool>,
    idle_exit: Arc<Mutex<Option<IdleExitState>>>,
    running: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    command_executing: Arc<AtomicBool>,
    song_command_executing: Arc<AtomicBool>,
    console_reply_context: Arc<AtomicBool>,
    chat_listener: ChatListenerShared,
    chat_observations: ChatObservationShared,
    monitor: MonitorShared,
}

struct DeferredCardGamePort<'a> {
    app: &'a AutomationApp,
}

impl CardGameDeliveryPort for DeferredCardGamePort<'_> {
    fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Err(anyhow!("延迟牌类端口不能执行好友验证"))
    }

    fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Err(anyhow!("延迟牌类端口不能发送好友消息"))
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        self.app.enqueue_current_hall_reply(message)
    }
}

#[derive(Clone, Debug)]
struct IdleExitState {
    timeout: Duration,
    last_command_at: Instant,
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
    skip_match_check: bool,
    friend_username: String,
    console_bypass_dedup: bool,
}

impl ResolvedSongRequest {
    fn match_keyword(&self) -> &str {
        if self.ai_original_text.trim().is_empty() {
            &self.keyword
        } else {
            &self.ai_original_text
        }
    }
}

enum PendingTask {
    Command(Box<PendingCommand>),
    AdvanceQueue {
        reason: &'static str,
    },
    ConsoleChat {
        text: String,
        prefix: String,
    },
    StartGame {
        source: &'static str,
    },
    EnterWonderland {
        source: &'static str,
    },
    ClearIdleExit,
    ModerationVoteResult {
        command: Box<command::ModerationCommand>,
        approved: bool,
        workflow_key: String,
        temporary_primary_hold: TemporaryPrimaryHold,
    },
    SetChatListenerMode {
        target: ChatListenerMode,
    },
    SecondaryUnread {
        hit: UnreadFriendHit,
        discard_only: bool,
    },
    RestoreSecondaryHall,
    CardGameOutcome {
        outcome: LandlordOutcome,
    },
    UndercoverDelivery {
        deliveries: Vec<UndercoverDelivery>,
    },
}

struct TrackedPendingTask {
    id: u64,
    task: PendingTask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingTaskExecution {
    Completed,
    Requeued,
}

impl TrackedPendingTask {
    fn label(&self) -> String {
        self.task.label()
    }

    fn same_lock_command(&self, parsed: &ParsedCommand) -> bool {
        self.task.same_lock_command(parsed)
    }

    fn is_playback_task(&self) -> bool {
        self.task.is_playback_task()
    }
}

impl PendingTask {
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
            Self::StartGame { source } => format!("启动游戏({})", source),
            Self::EnterWonderland { source } => format!("进入千星({})", source),
            Self::ClearIdleExit => "取消闲置退出".to_string(),
            Self::ModerationVoteResult {
                command, approved, ..
            } => format!(
                "{} UID{} 投票{}",
                command.action.label(),
                command.uid,
                if *approved { "通过" } else { "未通过" }
            ),
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
            Self::CardGameOutcome { outcome } => {
                format!("发送牌局计时结果({})", outcome.action)
            }
            Self::UndercoverDelivery { .. } => "发送谁是卧底阶段消息".to_string(),
        }
    }

    fn same_lock_command(&self, parsed: &ParsedCommand) -> bool {
        match self {
            Self::Command(pending) => command::same_lock_command(&pending.parsed, parsed),
            Self::AdvanceQueue { .. } => false,
            Self::ConsoleChat { .. } => false,
            Self::StartGame { .. } => false,
            Self::EnterWonderland { .. } => false,
            Self::ClearIdleExit => false,
            Self::ModerationVoteResult { command, .. } => {
                matches!(
                    &parsed.command,
                    UserCommand::Moderation(parsed_command)
                        if parsed_command.action == command.action && parsed_command.uid == command.uid
                )
            }
            Self::SetChatListenerMode { .. }
            | Self::SecondaryUnread { .. }
            | Self::RestoreSecondaryHall
            | Self::CardGameOutcome { .. }
            | Self::UndercoverDelivery { .. } => false,
        }
    }

    fn is_playback_task(&self) -> bool {
        match self {
            Self::AdvanceQueue { .. } => true,
            Self::Command(pending) => matches!(
                &pending.parsed.command,
                UserCommand::Song(_)
                    | UserCommand::Pause
                    | UserCommand::Resume
                    | UserCommand::Play
                    | UserCommand::Next
                    | UserCommand::Previous
            ),
            Self::ConsoleChat { .. }
            | Self::StartGame { .. }
            | Self::EnterWonderland { .. }
            | Self::ClearIdleExit
            | Self::ModerationVoteResult { .. }
            | Self::SetChatListenerMode { .. }
            | Self::SecondaryUnread { .. }
            | Self::RestoreSecondaryHall
            | Self::CardGameOutcome { .. }
            | Self::UndercoverDelivery { .. } => false,
        }
    }

    fn restores_listener_residency_after_execution(&self) -> bool {
        match self {
            Self::SetChatListenerMode { .. }
            | Self::SecondaryUnread { .. }
            | Self::RestoreSecondaryHall
            | Self::ClearIdleExit => false,
            Self::Command(_)
            | Self::AdvanceQueue { .. }
            | Self::ConsoleChat { .. }
            | Self::StartGame { .. }
            | Self::EnterWonderland { .. }
            | Self::ModerationVoteResult { .. }
            | Self::CardGameOutcome { .. }
            | Self::UndercoverDelivery { .. } => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiResidency {
    Primary,
    SecondaryCurrentHall,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimaryReturnObservation {
    Primary,
    Secondary,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimaryReturnAction {
    Complete,
    WaitForPrimaryStability,
    WaitForTransition,
    PressEscape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrimaryRegionReadiness {
    Pending,
    Stable,
    TimedOut,
}

#[derive(Default)]
struct PrimaryRegionStability {
    started_at: Option<Instant>,
    previous: Option<ChangeFingerprint>,
}

impl PrimaryRegionStability {
    fn observe(
        &mut self,
        current: ChangeFingerprint,
        now: Instant,
        mean_threshold: f32,
        changed_ratio_threshold: f32,
    ) -> PrimaryRegionReadiness {
        let started_at = *self.started_at.get_or_insert(now);
        let stable = self.previous.as_ref().is_some_and(|previous| {
            let stats = change_stats(previous, &current);
            stats.mean_abs_diff <= mean_threshold && stats.changed_ratio <= changed_ratio_threshold
        });
        self.previous = Some(current);

        if stable {
            PrimaryRegionReadiness::Stable
        } else if now.duration_since(started_at)
            >= Duration::from_millis(PRIMARY_REGION_STABILITY_TIMEOUT_MS)
        {
            PrimaryRegionReadiness::TimedOut
        } else {
            PrimaryRegionReadiness::Pending
        }
    }

    fn reset(&mut self) {
        self.started_at = None;
        self.previous = None;
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

struct TemporaryPrimaryHold {
    listener: ChatListenerShared,
    active: bool,
}

impl TemporaryPrimaryHold {
    fn new(listener: ChatListenerShared) -> Result<Self> {
        let active = listener.snapshot().mode == ChatListenerMode::Secondary;
        if active {
            listener.begin_temporary_primary()?;
        }
        Ok(Self { listener, active })
    }

    fn release(&mut self) {
        if self.active {
            self.listener.end_temporary_primary();
            self.active = false;
        }
    }
}

impl Drop for TemporaryPrimaryHold {
    fn drop(&mut self) {
        self.release();
    }
}

struct CommandExecutingGuard {
    flag: Arc<AtomicBool>,
    pending: Arc<(Mutex<VecDeque<TrackedPendingTask>>, Condvar)>,
}

impl CommandExecutingGuard {
    fn new(
        flag: Arc<AtomicBool>,
        pending: Arc<(Mutex<VecDeque<TrackedPendingTask>>, Condvar)>,
    ) -> Self {
        flag.store(true, AtomicOrdering::SeqCst);
        Self { flag, pending }
    }
}

impl Drop for CommandExecutingGuard {
    fn drop(&mut self) {
        self.flag.store(false, AtomicOrdering::SeqCst);
        let (_, cvar) = &*self.pending;
        cvar.notify_all();
    }
}

struct SongCommandExecutingGuard {
    flag: Arc<AtomicBool>,
}

impl SongCommandExecutingGuard {
    fn new(flag: Arc<AtomicBool>) -> Self {
        flag.store(true, AtomicOrdering::SeqCst);
        Self { flag }
    }
}

impl Drop for SongCommandExecutingGuard {
    fn drop(&mut self) {
        self.flag.store(false, AtomicOrdering::SeqCst);
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

impl AutomationApp {
    pub(crate) fn new(
        config: AppConfig,
        runtime_state: PersistentRuntimeState,
        queue: PersistentQueue,
        song_dedup_history: PersistentSongDedupHistory,
        monitor: MonitorShared,
    ) -> Result<Self> {
        let ui_runtime = UiRuntime::start(
            WindowsUiDevice::new(config.window.clone()),
            UI_RUNTIME_QUEUE_CAPACITY,
        )?;
        let game_ui = GameUi::runtime(ui_runtime.handle());
        let ocr_args = OcrArgs::default().resolve(&config);
        let ocr_runtime = OcrRuntime::start(
            ProductionOcrDevice::new(ocr_args)?,
            OCR_RUNTIME_QUEUE_CAPACITY,
        )?;
        let ocr = ocr_runtime.handle();
        let feeluown = FeelUOwnClient::new(&config.feeluown, &config.timing);
        let ai = ai::AiClient::new(&config.ai, &config.timing);
        let song_review = SongReviewClient::new(&config.song_review, &config.timing);
        let chat_output = ChatOutput::new(
            &config.output,
            &config.timing,
            game_ui.clone(),
            &config.screen,
            &config.templates,
            &config.invite,
        );
        let runtime_state = Arc::new(Mutex::new(runtime_state));
        let entertainment = EntertainmentCoordinator::new();
        let idiom_chain =
            IdiomChainService::load(config.idiom_chain.clone(), entertainment.clone())?;
        if config.idiom_chain.enabled {
            log::info!("已加载成语接龙词库: {} 条", idiom_chain.lexicon_len()?);
        }
        let landlord = CardGameService::new(config.landlord.clone(), entertainment.clone());
        let undercover = UndercoverService::new(config.undercover.clone(), entertainment.clone());
        let deferred_chat = DeferredChatQueue::new(DEFERRED_CHAT_CAPACITY);
        let mut turtle_soup_config = config.turtle_soup.clone();
        turtle_soup_config.nickname_stable_count =
            config.resolve_stability_count_usize(turtle_soup_config.nickname_stable_count);
        turtle_soup_config.content_stable_count =
            config.resolve_stability_count_usize(turtle_soup_config.content_stable_count);
        let turtle_soup = TurtleSoupService::new(
            turtle_soup_config,
            entertainment.clone(),
            deferred_chat.clone(),
        );
        let queue = Arc::new(Mutex::new(queue));
        let song_dedup_history = Arc::new(Mutex::new(song_dedup_history));
        let player = PlayerController::new(
            feeluown,
            runtime_state.clone(),
            song_dedup_history.clone(),
            &config.timing.playback,
            &config.queue,
            &config.matching,
            &config.song_dedup,
        );
        let chat_observations = ChatObservationShared::new(
            config.ocr.change_mean_threshold,
            config.ocr.change_pixel_threshold,
        );
        Ok(Self {
            config,
            game_ui,
            ui_runtime: Some(ui_runtime),
            runtime_state,
            entertainment,
            idiom_chain,
            landlord,
            undercover,
            turtle_soup,
            deferred_chat,
            queue,
            song_dedup_history,
            player,
            ai,
            song_review,
            chat_output,
            ocr,
            ocr_runtime: Some(ocr_runtime),
            latest_frame: Arc::new(Mutex::new(None)),
            locks: CommandLockState::default(),
            pending: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
            task_tracker: TaskTrackerShared::new(),
            decision_control: DecisionControlShared::new(),
            web_tools: WebToolShared::new(),
            window_detection_signal: WindowDetectionSignal::new(),
            screen_lock_primed: Arc::new(AtomicBool::new(false)),
            reset_locks_requested: Arc::new(AtomicBool::new(false)),
            invite_executed_seqs: Arc::new(Mutex::new(HashSet::new())),
            moderation_workflows: Arc::new(Mutex::new(HashSet::new())),
            commands_enabled: Arc::new(AtomicBool::new(true)),
            idle_exit: Arc::new(Mutex::new(None)),
            running: Arc::new(AtomicBool::new(true)),
            paused: Arc::new(AtomicBool::new(false)),
            command_executing: Arc::new(AtomicBool::new(false)),
            song_command_executing: Arc::new(AtomicBool::new(false)),
            console_reply_context: Arc::new(AtomicBool::new(false)),
            chat_listener: ChatListenerShared::new(),
            chat_observations,
            monitor,
        })
    }

    pub(crate) fn run(&mut self) -> Result<()> {
        self.monitor.set_status("运行中");
        self.update_monitor_queue_snapshot();
        self.update_monitor_playback_controller();
        self.update_monitor_chat_listener();
        self.update_monitor_operational_state();
        self.warn_if_screen_size_mismatch()?;
        self.start_http_server()?;
        self.start_hotkeys()?;
        self.turtle_soup.start_workers();
        let executor = self.start_command_executor();
        let deferred_chat_sender = self.start_deferred_chat_sender();
        self.enqueue_startup_task_if_enabled()?;
        let web_tool_executor = self.start_web_tool_executor();
        let playback_monitor = self.start_playback_monitor();
        let result = self.run_scan_loop();
        self.running.store(false, AtomicOrdering::SeqCst);
        self.turtle_soup.shutdown();
        self.notify_pending_executor();
        self.deferred_chat.notify_all();
        if let Err(error) = executor.join() {
            log::error!("命令执行线程 panic: {error:?}");
        }
        if let Err(error) = deferred_chat_sender.join() {
            log::error!("延迟聊天发送线程 panic: {error:?}");
        }
        if let Err(error) = web_tool_executor.join() {
            log::error!("Web 工具执行线程 panic: {error:?}");
        }
        if let Err(error) = playback_monitor.join() {
            log::error!("播放监控线程 panic: {error:?}");
        }
        if let Some(ui_runtime) = self.ui_runtime.take()
            && let Err(error) = ui_runtime.shutdown()
        {
            log::error!("UI 运行时关闭失败: {error}");
        }
        if let Some(ocr_runtime) = self.ocr_runtime.take()
            && let Err(error) = ocr_runtime.shutdown()
        {
            log::error!("OCR 运行时关闭失败: {error:#}");
        }
        if let Err(error) = self.queue().and_then(|queue| queue.save()) {
            log::error!("退出前保存队列失败: {error:#}");
        }
        if let Err(error) = self.runtime_state().and_then(|state| state.save()) {
            log::error!("退出前保存运行状态失败: {error:#}");
        }
        self.monitor.set_status("已退出");
        result
    }

    fn start_command_executor(&self) -> thread::JoinHandle<()> {
        let mut executor = self.clone_for_background_task();
        thread::spawn(move || {
            log::info!("命令执行线程已启动");
            if let Err(error) = executor.run_pending_command_loop() {
                log::error!("命令执行线程异常退出: {error:#}");
            }
        })
    }

    fn start_deferred_chat_sender(&self) -> thread::JoinHandle<()> {
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
            let Some(item) = self.deferred_chat.wait_take(retry_delay)? else {
                continue;
            };
            if !self.running.load(AtomicOrdering::SeqCst) {
                break;
            }

            if let DeferredChatItem::Batch(batch) = &item
                && !self.turtle_soup.delivery_is_current(batch.turtle_soup)
            {
                log::debug!(
                    "延迟聊天分段批次所属海龟汤会话已失效，跳过: {:?}",
                    batch.turtle_soup
                );
                continue;
            }

            let target = item.target();

            if !self.deferred_chat_target_is_active(target) {
                match self.deferred_chat.requeue_back(item)? {
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

            let Some(sending) = self.try_begin_deferred_chat_send(target)? else {
                match self.deferred_chat.requeue_front(item)? {
                    EnqueueOutcome::DroppedMessage => {
                        log::warn!("延迟聊天重排时淘汰了一条较新的普通回复")
                    }
                    EnqueueOutcome::Rejected => {
                        log::warn!("延迟聊天重排失败，当前普通回复已丢弃")
                    }
                    EnqueueOutcome::Added => {}
                }
                sleep(retry_delay);
                continue;
            };

            match item {
                DeferredChatItem::Message(message) => {
                    let result = match target {
                        DeferredChatTarget::Primary => self.chat_output.send(&message.text),
                        DeferredChatTarget::SecondaryCurrentHall => {
                            self.chat_output.send_current_chat(&message.text)
                        }
                        DeferredChatTarget::CurrentHall => {
                            let residency = self.active_ui_residency();
                            self.ensure_ui_residency(residency, "大厅延迟回复发送前")
                                .and_then(|_| match residency {
                                    UiResidency::Primary => self.chat_output.send(&message.text),
                                    UiResidency::SecondaryCurrentHall => {
                                        self.chat_output.send_current_chat(&message.text)
                                    }
                                })
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
                        DeferredChatTarget::CurrentHall => self.active_ui_residency(),
                    };
                    let prepared = if target == DeferredChatTarget::CurrentHall {
                        self.ensure_ui_residency(residency, "大厅延迟批量回复发送前")
                    } else {
                        Ok(())
                    };
                    let outcome = match prepared {
                        Err(error) => ChatBatchSendOutcome::failed(0, error),
                        Ok(()) => {
                            let messages = batch.remaining_texts();
                            match residency {
                                UiResidency::Primary => {
                                    self.chat_output.send_batch_outcome(&messages, 0)
                                }
                                UiResidency::SecondaryCurrentHall => self
                                    .chat_output
                                    .send_current_chat_batch_outcome(&messages, 0),
                            }
                        }
                    };
                    drop(sending);

                    let ChatBatchSendOutcome { sent, status } = outcome;
                    let all_sent = match batch.mark_sent(sent) {
                        Ok(all_sent) => all_sent,
                        Err(error) => {
                            log::error!("海龟汤批量发送进度无效: {error:#}");
                            self.turtle_soup.handle_delivery_failure(delivery, &error);
                            continue;
                        }
                    };
                    if !self.running.load(AtomicOrdering::SeqCst)
                        || !self.turtle_soup.delivery_is_current(delivery)
                    {
                        continue;
                    }
                    if all_sent {
                        if let ChatBatchSendStatus::Failed(error) = &status {
                            log::warn!(
                                "海龟汤批次内容已完整发送，但聊天界面收尾失败，不重发内容: {error:#}"
                            );
                        }
                        self.turtle_soup.handle_delivery_success(delivery);
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
                            self.turtle_soup.handle_delivery_failure(delivery, &error);
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
                                        .deferred_chat
                                        .requeue_front(DeferredChatItem::Batch(batch))?
                                    {
                                        EnqueueOutcome::Added => {}
                                        EnqueueOutcome::DroppedMessage => {
                                            log::warn!("海龟汤批量重试入队时淘汰了一条普通回复")
                                        }
                                        EnqueueOutcome::Rejected => {
                                            let requeue_error =
                                                anyhow!("海龟汤批量重试无法重新进入延迟队列");
                                            log::error!("{requeue_error:#}");
                                            self.turtle_soup
                                                .handle_delivery_failure(delivery, &requeue_error);
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
                                    self.turtle_soup.handle_delivery_failure(delivery, &error);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn start_web_tool_executor(&self) -> thread::JoinHandle<()> {
        let mut worker = self.clone_for_background_task();
        thread::spawn(move || {
            log::info!("Web 工具执行线程已启动");
            worker.run_web_tool_loop();
        })
    }

    fn start_playback_monitor(&self) -> thread::JoinHandle<()> {
        let mut monitor = self.clone_for_background_task();
        thread::spawn(move || {
            log::info!("播放监控线程已启动");
            monitor.run_playback_monitor_loop();
        })
    }

    fn clone_for_background_task(&self) -> Self {
        Self {
            config: self.config.clone(),
            game_ui: self.game_ui.clone(),
            ui_runtime: None,
            runtime_state: self.runtime_state.clone(),
            entertainment: self.entertainment.clone(),
            idiom_chain: self.idiom_chain.clone(),
            landlord: self.landlord.clone(),
            undercover: self.undercover.clone(),
            turtle_soup: self.turtle_soup.clone(),
            deferred_chat: self.deferred_chat.clone(),
            queue: self.queue.clone(),
            song_dedup_history: self.song_dedup_history.clone(),
            player: self.player.clone(),
            ai: self.ai.clone(),
            song_review: self.song_review.clone(),
            chat_output: self.chat_output.clone(),
            ocr: self.ocr.clone(),
            ocr_runtime: None,
            latest_frame: self.latest_frame.clone(),
            locks: CommandLockState::default(),
            pending: self.pending.clone(),
            task_tracker: self.task_tracker.clone(),
            decision_control: self.decision_control.clone(),
            web_tools: self.web_tools.clone(),
            window_detection_signal: self.window_detection_signal.clone(),
            screen_lock_primed: self.screen_lock_primed.clone(),
            reset_locks_requested: self.reset_locks_requested.clone(),
            invite_executed_seqs: self.invite_executed_seqs.clone(),
            moderation_workflows: self.moderation_workflows.clone(),
            commands_enabled: self.commands_enabled.clone(),
            idle_exit: self.idle_exit.clone(),
            running: self.running.clone(),
            paused: self.paused.clone(),
            command_executing: self.command_executing.clone(),
            song_command_executing: self.song_command_executing.clone(),
            console_reply_context: self.console_reply_context.clone(),
            chat_listener: self.chat_listener.clone(),
            chat_observations: self.chat_observations.clone(),
            monitor: self.monitor.clone(),
        }
    }

    fn notify_pending_executor(&self) {
        let (_, cvar) = &*self.pending;
        cvar.notify_all();
    }

    fn queue(&self) -> Result<MutexGuard<'_, PersistentQueue>> {
        self.queue
            .lock()
            .map_err(|_| anyhow!("queue mutex poisoned"))
    }

    fn runtime_state(&self) -> Result<MutexGuard<'_, PersistentRuntimeState>> {
        self.runtime_state
            .lock()
            .map_err(|_| anyhow!("runtime state mutex poisoned"))
    }

    fn latest_frame(&self) -> Result<Arc<DynamicImage>> {
        self.latest_frame
            .lock()
            .map_err(|_| anyhow!("主扫描画面缓存锁已损坏"))?
            .clone()
            .ok_or_else(|| anyhow!("尚未获取主扫描画面，请稍后重试"))
    }

    fn run_web_tool_loop(&mut self) {
        while self.running.load(AtomicOrdering::SeqCst) {
            match self.take_web_tool_when_idle() {
                Ok(Some(task)) => {
                    if let Err(error) = self.execute_web_tool_task(task) {
                        log::error!("Web 工具任务收尾异常: {error:#}");
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    log::error!("Web 工具任务调度异常: {error:#}");
                    sleep(Duration::from_millis(250));
                }
            }
        }
    }

    fn take_web_tool_when_idle(&self) -> Result<Option<WebToolTask>> {
        let (lock, cvar) = &*self.pending;
        let mut pending = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
        while self.running.load(AtomicOrdering::SeqCst)
            && (!pending.is_empty() || self.command_executing.load(AtomicOrdering::SeqCst))
        {
            pending = cvar
                .wait_timeout(pending, Duration::from_millis(100))
                .map_err(|_| anyhow!("pending condvar poisoned"))?
                .0;
        }
        if !self.running.load(AtomicOrdering::SeqCst) {
            return Ok(None);
        }
        let task = self.web_tools.take_next()?;
        if let Some(task) = task {
            if task.request.requires_screen_exclusive() {
                self.command_executing.store(true, AtomicOrdering::SeqCst);
            }
            return Ok(Some(task));
        }
        {
            pending = cvar
                .wait_timeout(pending, Duration::from_millis(250))
                .map_err(|_| anyhow!("pending condvar poisoned"))?
                .0;
            drop(pending);
        }
        Ok(None)
    }

    fn scan_chat_with_shared_ocr(
        &self,
        image: &DynamicImage,
        templates: &ResolvedTemplateArgs,
    ) -> Result<Vec<ChatMessage>> {
        let total_started = Instant::now();
        let prepared = prepare_chat_scan(image, templates, self.config.screen.chat_rect.into())?;
        let messages = recognize_prepared_chat(
            &self.ocr,
            OcrPriority::ChatObservation,
            templates,
            prepared,
            Some(&self.monitor),
        );
        log::info!(target: "timing",
            "聊天扫描端到端耗时: total={}ms",
            elapsed_ms(total_started)
        );
        messages
    }

    fn warn_if_screen_size_mismatch(&self) -> Result<()> {
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

    fn start_http_server(&self) -> Result<()> {
        if !self.config.http.enabled {
            return Ok(());
        }
        http_server::start(http_server::HttpSharedState::new(
            self.config.clone(),
            Arc::clone(&self.queue),
            Arc::clone(&self.runtime_state),
            Arc::clone(&self.pending),
            self.chat_listener.clone(),
            self.turtle_soup.clone(),
            self.undercover.clone(),
            self.monitor.clone(),
            self.task_tracker.clone(),
            self.decision_control.clone(),
            self.moderation_workflows.clone(),
            self.web_tools.clone(),
            self.latest_frame.clone(),
        ))
    }

    fn start_hotkeys(&self) -> Result<()> {
        hotkeys::start(
            &self.config.hotkeys,
            Arc::clone(&self.running),
            Arc::clone(&self.paused),
        )
    }

    fn run_scan_loop(&mut self) -> Result<()> {
        let template_args = TemplateArgs::default().resolve(&self.config);
        let ui_template_args = UiTemplateArgs::default().resolve(&self.config);
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
                        Ok(ui_state) => self.monitor.set_ui_state(ui_state.to_string()),
                        Err(_) => self.monitor.set_ui_state("界面检测失败"),
                    }
                    let ui_ms = elapsed_ms(ui_started);
                    let listener_snapshot = self.chat_listener.snapshot();
                    let command_executing = self.command_executing.load(AtomicOrdering::SeqCst);
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
                                self.chat_listener.fail_mode_switch_to_primary();
                                self.update_monitor_chat_listener();
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
                    self.monitor.set_ui_state("目标窗口不可用");
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
            self.maybe_idle_exit()?;
            sleep(Duration::from_millis(self.config.timing.loop_idle_ms));
        }

        if let Some(subscription) = frame_subscription
            && let Err(error) = subscription.cancel()
        {
            log::warn!("扫描循环结束时撤销观察帧需求失败: {error}");
        }

        self.queue()?.save()?;
        self.runtime_state()?.save()?;
        Ok(())
    }

    fn run_secondary_listener_round(
        &mut self,
        image: &DynamicImage,
        last_friend_bubble: &mut Option<ChangeFingerprint>,
        hall_bubble_sequence: &mut Vec<SecondaryHallBubble>,
        last_title: &mut Option<ChangeFingerprint>,
        identity: &mut Option<SecondaryChatIdentity>,
    ) -> Result<bool> {
        if self.command_executing.load(AtomicOrdering::SeqCst) {
            return Ok(false);
        }
        let title_fingerprint =
            rect_chat_change_fingerprint(image, chat_listener::SECONDARY_TITLE_RECT)?;
        let title_changed = identity.is_none()
            || last_title
                .as_ref()
                .is_none_or(|previous| secondary_fingerprint_changed(previous, &title_fingerprint));
        if title_changed {
            *identity = Some(self.secondary_identity_from_frame(image)?);
        }
        *last_title = Some(title_fingerprint);

        let state = self.chat_listener.snapshot();
        if state.unread_task_pending {
            return Ok(false);
        }
        let current_identity = identity.clone().unwrap_or(SecondaryChatIdentity::Unknown);

        if state.initial_unread_clear {
            if let Some(hit) = find_unread_friend_hits(image).into_iter().next() {
                return self.queue_secondary_unread_task(hit, true);
            }
            *last_friend_bubble = latest_incoming_fingerprint(image)?;
            *hall_bubble_sequence = secondary_hall_bubbles(image)?;
            self.chat_listener.finish_initial_unread_clear();
            self.update_monitor_chat_listener();
            log::info!("二级监听初始未读清场完成，当前大厅已建立消息基线");
            return Ok(false);
        }

        match current_identity {
            SecondaryChatIdentity::CurrentHall => {
                if state.hall_round_required {
                    let scanned =
                        self.scan_secondary_hall_if_changed(image, hall_bubble_sequence)?;
                    self.chat_listener.finish_hall_round();
                    self.update_monitor_chat_listener();
                    return Ok(scanned);
                }
                if let Some(hit) = find_unread_friend_hits(image).into_iter().next() {
                    return self.queue_secondary_unread_task(hit, false);
                }
                self.scan_secondary_hall_if_changed(image, hall_bubble_sequence)
            }
            SecondaryChatIdentity::Friend(name) => {
                if title_changed {
                    *last_friend_bubble = latest_incoming_fingerprint(image)?;
                    return Ok(false);
                }
                self.scan_secondary_latest_if_changed(image, "pink", &name, last_friend_bubble)
            }
            SecondaryChatIdentity::PublicChannel => self.queue_secondary_hall_recovery(),
            SecondaryChatIdentity::StrangerMessages => Ok(false),
            SecondaryChatIdentity::Unknown => Ok(false),
        }
    }

    fn queue_secondary_unread_task(
        &self,
        hit: UnreadFriendHit,
        discard_only: bool,
    ) -> Result<bool> {
        if !self.chat_listener.claim_unread_task() {
            return Ok(false);
        }
        if let Err(error) =
            self.push_pending_task(PendingTask::SecondaryUnread { hit, discard_only })
        {
            self.chat_listener.release_unread_task();
            return Err(error);
        }
        self.update_monitor_chat_listener();
        log::info!(
            "二级监听检测到好友未读红点: y={} discard_only={}",
            hit.row_click.y,
            discard_only
        );
        Ok(false)
    }

    fn queue_secondary_hall_recovery(&self) -> Result<bool> {
        if !self.chat_listener.claim_unread_task() {
            return Ok(false);
        }
        if let Err(error) = self.push_pending_task(PendingTask::RestoreSecondaryHall) {
            self.chat_listener.release_unread_task();
            return Err(error);
        }
        self.update_monitor_chat_listener();
        log::info!("二级监听检测到不可执行会话，已加入恢复当前大厅任务");
        Ok(false)
    }

    fn scan_secondary_latest_if_changed(
        &mut self,
        image: &DynamicImage,
        message_type: &str,
        friend_name: &str,
        last_bubble: &mut Option<ChangeFingerprint>,
    ) -> Result<bool> {
        let current = latest_incoming_fingerprint(image)?;
        let changed = match (&*last_bubble, &current) {
            (None, Some(_)) => true,
            (Some(previous), Some(current)) => secondary_fingerprint_changed(previous, current),
            _ => false,
        };
        if !changed {
            *last_bubble = current;
            return Ok(false);
        }

        let refreshed = self.wait_for_secondary_bubble_stability()?;
        let refreshed_fingerprint = latest_incoming_fingerprint(&refreshed.image)?;
        let outcome = self.process_secondary_latest_message(
            &refreshed.image,
            refreshed.captured_at,
            message_type,
            friend_name,
        )?;
        *last_bubble = refreshed_fingerprint;
        Ok(outcome)
    }

    fn scan_secondary_hall_if_changed(
        &mut self,
        image: &DynamicImage,
        previous: &mut Vec<SecondaryHallBubble>,
    ) -> Result<bool> {
        let current = secondary_hall_bubbles(image)?;
        if previous.is_empty() {
            self.turtle_soup.clear_secondary_ocr_stability();
            *previous = current;
            log::debug!("二级大厅气泡序列尚未建立，当前仅记录基线");
            return Ok(false);
        }

        let overlap = hall_bubble_sequence_overlap(previous, &current);
        if overlap == 0 {
            self.turtle_soup.clear_secondary_ocr_stability();
            *previous = current;
            log::debug!("二级大厅气泡序列没有可靠重叠，已重建基线，不处理当前可见历史消息");
            return Ok(false);
        }
        if overlap == current.len() {
            self.turtle_soup.clear_secondary_ocr_stability();
            *previous = current;
            return Ok(false);
        }

        let refreshed = self.wait_for_secondary_bubble_stability()?;
        let refreshed_bubbles = secondary_hall_bubbles(&refreshed.image)?;
        let refreshed_overlap = hall_bubble_sequence_overlap(previous, &refreshed_bubbles);
        if refreshed_overlap == 0 {
            self.turtle_soup.clear_secondary_ocr_stability();
            *previous = refreshed_bubbles;
            log::debug!("二级大厅气泡稳定后没有可靠重叠，已重建基线，不处理当前可见历史消息");
            return Ok(false);
        }
        let new_bubbles = &refreshed_bubbles[refreshed_overlap..];
        if new_bubbles.is_empty() {
            self.turtle_soup.clear_secondary_ocr_stability();
            *previous = refreshed_bubbles;
            return Ok(false);
        }

        log::info!(
            "二级大厅检测到 {} 条新增气泡，按显示顺序 OCR",
            new_bubbles.len()
        );
        let outcome = self.process_secondary_bubble_rects(
            &refreshed.image,
            refreshed.captured_at,
            new_bubbles.iter().map(|bubble| bubble.rect),
            "blue",
            "",
        )?;
        if outcome.ocr_pending {
            log::debug!("二级大厅新增气泡的海龟汤 OCR 尚未稳定，保留旧气泡基线等待下轮复核");
            return Ok(false);
        }
        *previous = refreshed_bubbles;
        Ok(outcome.processed)
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

    fn dispatch_chat_observations(
        &mut self,
        dispatches: Vec<ChatObservationDispatch>,
    ) -> Result<bool> {
        let mut processed_secondary = false;
        for dispatch in dispatches {
            match dispatch {
                ChatObservationDispatch::Primary(messages) => {
                    let messages = messages
                        .into_iter()
                        .map(|PrimaryObservedMessage { id, message }| {
                            log::debug!("处理一级观察消息: id={id:?}");
                            message
                        })
                        .collect();
                    self.handle_scan_messages(messages)?;
                }
                ChatObservationDispatch::Secondary(observation) => {
                    processed_secondary |= self.process_secondary_chat_observation(observation)?;
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

    fn handle_scan_messages(&mut self, messages: Vec<ChatMessage>) -> Result<()> {
        if self
            .reset_locks_requested
            .swap(false, AtomicOrdering::SeqCst)
        {
            self.locks = CommandLockState::default();
            log::info!("已重置命令屏幕锁");
        }
        let active_entertainment = self.entertainment.active();
        let visible_turtle_questions = if self.turtle_soup.accepts_questions() {
            messages
                .iter()
                .filter(|message| message.message_type == "blue" && !message.text.is_empty())
                .filter(|message| {
                    command::parse_entertainment_shortcut(
                        &message.text,
                        &message.message_type,
                        active_entertainment,
                    )
                    .is_none()
                })
                .filter_map(|message| turtle_soup::parse_question_message(&message.text, None))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let suppress_new_turtle_questions = !self.screen_lock_primed.load(AtomicOrdering::SeqCst);
        let new_turtle_questions = self
            .turtle_soup
            .filter_new_primary_questions(visible_turtle_questions, suppress_new_turtle_questions);
        if messages.is_empty() {
            log::debug!("没有找到聊天标志，本轮不更新命令锁");
            return Ok(());
        }

        let mut parsed = Vec::new();
        for message in messages.iter().filter(|message| !message.text.is_empty()) {
            log::debug!(
                "识别文本: [{}] {}",
                message.message_type,
                redacted_chat_text(&message.text)
            );
            let Some(parsed_command) = command::parse_entertainment_shortcut(
                &message.text,
                &message.message_type,
                active_entertainment,
            )
            .or_else(|| command::parse_text(&message.text, &message.message_type))
            .or_else(|| {
                custom_workflow::parse_text(
                    &self.config.custom_workflows,
                    &message.text,
                    &message.message_type,
                )
            }) else {
                continue;
            };
            if !self.commands_enabled.load(AtomicOrdering::SeqCst) && message.message_type != "pink"
            {
                log::info!("命令识别已禁用，跳过: {}", parsed_command.raw);
                continue;
            }
            if let UserCommand::Invite(invite) = &parsed_command.command
                && let Some(seq) = invite.seq
            {
                let invite_executed = self
                    .invite_executed_seqs
                    .lock()
                    .map_err(|_| anyhow!("invite_executed_seqs mutex poisoned"))?
                    .contains(&seq);
                if invite_executed {
                    log::info!("邀请参数 {} 已执行过，跳过: {}", seq, parsed_command.raw);
                    continue;
                }
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
            .update(&parsed, self.command_executing.load(AtomicOrdering::SeqCst));
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
                    pending.parsed.raw
                );
            }
            return Ok(());
        }
        if self.commands_enabled.load(AtomicOrdering::SeqCst) {
            for question in new_turtle_questions {
                self.handle_turtle_soup_question(question)?;
            }
        }
        for pending in update.accepted {
            if self.handle_turtle_soup_command(&pending.parsed)? {
                continue;
            }
            if self.handle_idiom_chain_command(&pending.parsed)? {
                continue;
            }
            if self.handle_landlord_command(&pending.parsed)? {
                continue;
            }
            if self.enqueue_chat_listener_command(&pending.parsed)? {
                continue;
            }
            if self.pending_contains_command(&pending.parsed)? {
                log::info!("命令已在待处理队列，本轮跳过: {}", pending.parsed.raw);
                continue;
            }
            match &pending.parsed.command {
                UserCommand::DisableCommands { username: _ } => {
                    self.commands_enabled.store(false, AtomicOrdering::SeqCst);
                }
                UserCommand::EnableCommands { username: _ } => {
                    self.commands_enabled.store(true, AtomicOrdering::SeqCst);
                }
                UserCommand::IdleExit { minutes } => {
                    self.record_command_activity()?;
                    self.configure_idle_exit(*minutes)?;
                    if let Err(error) = self
                        .log_executed_command(&pending.parsed, &format!("idle exit {}", minutes))
                    {
                        log::error!("写入执行命令日志失败: {error:#}");
                    }
                    continue;
                }
                _ => {}
            }
            log::info!("命令已加入待处理队列: {}", pending.parsed.raw);
            self.record_command_activity()?;
            self.push_pending_task(PendingTask::Command(Box::new(pending)))?;
        }
        Ok(())
    }

    fn enqueue_chat_listener_command(&self, parsed: &ParsedCommand) -> Result<bool> {
        let UserCommand::ChatListenerMode(command) = &parsed.command else {
            return Ok(false);
        };
        match command {
            ChatListenerModeCommand::Status => {
                let snapshot = self.chat_listener.snapshot();
                let pending = snapshot
                    .pending_mode
                    .map(|mode| format!("，等待切换{}", mode.label()))
                    .unwrap_or_default();
                let message = format!("监听模式状态: {}{}", snapshot.mode.label(), pending);
                log::info!("{}", message);
                self.monitor
                    .push_command(format!("{} -> {}", parsed.user_command, message));
            }
            ChatListenerModeCommand::Primary | ChatListenerModeCommand::Secondary => {
                let target = match command {
                    ChatListenerModeCommand::Primary => ChatListenerMode::Primary,
                    ChatListenerModeCommand::Secondary => ChatListenerMode::Secondary,
                    ChatListenerModeCommand::Status => unreachable!(),
                };
                if !self.chat_listener.request_mode(target) {
                    let snapshot = self.chat_listener.snapshot();
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
                    self.chat_listener.cancel_mode_request(target);
                    self.update_monitor_chat_listener();
                    return Err(error);
                }
                self.update_monitor_chat_listener();
                log::info!("监听模式切换已加入待处理队列: {}", target.label());
            }
        }
        Ok(true)
    }

    fn handle_idiom_chain_command(&self, parsed: &ParsedCommand) -> Result<bool> {
        let UserCommand::IdiomChain(command) = &parsed.command else {
            return Ok(false);
        };
        if idiom_command_requires_executor(command) {
            return Ok(false);
        }
        let outcome = self.idiom_chain.handle(&parsed.username, command)?;
        let target = match self.active_ui_residency() {
            UiResidency::Primary => DeferredChatTarget::Primary,
            UiResidency::SecondaryCurrentHall => DeferredChatTarget::SecondaryCurrentHall,
        };
        let action = outcome.action;
        debug_assert!(outcome.explanation.is_none());
        let queue_outcome = self.deferred_chat.enqueue(DeferredChatMessage {
            text: outcome.reply,
            target,
        })?;
        log::info!(
            "成语接龙已处理，回复进入延迟发送队列: command={} action={}",
            parsed.raw,
            action
        );
        if queue_outcome == EnqueueOutcome::DroppedMessage {
            log::warn!("延迟聊天发送队列已满，已丢弃一条较早的回复");
        }
        if queue_outcome == EnqueueOutcome::Rejected {
            log::warn!("延迟聊天发送队列已被受保护批次占满，成语接龙回复已丢弃");
        }
        Ok(true)
    }

    fn handle_landlord_command(&self, parsed: &ParsedCommand) -> Result<bool> {
        let UserCommand::Landlord(command) = &parsed.command else {
            return Ok(false);
        };
        if command.requires_executor() {
            return Ok(false);
        }
        self.landlord.execute(
            &parsed.username,
            command,
            &DeferredCardGamePort { app: self },
            Instant::now(),
        )?;
        log::info!(
            "牌局命令已处理: command={} user={}",
            parsed.raw,
            parsed.username
        );
        Ok(true)
    }

    fn handle_turtle_soup_command(&self, parsed: &ParsedCommand) -> Result<bool> {
        let UserCommand::TurtleSoup(command) = &parsed.command else {
            return Ok(false);
        };
        let outcome = if parsed.message_type == "pink" {
            self.turtle_soup
                .handle_friend_command(&parsed.username, command)
        } else {
            self.turtle_soup
                .handle_hall_command(&parsed.username, command)
        };
        if let Some(reply) = outcome.immediate_reply {
            self.enqueue_current_hall_reply(&reply)?;
        }
        log::info!(
            "海龟汤命令已处理: command={} action={}",
            parsed.raw,
            outcome.action
        );
        Ok(true)
    }

    fn handle_turtle_soup_question(
        &self,
        question: turtle_soup::TurtleSoupQuestion,
    ) -> Result<bool> {
        match self.turtle_soup.submit_question(question)? {
            QuestionSubmitOutcome::Ignored => Ok(false),
            QuestionSubmitOutcome::Queued { request_id } => {
                log::info!("海龟汤提问已进入 AI 队列: request_id={}", request_id);
                Ok(true)
            }
            QuestionSubmitOutcome::Reply(reply) => {
                self.enqueue_current_hall_reply(&reply)?;
                Ok(true)
            }
        }
    }

    fn enqueue_current_hall_reply(&self, text: &str) -> Result<()> {
        match self.deferred_chat.enqueue(DeferredChatMessage {
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

    pub(super) fn abort_entertainment_for_context_loss(&self, reason: &str) {
        self.turtle_soup.abort_for_context_loss(reason);
        match self.undercover.abort() {
            Ok(true) => log::warn!("谁是卧底已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧谁是卧底牌局: {error:#}"),
        }
        match self.landlord.abort() {
            Ok(true) => log::warn!("牌局已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧牌局: {error:#}"),
        }
        match self.idiom_chain.abort() {
            Ok(true) => log::warn!("成语接龙已因聊天上下文变化中止: {}", reason),
            Ok(false) => {}
            Err(error) => log::error!("无法中止旧成语接龙会话: {error:#}"),
        }
    }

    fn tick_entertainment(&self) {
        self.turtle_soup.tick();
        let clock_active = !self.paused.load(AtomicOrdering::SeqCst)
            && !self.command_executing.load(AtomicOrdering::SeqCst);
        let card_game_outcome = match self.landlord.tick(Instant::now(), clock_active) {
            Ok(outcome) => outcome,
            Err(error) => {
                log::error!("无法推进牌局回合计时: {error:#}");
                None
            }
        };
        if let Some(outcome) = card_game_outcome {
            let should_abort_on_failure = !outcome.private_deliveries.is_empty();
            let ended = outcome.ended;
            if let Err(error) = self.push_pending_task(PendingTask::CardGameOutcome { outcome }) {
                log::error!("牌局计时结果入队失败: {error:#}");
                if (ended || should_abort_on_failure)
                    && let Err(abort_error) = self.landlord.abort()
                {
                    log::error!("牌局计时结果入队失败后无法中止牌局: {abort_error:#}");
                }
            }
        }
        match self.undercover.tick(Instant::now(), clock_active) {
            Ok(deliveries) => {
                if !deliveries.is_empty()
                    && let Err(error) =
                        self.push_pending_task(PendingTask::UndercoverDelivery { deliveries })
                {
                    log::error!("谁是卧底计时消息入队失败: {error:#}");
                }
            }
            Err(error) => log::error!("无法推进谁是卧底计时: {error:#}"),
        }
        match self.idiom_chain.expire_idle_now() {
            Ok(true) => log::info!("成语接龙已因空闲超时结束，娱乐互斥已释放"),
            Ok(false) => {}
            Err(error) => log::error!("无法检查成语接龙空闲超时: {error:#}"),
        }
    }

    fn submit_secondary_command(&self, parsed: ParsedCommand) -> Result<()> {
        if self.enqueue_chat_listener_command(&parsed)? {
            return Ok(());
        }
        if !self.commands_enabled.load(AtomicOrdering::SeqCst) && parsed.message_type != "pink" {
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
        if let UserCommand::Invite(invite) = &parsed.command
            && let Some(seq) = invite.seq
        {
            let executed = self
                .invite_executed_seqs
                .lock()
                .map_err(|_| anyhow!("invite_executed_seqs mutex poisoned"))?
                .contains(&seq);
            if executed {
                log::info!("邀请参数 {} 已执行过，跳过: {}", seq, parsed.raw);
                return Ok(());
            }
        }
        if self.pending_contains_command(&parsed)? {
            log::info!("二级监听命令已在待处理队列，跳过: {}", parsed.raw);
            return Ok(());
        }
        match &parsed.command {
            UserCommand::DisableCommands { .. } => {
                self.commands_enabled.store(false, AtomicOrdering::SeqCst);
            }
            UserCommand::EnableCommands { .. } => {
                self.commands_enabled.store(true, AtomicOrdering::SeqCst);
            }
            UserCommand::IdleExit { minutes } => {
                self.record_command_activity()?;
                self.configure_idle_exit(*minutes)?;
                self.log_executed_command(&parsed, &format!("idle exit {}", minutes))?;
                return Ok(());
            }
            _ => {}
        }
        self.record_command_activity()?;
        log::info!("二级监听命令已加入待处理队列: {}", parsed.raw);
        self.push_pending_task(PendingTask::Command(Box::new(PendingCommand {
            lock_key: command::lock_key(&parsed),
            parsed,
        })))
    }

    fn record_command_activity(&self) -> Result<()> {
        let mut state = self
            .idle_exit
            .lock()
            .map_err(|_| anyhow!("idle_exit mutex poisoned"))?;
        if let Some(state) = state.as_mut() {
            state.last_command_at = Instant::now();
        }
        Ok(())
    }

    fn maybe_idle_exit(&self) -> Result<()> {
        let Some(timeout) = self.idle_exit_due()? else {
            return Ok(());
        };
        if !self.executor_is_idle()? {
            return Ok(());
        }
        log::info!(
            "闲置退出触发: {}分钟无新命令，关闭目标游戏进程并保留软件进程",
            timeout.as_secs() / 60
        );
        self.abort_entertainment_for_context_loss("闲置退出即将关闭游戏");
        if let Err(error) = self.game_ui.close_window() {
            log::error!("关闭目标窗口失败: {error:#}");
        }
        self.clear_idle_exit_timer()?;
        Ok(())
    }

    fn clear_idle_exit_timer(&self) -> Result<()> {
        let mut state = self
            .idle_exit
            .lock()
            .map_err(|_| anyhow!("idle_exit mutex poisoned"))?;
        *state = None;
        Ok(())
    }

    fn idle_exit_due(&self) -> Result<Option<Duration>> {
        let state = self
            .idle_exit
            .lock()
            .map_err(|_| anyhow!("idle_exit mutex poisoned"))?;
        let Some(state) = state.as_ref() else {
            return Ok(None);
        };
        if state.last_command_at.elapsed() >= state.timeout {
            Ok(Some(state.timeout))
        } else {
            Ok(None)
        }
    }

    fn run_pending_command_loop(&mut self) -> Result<()> {
        while self.running.load(AtomicOrdering::SeqCst) {
            if self.paused.load(AtomicOrdering::SeqCst) {
                sleep(Duration::from_millis(self.config.timing.loop_idle_ms));
                continue;
            }
            let Some((task, executing)) = self.wait_for_pending_task()? else {
                continue;
            };
            if self.paused.load(AtomicOrdering::SeqCst) {
                self.push_pending_task_front(task)?;
                drop(executing);
                sleep(Duration::from_millis(self.config.timing.loop_idle_ms));
                continue;
            }
            let task_id = task.id;
            let task_label = task.label();
            self.task_tracker.mark_running(task_id);
            let result = match catch_unwind(AssertUnwindSafe(|| self.execute_pending_task(task))) {
                Ok(result) => result,
                Err(_) => Err(anyhow!("待处理任务执行发生未捕获异常")),
            };
            match result {
                Ok(PendingTaskExecution::Completed) => {
                    self.task_tracker
                        .finish_ok(task_id, format!("{}执行完成", task_label));
                    sleep(Duration::from_millis(
                        self.config.timing.command.post_settle_ms,
                    ));
                }
                Ok(PendingTaskExecution::Requeued) => {
                    sleep(Duration::from_millis(
                        self.config.timing.command.post_settle_ms,
                    ));
                }
                Err(error) => {
                    self.task_tracker.finish_error(task_id, &error);
                    log::error!("待处理任务执行异常: {error:#}");
                }
            }
        }
        Ok(())
    }

    fn wait_for_pending_task(&self) -> Result<Option<(TrackedPendingTask, CommandExecutingGuard)>> {
        let (lock, cvar) = &*self.pending;
        let mut guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
        while (guard.is_empty() || self.command_executing.load(AtomicOrdering::SeqCst))
            && self.running.load(AtomicOrdering::SeqCst)
        {
            guard = cvar
                .wait_timeout(guard, Duration::from_secs(1))
                .map_err(|_| anyhow!("pending condvar poisoned"))?
                .0;
        }
        if !self.running.load(AtomicOrdering::SeqCst) {
            return Ok(None);
        }
        let executing = CommandExecutingGuard::new(
            Arc::clone(&self.command_executing),
            Arc::clone(&self.pending),
        );
        Ok(guard.pop_front().map(|task| {
            log::info!("待处理任务开始: {}", task.label());
            (task, executing)
        }))
    }

    fn try_begin_deferred_chat_send(
        &self,
        target: DeferredChatTarget,
    ) -> Result<Option<CommandExecutingGuard>> {
        let (lock, _) = &*self.pending;
        let pending = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
        if !self.running.load(AtomicOrdering::SeqCst)
            || self.paused.load(AtomicOrdering::SeqCst)
            || !pending.is_empty()
            || self.command_executing.load(AtomicOrdering::SeqCst)
            || !self.deferred_chat_target_is_active(target)
        {
            return Ok(None);
        }
        Ok(Some(CommandExecutingGuard::new(
            Arc::clone(&self.command_executing),
            Arc::clone(&self.pending),
        )))
    }

    fn deferred_chat_target_is_active(&self, target: DeferredChatTarget) -> bool {
        matches!(
            (target, self.active_ui_residency()),
            (DeferredChatTarget::Primary, UiResidency::Primary)
                | (
                    DeferredChatTarget::SecondaryCurrentHall,
                    UiResidency::SecondaryCurrentHall
                )
                | (DeferredChatTarget::CurrentHall, _)
        )
    }

    fn execute_pending_task(
        &mut self,
        tracked: TrackedPendingTask,
    ) -> Result<PendingTaskExecution> {
        let task_id = tracked.id;
        let task = tracked.task;
        let label = task.label();
        let restore_residency_after_execution = task.restores_listener_residency_after_execution();
        let result = match task {
            PendingTask::Command(pending) => {
                let _song_command_guard = if matches!(&pending.parsed.command, UserCommand::Song(_))
                {
                    Some(SongCommandExecutingGuard::new(Arc::clone(
                        &self.song_command_executing,
                    )))
                } else {
                    None
                };
                self.execute_pending_command(*pending)
                    .map(|_| PendingTaskExecution::Completed)
            }
            PendingTask::AdvanceQueue { reason } => self
                .execute_advance_queue_task(reason)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::ConsoleChat { text, prefix } => self
                .execute_console_chat_task(text, prefix)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::StartGame { source } => self
                .execute_start_game_task(source)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::EnterWonderland { source } => self
                .execute_enter_wonderland_task(source)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::ClearIdleExit => self
                .clear_idle_exit_timer()
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::ModerationVoteResult {
                command,
                approved,
                workflow_key,
                temporary_primary_hold,
            } => self.execute_moderation_vote_result(
                task_id,
                *command,
                approved,
                workflow_key,
                temporary_primary_hold,
            ),
            PendingTask::SetChatListenerMode { target } => self
                .execute_set_chat_listener_mode(target)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::SecondaryUnread { hit, discard_only } => self
                .execute_secondary_unread_task(hit, discard_only)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::RestoreSecondaryHall => self
                .execute_restore_secondary_hall_task()
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::CardGameOutcome { outcome } => self
                .landlord
                .deliver(outcome, self)
                .map(|_| PendingTaskExecution::Completed),
            PendingTask::UndercoverDelivery { deliveries } => self
                .undercover
                .deliver(deliveries, self)
                .map(|_| PendingTaskExecution::Completed),
        };
        match result {
            Ok(PendingTaskExecution::Completed) => {
                if restore_residency_after_execution {
                    self.restore_listener_residency_after_task(&label)?;
                }
                log::info!("待处理任务完成: {}", label);
                Ok(PendingTaskExecution::Completed)
            }
            Ok(PendingTaskExecution::Requeued) => {
                log::info!("待处理任务已重新排队: {}", label);
                Ok(PendingTaskExecution::Requeued)
            }
            Err(error) => {
                log::error!("待处理任务失败 {}: {error:#}", label);
                self.handle_task_error_after_execution(
                    &label,
                    &error,
                    restore_residency_after_execution,
                );
                Err(error)
            }
        }
    }

    fn handle_task_error_after_execution(
        &self,
        label: &str,
        error: &anyhow::Error,
        restore_listener_residency: bool,
    ) {
        if is_target_window_unavailable_error(error) {
            log::warn!("任务失败且目标游戏窗口不可用，跳过界面恢复: {}", label);
        } else if restore_listener_residency {
            if let Err(recovery_error) = self.restore_listener_residency_after_task(label) {
                log::error!(
                    "任务失败后恢复监听驻留界面失败 {}: {recovery_error:#}",
                    label
                );
            }
        } else {
            self.return_to_primary_after_command_failure(label);
        }
    }

    fn execute_web_tool_task(&mut self, task: WebToolTask) -> Result<()> {
        let id = task.id;
        let label = task.request.label();
        let requires_screen_exclusive = task.request.requires_screen_exclusive();
        let result = match catch_unwind(AssertUnwindSafe(|| {
            self.execute_web_tool_request(task.request)
        })) {
            Ok(result) => result,
            Err(_) => Err(anyhow!("Web 工具执行发生未捕获异常")),
        };
        if let Err(error) = &result {
            log::error!("Web 工具执行失败 {}: {error:#}", label);
        }
        self.web_tools.finish(id, result);
        if requires_screen_exclusive {
            self.command_executing.store(false, AtomicOrdering::SeqCst);
            self.notify_pending_executor();
        }
        Ok(())
    }

    fn execute_web_tool_request(&mut self, request: WebToolRequest) -> Result<String> {
        match request {
            WebToolRequest::Ocr { rect } => {
                let frame = self.latest_frame()?;
                let image = match rect {
                    Some(rect) => crop_canvas(&frame, rect)?,
                    None => (*frame).clone(),
                };
                serde_json::to_string_pretty(
                    &self.ocr.recognize_lines(image, OcrPriority::Diagnostic)?,
                )
                .map_err(|error| anyhow!(error))
            }
            WebToolRequest::ScanChat => {
                let frame = self.latest_frame()?;
                let templates = TemplateArgs::default().resolve(&self.config);
                let prepared =
                    prepare_chat_scan(&frame, &templates, self.config.screen.chat_rect.into())?;
                serde_json::to_string_pretty(&recognize_prepared_chat(
                    &self.ocr,
                    OcrPriority::Diagnostic,
                    &templates,
                    prepared,
                    None,
                )?)
                .map_err(|error| anyhow!(error))
            }
            WebToolRequest::UiState => {
                let frame = self.latest_frame()?;
                let templates = UiTemplateArgs::default().resolve(&self.config);
                Ok(detect_ui_state(&frame, &templates, &self.config.screen)?.to_string())
            }
            WebToolRequest::HallName => {
                let frame = self.latest_frame()?;
                let image = crop_canvas(&frame, self.config.screen.hall_name_rect.into())?;
                self.ocr.merged_text(
                    image,
                    self.config.ocr.same_line_y_tolerance,
                    OcrPriority::Diagnostic,
                )
            }
            WebToolRequest::MatchTemplate {
                template,
                rect,
                threshold,
                click,
            } => {
                let frame = self.latest_frame()?;
                let default_threshold = match &template {
                    WebToolTemplate::WonderlandEnterButton => {
                        self.config.startup.wonderland_enter_button_threshold
                    }
                    WebToolTemplate::PaimonMenu | WebToolTemplate::WonderlandClose => {
                        self.config.startup.template_threshold
                    }
                    WebToolTemplate::Custom(_) => self.config.custom_workflows.default_threshold,
                    _ => self.config.templates.marker_threshold,
                };
                let path = match &template {
                    WebToolTemplate::BlueMarker => self.config.templates.blue_marker.clone(),
                    WebToolTemplate::YellowMarker => self.config.templates.yellow_marker.clone(),
                    WebToolTemplate::PinkMarker => self.config.templates.pink_marker.clone(),
                    WebToolTemplate::Friend => self.config.templates.friend.clone(),
                    WebToolTemplate::SecondaryBack => self.config.templates.secondary_back.clone(),
                    WebToolTemplate::SecondaryHall => self.config.templates.secondary_hall.clone(),
                    WebToolTemplate::InviteViewStar => {
                        self.config.templates.invite_view_star.clone()
                    }
                    WebToolTemplate::InviteGotoHall => {
                        self.config.templates.invite_goto_hall.clone()
                    }
                    WebToolTemplate::InviteEnterHall => {
                        self.config.templates.invite_enter_hall.clone()
                    }
                    WebToolTemplate::FriendPanel => self.config.templates.friend_panel.clone(),
                    WebToolTemplate::FriendSearchPanel => {
                        self.config.templates.friend_search_panel.clone()
                    }
                    WebToolTemplate::FriendMoreSettings => {
                        self.config.templates.friend_more_settings.clone()
                    }
                    WebToolTemplate::FriendBlockChat => {
                        self.config.templates.friend_block_chat.clone()
                    }
                    WebToolTemplate::FriendBlacklist => {
                        self.config.templates.friend_blacklist.clone()
                    }
                    WebToolTemplate::FriendConfirm => self.config.templates.friend_confirm.clone(),
                    WebToolTemplate::WonderlandEnterButton => self
                        .config
                        .startup
                        .templates
                        .wonderland_enter_button
                        .clone(),
                    WebToolTemplate::PaimonMenu => {
                        self.config.startup.templates.paimon_menu.clone()
                    }
                    WebToolTemplate::WonderlandClose => {
                        self.config.startup.templates.wonderland_close.clone()
                    }
                    WebToolTemplate::Custom(name) => self
                        .config
                        .custom_workflows
                        .templates
                        .get(name)
                        .cloned()
                        .ok_or_else(|| anyhow!("自定义模板不存在: {name}"))?,
                };
                let threshold = threshold.unwrap_or(default_threshold);
                if click {
                    self.ensure_web_tool_input_still_idle()?;
                    let hit = best_template_hit(&frame, rect, &path, threshold)?
                        .ok_or_else(|| anyhow!("未找到超过阈值的模板: {}", template.label()))?;
                    let point = hit.center();
                    self.game_ui
                        .ensure_ready(self.config.timing.input.after_activate_ms)?;
                    self.game_ui
                        .click_point(PointConfig::new(point.x, point.y))?;
                    Ok(format!(
                        "已点击 {}: x={} y={} score={:.3}",
                        template.label(),
                        point.x,
                        point.y,
                        hit.score
                    ))
                } else {
                    serde_json::to_string_pretty(&find_template_hits(
                        &frame, rect, &path, threshold,
                    )?)
                    .map_err(|error| anyhow!(error))
                }
            }
            WebToolRequest::Click { x, y } => {
                let width = self.config.screen.expected_width as i32;
                let height = self.config.screen.expected_height as i32;
                if !(0..width).contains(&x) || !(0..height).contains(&y) {
                    return Err(anyhow!(
                        "坐标超出画布范围: x=0..{} y=0..{}",
                        width - 1,
                        height - 1
                    ));
                }
                self.ensure_web_tool_input_still_idle()?;
                self.game_ui
                    .ensure_ready(self.config.timing.input.after_activate_ms)?;
                self.game_ui.click_point(PointConfig::new(x, y))?;
                Ok(format!("已点击坐标: {x},{y}"))
            }
            WebToolRequest::Key { key } => {
                let key = parse_key(&key)?;
                self.ensure_web_tool_input_still_idle()?;
                self.game_ui
                    .ensure_ready(self.config.timing.input.after_activate_ms)?;
                self.game_ui.press_key(key)?;
                Ok("按键已发送".to_string())
            }
            WebToolRequest::ChatChangeSamples {
                samples,
                interval_ms,
            } => self.sample_web_tool_chat_changes(samples, interval_ms),
            WebToolRequest::PanelResponseBenchmark { rounds } => {
                self.run_web_tool_panel_benchmark(rounds)
            }
            WebToolRequest::OcrBackendProbe => {
                let args = OcrArgs::default().resolve(&self.config);
                let result = probe_ocr_backend_support(&args)
                    .into_iter()
                    .map(|probe| match probe.status {
                        OcrBackendProbeStatus::Available {
                            init_ms,
                            detect_ms,
                            rec_ms,
                        } => format!(
                            "{} [{}] 可用: 初始化={}ms 检测={}ms 识别={}ms",
                            probe.name,
                            if probe.gpu { "GPU" } else { "CPU" },
                            init_ms,
                            detect_ms,
                            rec_ms
                        ),
                        OcrBackendProbeStatus::Failed { elapsed_ms, error } => format!(
                            "{} [{}] 不可用: {}ms {error}",
                            probe.name,
                            if probe.gpu { "GPU" } else { "CPU" },
                            elapsed_ms
                        ),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(result)
            }
            WebToolRequest::AiSearchPreview {
                keyword,
                prefer_accompaniment,
            } => {
                if !self.ai.enabled() {
                    return Err(anyhow!("AI 点歌未启用，请先配置 ai.api_key"));
                }
                let feeluown = FeelUOwnClient::new(&self.config.feeluown, &self.config.timing);
                let result = self
                    .ai
                    .search_and_pick(&feeluown, &keyword, prefer_accompaniment)?;
                let mut lines = vec![
                    format!("用户请求: {}", result.request),
                    format!("候选数量: {}", result.candidates.len()),
                ];
                lines.extend(
                    result
                        .candidates
                        .iter()
                        .enumerate()
                        .map(|(index, candidate)| {
                            format!("{}. {} -> {}", index + 1, candidate.text, candidate.uri)
                        }),
                );
                if let Some(pick) = result.pick {
                    lines.push(format!(
                        "AI 选择: {} score={:.2} reason={}",
                        pick.uri, pick.score, pick.reason
                    ));
                }
                Ok(lines.join("\n"))
            }
        }
    }

    fn sample_web_tool_chat_changes(&self, samples: u32, interval_ms: u64) -> Result<String> {
        let baseline = self.latest_frame()?;
        let mut previous =
            rect_chat_change_fingerprint(&baseline, self.config.screen.chat_rect.into())?;
        let templates = TemplateArgs::default().resolve(&self.config);
        let mut lines = vec![format!(
            "采样次数={} 间隔={}ms，区域为一级聊天区",
            samples, interval_ms
        )];

        for index in 1..=samples {
            sleep(Duration::from_millis(interval_ms));
            let frame = self.latest_frame()?;
            let current =
                rect_chat_change_fingerprint(&frame, self.config.screen.chat_rect.into())?;
            let stats = change_stats(&previous, &current);
            let changed = stats.mean_abs_diff >= self.config.ocr.change_mean_threshold
                || stats.changed_ratio >= self.config.ocr.change_pixel_threshold;
            let markers = if changed {
                let (blue, yellow, pink) = self::chat_scan::count_chat_markers(
                    &frame,
                    &templates,
                    self.config.screen.chat_rect,
                )?;
                format!(" 蓝={} 黄={} 粉={}", blue, yellow, pink)
            } else {
                String::new()
            };
            lines.push(format!(
                "#{} mean={:.3} ratio={:.5} changed={}{}",
                index, stats.mean_abs_diff, stats.changed_ratio, changed, markers
            ));
            previous = current;
        }
        Ok(lines.join("\n"))
    }

    fn run_web_tool_panel_benchmark(&self, rounds: u32) -> Result<String> {
        const TIMEOUT_MS: u64 = 1_500;
        const POLL_MS: u64 = 50;
        const STABLE_SAMPLES: usize = 3;

        self.ensure_web_tool_input_still_idle()?;
        self.game_ui
            .ensure_ready(self.config.timing.input.after_activate_ms)?;
        let mut open_times = Vec::new();
        let mut close_times = Vec::new();
        let mut failures = 0u32;
        let detect_rect = web_tool_panel_response_rect(&self.config);

        for _ in 0..rounds {
            self.ensure_web_tool_input_still_idle()?;
            self.game_ui.press_key(Key::Escape)?;
            let closed = self.latest_frame()?;
            let closed = rect_chat_change_fingerprint(&closed, detect_rect)?;

            let opened_at = Instant::now();
            self.ensure_web_tool_input_still_idle()?;
            self.game_ui.press_key(Key::Return)?;
            let Some(opened) = self.wait_for_web_tool_change(
                &closed,
                detect_rect,
                opened_at,
                TIMEOUT_MS,
                POLL_MS,
                STABLE_SAMPLES,
            )?
            else {
                failures += 1;
                continue;
            };
            open_times.push(opened.0);

            let closed_at = Instant::now();
            self.ensure_web_tool_input_still_idle()?;
            self.game_ui.press_key(Key::Escape)?;
            let Some(closed_again) = self.wait_for_web_tool_change(
                &opened.1,
                detect_rect,
                closed_at,
                TIMEOUT_MS,
                POLL_MS,
                STABLE_SAMPLES,
            )?
            else {
                failures += 1;
                continue;
            };
            close_times.push(closed_again.0);
        }

        let _ = self.game_ui.press_key(Key::Escape);
        Ok(format!(
            "轮数={} 失败={}\n打开: {}\n关闭: {}",
            rounds,
            failures,
            format_web_tool_latency_summary(&open_times),
            format_web_tool_latency_summary(&close_times)
        ))
    }

    fn ensure_web_tool_input_still_idle(&self) -> Result<()> {
        let (lock, _) = &*self.pending;
        let pending = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
        if pending.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("正式任务已进入队列，已取消 Web 工具输入"))
        }
    }

    fn wait_for_web_tool_change(
        &self,
        baseline: &ChangeFingerprint,
        detect_rect: Rect,
        started: Instant,
        timeout_ms: u64,
        poll_ms: u64,
        stable_samples: usize,
    ) -> Result<Option<(u128, ChangeFingerprint)>> {
        let mut previous = baseline.clone();
        let mut stable_count = 0usize;
        let deadline = started + Duration::from_millis(timeout_ms);

        while Instant::now() < deadline {
            sleep(Duration::from_millis(poll_ms));
            let frame = self.latest_frame()?;
            let current = rect_chat_change_fingerprint(&frame, detect_rect)?;
            let from_baseline = change_stats(baseline, &current);
            let from_previous = change_stats(&previous, &current);
            let changed = from_baseline.mean_abs_diff >= self.config.ocr.change_mean_threshold
                || from_baseline.changed_ratio >= self.config.ocr.change_pixel_threshold;
            let stable = from_previous.mean_abs_diff < self.config.ocr.change_mean_threshold
                && from_previous.changed_ratio < self.config.ocr.change_pixel_threshold;

            if changed && stable {
                stable_count += 1;
                if stable_count >= stable_samples {
                    return Ok(Some((started.elapsed().as_millis(), current)));
                }
            } else if !stable {
                stable_count = 0;
            }
            previous = current;
        }
        Ok(None)
    }

    fn execute_console_chat_task(&mut self, text: String, prefix: String) -> Result<()> {
        let message = format!("{}{}", prefix, text);
        self.ensure_game_ready_for_input("控制台发言前准备")?;
        self.reply(&message)
    }

    fn execute_set_chat_listener_mode(&mut self, target: ChatListenerMode) -> Result<()> {
        self.abort_entertainment_for_context_loss("聊天监听模式即将切换");
        let switched = match target {
            ChatListenerMode::Primary => {
                self.ensure_game_ready_for_input("切换一级监听")?;
                self.return_to_primary_fixed()
            }
            ChatListenerMode::Secondary => self.open_secondary_current_hall()?,
        };
        if switched {
            self.chat_listener.complete_mode_switch(target);
            self.update_monitor_chat_listener();
            log::info!("聊天监听模式已切换为{}", target.label());
            return Ok(());
        }

        self.chat_listener.fail_mode_switch_to_primary();
        self.update_monitor_chat_listener();
        let _ = self.return_to_primary_fixed();
        Err(anyhow!("切换{}失败，已回退一级监听", target.label()))
    }

    fn execute_secondary_unread_task(
        &mut self,
        hit: UnreadFriendHit,
        discard_only: bool,
    ) -> Result<()> {
        let result = (|| {
            if !self.restore_secondary_current_hall()? {
                return Err(anyhow!("二级监听未能恢复当前大厅"));
            }

            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let frame_args = FrameArgs { image: None };
            let mut opened = false;
            for attempt in 1..=2 {
                log::info!(
                    "二级监听好友未读: 点击红点行 attempt={}/2 y={}",
                    attempt,
                    hit.row_click.y
                );
                self.game_ui
                    .click_point(PointConfig::new(hit.row_click.x, hit.row_click.y))?;
                sleep(Duration::from_millis(self.config.timing.input.click_ms));
                let frame = load_frame(&frame_args, &canvas, &self.game_ui)?;
                if !unread_hit_still_visible(&frame.image, hit) {
                    opened = true;
                    break;
                }
            }
            if !opened {
                log::warn!("二级监听好友未读: 红点未消失，放弃本次识别");
                let _ = self.restore_secondary_current_hall()?;
                return Ok(());
            }

            if !discard_only {
                let frame = self.wait_for_secondary_bubble_stability()?;
                match self.secondary_identity_from_frame(&frame.image)? {
                    SecondaryChatIdentity::Friend(name) => {
                        self.process_secondary_latest_message(
                            &frame.image,
                            frame.captured_at,
                            "pink",
                            &name,
                        )?;
                    }
                    SecondaryChatIdentity::Unknown => {
                        self.process_secondary_latest_message(
                            &frame.image,
                            frame.captured_at,
                            "pink",
                            "二级好友",
                        )?;
                    }
                    SecondaryChatIdentity::CurrentHall
                    | SecondaryChatIdentity::PublicChannel
                    | SecondaryChatIdentity::StrangerMessages => {
                        log::warn!("二级监听好友未读: 打开后不是可执行好友会话，跳过 OCR");
                    }
                }
            }
            if !self.restore_secondary_current_hall()? {
                return Err(anyhow!("二级监听好友未读后未能回到当前大厅"));
            }
            Ok(())
        })();

        self.chat_listener.finish_unread_task(!discard_only);
        self.update_monitor_chat_listener();
        if result.is_err() {
            self.chat_listener.fail_mode_switch_to_primary();
            self.update_monitor_chat_listener();
            let _ = self.return_to_primary_fixed();
        }
        result
    }

    fn execute_restore_secondary_hall_task(&mut self) -> Result<()> {
        let result = self.restore_secondary_current_hall();
        self.chat_listener.finish_unread_task(false);
        self.update_monitor_chat_listener();
        match result {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => {
                self.chat_listener.fail_mode_switch_to_primary();
                self.update_monitor_chat_listener();
                let _ = self.return_to_primary_fixed();
                Err(anyhow!("二级监听无法恢复当前大厅，已回退一级监听"))
            }
        }
    }

    fn restore_listener_residency_after_task(&self, task_label: &str) -> Result<()> {
        match self.active_ui_residency() {
            UiResidency::Primary => self.ensure_ui_residency(
                UiResidency::Primary,
                &format!("任务结束恢复一级界面: {}", task_label),
            ),
            UiResidency::SecondaryCurrentHall => {
                self.restore_secondary_listener_after_task(task_label)
            }
        }
    }

    fn restore_secondary_listener_after_task(&self, task_label: &str) -> Result<()> {
        match self.restore_secondary_current_hall() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(error) if is_target_window_unavailable_error(&error) => return Err(error),
            Err(error) => {
                log::error!("二级监听恢复过程异常 {}: {error:#}", task_label);
            }
        }
        self.chat_listener.fail_mode_switch_to_primary();
        self.update_monitor_chat_listener();
        let _ = self.return_to_primary_fixed();
        let message = format!("二级监听恢复失败，已回退一级监听: {}", task_label);
        log::error!("{}", message);
        if let Err(error) = self.reply(&message) {
            log::error!("二级监听回退异常信息发送失败: {error:#}");
        }
        Ok(())
    }

    fn open_secondary_current_hall(&self) -> Result<bool> {
        for attempt in 1..=2 {
            if !self.ensure_secondary_chat_open("进入二级监听")? {
                continue;
            }
            if self.secondary_title_is_current_hall()? {
                return Ok(true);
            }
            if self.click_secondary_hall_template()? && self.secondary_title_is_current_hall()? {
                return Ok(true);
            }
            log::warn!("二级监听进入 attempt={}/2 未确认当前大厅", attempt);
            if attempt < 2 {
                let _ = self.return_to_primary_from_transient_ui("二级监听第二次进入前重置");
            }
        }
        Ok(false)
    }

    fn ensure_secondary_chat_open(&self, context: &str) -> Result<bool> {
        for attempt in 1..=2 {
            self.ensure_game_ready_for_input(context)?;
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let frame_args = FrameArgs { image: None };
            let templates = UiTemplateArgs::default().resolve(&self.config);
            let frame = load_frame(&frame_args, &canvas, &self.game_ui)?;
            let mut ui_state = detect_ui_state(&frame.image, &templates, &self.config.screen)?;
            if ui_state.is_secondary() {
                return Ok(true);
            }
            if !ui_state.is_primary() {
                log::warn!(
                    "{}: 当前界面为 {}，先恢复一级 attempt={}/2",
                    context,
                    ui_state,
                    attempt
                );
                if !self.return_to_primary_from_transient_ui(context) {
                    continue;
                }
                let frame = load_frame(&frame_args, &canvas, &self.game_ui)?;
                ui_state = detect_ui_state(&frame.image, &templates, &self.config.screen)?;
                if !ui_state.is_primary() {
                    continue;
                }
            }

            log::info!(
                "{}: 按 Enter 打开二级聊天界面 attempt={}/2",
                context,
                attempt
            );
            self.game_ui.press_key(Key::Return)?;
            sleep(Duration::from_millis(self.config.timing.input.open_chat_ms));
            let frame = load_frame(&frame_args, &canvas, &self.game_ui)?;
            let ui_state = detect_ui_state(&frame.image, &templates, &self.config.screen)?;
            if ui_state.is_secondary() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn restore_secondary_current_hall(&self) -> Result<bool> {
        self.ensure_game_ready_for_input("二级大厅恢复")?;
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let frame = load_frame(&FrameArgs { image: None }, &canvas, &self.game_ui)?;
        let templates = UiTemplateArgs::default().resolve(&self.config);
        let ui_state = detect_ui_state(&frame.image, &templates, &self.config.screen)?;
        if ui_state.is_secondary() {
            if matches!(
                self.secondary_identity_from_frame(&frame.image)?,
                SecondaryChatIdentity::CurrentHall
            ) {
                return Ok(true);
            }
            return Ok(
                self.click_secondary_hall_template()? && self.secondary_title_is_current_hall()?
            );
        }
        self.open_secondary_current_hall()
    }

    fn click_secondary_hall_template(&self) -> Result<bool> {
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let templates = UiTemplateArgs::default().resolve(&self.config);
        let search_rect = secondary_hall_search_rect(
            self.config.screen.secondary_hall_rect.into(),
            self.config.invite.friend_list_region.into(),
        );
        let friend_list: Rect = self.config.invite.friend_list_region.into();
        let locator = UiLocator::new(
            canvas,
            FrameArgs { image: None },
            self.game_ui.clone(),
            self.config.timing.workflow.default_poll_ms,
        );
        let hit = workflow_actions::click_scrollable_template(
            &locator,
            &templates.secondary_hall_template,
            search_rect,
            friend_list,
            templates.chat_templates.marker_threshold,
            workflow_actions::ScrollTemplateOptions {
                max_scrolls: 3,
                scroll_length: -8,
                settle_ms: self.config.timing.input.click_ms,
            },
            || self.running.load(AtomicOrdering::SeqCst),
        )?;
        if let Some(hit) = hit {
            let point = hit.center();
            log::info!(
                "二级大厅恢复: 已点击当前大厅模板 {},{} score={:.3}",
                point.x,
                point.y,
                hit.score
            );
            sleep(Duration::from_millis(self.config.timing.input.click_ms));
            return Ok(true);
        }
        log::warn!("二级大厅恢复: 滚动好友列表后仍未找到当前大厅模板");
        Ok(false)
    }

    fn secondary_title_is_current_hall(&self) -> Result<bool> {
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let frame = load_frame(&FrameArgs { image: None }, &canvas, &self.game_ui)?;
        Ok(matches!(
            self.secondary_identity_from_frame(&frame.image)?,
            SecondaryChatIdentity::CurrentHall
        ))
    }

    fn secondary_identity_from_frame(&self, image: &DynamicImage) -> Result<SecondaryChatIdentity> {
        let crop = crop_canvas(image, chat_listener::SECONDARY_TITLE_RECT)?;
        let title = self.ocr.merged_text(
            crop,
            self.config.ocr.same_line_y_tolerance,
            OcrPriority::ChatObservation,
        )?;
        log::debug!("二级监听顶部标题 OCR: {}", title);
        Ok(classify_title(&title))
    }

    fn begin_chat_decision_reader<A, P>(
        &self,
        scope: ChatDecisionScope,
        accepts_message_type: &A,
        is_decision: &P,
    ) -> Result<ChatDecisionReader>
    where
        A: Fn(&str) -> bool,
        P: Fn(&str) -> bool,
    {
        let observation_session = self.chat_observations.begin_exclusive()?;
        let use_secondary = scope == ChatDecisionScope::CurrentHall
            && self.active_ui_residency() == UiResidency::SecondaryCurrentHall;
        if use_secondary {
            self.ensure_ui_residency(
                UiResidency::SecondaryCurrentHall,
                "建立二级当前大厅确认基线",
            )?;
            let canvas = Canvas {
                width: self.config.screen.expected_width,
                height: self.config.screen.expected_height,
                resize: true,
            };
            let frame = load_frame(&FrameArgs { image: None }, &canvas, &self.game_ui)?;
            let previous = secondary_hall_bubbles(&frame.image)?;
            return Ok(ChatDecisionReader {
                kind: ChatDecisionReaderKind::SecondaryCurrentHall { previous },
                screen_lock: DecisionScreenLock::default(),
                _observation_session: observation_session,
            });
        }

        self.ensure_ui_residency(UiResidency::Primary, "建立一级聊天确认基线")?;
        let template_args = TemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let frame = load_frame(&FrameArgs { image: None }, &canvas, &self.game_ui)?;
        let messages = self.scan_chat_with_shared_ocr(&frame.image, &template_args)?;
        Ok(ChatDecisionReader {
            kind: ChatDecisionReaderKind::Primary,
            screen_lock: DecisionScreenLock::from_messages(
                &messages,
                accepts_message_type,
                is_decision,
            ),
            _observation_session: observation_session,
        })
    }

    fn poll_chat_decision_reader(
        &self,
        reader: &mut ChatDecisionReader,
    ) -> Result<Vec<ChatMessage>> {
        match &mut reader.kind {
            ChatDecisionReaderKind::Primary => {
                let template_args = TemplateArgs::default().resolve(&self.config);
                let canvas = Canvas {
                    width: self.config.screen.expected_width,
                    height: self.config.screen.expected_height,
                    resize: true,
                };
                let frame = load_frame(&FrameArgs { image: None }, &canvas, &self.game_ui)?;
                self.scan_chat_with_shared_ocr(&frame.image, &template_args)
            }
            ChatDecisionReaderKind::SecondaryCurrentHall { previous } => {
                self.scan_secondary_decision_messages(previous)
            }
        }
    }

    fn scan_secondary_decision_messages(
        &self,
        previous: &mut Vec<SecondaryHallBubble>,
    ) -> Result<Vec<ChatMessage>> {
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let frame = load_frame(&FrameArgs { image: None }, &canvas, &self.game_ui)?;
        let current = secondary_hall_bubbles(&frame.image)?;
        let Some(start) = secondary_new_bubble_start(previous, &current) else {
            *previous = current;
            log::debug!("二级确认气泡序列失去重叠，已重建基线");
            return Ok(Vec::new());
        };
        if start >= current.len() {
            *previous = current;
            return Ok(Vec::new());
        }

        let refreshed = self.wait_for_secondary_bubble_stability()?;
        let refreshed_bubbles = secondary_hall_bubbles(&refreshed.image)?;
        let Some(start) = secondary_new_bubble_start(previous, &refreshed_bubbles) else {
            *previous = refreshed_bubbles;
            log::debug!("二级确认气泡稳定后失去重叠，已重建基线");
            return Ok(Vec::new());
        };
        let rects = refreshed_bubbles[start..]
            .iter()
            .map(|bubble| bubble.rect)
            .collect::<Vec<_>>();
        let messages = self.recognize_secondary_hall_messages(&refreshed.image, &rects)?;
        *previous = refreshed_bubbles;
        Ok(messages)
    }

    fn recognize_secondary_hall_messages(
        &self,
        image: &DynamicImage,
        rects: &[Rect],
    ) -> Result<Vec<ChatMessage>> {
        let started = Instant::now();
        let mut messages = Vec::with_capacity(rects.len());
        for rect in rects {
            let crop = crop_canvas(image, *rect)?;
            let text = self.ocr.merged_text(
                crop,
                self.config.ocr.same_line_y_tolerance,
                OcrPriority::ChatObservation,
            )?;
            messages.push(ChatMessage {
                message_type: "blue".to_string(),
                block: *rect,
                text,
                visual: rect_chat_change_fingerprint(image, *rect)?,
            });
        }
        let ocr_ms = elapsed_ms(started);
        self.monitor.set_ocr(OcrSnapshot::new(
            messages.len(),
            messages
                .iter()
                .map(|message| format!("[blue] {}", redacted_chat_text(&message.text)))
                .collect(),
            0,
            ocr_ms,
            ocr_ms,
            "二级当前大厅",
        ));
        Ok(messages)
    }

    fn process_secondary_latest_message(
        &mut self,
        image: &DynamicImage,
        captured_at: Instant,
        message_type: &str,
        friend_name: &str,
    ) -> Result<bool> {
        let Some(rect) = latest_incoming_bubble_rect(image) else {
            return Ok(false);
        };
        Ok(self
            .process_secondary_bubble_rects(
                image,
                captured_at,
                std::iter::once(rect),
                message_type,
                friend_name,
            )?
            .processed)
    }

    fn process_secondary_bubble_rects(
        &mut self,
        image: &DynamicImage,
        captured_at: Instant,
        rects: impl IntoIterator<Item = Rect>,
        message_type: &str,
        friend_name: &str,
    ) -> Result<SecondaryBubbleProcessOutcome> {
        let started = Instant::now();
        let observation_frame = self.chat_observations.begin_frame(captured_at)?;
        let ocr_started = Instant::now();
        let accepts_turtle_questions = message_type == "blue"
            && self.commands_enabled.load(AtomicOrdering::SeqCst)
            && self.turtle_soup.accepts_questions();
        let captures_hash_sender =
            message_type == "blue" && self.commands_enabled.load(AtomicOrdering::SeqCst);
        let texts = (|| -> Result<Vec<(Rect, String, Option<String>)>> {
            let mut texts = Vec::new();
            for rect in rects {
                let crop = crop_canvas(image, rect)?;
                let text = self.ocr.merged_text(
                    crop,
                    self.config.ocr.same_line_y_tolerance,
                    OcrPriority::ChatObservation,
                )?;
                let trimmed_text = text.trim_start();
                let starts_with_hash =
                    trimmed_text.starts_with('#') || trimmed_text.starts_with('＃');
                let message_sender = if captures_hash_sender && starts_with_hash {
                    let sender_rect = secondary_message_sender_rect(image, rect);
                    let crop = crop_canvas(image, sender_rect)?;
                    Some(self.ocr.merged_text(
                        crop,
                        self.config.ocr.same_line_y_tolerance,
                        OcrPriority::ChatObservation,
                    )?)
                } else {
                    None
                };
                texts.push((rect, text, message_sender));
            }
            Ok(texts)
        })();
        let texts = match texts {
            Ok(texts) => texts,
            Err(error) => {
                if let Err(record_error) = self
                    .chat_observations
                    .record_terminal_failure(observation_frame, format!("{error:#}"))
                {
                    log::error!("记录二级聊天观察终止失败异常: {record_error:#}");
                }
                return Err(error);
            }
        };
        let ocr_ms = elapsed_ms(ocr_started);
        self.monitor.set_ocr(OcrSnapshot::new(
            texts.len(),
            texts
                .iter()
                .map(|(_, text, _)| format!("[{}] {}", message_type, redacted_chat_text(text)))
                .collect(),
            0,
            ocr_ms,
            elapsed_ms(started),
            if message_type == "pink" {
                "二级好友私聊"
            } else {
                "二级当前大厅"
            },
        ));

        let texts = if accepts_turtle_questions {
            let observations = texts
                .into_iter()
                .map(|(_, text, message_sender)| SecondaryOcrObservation {
                    text,
                    player: message_sender.unwrap_or_default(),
                })
                .collect::<Vec<_>>();
            match self.turtle_soup.stabilize_secondary_ocr(observations) {
                SecondaryOcrStability::Pending => {
                    self.chat_observations
                        .complete_without_messages(observation_frame)?;
                    return Ok(SecondaryBubbleProcessOutcome {
                        processed: false,
                        ocr_pending: true,
                    });
                }
                SecondaryOcrStability::Stable(observations) => observations
                    .into_iter()
                    .map(|observation| (observation.text, Some(observation.player)))
                    .collect::<Vec<_>>(),
            }
        } else {
            self.turtle_soup.clear_secondary_ocr_stability();
            texts
                .into_iter()
                .map(|(_, text, message_sender)| (text, message_sender))
                .collect::<Vec<_>>()
        };

        let messages = texts
            .into_iter()
            .map(|(text, sender)| SecondaryRecognizedMessage { text, sender })
            .collect();
        let dispatches = self.chat_observations.publish_secondary(
            observation_frame,
            message_type,
            friend_name,
            accepts_turtle_questions,
            messages,
        )?;
        let processed = self.dispatch_chat_observations(dispatches)?;
        Ok(SecondaryBubbleProcessOutcome {
            processed,
            ocr_pending: false,
        })
    }

    fn process_secondary_chat_observation(
        &self,
        observation: SecondaryChatObservation,
    ) -> Result<bool> {
        let SecondaryChatObservation {
            message_type,
            friend_name,
            accepts_turtle_questions,
            messages,
        } = observation;
        let mut processed = false;
        for SecondaryObservedMessage {
            id: message_id,
            text,
            sender: message_sender,
        } in messages
        {
            log::debug!("处理二级观察消息: id={message_id:?}");
            let shortcut_player = if message_type == "pink" {
                friend_name.trim()
            } else {
                message_sender.as_deref().map(str::trim).unwrap_or_default()
            };
            if !shortcut_player.is_empty() {
                let synthetic = if message_type == "pink" {
                    format!("[{}]：{}", shortcut_player, text.trim())
                } else {
                    format!("{}：{}", shortcut_player, text.trim())
                };
                if let Some(parsed) = command::parse_entertainment_shortcut(
                    &synthetic,
                    &message_type,
                    self.entertainment.active(),
                ) {
                    self.submit_secondary_command(parsed)?;
                    processed = true;
                    continue;
                }
            }
            if accepts_turtle_questions {
                let question = message_sender
                    .as_deref()
                    .map(str::trim)
                    .filter(|player| !player.is_empty())
                    .and_then(|player| turtle_soup::parse_question_message(&text, Some(player)));
                if let Some(question) = question {
                    processed |= self.handle_turtle_soup_question(question)?;
                    continue;
                }
            }
            let Some(index) = text.find('@') else {
                log::debug!("二级监听气泡不是命令: {}", redacted_chat_text(&text));
                continue;
            };
            let command_text = text[index..].trim();
            let synthetic = if message_type == "pink" {
                let username = if friend_name.trim().is_empty() {
                    "二级好友"
                } else {
                    friend_name.trim()
                };
                format!("[{}]：{}", username, command_text)
            } else {
                format!("二级大厅：{}", command_text)
            };
            let parsed = command::parse_text(&synthetic, &message_type).or_else(|| {
                custom_workflow::parse_text(
                    &self.config.custom_workflows,
                    &synthetic,
                    &message_type,
                )
            });
            let Some(parsed) = parsed else {
                log::debug!("二级监听气泡未解析为命令");
                continue;
            };
            self.submit_secondary_command(parsed)?;
            processed = true;
        }
        Ok(processed)
    }

    fn wait_for_secondary_bubble_stability(&self) -> Result<frame_source::Frame> {
        const STABILITY_TIMEOUT_MS: u64 = 500;

        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let frame_args = FrameArgs { image: None };
        let first = load_frame(&frame_args, &canvas, &self.game_ui)?;
        let mut previous = latest_incoming_fingerprint(&first.image)?;
        let mut latest_frame = first;
        let poll_ms = self
            .config
            .timing
            .chat_scan
            .change_debounce_ms
            .clamp(100, 200);
        let deadline = Instant::now() + Duration::from_millis(STABILITY_TIMEOUT_MS);

        while Instant::now() < deadline {
            sleep(Duration::from_millis(poll_ms));
            let frame = load_frame(&frame_args, &canvas, &self.game_ui)?;
            let current = latest_incoming_fingerprint(&frame.image)?;
            if !secondary_optional_fingerprint_changed(previous.as_ref(), current.as_ref()) {
                return Ok(frame);
            }
            previous = current;
            latest_frame = frame;
        }
        log::debug!("二级监听气泡稳定等待超时，按当前画面继续 OCR");
        Ok(latest_frame)
    }

    fn execute_start_game_task(&mut self, source: &'static str) -> Result<()> {
        log::info!("执行启动游戏任务: {}", source);
        self.abort_entertainment_for_context_loss("启动游戏任务将重建聊天上下文");
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        let window_detection_signal = self.window_detection_signal.clone();
        window_detection_signal.request("启动游戏任务开始")?;
        game_startup::start_game(
            &config,
            &self.game_ui,
            &self.ocr,
            || running.load(AtomicOrdering::SeqCst),
            |reason| {
                if let Err(error) = window_detection_signal.request(reason) {
                    log::error!("请求重置窗口检测退避失败: {error:#}");
                }
            },
        )
    }

    fn execute_enter_wonderland_task(&mut self, source: &'static str) -> Result<()> {
        log::info!("执行进入千星任务: {}", source);
        self.abort_entertainment_for_context_loss("进入千星任务将切换大厅");
        let config = self.config.clone();
        let running = Arc::clone(&self.running);
        self.window_detection_signal.request("进入千星任务开始")?;
        startup_flow::enter_wonderland(&config, &self.game_ui, || {
            running.load(AtomicOrdering::SeqCst)
        })?;
        log::info!("进入千星完成信号已确认，执行返回一级界面");
        let returned = self.return_to_primary_fixed();
        log::info!(
            "进入千星完成后返回一级界面结束，后续待处理任务将继续执行: returned_primary={}",
            returned
        );
        self.window_detection_signal.request("进入千星任务完成")?;
        Ok(())
    }

    fn execute_pending_command(&mut self, pending: PendingCommand) -> Result<()> {
        self.ensure_game_ready_for_input("命令执行前准备")?;
        let command_log = private_safe_command_log(&pending.parsed);
        log::info!(
            "执行待处理命令: {} lock={}",
            command_log,
            if is_private_undercover_input(&pending.parsed) {
                "[hidden]"
            } else {
                pending.lock_key.as_str()
            }
        );
        let _console_reply_context = if pending.parsed.message_type == "控制台" {
            Some(ConsoleReplyContextGuard::new(Arc::clone(
                &self.console_reply_context,
            )))
        } else {
            None
        };
        let command_started = Instant::now();
        match self.execute_command(&pending.parsed) {
            Ok(()) => {
                let command_ms = elapsed_ms(command_started);
                log::info!("命令执行完成: {}", command_log);
                log::info!(target: "timing",
                    "命令执行耗时: command={} success=true total={}ms",
                    command_log,
                    command_ms
                );
            }
            Err(error) => {
                let command_ms = elapsed_ms(command_started);
                log::error!("命令执行失败 {}: {error:#}", command_log);
                log::info!(target: "timing",
                    "命令执行耗时: command={} success=false total={}ms",
                    command_log,
                    command_ms
                );
                return Err(error);
            }
        }
        Ok(())
    }

    fn log_executed_command(&self, parsed: &ParsedCommand, final_command: &str) -> Result<()> {
        self.monitor
            .push_command(format!("{} -> {}", parsed.user_command, final_command));
        let path = &self.config.state.executed_commands_log_path;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("create command log directory {}", parent.display()))?;
        }
        let line = format!(
            "{}-{}-{}-{}-{}\n",
            command_log_timestamp(),
            command_log_field(command_location(&parsed.message_type)),
            command_log_field(command_username(parsed)),
            command_log_field(&parsed.user_command),
            command_log_field(final_command),
        );
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open command log {}", path.display()))?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("write command log {}", path.display()))
    }

    fn pending_contains_command(&self, parsed: &ParsedCommand) -> Result<bool> {
        let (lock, _) = &*self.pending;
        let guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
        Ok(guard.iter().any(|task| task.same_lock_command(parsed)))
    }

    fn executor_is_idle(&self) -> Result<bool> {
        let (lock, _) = &*self.pending;
        let guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
        Ok(guard.is_empty() && !self.command_executing.load(AtomicOrdering::SeqCst))
    }

    fn push_pending_task(&self, task: PendingTask) -> Result<()> {
        let (lock, cvar) = &*self.pending;
        let mut guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
        let id = self.task_tracker.create(task.label())?;
        guard.push_back(TrackedPendingTask { id, task });
        cvar.notify_one();
        Ok(())
    }

    fn push_pending_task_front(&self, task: TrackedPendingTask) -> Result<()> {
        let (lock, cvar) = &*self.pending;
        let mut guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
        self.task_tracker.mark_queued(task.id);
        guard.push_front(task);
        cvar.notify_one();
        Ok(())
    }

    fn enqueue_startup_task_if_enabled(&self) -> Result<()> {
        if !self.config.startup.enabled {
            return Ok(());
        }
        if self.config.startup.launch_game || self.config.startup.enter_game {
            self.push_pending_task(PendingTask::StartGame {
                source: "启动配置"
            })?;
        }
        if self.config.startup.enter_wonderland {
            self.push_pending_task(PendingTask::EnterWonderland {
                source: "启动配置"
            })?;
        }
        Ok(())
    }

    fn ensure_game_ready_for_input(&self, context: &str) -> Result<()> {
        log::info!("{}: 激活并聚焦游戏窗口", context);
        self.game_ui
            .ensure_ready(self.config.timing.input.after_activate_ms)
            .with_context(|| format!("{}: 激活并聚焦游戏窗口失败", context))
    }

    fn active_ui_residency(&self) -> UiResidency {
        let snapshot = self.chat_listener.snapshot();
        listener_residency(snapshot.mode, snapshot.temporary_primary)
    }

    fn ensure_ui_residency(&self, target: UiResidency, context: &str) -> Result<()> {
        match target {
            UiResidency::Primary => match self.prepare_command_ui(context)? {
                true => Ok(()),
                false => Err(anyhow!("{}: 未能到达一级界面", context)),
            },
            UiResidency::SecondaryCurrentHall => {
                if self.restore_secondary_current_hall()? {
                    Ok(())
                } else {
                    Err(anyhow!("{}: 未能到达二级当前大厅", context))
                }
            }
        }
    }

    fn prepare_command_ui(&self, command: &str) -> Result<bool> {
        self.ensure_game_ready_for_input("命令执行前准备")?;
        let templates = UiTemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let frame_args = FrameArgs { image: None };
        let deadline =
            Instant::now() + Duration::from_millis(self.config.timing.command.ui_timeout_ms);
        let mut allow_transition_wait = true;
        let mut primary_region_stability = PrimaryRegionStability::default();
        let mut primary_stability_required = false;

        while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            let frame = load_frame(&frame_args, &canvas, &self.game_ui)?;
            let ui_state = detect_ui_state(&frame.image, &templates, &self.config.screen)?;
            let observation = primary_return_observation(&ui_state);
            if observation != PrimaryReturnObservation::Primary {
                primary_stability_required = true;
            }
            let primary_region_readiness = primary_region_stability.observe(
                rect_chat_change_fingerprint(&frame.image, self.config.screen.friend_rect.into())?,
                Instant::now(),
                self.config.ocr.change_mean_threshold,
                self.config.ocr.change_pixel_threshold,
            );
            match primary_return_action(
                observation,
                allow_transition_wait,
                primary_region_ready_for_command(
                    primary_stability_required,
                    primary_region_readiness,
                ),
            ) {
                PrimaryReturnAction::Complete => {
                    if primary_region_readiness == PrimaryRegionReadiness::TimedOut {
                        log::warn!(
                            "命令执行前好友按钮区域持续变化 {}ms，按当前一级界面识别结果继续",
                            PRIMARY_REGION_STABILITY_TIMEOUT_MS
                        );
                    }
                    log::info!("命令执行前界面: {}", ui_state);
                    return Ok(true);
                }
                PrimaryReturnAction::WaitForPrimaryStability => {
                    sleep(Duration::from_millis(PRIMARY_REGION_STABILITY_POLL_MS));
                    continue;
                }
                PrimaryReturnAction::WaitForTransition => {
                    let wait_ms = self.config.timing.command.post_settle_ms;
                    log::info!(
                        "命令执行前界面: {}，等待界面过渡后重新检测: {}ms",
                        ui_state,
                        wait_ms
                    );
                    sleep(Duration::from_millis(wait_ms));
                    allow_transition_wait = false;
                    continue;
                }
                PrimaryReturnAction::PressEscape => {}
            }

            log::info!("命令执行前界面: {}，按 ESC 返回一级: {}", ui_state, command);
            self.game_ui.press_key(Key::Escape)?;
            sleep(Duration::from_millis(
                self.config.timing.command.return_retry_ms,
            ));
            primary_region_stability.reset();
            primary_stability_required = true;
            allow_transition_wait = allow_primary_transition_wait_after_escape();
        }

        Ok(false)
    }

    fn execute_advance_queue_task(&mut self, reason: &'static str) -> Result<()> {
        self.consume_queue(reason)
    }

    fn run_playback_monitor_loop(&mut self) {
        let tick_ms = self.config.timing.playback.monitor_tick_ms.max(50);
        let status_ms = self.config.timing.playback.monitor_status_ms.max(tick_ms);
        let mut snapshot: Option<PlaybackSnapshot> = None;
        let mut next_status_at = Instant::now();

        while self.running.load(AtomicOrdering::SeqCst) {
            if self.paused.load(AtomicOrdering::SeqCst) {
                sleep(Duration::from_millis(tick_ms));
                continue;
            }

            let now = Instant::now();
            if snapshot.is_none() || now >= next_status_at {
                match self.player.status() {
                    Ok(status) => {
                        snapshot = Some(PlaybackSnapshot {
                            status,
                            captured_at: now,
                        });
                        next_status_at = now + Duration::from_millis(status_ms);
                    }
                    Err(error) => {
                        log::error!("播放监控状态查询失败: {error:#}");
                        snapshot = None;
                        next_status_at = now + Duration::from_millis(status_ms);
                    }
                }
            }

            if let Some(playback_snapshot) = snapshot.as_ref() {
                match self.handle_playback_monitor_snapshot(playback_snapshot) {
                    Ok(true) => {
                        if let Ok(status) = self.player.status() {
                            snapshot = Some(PlaybackSnapshot {
                                status,
                                captured_at: Instant::now(),
                            });
                        } else {
                            snapshot = None;
                        }
                        next_status_at = Instant::now() + Duration::from_millis(status_ms);
                    }
                    Ok(false) => {}
                    Err(error) => {
                        log::error!("播放监控处理失败: {error:#}");
                        next_status_at = Instant::now() + Duration::from_millis(status_ms);
                    }
                }
            }

            sleep(Duration::from_millis(tick_ms));
        }
    }

    fn handle_playback_monitor_snapshot(&mut self, snapshot: &PlaybackSnapshot) -> Result<bool> {
        let context = QueueAdvanceContext {
            queue_empty: self.queue()?.is_empty(),
            has_pending_playback_task: self.has_pending_playback_task()?,
            command_executing: self.command_executing.load(AtomicOrdering::SeqCst),
            song_command_executing: self.song_command_executing.load(AtomicOrdering::SeqCst),
        };
        let decision = self
            .player
            .maybe_advance_queue(estimated_player_status(snapshot), context)?;
        self.update_monitor_playback_controller();
        match decision {
            QueueAdvanceDecision::None => Ok(false),
            QueueAdvanceDecision::PlaybackStateChanged
            | QueueAdvanceDecision::PauseForQueue
            | QueueAdvanceDecision::ResumeIfIdle => Ok(true),
            QueueAdvanceDecision::AdvanceQueue { reason } => {
                self.push_pending_task(PendingTask::AdvanceQueue { reason })?;
                Ok(true)
            }
        }
    }

    fn has_pending_playback_task(&self) -> Result<bool> {
        let (lock, _) = &*self.pending;
        let guard = lock.lock().map_err(|_| anyhow!("pending mutex poisoned"))?;
        Ok(guard.iter().any(TrackedPendingTask::is_playback_task))
    }

    fn maybe_warn_hall_expiring(&mut self) -> Result<bool> {
        if !self.executor_is_idle()? {
            return Ok(false);
        }
        let minutes = {
            let runtime_state = self.runtime_state()?;
            let state = runtime_state.state();
            if state.hall_expiring_warning_sent {
                return Ok(false);
            }
            let Some(minutes) = state.hall_remaining_minutes_now() else {
                return Ok(false);
            };
            if minutes > HALL_EXPIRING_WARNING_MINUTES {
                return Ok(false);
            }
            minutes
        };

        let message = if minutes == 0 {
            "大厅即将到期".to_string()
        } else {
            format!("大厅即将到期，剩余{}分钟", minutes)
        };
        self.reply(&message)?;

        let mut runtime_state = self.runtime_state()?;
        runtime_state.state_mut().hall_expiring_warning_sent = true;
        runtime_state.save()?;
        Ok(true)
    }

    fn clear_hall_countdown_cache_for_new_visual_session(&self, reason: &str) -> Result<bool> {
        let cleared = {
            let mut runtime_state = self.runtime_state()?;
            let cleared = runtime_state.state_mut().clear_hall_countdown_cache();
            if cleared {
                runtime_state.save()?;
            }
            cleared
        };
        let visual_session = self.chat_observations.begin_visual_session()?;
        if cleared {
            log::info!("{reason}，已清理大厅倒计时缓存，等待本次大厅检测重新确认");
        }
        log::info!("{reason}，聊天观察进入新视觉会话: {}", visual_session.get());
        Ok(cleared)
    }

    fn resolve_song_request(
        &mut self,
        song: &command::SongCommand,
    ) -> Result<Option<ResolvedSongRequest>> {
        if !song.ai_assisted {
            return Ok(Some(ResolvedSongRequest {
                keyword: song.keyword.clone(),
                source: song.source.as_str().to_string(),
                prefer_accompaniment: song.prefer_accompaniment,
                ai_original_text: String::new(),
                uri: String::new(),
                skip_match_check: false,
                friend_username: song.friend_username.clone(),
                console_bypass_dedup: false,
            }));
        }
        let label = song_label(song);
        if !self.ai.enabled() {
            self.reply(&format!("{}AI点歌未启用，请先配置 ai.api_key", label))?;
            return Ok(None);
        }

        self.reply(&format!("{}AI匹配中", label))?;

        let search_source = ai_candidate_source(song);
        let candidates = match self.player.search_candidates(&song.keyword, search_source) {
            Ok(candidates) => candidates,
            Err(error) => {
                log::error!("AI点歌搜索候选失败: {error:#}");
                self.reply(&format!("{}平台无对应歌曲音源", label))?;
                return Ok(None);
            }
        };
        if candidates.is_empty() {
            self.reply(&format!("{}平台无对应歌曲音源", label))?;
            return Ok(None);
        }

        let pick =
            match self
                .ai
                .pick_song_candidate(&song.keyword, song.prefer_accompaniment, &candidates)
            {
                Ok(pick) => pick,
                Err(error) => {
                    log::error!("AI点歌选择候选失败: {error:#}");
                    self.reply(&format!("{}AI点歌识别失败", label))?;
                    return Ok(None);
                }
            };
        let Some(candidate) = candidates
            .iter()
            .find(|candidate| candidate.uri == pick.uri)
        else {
            log::error!("AI点歌返回未知候选: {}", pick.uri);
            self.reply(&format!("{}AI点歌识别失败", label))?;
            return Ok(None);
        };
        log::info!(
            "AI点歌候选: raw={} pick={} uri={} score={:.2} reason={}",
            song.keyword,
            candidate.text,
            candidate.uri,
            pick.score,
            pick.reason
        );
        let message = format!("{}AI匹配:{},@确认@跳过", label, candidate.text);
        self.reply(&message)?;
        match self.wait_for_decision(false, false, true)? {
            UserDecision::Confirm | UserDecision::Timeout => {}
            UserDecision::Skip => return Ok(None),
            UserDecision::PromptFailed | UserDecision::Stopped => return Ok(None),
            _ => return Ok(None),
        }
        Ok(Some(ResolvedSongRequest {
            keyword: candidate.text.clone(),
            source: String::new(),
            prefer_accompaniment: song.prefer_accompaniment,
            ai_original_text: song.keyword.clone(),
            uri: candidate.uri.clone(),
            skip_match_check: true,
            friend_username: song.friend_username.clone(),
            console_bypass_dedup: false,
        }))
    }

    fn resolve_and_confirm_song(
        &mut self,
        song: &command::SongCommand,
    ) -> Result<Option<ResolvedSongRequest>> {
        let Some(request) = self.resolve_song_request(song)? else {
            return Ok(None);
        };
        if request.uri.is_empty() {
            let source = if request.source.trim().is_empty() {
                "qqmusic"
            } else {
                &request.source
            };
            let picked = match self.player.search_and_pick(
                &request.keyword,
                source,
                request.prefer_accompaniment,
            ) {
                Ok(Some(picked)) => picked,
                _ => {
                    let actions = if self.ai.enabled() {
                        "@换源@AI"
                    } else {
                        "@换源"
                    };
                    self.reply(&format!(
                        "{}平台无对应歌曲音源,{}",
                        request_label(&request),
                        actions
                    ))?;
                    let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
                    match decision {
                        UserDecision::SwitchSource => {
                            let next_source = if source == "netease" {
                                "qqmusic"
                            } else {
                                "netease"
                            };
                            return self.resolve_and_confirm_song_with_source(song, next_source);
                        }
                        UserDecision::Ai if self.ai.enabled() => {
                            return self.resolve_and_confirm_song_ai(song);
                        }
                        _ => return Ok(None),
                    }
                }
            };
            let song_title = picked.0.text.clone();
            let uri = picked.0.uri.clone();
            let actions = if self.ai.enabled() {
                "@确认@跳过@换源@AI"
            } else {
                "@确认@跳过@换源"
            };
            self.reply(&format!(
                "{}搜索到:{},{}",
                request_label(&request),
                song_title,
                actions
            ))?;
            let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
            match decision {
                UserDecision::Confirm | UserDecision::Timeout => {
                    return Ok(Some(ResolvedSongRequest {
                        keyword: picked.0.text.clone(),
                        source: source.to_string(),
                        prefer_accompaniment: request.prefer_accompaniment,
                        ai_original_text: String::new(),
                        uri,
                        skip_match_check: false,
                        friend_username: request.friend_username.clone(),
                        console_bypass_dedup: request.console_bypass_dedup,
                    }));
                }
                UserDecision::Skip => {
                    return Ok(None);
                }
                UserDecision::SwitchSource => {
                    let next_source = if source == "netease" {
                        "qqmusic"
                    } else {
                        "netease"
                    };
                    return self.resolve_and_confirm_song_with_source(song, next_source);
                }
                UserDecision::Ai if self.ai.enabled() => {
                    return self.resolve_and_confirm_song_ai(song);
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(request))
    }

    fn resolve_and_confirm_song_with_source(
        &mut self,
        song: &command::SongCommand,
        source: &str,
    ) -> Result<Option<ResolvedSongRequest>> {
        let picked =
            match self
                .player
                .search_and_pick(&song.keyword, source, song.prefer_accompaniment)
            {
                Ok(Some(picked)) => picked,
                _ => {
                    let actions = if self.ai.enabled() {
                        "@换源@AI"
                    } else {
                        "@换源"
                    };
                    self.reply(&format!("{}换源后仍无音源,{}", song_label(song), actions))?;
                    let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
                    match decision {
                        UserDecision::SwitchSource => {
                            let next_source = if source == "netease" {
                                "qqmusic"
                            } else {
                                "netease"
                            };
                            return self.resolve_and_confirm_song_with_source(song, next_source);
                        }
                        UserDecision::Ai if self.ai.enabled() => {
                            return self.resolve_and_confirm_song_ai(song);
                        }
                        _ => return Ok(None),
                    }
                }
            };
        let actions = if self.ai.enabled() {
            "@确认@跳过@换源@AI"
        } else {
            "@确认@跳过@换源"
        };
        self.reply(&format!(
            "{}搜索到:{},{}",
            song_label(song),
            picked.0.text,
            actions
        ))?;
        let decision = self.wait_for_decision(true, self.ai.enabled(), true)?;
        match decision {
            UserDecision::Confirm | UserDecision::Timeout => Ok(Some(ResolvedSongRequest {
                keyword: picked.0.text.clone(),
                source: source.to_string(),
                prefer_accompaniment: song.prefer_accompaniment,
                ai_original_text: String::new(),
                uri: picked.0.uri.clone(),
                skip_match_check: false,
                friend_username: song.friend_username.clone(),
                console_bypass_dedup: false,
            })),
            UserDecision::Skip => Ok(None),
            UserDecision::SwitchSource => {
                let next_source = if source == "netease" {
                    "qqmusic"
                } else {
                    "netease"
                };
                self.resolve_and_confirm_song_with_source(song, next_source)
            }
            UserDecision::Ai if self.ai.enabled() => self.resolve_and_confirm_song_ai(song),
            _ => Ok(None),
        }
    }

    fn resolve_and_confirm_song_ai(
        &mut self,
        song: &command::SongCommand,
    ) -> Result<Option<ResolvedSongRequest>> {
        let label = song_label(song);
        if !self.ai.enabled() {
            self.reply(&format!("{}AI点歌未启用", label))?;
            return Ok(None);
        }
        self.reply(&format!("{}AI匹配中", label))?;
        let search_source = ai_candidate_source(song);
        let candidates = match self.player.search_candidates(&song.keyword, search_source) {
            Ok(candidates) => candidates,
            Err(error) => {
                log::error!("AI点歌搜索候选失败: {error:#}");
                self.reply(&format!("{}平台无对应歌曲音源", label))?;
                return Ok(None);
            }
        };
        if candidates.is_empty() {
            self.reply(&format!("{}平台无对应歌曲音源", label))?;
            return Ok(None);
        }
        let pick =
            match self
                .ai
                .pick_song_candidate(&song.keyword, song.prefer_accompaniment, &candidates)
            {
                Ok(pick) => pick,
                Err(error) => {
                    log::error!("AI点歌选择候选失败: {error:#}");
                    self.reply(&format!("{}AI点歌识别失败", label))?;
                    return Ok(None);
                }
            };
        let Some(candidate) = candidates.iter().find(|c| c.uri == pick.uri) else {
            log::error!("AI点歌返回未知候选: {}", pick.uri);
            self.reply(&format!("{}AI点歌识别失败", label))?;
            return Ok(None);
        };
        log::info!(
            "AI点歌候选: raw={} pick={} uri={} score={:.2} reason={}",
            song.keyword,
            candidate.text,
            candidate.uri,
            pick.score,
            pick.reason
        );
        let message = format!("{}AI匹配:{},@确认@跳过", label, candidate.text);
        self.reply(&message)?;
        match self.wait_for_decision(false, false, true)? {
            UserDecision::Confirm | UserDecision::Timeout => {}
            UserDecision::Skip => return Ok(None),
            UserDecision::PromptFailed | UserDecision::Stopped => return Ok(None),
            _ => return Ok(None),
        }
        Ok(Some(ResolvedSongRequest {
            keyword: candidate.text.clone(),
            source: String::new(),
            prefer_accompaniment: song.prefer_accompaniment,
            ai_original_text: song.keyword.clone(),
            uri: candidate.uri.clone(),
            skip_match_check: true,
            friend_username: song.friend_username.clone(),
            console_bypass_dedup: false,
        }))
    }

    fn queue_contains_request(&self, request: &ResolvedSongRequest) -> Result<bool> {
        let queue = self.queue()?;
        if !request.uri.is_empty() {
            return Ok(queue.has_duplicate_uri(&request.uri));
        }
        Ok(queue.has_duplicate(
            &request.keyword,
            &request.source,
            request.prefer_accompaniment,
        ))
    }

    fn push_queue_request(&self, request: &ResolvedSongRequest) -> Result<QueuePushOutcome> {
        if self.song_dedup_limited(request)? {
            log::info!(
                "长时间同歌去重入队拦截: keyword={} uri={}",
                request.keyword,
                request.uri
            );
            return Ok(QueuePushOutcome::DedupLimited);
        }
        let mut queue = self.queue()?;
        if queue.is_full() {
            return Ok(QueuePushOutcome::Full);
        }
        queue.push(queue::QueueItem {
            id: 0,
            keyword: request.keyword.clone(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            ai_original_text: request.ai_original_text.clone(),
            uri: request.uri.clone(),
            friend_username: request.friend_username.clone(),
            dedup_bypass: request.console_bypass_dedup,
        })?;
        let len = queue.len();
        drop(queue);
        self.update_monitor_queue_snapshot();
        Ok(QueuePushOutcome::Added(len))
    }

    fn handle_queue_push_outcome(
        &self,
        parsed: &ParsedCommand,
        request: &ResolvedSongRequest,
        outcome: QueuePushOutcome,
        feedback: QueuePushFeedback,
    ) -> Result<()> {
        match outcome {
            QueuePushOutcome::Added(len) => {
                self.log_executed_command(
                    parsed,
                    &final_song_command_text(request, feedback.queued_action),
                )?;
                self.reply(&format!(
                    "{}({}/{}): {}",
                    feedback.queued_prefix, len, self.config.queue.max_size, request.keyword
                ))?;
            }
            QueuePushOutcome::Full => {
                self.log_executed_command(
                    parsed,
                    &final_song_command_text(request, feedback.full_action),
                )?;
                self.reply(feedback.full_reply)?;
            }
            QueuePushOutcome::DedupLimited => {
                self.log_executed_command(
                    parsed,
                    &final_song_command_text(request, "dedup-limited-queue"),
                )?;
                self.reply(&self.song_dedup_reject_message(request))?;
            }
        }
        Ok(())
    }

    fn log_play_request_outcome(
        &self,
        parsed: &ParsedCommand,
        request: &ResolvedSongRequest,
        outcome: PlaybackOutcome,
    ) -> Result<()> {
        let action = match outcome {
            PlaybackOutcome::Success => "play",
            PlaybackOutcome::NoSource => "no-source",
            PlaybackOutcome::Error => "play-error",
            PlaybackOutcome::DedupLimited => "dedup-limited",
        };
        self.log_executed_command(parsed, &final_song_command_text(request, action))
    }

    fn song_dedup_limited(&self, request: &ResolvedSongRequest) -> Result<bool> {
        if request.console_bypass_dedup && self.config.song_dedup.console_bypass {
            return Ok(false);
        }
        self.player
            .song_dedup_limited(&self.playback_request_from_resolved(request))
    }

    fn song_dedup_reject_message(&self, request: &ResolvedSongRequest) -> String {
        format!("{}近期已播放过,请稍后再点", request.keyword)
    }

    fn song_dedup_skip_message(&self, request: &ResolvedSongRequest) -> String {
        format!("{}近期已播放过,已跳过", request.keyword)
    }

    fn playback_request_from_resolved(&self, request: &ResolvedSongRequest) -> PlaybackRequest {
        PlaybackRequest {
            keyword: request.keyword.clone(),
            match_keyword: request.match_keyword().to_string(),
            source: request.source.clone(),
            prefer_accompaniment: request.prefer_accompaniment,
            uri: request.uri.clone(),
            skip_match_check: request.skip_match_check,
        }
    }

    fn review_song_candidate(
        &self,
        parsed: &ParsedCommand,
        request: &ResolvedSongRequest,
    ) -> Result<bool> {
        if !self.song_review.enabled() {
            return Ok(true);
        }
        if parsed.message_type == "控制台" {
            log::info!(
                "候选歌曲审核跳过: 控制台最高权限免审 command={} uri={}",
                parsed.raw,
                request.uri
            );
            return Ok(true);
        }

        let (title, artist) = song_review::split_candidate_title_artist(&request.keyword);
        let candidate = SongReviewCandidate {
            source: request.source.clone(),
            title,
            artist,
            uri: request.uri.clone(),
            message_type: parsed.message_type.clone(),
            username: command_username(parsed).to_string(),
        };
        let decision = self.song_review.review(&candidate);
        let level = song_review_level_text(decision.level);
        let reason = normalized_review_reason(&decision.reason);
        let tags = if decision.tags.is_empty() {
            "无".to_string()
        } else {
            decision.tags.join(",")
        };

        if decision.allowed {
            if decision.failed_open {
                log::warn!(
                    "候选歌曲审核放行: failure_policy=allow attempts={} threshold={} command={} title={} artist={} source={} uri={} reason={}",
                    decision.attempts,
                    decision.threshold,
                    parsed.raw,
                    candidate.title,
                    candidate.artist,
                    candidate.source,
                    candidate.uri,
                    reason
                );
            } else {
                log::info!(
                    "候选歌曲审核通过: level={} threshold={} attempts={} command={} title={} artist={} source={} uri={} reason={} tags={}",
                    level,
                    decision.threshold,
                    decision.attempts,
                    parsed.raw,
                    candidate.title,
                    candidate.artist,
                    candidate.source,
                    candidate.uri,
                    reason,
                    tags
                );
            }
            return Ok(true);
        }

        log::warn!(
            "候选歌曲审核拒绝: level={} threshold={} attempts={} command={} title={} artist={} source={} uri={} reason={} tags={}",
            level,
            decision.threshold,
            decision.attempts,
            parsed.raw,
            candidate.title,
            candidate.artist,
            candidate.source,
            candidate.uri,
            reason,
            tags
        );
        let action = decision.level.map_or_else(
            || "review-reject-failed".to_string(),
            |level| format!("review-reject-level-{level}"),
        );
        self.log_executed_command(parsed, &final_song_command_text(request, &action))?;
        self.reply(&review_reject_reply(
            &reason,
            self.song_review.reply_reason_max_chars(),
        ))?;
        Ok(false)
    }

    fn execute_command(&mut self, parsed: &ParsedCommand) -> Result<()> {
        match &parsed.command {
            UserCommand::Song(song) => {
                let Some(mut request) = self.resolve_and_confirm_song(song)? else {
                    return Ok(());
                };
                request.console_bypass_dedup = parsed.message_type == "控制台";
                if !self.review_song_candidate(parsed, &request)? {
                    return Ok(());
                }
                if self.queue_contains_request(&request)? {
                    log::info!("队列已有: {}", request.keyword);
                    self.log_executed_command(
                        parsed,
                        &final_song_command_text(&request, "duplicate"),
                    )?;
                    self.reply(&format!("队列已有: {}", request.keyword))?;
                    return Ok(());
                }
                if !self.queue()?.is_empty() {
                    let outcome = self.push_queue_request(&request)?;
                    self.handle_queue_push_outcome(parsed, &request, outcome, QUEUE_PUSH_FEEDBACK)?;
                    return Ok(());
                }

                let status = self.player.status();
                match status {
                    Ok(status) if is_playing(&status) => {
                        if !request.uri.trim().is_empty()
                            && status.current_uri.trim() == request.uri.trim()
                        {
                            self.log_executed_command(
                                parsed,
                                &final_song_command_text(&request, "already-playing"),
                            )?;
                            self.reply(&format!("当前正在播放: {}", request.keyword))?;
                            return Ok(());
                        }
                        if !request.skip_match_check {
                            let current_match = song_matcher::match_song_query(
                                &self.config.matching,
                                request.match_keyword(),
                                &status.name,
                                &status.singer,
                                request.prefer_accompaniment,
                            );
                            if current_match.ok {
                                self.log_executed_command(
                                    parsed,
                                    &final_song_command_text(&request, "already-playing"),
                                )?;
                                self.reply(&format!("当前正在播放: {}", request.keyword))?;
                                return Ok(());
                            }
                        }
                        if self
                            .player
                            .should_queue_until_current_song_finished(&status)?
                        {
                            let outcome = self.push_queue_request(&request)?;
                            self.handle_queue_push_outcome(
                                parsed,
                                &request,
                                outcome,
                                QUEUE_PUSH_FEEDBACK,
                            )?;
                            return Ok(());
                        }
                        if !self.player.current_status_matches_request(&status)? {
                            let outcome = self.play_request_confirmed(&request, true)?;
                            self.log_play_request_outcome(parsed, &request, outcome)?;
                            return Ok(());
                        }
                        let outcome = self.push_queue_request(&request)?;
                        self.handle_queue_push_outcome(
                            parsed,
                            &request,
                            outcome,
                            QUEUE_PUSH_FEEDBACK,
                        )?;
                        return Ok(());
                    }
                    Ok(status) => {
                        if self
                            .player
                            .should_queue_until_current_song_finished(&status)?
                        {
                            let outcome = self.push_queue_request(&request)?;
                            self.handle_queue_push_outcome(
                                parsed,
                                &request,
                                outcome,
                                QUEUE_PUSH_FEEDBACK,
                            )?;
                            return Ok(());
                        }
                    }
                    Err(error) => {
                        log::error!("获取播放状态失败: {error:#}");
                        let outcome = self.push_queue_request(&request)?;
                        self.handle_queue_push_outcome(
                            parsed,
                            &request,
                            outcome,
                            UNKNOWN_STATUS_QUEUE_PUSH_FEEDBACK,
                        )?;
                        return Ok(());
                    }
                }

                let outcome = self.play_request_confirmed(&request, true)?;
                self.log_play_request_outcome(parsed, &request, outcome)?;
            }
            UserCommand::Pause => {
                let message = self.player.pause_by_user()?;
                self.log_executed_command(parsed, "pause")?;
                self.update_monitor_playback_controller();
                self.reply(if message.trim().is_empty() {
                    "已暂停"
                } else {
                    message.trim()
                })?;
            }
            UserCommand::Resume | UserCommand::Play => {
                let message = self.player.resume_by_user()?;
                self.log_executed_command(parsed, "resume")?;
                self.update_monitor_playback_controller();
                self.reply(if message.trim().is_empty() {
                    "已恢复播放"
                } else {
                    message.trim()
                })?;
            }
            UserCommand::Next => {
                if !self.queue()?.is_empty() {
                    self.consume_queue("手动下一首")?;
                    self.log_executed_command(parsed, "next queue")?;
                } else {
                    let message = self.player.next_external()?;
                    self.update_monitor_playback_controller();
                    self.log_executed_command(parsed, "next feeluown")?;
                    self.reply_player_status_after_skip(message.trim())?;
                }
            }
            UserCommand::Previous => {
                let message = self.player.previous_external()?;
                self.update_monitor_playback_controller();
                self.log_executed_command(parsed, "previous")?;
                self.reply_player_status_after_skip(message.trim())?;
            }
            UserCommand::Volume(volume) => {
                self.player.set_volume(volume)?;
                self.log_executed_command(parsed, &format!("volume {}", volume))?;
                self.reply(&format!("音量已设置为 {}", volume))?;
            }
            UserCommand::Status => {
                let status = self.player.status()?;
                self.log_executed_command(parsed, "status")?;
                self.reply(&format_status(&status))?;
            }
            UserCommand::Lyrics => {
                let status = self.player.status()?;
                self.log_executed_command(parsed, "lyrics")?;
                self.reply(&format_lyrics(&status))?;
            }
            UserCommand::Queue => {
                self.log_executed_command(parsed, "queue list")?;
                self.log_queue()?;
            }
            UserCommand::QueueDelete(indexes) => {
                if indexes.is_empty() {
                    self.log_executed_command(parsed, "queue delete invalid")?;
                    self.reply("没有匹配到有效队列序号")?;
                    return Ok(());
                }
                let removed = self.queue()?.remove_indexes(indexes)?;
                if removed.is_empty() {
                    self.log_executed_command(parsed, "queue delete none")?;
                    self.reply("队列删除失败或序号不存在")?;
                } else {
                    self.update_monitor_queue_snapshot();
                    let removed_text = removed
                        .iter()
                        .map(|(index, item)| format!("{}.{}", index, item.keyword))
                        .collect::<Vec<_>>()
                        .join(", ");
                    self.log_executed_command(parsed, &format!("queue delete {}", removed_text))?;
                    self.reply(&format!("队列已删除: {}", removed_text))?;
                }
            }
            UserCommand::QueueClear => {
                let count = self.queue()?.clear()?;
                self.update_monitor_queue_snapshot();
                self.log_executed_command(parsed, &format!("queue clear {}", count))?;
                if count == 0 {
                    self.reply("队列为空")?;
                } else {
                    self.reply(&format!("队列已清空: {} 首", count))?;
                }
            }
            UserCommand::HallDetect => {
                self.log_executed_command(parsed, "hall detect")?;
                self.execute_hall_detect()?;
            }
            UserCommand::HallTime => {
                self.log_executed_command(parsed, "hall time")?;
                self.reply_hall_time()?;
            }
            UserCommand::Help => {
                self.log_executed_command(parsed, "help")?;
                self.send_help()?;
            }
            UserCommand::EntertainmentHelp => {
                self.log_executed_command(parsed, "entertainment help")?;
                self.send_entertainment_help()?;
            }
            UserCommand::IdiomChain(command) => {
                if idiom_command_requires_executor(command) {
                    self.execute_idiom_explanation(&parsed.username, command)?;
                } else {
                    log::warn!("成语接龙命令错误进入主执行器，改由延迟聊天队列处理");
                    let _ = self.handle_idiom_chain_command(parsed)?;
                }
            }
            UserCommand::Landlord(command) => {
                self.execute_landlord_command(&parsed.username, command)?;
            }
            UserCommand::TurtleSoup(_) => {
                log::warn!("海龟汤命令错误进入主执行器，改由娱乐模块处理");
                let _ = self.handle_turtle_soup_command(parsed)?;
            }
            UserCommand::Undercover(command) => {
                self.execute_undercover_command(parsed, command)?;
            }
            UserCommand::Invite(invite) => {
                if let Some(seq) = invite.seq {
                    let mut executed = self
                        .invite_executed_seqs
                        .lock()
                        .map_err(|_| anyhow!("invite_executed_seqs mutex poisoned"))?;
                    if !executed.insert(seq) {
                        log::info!("邀请参数 {} 已执行过，跳过", seq);
                        return Ok(());
                    }
                }
                self.log_executed_command(parsed, &format!("invite {}", invite.username))?;
                self.execute_invite_with_announce(&invite.username, invite.password.as_deref())?;
            }
            UserCommand::Moderation(command) => {
                self.log_executed_command(
                    parsed,
                    &format!("{} uid {}", command.action.label(), command.uid),
                )?;
                self.execute_moderation_with_vote(command)?;
            }
            UserCommand::Microphone { username } => {
                log::info!("收到麦克风命令: {}", username);
                if self.check_public_hall()? {
                    self.log_executed_command(
                        parsed,
                        &format!("microphone skipped publicHall {}", username),
                    )?;
                    log::info!("麦克风: 当前在公共大厅，跳过状态切换和通告");
                } else {
                    self.log_executed_command(parsed, &format!("microphone toggle {}", username))?;
                    self.execute_microphone_command(username)?;
                }
            }
            UserCommand::DisableCommands { username: _ } => {
                log::info!("收到禁用命令");
                self.commands_enabled.store(false, AtomicOrdering::SeqCst);
                self.log_executed_command(parsed, "disable commands")?;
                self.reply("管理员已禁用大厅命令识别功能")?;
            }
            UserCommand::EnableCommands { username: _ } => {
                log::info!("收到启用命令");
                self.commands_enabled.store(true, AtomicOrdering::SeqCst);
                self.log_executed_command(parsed, "enable commands")?;
                self.reply("管理员已启用大厅命令识别功能")?;
            }
            UserCommand::IdleExit { minutes } => {
                self.configure_idle_exit(*minutes)?;
                self.log_executed_command(parsed, &format!("idle exit {}", minutes))?;
            }
            UserCommand::ChatListenerMode(command) => {
                self.log_executed_command(parsed, &format!("chat listener {}", command.label()))?;
                log::warn!(
                    "监听模式命令未经过专用队列分发，已只记录: {}",
                    command.label()
                );
            }
            UserCommand::CustomWorkflow(command) => {
                self.log_executed_command(
                    parsed,
                    &format!("custom workflow {}", command.workflow),
                )?;
                self.execute_custom_workflow(command, parsed)?;
            }
        };
        Ok(())
    }

    fn configure_idle_exit(&self, minutes: u32) -> Result<()> {
        let minutes = minutes.max(IDLE_EXIT_MIN_MINUTES);
        let mut state = self
            .idle_exit
            .lock()
            .map_err(|_| anyhow!("idle_exit mutex poisoned"))?;
        *state = Some(IdleExitState {
            timeout: Duration::from_secs(minutes as u64 * 60),
            last_command_at: Instant::now(),
        });
        log::info!(
            "已设置闲置退出: {}分钟无新命令后关闭目标游戏进程，软件主进程继续运行",
            minutes
        );
        Ok(())
    }

    fn execute_microphone_command(&self, username: &str) -> Result<()> {
        self.ensure_ui_residency(UiResidency::Primary, "麦克风切换前准备")?;
        log::info!("麦克风: 按 N 切换状态");
        self.game_ui.press_key(Key::Unicode('n'))?;
        sleep(Duration::from_millis(100));
        self.reply(&format!("@{} 执行了切换麦克风状态！", username))
    }

    fn execute_hall_detect(&mut self) -> Result<()> {
        self.ensure_ui_residency(UiResidency::Primary, "大厅检测前准备")?;
        log::info!("大厅检测: 按 F2 进入大厅页面");
        self.game_ui.press_key(Key::F2)?;
        sleep(Duration::from_millis(
            self.config.timing.hall.page_settle_ms,
        ));

        let result = self.read_hall_info();

        self.return_to_primary_from_transient_ui("大厅检测");

        match result {
            Ok(info) => {
                let name = info.name;
                log::info!("大厅检测 OCR 结果: {}", name);
                if command::normalize_lock_text(&name) == command::normalize_lock_text("公共大厅")
                {
                    self.clear_hall_remaining_minutes()?;
                    self.reply("当前为公共大厅")?;
                } else {
                    if let Some(minutes) = info.remaining_minutes {
                        self.update_hall_remaining_minutes(minutes)?;
                        log::info!("大厅剩余时间 OCR 结果: {}分钟", minutes);
                    }
                    let time_suffix = format_hall_remaining_suffix(info.remaining_minutes);
                    self.reply(&format!(
                        "当前为{}{}",
                        if name.is_empty() {
                            "未识别到大厅名称"
                        } else {
                            name.as_str()
                        },
                        time_suffix
                    ))?;
                }
            }
            Err(error) => {
                log::error!("大厅检测 OCR 失败: {error:#}");
                self.reply("大厅检测失败")?;
            }
        }
        Ok(())
    }

    fn reply_player_status_after_skip(&self, fallback: &str) -> Result<()> {
        sleep(Duration::from_millis(
            self.config.timing.playback.skip_status_initial_ms,
        ));
        for _ in 0..self.config.timing.playback.skip_status_retries {
            match self.player.status() {
                Ok(status) if is_playing(&status) || status.status == "paused" => {
                    return self.reply(&format_play_message(&status));
                }
                Ok(_) => sleep(Duration::from_millis(
                    self.config.timing.playback.skip_status_poll_ms,
                )),
                Err(error) => {
                    log::error!("切歌后查询播放状态失败: {error:#}");
                    break;
                }
            }
        }
        if fallback.is_empty() {
            self.reply("切歌完成")
        } else {
            self.reply(fallback)
        }
    }

    fn reply_hall_time(&mut self) -> Result<()> {
        let minutes = self.runtime_state()?.state().hall_remaining_minutes_now();
        if let Some(minutes) = minutes.filter(|minutes| *minutes > 0) {
            return self.reply(&format!("大厅到期时间，剩余{}分钟", minutes));
        }

        log::info!("大厅时间未知，执行一次大厅识别");
        self.ensure_ui_residency(UiResidency::Primary, "大厅时间识别前准备")?;
        self.game_ui.press_key(Key::F2)?;
        sleep(Duration::from_millis(
            self.config.timing.hall.page_settle_ms,
        ));
        let result = self.read_hall_info();
        self.return_to_primary_from_transient_ui("大厅时间");

        let info = match result {
            Ok(info) => info,
            Err(error) => {
                log::error!("大厅时间 OCR 失败: {error:#}");
                return self.reply("大厅时间未知");
            }
        };
        let is_public_hall =
            command::normalize_lock_text(&info.name) == command::normalize_lock_text("公共大厅");
        if is_public_hall {
            self.clear_hall_remaining_minutes()?;
            return self.reply("公共大厅无时间限制");
        }
        if let Some(minutes) = info.remaining_minutes {
            self.update_hall_remaining_minutes(minutes)?;
            return self.reply(&format!("大厅到期时间，剩余{}分钟", minutes));
        }
        self.reply("大厅时间未知")
    }

    fn check_public_hall(&self) -> Result<bool> {
        self.ensure_ui_residency(UiResidency::Primary, "公共大厅检测前准备")?;
        log::info!("大厅检测: 按 F2 进入大厅页面");
        self.game_ui.press_key(Key::F2)?;
        sleep(Duration::from_millis(
            self.config.timing.hall.page_settle_ms,
        ));
        let result = self.read_hall_info();
        self.return_to_primary_from_transient_ui("大厅检测");
        let info = match result {
            Ok(info) => info,
            Err(error) => {
                log::error!("大厅检测 OCR 失败，按非公共大厅处理: {error:#}");
                return Ok(false);
            }
        };
        let name = info.name;
        log::info!("大厅检测 OCR 结果: {}", name);
        let is_public_hall =
            command::normalize_lock_text(&name) == command::normalize_lock_text("公共大厅");
        if is_public_hall {
            self.clear_hall_remaining_minutes()?;
        } else if let Some(minutes) = info.remaining_minutes {
            self.update_hall_remaining_minutes(minutes)?;
            log::info!("大厅剩余时间 OCR 结果: {}分钟", minutes);
        }
        Ok(is_public_hall)
    }

    fn return_to_primary_fixed(&self) -> bool {
        self.return_to_primary_by_escape("返回一级界面")
    }

    fn return_to_primary_after_command_failure(&self, command: &str) {
        log::info!("命令失败后返回一级界面: {}", command);
        let _ = self.return_to_primary_fixed();
    }

    fn return_to_primary_from_transient_ui(&self, context: &str) -> bool {
        self.return_to_primary_by_escape(context)
    }

    fn return_to_primary_by_escape(&self, context: &str) -> bool {
        if let Err(error) = self.game_ui.ensure_window() {
            log::warn!(
                "{}: 目标游戏窗口不可用，跳过返回一级界面: {error:#}",
                context
            );
            return false;
        }

        let templates = UiTemplateArgs::default().resolve(&self.config);
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };
        let frame_args = FrameArgs { image: None };
        let deadline =
            Instant::now() + Duration::from_millis(self.config.timing.command.ui_timeout_ms);

        let mut failed_returns = 0_u32;
        let mut allow_transition_wait = true;
        let mut primary_region_stability = PrimaryRegionStability::default();

        while Instant::now() < deadline {
            match load_frame(&frame_args, &canvas, &self.game_ui).and_then(|frame| {
                let ui_state = detect_ui_state(&frame.image, &templates, &self.config.screen)?;
                let primary_region_fingerprint = rect_chat_change_fingerprint(
                    &frame.image,
                    self.config.screen.friend_rect.into(),
                )?;
                Ok((ui_state, primary_region_fingerprint))
            }) {
                Ok((ui_state, primary_region_fingerprint)) => {
                    let primary_region_readiness = primary_region_stability.observe(
                        primary_region_fingerprint,
                        Instant::now(),
                        self.config.ocr.change_mean_threshold,
                        self.config.ocr.change_pixel_threshold,
                    );
                    match primary_return_action(
                        primary_return_observation(&ui_state),
                        allow_transition_wait,
                        primary_region_readiness != PrimaryRegionReadiness::Pending,
                    ) {
                        PrimaryReturnAction::Complete => {
                            if primary_region_readiness == PrimaryRegionReadiness::TimedOut {
                                log::warn!(
                                    "{}: 好友按钮区域持续变化 {}ms，按当前一级界面识别结果完成返回",
                                    context,
                                    PRIMARY_REGION_STABILITY_TIMEOUT_MS
                                );
                            }
                            log::info!("{}: 已返回一级界面: {}", context, ui_state);
                            return true;
                        }
                        PrimaryReturnAction::WaitForPrimaryStability => {
                            sleep(Duration::from_millis(PRIMARY_REGION_STABILITY_POLL_MS));
                            continue;
                        }
                        PrimaryReturnAction::WaitForTransition => {
                            let wait_ms = self.config.timing.command.post_settle_ms;
                            log::info!(
                                "{}: 当前 {}，等待界面过渡后重新检测: {}ms",
                                context,
                                ui_state,
                                wait_ms
                            );
                            sleep(Duration::from_millis(wait_ms));
                            allow_transition_wait = false;
                            continue;
                        }
                        PrimaryReturnAction::PressEscape => {
                            log::info!("{}: 当前 {}，按 ESC 返回上一级", context, ui_state);
                        }
                    }
                }
                Err(error) if is_target_window_unavailable_error(&error) => {
                    log::warn!(
                        "{}: 目标游戏窗口不可用，停止返回一级界面: {error:#}",
                        context
                    );
                    return false;
                }
                Err(error) => {
                    log::error!("{}: 返回一级界面检测失败，继续按 ESC: {error:#}", context);
                }
            }
            failed_returns = failed_returns.saturating_add(1);
            if !self.press_escape_for_primary_return(context, failed_returns) {
                return false;
            }
            primary_region_stability.reset();
            allow_transition_wait = allow_primary_transition_wait_after_escape();
        }
        log::error!("{}: 返回一级界面超时", context);
        false
    }

    fn press_escape_for_primary_return(&self, context: &str, failed_returns: u32) -> bool {
        let wait_ms = return_to_primary_retry_wait_ms(
            self.config.timing.command.return_retry_ms,
            failed_returns,
        );
        log::info!(
            "{}: 按 ESC 返回上一级，连续失败={} wait={}ms",
            context,
            failed_returns,
            wait_ms
        );
        if let Err(error) = self.game_ui.press_key(Key::Escape) {
            log::error!("{}: 返回一级界面按 ESC 失败: {error:#}", context);
            return false;
        }
        sleep(Duration::from_millis(wait_ms));
        true
    }

    fn read_hall_info(&self) -> Result<HallInfo> {
        let canvas = Canvas {
            width: self.config.screen.expected_width,
            height: self.config.screen.expected_height,
            resize: true,
        };

        let mut samples = Vec::new();
        for index in 0..HALL_INFO_OCR_SAMPLES {
            if index > 0 {
                sleep(Duration::from_millis(
                    self.config.timing.hall.ocr_sample_interval_ms,
                ));
            }
            let frame = load_frame(&FrameArgs { image: None }, &canvas, &self.game_ui)?;
            let sample = self.read_hall_info_sample_from_frame(&frame.image)?;
            log::info!(
                "大厅检测 OCR 采样: {}/{} name={} time={} minutes={}",
                index + 1,
                HALL_INFO_OCR_SAMPLES,
                display_or_empty(&sample.name),
                display_or_empty(&sample.time_text),
                sample
                    .remaining_minutes
                    .map(|minutes| minutes.to_string())
                    .unwrap_or_else(|| "未知".to_string())
            );
            samples.push(sample);
        }
        Ok(merge_hall_info_samples(&samples))
    }

    fn read_hall_info_sample_from_frame(&self, image: &DynamicImage) -> Result<HallInfoSample> {
        let name_crop = crop_canvas(image, self.config.screen.hall_name_rect.into())?;
        let name = self.ocr.merged_text(
            name_crop,
            self.config.ocr.same_line_y_tolerance,
            OcrPriority::BackgroundObservation,
        )?;
        let time_crop = crop_canvas(image, self.config.screen.hall_time_rect.into())?;
        let time_text = self.ocr.merged_text(
            time_crop,
            self.config.ocr.same_line_y_tolerance,
            OcrPriority::BackgroundObservation,
        )?;
        Ok(HallInfoSample {
            name,
            time_text: time_text.clone(),
            remaining_minutes: parse_hall_remaining_minutes(&time_text),
        })
    }

    fn update_hall_remaining_minutes(&self, minutes: u32) -> Result<()> {
        let mut runtime_state = self.runtime_state()?;
        runtime_state
            .state_mut()
            .update_hall_remaining_minutes(minutes);
        runtime_state.save()
    }

    fn clear_hall_remaining_minutes(&self) -> Result<()> {
        let mut runtime_state = self.runtime_state()?;
        runtime_state.state_mut().clear_hall_remaining_minutes();
        runtime_state.save()
    }

    fn send_help(&self) -> Result<()> {
        self.reply_batch(
            &[
                "点歌示例: @点歌/@AI点歌 歌名 歌手 伴奏,输入伴奏时优先匹配伴奏",
                "切换网易平台: @网易点歌 歌名 歌手 伴奏,默认为QQ平台",
                "可用 @QQ点歌/@网易点歌 指定来源,@AI点歌用于智能识别歌名歌手",
            ],
            self.config.timing.command.help_batch_ms,
        )
    }

    fn send_entertainment_help(&self) -> Result<()> {
        self.reply_batch(
            &[
                "成语接龙: #接龙 成语;同音模式用 #同音接龙 成语;进行中用 #成语/#提示/#解释",
                "斗地主: #斗地主,#加入,#抢/#不抢,#牌组/#出牌组,#过;好友私聊 #手牌",
                "跑得快: #跑得快,#加入,#牌组/#出牌组,#过;好友私聊 #手牌",
                "海龟汤: #海龟汤;进行中 #状态/#结束;其他 #内容 作为问题",
                "海龟汤长答案: ##1第一段,##2第二段,最后发送##提交",
                "谁是卧底: #卧底/#卧底双;好友私聊 #加入;公屏 #开局/#状态/#退出",
                "谁是卧底: 描述用公屏 #内容;投票用好友私聊 #A 或 #投A",
            ],
            self.config.timing.command.help_batch_ms,
        )
    }

    fn execute_idiom_explanation(
        &self,
        player: &str,
        command: &idiom_chain::IdiomChainCommand,
    ) -> Result<()> {
        let outcome = self.idiom_chain.explain(player, command)?;
        let mut messages = vec![outcome.reply];
        if let Some(explanation) = outcome.explanation {
            messages.extend(split_numbered_chat_message("来源", &explanation.source));
            messages.extend(split_numbered_chat_message(
                "解释",
                &explanation.explanation,
            ));
        }
        let message_refs = messages.iter().map(String::as_str).collect::<Vec<_>>();
        self.reply_batch(&message_refs, self.config.timing.command.help_batch_ms)
    }

    fn execute_landlord_command(&self, player: &str, command: &LandlordCommand) -> Result<()> {
        self.landlord.execute(player, command, self, Instant::now())
    }

    fn execute_undercover_command(
        &self,
        parsed: &ParsedCommand,
        command: &UndercoverCommand,
    ) -> Result<()> {
        let source = match parsed.message_type.as_str() {
            "pink" => UndercoverCommandSource::Friend,
            "控制台" => UndercoverCommandSource::Console,
            _ => UndercoverCommandSource::Hall,
        };
        self.undercover.execute(
            UndercoverCommandContext {
                player: &parsed.username,
                source,
            },
            command,
            self,
            Instant::now(),
        )
    }

    fn play_request_confirmed(
        &mut self,
        request: &ResolvedSongRequest,
        allow_switch_source: bool,
    ) -> Result<PlaybackOutcome> {
        if self.song_dedup_limited(request)? {
            log::info!(
                "长时间同歌去重拦截: keyword={} uri={}",
                request.keyword,
                request.uri
            );
            self.reply(&self.song_dedup_reject_message(request))?;
            return Ok(PlaybackOutcome::DedupLimited);
        }
        if request.uri.trim().is_empty() {
            let source = if request.source.trim().is_empty() {
                "qqmusic"
            } else {
                &request.source
            };
            let picked = match self.player.search_and_pick(
                &request.keyword,
                source,
                request.prefer_accompaniment,
            ) {
                Ok(Some(picked)) => picked,
                Err(error) => {
                    let message = error.to_string();
                    log::error!("点歌搜索失败: {message}");
                    self.reply(if message.trim().is_empty() {
                        "平台无对应歌曲音源"
                    } else {
                        message.trim()
                    })?;
                    return Ok(if message.contains("平台无对应歌曲音源") {
                        PlaybackOutcome::NoSource
                    } else {
                        PlaybackOutcome::Error
                    });
                }
                Ok(None) => {
                    self.reply("平台无对应歌曲音源")?;
                    return Ok(PlaybackOutcome::NoSource);
                }
            };
            log::info!("播放器候选: {} -> {}", picked.0.text, picked.0.uri);
            let mut resolved = request.clone();
            resolved.keyword = picked.0.text;
            resolved.source = source.to_string();
            resolved.uri = picked.0.uri;
            return self.play_request_confirmed(&resolved, allow_switch_source);
        }
        let playback_request = self.playback_request_from_resolved(request);
        self.play_playback_request(&playback_request, allow_switch_source, false)
    }

    fn play_playback_request(
        &mut self,
        request: &PlaybackRequest,
        allow_switch_source: bool,
        confirm_after_switch: bool,
    ) -> Result<PlaybackOutcome> {
        let mut attempt = match self.player.play_request_uri(request) {
            Ok(attempt) => attempt,
            Err(error) => {
                let message = error.to_string();
                log::error!("播放候选失败: {message}");
                self.reply(if message.trim().is_empty() {
                    "平台无对应歌曲音源"
                } else {
                    message.trim()
                })?;
                return Ok(PlaybackOutcome::Error);
            }
        };
        self.complete_playback_verification(
            request,
            &mut attempt,
            allow_switch_source,
            confirm_after_switch,
        )
    }

    fn complete_playback_verification(
        &mut self,
        request: &PlaybackRequest,
        attempt: &mut PlaybackAttempt,
        allow_switch_source: bool,
        confirm_after_switch: bool,
    ) -> Result<PlaybackOutcome> {
        match self.player.verify_playback_started(request, attempt)? {
            PlaybackVerification::Success { status, message } => {
                if confirm_after_switch {
                    match self.confirm_switched_source_result(&status)? {
                        UserDecision::Skip => {
                            self.player.reject_mismatch_as_no_source(Some(&status))?;
                            self.report_no_source(Some(&status), false)?;
                            self.update_monitor_playback_controller();
                            return Ok(PlaybackOutcome::NoSource);
                        }
                        UserDecision::Stopped => return Ok(PlaybackOutcome::Error),
                        _ => {}
                    }
                }
                self.reply(&message)?;
                self.update_monitor_playback_controller();
                Ok(PlaybackOutcome::Success)
            }
            PlaybackVerification::NoSource => {
                self.report_no_source(None, false)?;
                self.update_monitor_playback_controller();
                Ok(PlaybackOutcome::NoSource)
            }
            PlaybackVerification::MismatchedCandidate(mismatch) => {
                match self.handle_playback_mismatch(
                    request,
                    &mismatch.status,
                    &mismatch.local_reason,
                    allow_switch_source,
                )? {
                    MismatchDecision::Accept => {
                        match self.player.accept_mismatch(request, &mismatch.status)? {
                            PlaybackVerification::Success { status, message } => {
                                if confirm_after_switch {
                                    match self.confirm_switched_source_result(&status)? {
                                        UserDecision::Skip => {
                                            self.player
                                                .reject_mismatch_as_no_source(Some(&status))?;
                                            self.report_no_source(Some(&status), false)?;
                                            self.update_monitor_playback_controller();
                                            return Ok(PlaybackOutcome::NoSource);
                                        }
                                        UserDecision::Stopped => {
                                            return Ok(PlaybackOutcome::Error);
                                        }
                                        _ => {}
                                    }
                                }
                                self.reply(&message)?;
                                self.update_monitor_playback_controller();
                                Ok(PlaybackOutcome::Success)
                            }
                            PlaybackVerification::NoSource => {
                                self.report_no_source(Some(&mismatch.status), true)?;
                                self.update_monitor_playback_controller();
                                Ok(PlaybackOutcome::NoSource)
                            }
                            _ => Ok(PlaybackOutcome::Error),
                        }
                    }
                    MismatchDecision::NoSource => {
                        self.player
                            .reject_mismatch_as_no_source(Some(&mismatch.status))?;
                        self.report_no_source(Some(&mismatch.status), false)?;
                        self.update_monitor_playback_controller();
                        Ok(PlaybackOutcome::NoSource)
                    }
                    MismatchDecision::SwitchSource => self.switch_source_and_play(
                        &request.keyword,
                        &request.source,
                        request.prefer_accompaniment,
                    ),
                    MismatchDecision::Error => Ok(PlaybackOutcome::Error),
                }
            }
        }
    }

    fn handle_playback_mismatch(
        &mut self,
        request: &PlaybackRequest,
        status: &PlayerStatus,
        local_reason: &str,
        allow_switch_source: bool,
    ) -> Result<MismatchDecision> {
        log::info!("歌曲暂不匹配: {}", local_reason);
        if self.ai.enabled() {
            match self
                .ai
                .match_same_song(&request.match_keyword, &status.name, &status.singer)
            {
                Ok(ai_match) if ai_match.matched => {
                    log::info!(
                        "AI自动匹配通过: {} score={}",
                        ai_match.reason,
                        ai_match.score
                    );
                    return Ok(
                        match self.confirm_ai_auto_match(status, allow_switch_source)? {
                            UserDecision::Skip => MismatchDecision::NoSource,
                            UserDecision::SwitchSource if allow_switch_source => {
                                MismatchDecision::SwitchSource
                            }
                            UserDecision::Stopped => MismatchDecision::Error,
                            _ => MismatchDecision::Accept,
                        },
                    );
                }
                Ok(ai_match) => {
                    log::info!("AI判断不是同一首: {}", ai_match.reason);
                }
                Err(error) => {
                    log::info!("AI判断异常，回退到人工确认: {error:#}");
                }
            }
        }

        Ok(match self.confirm_song(status, allow_switch_source)? {
            UserDecision::PromptFailed | UserDecision::Stopped => MismatchDecision::Error,
            UserDecision::SwitchSource if allow_switch_source => MismatchDecision::SwitchSource,
            UserDecision::Timeout => MismatchDecision::NoSource,
            UserDecision::Confirm => MismatchDecision::Accept,
            UserDecision::Skip => MismatchDecision::NoSource,
            _ => MismatchDecision::NoSource,
        })
    }

    fn switch_source_and_play(
        &mut self,
        keyword: &str,
        current_source: &str,
        prefer_accompaniment: bool,
    ) -> Result<PlaybackOutcome> {
        let next_source = if current_source == "netease" {
            "qqmusic"
        } else {
            "netease"
        };
        let label = if next_source == "netease" {
            "网易"
        } else {
            "QQ"
        };
        self.reply(&format!("换源到{}: {}", label, keyword))?;
        let request = ResolvedSongRequest {
            keyword: keyword.to_string(),
            source: next_source.to_string(),
            prefer_accompaniment,
            ai_original_text: String::new(),
            uri: String::new(),
            skip_match_check: false,
            friend_username: String::new(),
            console_bypass_dedup: false,
        };
        let outcome = self.play_request_confirmed(&request, false)?;
        if outcome == PlaybackOutcome::Success {
            // 换源成功后仍让用户有一次跳过机会。
            if let Ok(status) = self.player.status()
                && matches!(
                    self.confirm_switched_source_result(&status)?,
                    UserDecision::Skip
                )
            {
                self.player.reject_mismatch_as_no_source(Some(&status))?;
                self.report_no_source(Some(&status), false)?;
                return Ok(PlaybackOutcome::NoSource);
            }
        }
        Ok(outcome)
    }

    fn confirm_switched_source_result(&mut self, status: &PlayerStatus) -> Result<UserDecision> {
        let message = format!(
            "换源结果:{},@确认@跳过",
            song_title(&status.name, &status.singer)
        );
        if self.reply(&message).is_err() {
            return Ok(UserDecision::Timeout);
        }
        self.wait_for_decision(false, false, true)
    }

    fn confirm_song(
        &mut self,
        status: &PlayerStatus,
        allow_switch_source: bool,
    ) -> Result<UserDecision> {
        let actions = if allow_switch_source {
            "@确认@跳过@换源"
        } else {
            "@确认@跳过"
        };
        let message = format!(
            "匹配失败:{},{}",
            song_title(&status.name, &status.singer),
            actions
        );
        if self.reply(&message).is_err() {
            return Ok(UserDecision::PromptFailed);
        }
        self.wait_for_decision(allow_switch_source, false, false)
    }

    fn confirm_ai_auto_match(
        &mut self,
        status: &PlayerStatus,
        allow_switch_source: bool,
    ) -> Result<UserDecision> {
        let actions = if allow_switch_source {
            "@跳过@换源"
        } else {
            "@跳过"
        };
        let message = format!(
            "AI自动匹配:{},如非预期可{}",
            song_title(&status.name, &status.singer),
            actions
        );
        if self.reply(&message).is_err() {
            return Ok(UserDecision::Timeout);
        }
        self.wait_for_decision(allow_switch_source, false, true)
    }

    fn wait_for_decision(
        &mut self,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout_confirms: bool,
    ) -> Result<UserDecision> {
        let accepts_message_type = |message_type: &str| message_type == "blue";
        let is_decision = |text: &str| parse_decision_command(text).is_some();
        let mut reader = self.begin_chat_decision_reader(
            ChatDecisionScope::CurrentHall,
            &accepts_message_type,
            &is_decision,
        )?;
        let timeout = Duration::from_millis(self.config.timing.decision.timeout_ms);
        let map_web_decision = |decision| match decision {
            DecisionAction::Confirm => UserDecision::Confirm,
            DecisionAction::Skip => UserDecision::Skip,
            DecisionAction::SwitchSource => UserDecision::SwitchSource,
            DecisionAction::Ai => UserDecision::Ai,
        };
        let web_decision =
            self.decision_control
                .begin("点歌候选确认", allow_switch_source, allow_ai, timeout)?;
        let deadline = Instant::now() + timeout;
        while self.running.load(AtomicOrdering::SeqCst) && Instant::now() < deadline {
            let wait =
                Duration::from_millis(reader.poll_interval_ms(self.config.timing.decision.poll_ms))
                    .min(deadline.saturating_duration_since(Instant::now()));
            if let Some(decision) = web_decision.wait(wait)? {
                return Ok(map_web_decision(decision));
            }
            let messages = match self.poll_chat_decision_reader(&mut reader) {
                Ok(messages) => messages,
                Err(error) => {
                    log::error!("确认命令扫描失败: {error:#}");
                    continue;
                }
            };
            for message in messages {
                if message.message_type != "blue" {
                    continue;
                }
                if message.text.is_empty() || is_decision_feedback_text(&message.text) {
                    continue;
                }
                let Some(decision) = parse_decision_command(&message.text) else {
                    continue;
                };
                if !reader.accept_once(&message) {
                    continue;
                }
                match decision {
                    UserDecision::Confirm => return Ok(UserDecision::Confirm),
                    UserDecision::Skip => return Ok(UserDecision::Skip),
                    UserDecision::SwitchSource if allow_switch_source => {
                        return Ok(UserDecision::SwitchSource);
                    }
                    UserDecision::Ai if allow_ai => {
                        return Ok(UserDecision::Ai);
                    }
                    _ => {}
                }
            }
        }
        if let Some(decision) = web_decision.wait(Duration::from_millis(0))? {
            return Ok(map_web_decision(decision));
        }
        if !self.running.load(AtomicOrdering::SeqCst) {
            Ok(UserDecision::Stopped)
        } else if timeout_confirms {
            Ok(UserDecision::Timeout)
        } else {
            self.reply(if allow_switch_source {
                "此平台匹配失败,命令已超时(20s)下次可以尝试@确认@跳过@换源"
            } else {
                "此平台匹配失败,命令已超时(20s)下次可以尝试@确认@跳过"
            })?;
            Ok(UserDecision::Timeout)
        }
    }

    fn report_no_source(&self, status: Option<&PlayerStatus>, pause_playback: bool) -> Result<()> {
        if pause_playback && status.is_some_and(|status| status.status == "playing") {
            let _ = self.player.reject_mismatch_as_no_source(status);
            self.update_monitor_playback_controller();
        }
        self.reply("平台无对应歌曲音源")
    }

    fn consume_queue(&mut self, reason: &str) -> Result<()> {
        loop {
            let Some(item) = ({ self.queue()?.front().cloned() }) else {
                return Ok(());
            };
            log::info!("消费队列({}): {}", reason, item.keyword);
            let request = ResolvedSongRequest {
                keyword: item.keyword.clone(),
                source: item.source.clone(),
                prefer_accompaniment: item.prefer_accompaniment,
                ai_original_text: item.ai_original_text.clone(),
                uri: item.uri.clone(),
                skip_match_check: !item.ai_original_text.trim().is_empty()
                    && !item.uri.trim().is_empty(),
                friend_username: item.friend_username.clone(),
                console_bypass_dedup: item.dedup_bypass,
            };
            if self.song_dedup_limited(&request)? {
                self.queue()?.shift()?;
                self.update_monitor_queue_snapshot();
                log::info!("队列项近期已播放过，已跳过: {}", item.keyword);
                self.reply(&self.song_dedup_skip_message(&request))?;
                continue;
            }
            let outcome = self.play_request_confirmed(&request, true)?;
            match outcome {
                PlaybackOutcome::Success => {
                    self.queue()?.shift()?;
                    self.update_monitor_queue_snapshot();
                    return Ok(());
                }
                PlaybackOutcome::NoSource => {
                    self.queue()?.shift()?;
                    self.update_monitor_queue_snapshot();
                    log::error!("队列项无音源，已丢弃: {}", item.keyword);
                    continue;
                }
                PlaybackOutcome::Error => {
                    log::error!("队列项播放失败，保留在队首: {}", item.keyword);
                    return Ok(());
                }
                PlaybackOutcome::DedupLimited => {
                    self.queue()?.shift()?;
                    self.update_monitor_queue_snapshot();
                    log::info!("队列项近期已播放过，已跳过: {}", item.keyword);
                    continue;
                }
            }
        }
    }

    fn reply(&self, message: &str) -> Result<()> {
        let prefixed;
        let message = if self.console_reply_context.load(AtomicOrdering::SeqCst)
            && !message.starts_with("[控制台]:")
        {
            prefixed = format!("[控制台]: {}", message);
            prefixed.as_str()
        } else {
            message
        };
        match self.active_ui_residency() {
            UiResidency::Primary => {
                self.ensure_ui_residency(UiResidency::Primary, "发送一级聊天回复")?;
                self.chat_output.send_for_command(message)
            }
            UiResidency::SecondaryCurrentHall => {
                self.ensure_ui_residency(
                    UiResidency::SecondaryCurrentHall,
                    "发送二级当前大厅回复",
                )?;
                self.chat_output.send_current_chat(message)
            }
        }
    }

    fn reply_batch(&self, messages: &[&str], delay_ms: u64) -> Result<()> {
        match self.active_ui_residency() {
            UiResidency::Primary => {
                self.ensure_ui_residency(UiResidency::Primary, "发送一级批量回复")?;
                self.chat_output.send_batch_for_command(messages, delay_ms)
            }
            UiResidency::SecondaryCurrentHall => {
                self.ensure_ui_residency(
                    UiResidency::SecondaryCurrentHall,
                    "发送二级当前大厅批量回复",
                )?;
                self.chat_output.send_current_chat_batch(messages, delay_ms)
            }
        }
    }

    fn log_queue(&self) -> Result<()> {
        let (len, entries) = {
            let queue = self.queue()?;
            let entries = queue
                .items()
                .iter()
                .enumerate()
                .map(|(index, item)| format!("{}.{}", index + 1, item.keyword))
                .collect::<Vec<_>>()
                .join(", ");
            (queue.len(), entries)
        };
        if len == 0 {
            self.reply("队列为空")?;
        } else {
            self.reply(&format!(
                "队列({}/{}): {}",
                len, self.config.queue.max_size, entries
            ))?;
        }
        Ok(())
    }

    fn update_monitor_queue_snapshot(&self) {
        match self.queue() {
            Ok(queue) => self.monitor.set_queue(
                queue
                    .items()
                    .iter()
                    .map(|item| MonitorQueueItem {
                        id: item.id,
                        keyword: item.keyword.clone(),
                        source: item.source.clone(),
                        prefer_accompaniment: item.prefer_accompaniment,
                        friend_username: item.friend_username.clone(),
                    })
                    .collect(),
            ),
            Err(error) => log::warn!("更新监控队列失败: {error:#}"),
        }
    }

    fn update_monitor_playback_controller(&self) {
        self.monitor.set_playback_controller(self.player.snapshot());
    }

    fn update_monitor_chat_listener(&self) {
        let snapshot = self.chat_listener.snapshot();
        self.monitor.set_chat_listener(
            snapshot.display_mode(),
            snapshot.pending_mode.map(|mode| mode.label().to_string()),
        );
    }

    fn update_monitor_operational_state(&self) {
        let idle_exit_remaining_seconds = self.idle_exit.lock().ok().and_then(|state| {
            state.as_ref().map(|state| {
                state
                    .timeout
                    .saturating_sub(state.last_command_at.elapsed())
                    .as_secs()
            })
        });
        let hall_remaining_minutes = self
            .runtime_state
            .lock()
            .ok()
            .and_then(|state| state.state().hall_remaining_minutes_now());
        self.monitor.set_operational(
            self.paused.load(AtomicOrdering::SeqCst),
            self.commands_enabled.load(AtomicOrdering::SeqCst),
            idle_exit_remaining_seconds,
            hall_remaining_minutes,
        );
    }
}

impl CardGameDeliveryPort for AutomationApp {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_unique_friend_message(player, message)
    }

    fn send_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_friend_message(player, message)
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        self.reply(message)
    }
}

impl UndercoverDeliveryPort for AutomationApp {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_stable_unique_friend_message(player, message)
    }

    fn send_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_friend_message(player, message)
    }

    fn send_secret_friend(&self, player: &str, message: &str) -> Result<bool> {
        self.send_secret_friend_message(player, message)
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        self.reply(message)
    }

    fn send_hall_batch(&self, messages: &[String]) -> Result<()> {
        let refs = messages.iter().map(String::as_str).collect::<Vec<_>>();
        match self.active_ui_residency() {
            UiResidency::Primary => {
                self.ensure_ui_residency(UiResidency::Primary, "发送谁是卧底批量消息")?;
                self.chat_output.send_batch_for_command_redacted(
                    &refs,
                    self.config.timing.command.help_batch_ms,
                )
            }
            UiResidency::SecondaryCurrentHall => {
                self.ensure_ui_residency(
                    UiResidency::SecondaryCurrentHall,
                    "发送谁是卧底批量消息",
                )?;
                self.chat_output.send_current_chat_batch_redacted(
                    &refs,
                    self.config.timing.command.help_batch_ms,
                )
            }
        }
    }
}

fn ai_candidate_source(song: &command::SongCommand) -> &'static str {
    if song.friend_username.trim().is_empty() {
        "qqmusic,netease"
    } else {
        song.source.as_str()
    }
}

fn song_label(song: &command::SongCommand) -> String {
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
        UserCommand::Song(song) if !song.friend_username.trim().is_empty() => &song.friend_username,
        _ => &parsed.username,
    }
}

fn is_private_undercover_input(parsed: &ParsedCommand) -> bool {
    matches!(
        &parsed.command,
        UserCommand::Undercover(UndercoverCommand::Vote(_))
    )
}

fn private_safe_command_log(parsed: &ParsedCommand) -> &str {
    match &parsed.command {
        UserCommand::Undercover(UndercoverCommand::Vote(_)) => "谁是卧底投票",
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

fn return_to_primary_retry_wait_ms(configured_retry_ms: u64, failed_returns: u32) -> u64 {
    let base_ms = configured_retry_ms.min(RETURN_TO_PRIMARY_SLOW_RETRY_MS);
    if failed_returns > RETURN_TO_PRIMARY_SLOW_RETRY_AFTER {
        return RETURN_TO_PRIMARY_SLOW_RETRY_MS;
    }
    if failed_returns <= 1 {
        return base_ms;
    }
    let steps = RETURN_TO_PRIMARY_SLOW_RETRY_AFTER as u64;
    let progress = failed_returns.saturating_sub(1) as u64;
    base_ms + (RETURN_TO_PRIMARY_SLOW_RETRY_MS - base_ms) * progress / steps
}

fn primary_return_observation(ui_state: &UiState) -> PrimaryReturnObservation {
    if ui_state.is_primary() {
        PrimaryReturnObservation::Primary
    } else if ui_state.is_secondary() {
        PrimaryReturnObservation::Secondary
    } else {
        PrimaryReturnObservation::Unknown
    }
}

fn primary_return_action(
    observation: PrimaryReturnObservation,
    allow_transition_wait: bool,
    primary_region_ready: bool,
) -> PrimaryReturnAction {
    match observation {
        PrimaryReturnObservation::Secondary => PrimaryReturnAction::PressEscape,
        _ if !primary_region_ready => PrimaryReturnAction::WaitForPrimaryStability,
        PrimaryReturnObservation::Primary if primary_region_ready => PrimaryReturnAction::Complete,
        PrimaryReturnObservation::Unknown if allow_transition_wait => {
            PrimaryReturnAction::WaitForTransition
        }
        PrimaryReturnObservation::Unknown => PrimaryReturnAction::PressEscape,
        PrimaryReturnObservation::Primary => unreachable!("primary readiness was checked"),
    }
}

fn primary_region_ready_for_command(
    stability_required: bool,
    readiness: PrimaryRegionReadiness,
) -> bool {
    !stability_required || readiness != PrimaryRegionReadiness::Pending
}

fn allow_primary_transition_wait_after_escape() -> bool {
    true
}

fn is_target_window_unavailable_error(error: &anyhow::Error) -> bool {
    window::is_target_window_unavailable(error)
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
        assert!(LandlordCommand::Rob.requires_executor());
        assert!(LandlordCommand::Decline.requires_executor());
        assert!(!LandlordCommand::Play("3".to_string()).requires_executor());
        assert!(!LandlordCommand::Pass.requires_executor());
    }

    #[test]
    fn undercover_tick_does_not_release_signup_reservation_during_command() {
        let entertainment = EntertainmentCoordinator::new();
        assert_eq!(
            entertainment
                .try_acquire(EntertainmentKind::Undercover)
                .expect("acquire undercover"),
            AcquireOutcome::Acquired
        );
        let service = UndercoverService::new(
            undercover::UndercoverConfig::default(),
            entertainment.clone(),
        );

        let deliveries = service
            .tick(Instant::now(), false)
            .expect("tick undercover");

        assert!(deliveries.is_empty());
        assert_eq!(entertainment.active(), Some(EntertainmentKind::Undercover));
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
    fn return_to_primary_retry_wait_increases_then_caps() {
        let waits = (1..=7)
            .map(|failed_returns| return_to_primary_retry_wait_ms(1_000, failed_returns))
            .collect::<Vec<_>>();

        assert_eq!(waits, vec![1_000, 1_200, 1_400, 1_600, 1_800, 2_000, 2_000]);
    }

    #[test]
    fn primary_return_waits_for_transitional_unknown_before_escape() {
        assert_eq!(
            primary_return_action(PrimaryReturnObservation::Unknown, true, true),
            PrimaryReturnAction::WaitForTransition
        );
        assert_eq!(
            primary_return_action(PrimaryReturnObservation::Unknown, false, true),
            PrimaryReturnAction::PressEscape
        );
        assert_eq!(
            primary_return_action(PrimaryReturnObservation::Secondary, true, true),
            PrimaryReturnAction::PressEscape
        );
        assert_eq!(
            primary_return_action(PrimaryReturnObservation::Primary, true, true),
            PrimaryReturnAction::Complete
        );
        assert_eq!(
            primary_return_action(PrimaryReturnObservation::Primary, true, false),
            PrimaryReturnAction::WaitForPrimaryStability
        );
        assert_eq!(
            primary_return_action(PrimaryReturnObservation::Unknown, true, false),
            PrimaryReturnAction::WaitForPrimaryStability
        );
        assert_eq!(
            primary_return_action(PrimaryReturnObservation::Secondary, true, false),
            PrimaryReturnAction::PressEscape
        );
        assert_eq!(
            primary_return_action(
                PrimaryReturnObservation::Unknown,
                allow_primary_transition_wait_after_escape(),
                true,
            ),
            PrimaryReturnAction::WaitForTransition
        );
    }

    #[test]
    fn primary_region_stability_waits_for_stability_or_timeout() {
        fn fingerprint(value: u8) -> ChangeFingerprint {
            ChangeFingerprint {
                pixels: vec![value; 4],
                width: 2,
                height: 2,
            }
        }

        let started_at = Instant::now();
        let mut stability = PrimaryRegionStability::default();
        assert_eq!(
            stability.observe(fingerprint(0), started_at, 1.0, 0.01),
            PrimaryRegionReadiness::Pending
        );
        assert_eq!(
            stability.observe(
                fingerprint(255),
                started_at + Duration::from_millis(999),
                1.0,
                0.01,
            ),
            PrimaryRegionReadiness::Pending
        );
        assert_eq!(
            stability.observe(
                fingerprint(0),
                started_at + Duration::from_millis(1_000),
                1.0,
                0.01,
            ),
            PrimaryRegionReadiness::TimedOut
        );

        stability.reset();
        assert_eq!(
            stability.observe(fingerprint(10), started_at, 1.0, 0.01),
            PrimaryRegionReadiness::Pending
        );
        assert_eq!(
            stability.observe(
                fingerprint(10),
                started_at + Duration::from_millis(100),
                1.0,
                0.01,
            ),
            PrimaryRegionReadiness::Stable
        );
    }

    #[test]
    fn command_ui_uses_fast_path_only_for_initial_primary_state() {
        assert!(primary_region_ready_for_command(
            false,
            PrimaryRegionReadiness::Pending
        ));
        assert!(!primary_region_ready_for_command(
            true,
            PrimaryRegionReadiness::Pending
        ));
        assert!(primary_region_ready_for_command(
            true,
            PrimaryRegionReadiness::Stable
        ));
    }

    #[test]
    fn target_window_unavailable_error_is_detected() {
        let error =
            window::target_window_unavailable("进入千星前未找到游戏窗口，请先执行启动游戏任务");

        assert!(is_target_window_unavailable_error(&error));
        assert!(!is_target_window_unavailable_error(&anyhow!(
            "等待派蒙菜单模板超时"
        )));
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
