use std::collections::HashSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::features::administration::{
    AdministrationMutationIntent, AdministrationMutationOutcome,
};
use crate::features::card_games::{
    CardGameCancel, CardGameCommandStart, CardGameDeadlineKind, CardGameDeadlineToken,
    CardGameEffectClaim, CardGameEffectKey, CardGameEffectResult, CardGameResume,
    CardGameRuntimePort, CardGameService, CardGameTimedOutcome, LandlordCommand,
};
use crate::features::entertainment::{EntertainmentKind, EntertainmentState};
use crate::features::hall::{
    HallMutationIntent, HallMutationOutcome, HallRuntimeState, HallStatePatch, HallStateService,
};
use crate::features::idiom_chain::{
    IdiomChainCommand, IdiomChainDeadlineKind, IdiomChainDeadlineToken, IdiomChainOutcome,
    IdiomChainService,
};
use crate::features::invite::{InviteRequest, InviteService, InviteStart};
use crate::features::moderation::{ModerationWorkflowKey, ModerationWorkflowLedger};
use crate::features::playback::{
    ExternalPlaybackObservation, PlaybackMutationIntent, PlaybackMutationOutcome,
    PlaybackRuntimeState, PlaybackService, PlaybackStateUpdate, QueueItem, QueuePushOutcome,
    QueueRemoval, QueueRemoveOutcome, SongDedupCandidate,
};
use crate::features::turtle_soup::{
    QuestionSubmitOutcome, SecondaryOcrObservation, SecondaryOcrStability, TurtleSoupAiCompletion,
    TurtleSoupAiCompletionPort, TurtleSoupAppendReceipt, TurtleSoupCommand,
    TurtleSoupCommandOutcome, TurtleSoupDeadlineKind, TurtleSoupDeadlineToken, TurtleSoupDelivery,
    TurtleSoupMutationIntent, TurtleSoupMutationOutcome, TurtleSoupQuestion, TurtleSoupService,
    TurtleSoupSnapshot, TurtleSoupSubmission, TurtleSoupWorkerRuntime,
};
#[cfg(test)]
use crate::features::undercover::UndercoverConfig;
use crate::features::undercover::{
    UndercoverCommand, UndercoverCommandSource, UndercoverCommandStart, UndercoverDeadlineKind,
    UndercoverDeadlineToken, UndercoverEffectClaim, UndercoverEffectKey, UndercoverEffectResult,
    UndercoverResume, UndercoverRuntimePort, UndercoverRuntimeService, UndercoverSnapshot,
    UndercoverTimedOutcome,
};
use crate::observation::chat::{
    CompletionAdvance, ObservationCompletionEvent, ObservationWatermark,
};
use crate::observation::shared::ObservationGap;
use crate::runtime::chat_listener::{ChatListenerMode, ChatListenerSnapshot, ChatListenerState};
use crate::runtime::clock::Clock;
#[cfg(test)]
use crate::runtime::clock::SystemClock;
use crate::runtime::deadline::{BusinessDeadlineEvent, BusinessDeadlineToken};
use crate::runtime::decision::{DecisionAction, DecisionSnapshot, DecisionState};
use crate::runtime::deferred_chat::{
    DEFAULT_CAPACITY as DEFERRED_CHAT_CAPACITY, DeferredChatItem, DeferredChatQueue, EnqueueOutcome,
};
use crate::runtime::identity::{
    BusinessOperationId, BusinessOperationIdAllocator, SessionGeneration,
};
use crate::runtime::scheduler::{
    DiagnosticTaskCompletion, DiagnosticTaskLease, DiagnosticTaskSnapshot,
    DiagnosticTaskSubmission, FormalScheduler, FormalSchedulerSnapshot, FormalTaskCancelOutcome,
    FormalTaskCompletion, FormalTaskDedupKey, FormalTaskEnqueueOutcome, FormalTaskLease,
    FormalTaskSubmission, FormalTaskWork, SchedulerLane, SchedulerLaneLease,
};
use crate::runtime::timer::{
    DeadlineCancellation, DeadlineSchedule, TimerCommandKind, TimerRuntimeEvent, TimerRuntimeHandle,
};

const IDIOM_DEADLINE_TOKEN_ID: u64 = 1;
const CARD_GAME_DEADLINE_TOKEN_ID: u64 = 1;
const UNDERCOVER_DEADLINE_TOKEN_ID: u64 = 1;
const TURTLE_SOUP_DEADLINE_TOKEN_ID: u64 = 1;

#[derive(Clone, PartialEq, Eq)]
pub enum BusinessEvent {
    CompletionAdvance(CompletionAdvance),
    CompletionGap(ObservationGap),
    Timer(BusinessDeadlineEvent),
    TurtleSoupAiCompleted(TurtleSoupAiCompletion),
}

pub(crate) enum BusinessMutationIntent {
    Administration(AdministrationMutationIntent),
    Hall(HallMutationIntent),
    Playback(PlaybackMutationIntent),
    TurtleSoup(TurtleSoupMutationIntent),
}

pub(crate) enum BusinessMutationOutcome {
    Administration(AdministrationMutationOutcome),
    Hall(HallMutationOutcome),
    Playback(PlaybackMutationOutcome),
    TurtleSoup(TurtleSoupMutationOutcome),
}

/// Narrow projection port for business-owned public state. Implementations must only retain
/// the already-redacted snapshots; business internals never depend on the monitor shape.
pub(crate) trait BusinessStateSink: Send + Sync {
    fn publish_turtle_soup(&self, snapshot: TurtleSoupSnapshot);
    fn publish_undercover(&self, snapshot: UndercoverSnapshot);
    fn publish_playback_queue(&self, _queue: Vec<QueueItem>) {}
    fn publish_hall_remaining_minutes(&self, _minutes: Option<u32>) {}
    fn publish_scheduler(&self, _snapshot: FormalSchedulerSnapshot) {}
    fn publish_chat_listener(&self, _snapshot: ChatListenerSnapshot) {}
    fn publish_decision(&self, _snapshot: Option<DecisionSnapshot>) {}
    fn publish_operational(&self, _snapshot: BusinessOperationalSnapshot) {}
    fn publish_diagnostics(&self, _snapshot: Vec<DiagnosticTaskSnapshot>) {}
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusinessTimerCounts {
    card_game: u64,
    undercover: u64,
    turtle_soup: u64,
    idiom_chain: u64,
    command_completed: u64,
    command_failed: u64,
}

impl BusinessTimerCounts {
    pub const fn card_game(self) -> u64 {
        self.card_game
    }

    pub const fn undercover(self) -> u64 {
        self.undercover
    }

    pub const fn turtle_soup(self) -> u64 {
        self.turtle_soup
    }

    pub const fn idiom_chain(self) -> u64 {
        self.idiom_chain
    }

    pub const fn command_completed(self) -> u64 {
        self.command_completed
    }

    pub const fn command_failed(self) -> u64 {
        self.command_failed
    }

    fn record_timer_event(&mut self, event: &BusinessDeadlineEvent) {
        let (expiration_count, (is_completion, is_failure)) = match event {
            BusinessDeadlineEvent::CardGame(event) => {
                (&mut self.card_game, timer_event_facts(event))
            }
            BusinessDeadlineEvent::Undercover(event) => {
                (&mut self.undercover, timer_event_facts(event))
            }
            BusinessDeadlineEvent::TurtleSoup(event) => {
                (&mut self.turtle_soup, timer_event_facts(event))
            }
            BusinessDeadlineEvent::IdiomChain(event) => {
                (&mut self.idiom_chain, timer_event_facts(event))
            }
        };
        if !is_completion {
            *expiration_count = expiration_count.saturating_add(1);
        } else {
            self.command_completed = self.command_completed.saturating_add(1);
            if is_failure {
                self.command_failed = self.command_failed.saturating_add(1);
                log::warn!("计时运行时命令失败: {event:?}");
            }
        }
    }
}

fn timer_event_facts<T>(event: &TimerRuntimeEvent<T>) -> (bool, bool) {
    match event {
        TimerRuntimeEvent::DeadlineExpired(_) => (false, false),
        TimerRuntimeEvent::CommandCompleted(completed) => (true, completed.result().is_err()),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusinessRuntimeSnapshot {
    latest_watermark: Option<ObservationWatermark>,
    terminal_failure_count: u64,
    completion_gap_count: u64,
    timer_counts: BusinessTimerCounts,
    quiescing: bool,
}

impl BusinessRuntimeSnapshot {
    pub const fn latest_watermark(self) -> Option<ObservationWatermark> {
        self.latest_watermark
    }

    pub const fn terminal_failure_count(self) -> u64 {
        self.terminal_failure_count
    }

    pub const fn completion_gap_count(self) -> u64 {
        self.completion_gap_count
    }

    pub const fn timer_counts(self) -> BusinessTimerCounts {
        self.timer_counts
    }

    pub const fn is_quiescing(self) -> bool {
        self.quiescing
    }

    fn apply(&mut self, event: BusinessEvent) {
        match event {
            BusinessEvent::CompletionAdvance(advance) => {
                self.terminal_failure_count = self.terminal_failure_count.saturating_add(
                    advance
                        .events()
                        .iter()
                        .filter(|event| {
                            matches!(event, ObservationCompletionEvent::TerminalFailure { .. })
                        })
                        .count() as u64,
                );
                if let Some(watermark) = advance.watermark() {
                    self.latest_watermark = Some(watermark);
                }
            }
            BusinessEvent::CompletionGap(_) => {
                self.completion_gap_count = self.completion_gap_count.saturating_add(1);
            }
            BusinessEvent::Timer(event) => {
                self.timer_counts.record_timer_event(&event);
            }
            BusinessEvent::TurtleSoupAiCompleted(_) => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BusinessOperationalSnapshot {
    commands_enabled: bool,
    idle_exit_remaining_seconds: Option<u64>,
}

impl BusinessOperationalSnapshot {
    #[cfg(test)]
    pub(crate) const fn new(
        commands_enabled: bool,
        idle_exit_remaining_seconds: Option<u64>,
    ) -> Self {
        Self {
            commands_enabled,
            idle_exit_remaining_seconds,
        }
    }

    pub(crate) const fn commands_enabled(self) -> bool {
        self.commands_enabled
    }

    pub(crate) const fn idle_exit_remaining_seconds(self) -> Option<u64> {
        self.idle_exit_remaining_seconds
    }
}

struct IdleExitSchedule {
    timeout: Duration,
    last_command_at: Instant,
}

struct OperationalState {
    commands_enabled: bool,
    idle_exit: Option<IdleExitSchedule>,
}

impl OperationalState {
    fn new() -> Self {
        Self {
            commands_enabled: true,
            idle_exit: None,
        }
    }

    fn snapshot(&self, now: Instant) -> BusinessOperationalSnapshot {
        BusinessOperationalSnapshot {
            commands_enabled: self.commands_enabled,
            idle_exit_remaining_seconds: self.idle_exit.as_ref().map(|schedule| {
                schedule
                    .last_command_at
                    .checked_add(schedule.timeout)
                    .unwrap_or(schedule.last_command_at)
                    .saturating_duration_since(now)
                    .as_secs()
            }),
        }
    }

    fn configure_idle_exit(&mut self, timeout: Duration, now: Instant) {
        self.idle_exit = Some(IdleExitSchedule {
            timeout,
            last_command_at: now,
        });
    }

    fn record_command_activity(&mut self, now: Instant) {
        if let Some(schedule) = self.idle_exit.as_mut() {
            schedule.last_command_at = now;
        }
    }

    fn claim_idle_exit(&mut self, now: Instant, scheduler_idle: bool) -> Option<Duration> {
        if !scheduler_idle {
            return None;
        }
        let schedule = self.idle_exit.as_ref()?;
        let deadline = schedule
            .last_command_at
            .checked_add(schedule.timeout)
            .unwrap_or(schedule.last_command_at);
        if now < deadline {
            return None;
        }
        self.idle_exit.take().map(|schedule| schedule.timeout)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusinessRuntimeError {
    ZeroQueueCapacity,
    Quiescing,
    RuntimeStopped,
    WorkerPanicked,
    IdiomChainOperationFailed(String),
    CardGameOperationFailed(String),
    UndercoverOperationFailed(String),
    TurtleSoupOperationFailed(String),
    HallOperationFailed(String),
    PlaybackOperationFailed(String),
    TimerOperationFailed(String),
    SchedulerOperationFailed(String),
    DecisionOperationFailed(String),
}

impl Display for BusinessRuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroQueueCapacity => {
                formatter.write_str("business runtime queue capacity must be greater than zero")
            }
            Self::Quiescing => formatter.write_str("business runtime is quiescing"),
            Self::RuntimeStopped => formatter.write_str("business runtime is stopped"),
            Self::WorkerPanicked => formatter.write_str("business runtime worker panicked"),
            Self::IdiomChainOperationFailed(message) => {
                write!(formatter, "idiom chain operation failed: {message}")
            }
            Self::CardGameOperationFailed(message) => {
                write!(formatter, "card game operation failed: {message}")
            }
            Self::UndercoverOperationFailed(message) => {
                write!(formatter, "undercover operation failed: {message}")
            }
            Self::TurtleSoupOperationFailed(message) => {
                write!(formatter, "turtle soup operation failed: {message}")
            }
            Self::HallOperationFailed(message) => {
                write!(formatter, "hall operation failed: {message}")
            }
            Self::PlaybackOperationFailed(message) => {
                write!(formatter, "playback operation failed: {message}")
            }
            Self::TimerOperationFailed(message) => {
                write!(formatter, "business timer operation failed: {message}")
            }
            Self::SchedulerOperationFailed(message) => {
                write!(formatter, "business scheduler operation failed: {message}")
            }
            Self::DecisionOperationFailed(message) => {
                write!(formatter, "business decision operation failed: {message}")
            }
        }
    }
}

impl Error for BusinessRuntimeError {}

enum RuntimeMessage {
    Event(BusinessEvent),
    EnqueueFormalTask {
        submission: FormalTaskSubmission,
        response: SyncSender<Result<FormalTaskEnqueueOutcome, BusinessRuntimeError>>,
    },
    FormalSchedulerSnapshot(SyncSender<Result<FormalSchedulerSnapshot, BusinessRuntimeError>>),
    FormalTaskContainsDedupKey {
        key: FormalTaskDedupKey,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    TakeNextFormalTask(SyncSender<Result<Option<FormalTaskLease>, BusinessRuntimeError>>),
    RestoreFormalTask {
        lease: FormalTaskLease,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    CompleteFormalTask {
        task_id: u64,
        completion: FormalTaskCompletion,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    CancelFormalTask {
        task_id: u64,
        response: SyncSender<Result<Option<Box<dyn FormalTaskWork>>, BusinessRuntimeError>>,
    },
    ReleaseSchedulerLane {
        lease: SchedulerLaneLease,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    EnqueueDeferredChat {
        item: DeferredChatItem,
        response: SyncSender<Result<EnqueueOutcome, BusinessRuntimeError>>,
    },
    RequeueDeferredChatFront {
        item: DeferredChatItem,
        response: SyncSender<Result<EnqueueOutcome, BusinessRuntimeError>>,
    },
    RequeueDeferredChatBack {
        item: DeferredChatItem,
        response: SyncSender<Result<EnqueueOutcome, BusinessRuntimeError>>,
    },
    TakeNextDeferredChat(
        SyncSender<Result<Option<(DeferredChatItem, SchedulerLaneLease)>, BusinessRuntimeError>>,
    ),
    EnqueueDiagnosticTask {
        submission: DiagnosticTaskSubmission,
        response: SyncSender<Result<DiagnosticTaskSnapshot, BusinessRuntimeError>>,
    },
    TakeNextDiagnosticTask(SyncSender<Result<Option<DiagnosticTaskLease>, BusinessRuntimeError>>),
    CompleteDiagnosticTask {
        task_id: u64,
        completion: DiagnosticTaskCompletion,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    DiagnosticTaskSnapshot {
        id: u64,
        response: SyncSender<Result<Option<DiagnosticTaskSnapshot>, BusinessRuntimeError>>,
    },
    OperationalSnapshot {
        now: Instant,
        response: SyncSender<Result<BusinessOperationalSnapshot, BusinessRuntimeError>>,
    },
    SetCommandsEnabled {
        enabled: bool,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    ConfigureIdleExit {
        timeout: Duration,
        now: Instant,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    RecordCommandActivity {
        now: Instant,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    ClaimIdleExit {
        now: Instant,
        response: SyncSender<Result<Option<Duration>, BusinessRuntimeError>>,
    },
    ClearIdleExit(SyncSender<Result<(), BusinessRuntimeError>>),
    ChatListener(ChatListenerRuntimeMessage),
    Decision(DecisionRuntimeMessage),
    HandleIdiomChain {
        player: String,
        command: IdiomChainCommand,
        response: SyncSender<Result<IdiomChainOutcome, BusinessRuntimeError>>,
    },
    ExplainIdiomChain {
        player: String,
        command: IdiomChainCommand,
        response: SyncSender<Result<IdiomChainOutcome, BusinessRuntimeError>>,
    },
    AbortIdiomChain(SyncSender<Result<bool, BusinessRuntimeError>>),
    ExpireIdiomChain(SyncSender<Result<bool, BusinessRuntimeError>>),
    CardGame(CardGameRuntimeMessage),
    Undercover(UndercoverRuntimeMessage),
    UndercoverSnapshot(SyncSender<Result<UndercoverSnapshot, BusinessRuntimeError>>),
    TurtleSoup(TurtleSoupRuntimeMessage),
    TurtleSoupSnapshot(SyncSender<Result<TurtleSoupSnapshot, BusinessRuntimeError>>),
    InviteShouldAccept {
        sequence: Option<u32>,
        response: SyncSender<bool>,
    },
    BeginInvite {
        request: InviteRequest,
        response: SyncSender<InviteStart>,
    },
    AcquireModerationWorkflow {
        key: ModerationWorkflowKey,
        response: SyncSender<bool>,
    },
    ReleaseModerationWorkflow {
        key: ModerationWorkflowKey,
        response: SyncSender<bool>,
    },
    #[cfg(test)]
    ContainsModerationWorkflow {
        key: ModerationWorkflowKey,
        response: SyncSender<bool>,
    },
    Hall(HallRuntimeMessage),
    Playback(PlaybackRuntimeMessage),
    RefreshTurtleSoup {
        now: Instant,
        clock_active: bool,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    ActiveEntertainment(SyncSender<Option<EntertainmentKind>>),
    Snapshot(SyncSender<BusinessRuntimeSnapshot>),
    PrepareShutdown(SyncSender<BusinessRuntimeSnapshot>),
    Shutdown(SyncSender<BusinessRuntimeSnapshot>),
}

enum HallRuntimeMessage {
    PatchState {
        patch: HallStatePatch,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    StateSnapshot(SyncSender<Result<HallRuntimeState, BusinessRuntimeError>>),
    UpdateRemainingMinutes {
        minutes: u32,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    ClearRemainingMinutes(SyncSender<Result<(), BusinessRuntimeError>>),
    ClearCountdownCache(SyncSender<Result<bool, BusinessRuntimeError>>),
}

enum PlaybackRuntimeMessage {
    PushQueue {
        item: QueueItem,
        response: SyncSender<Result<QueuePushOutcome, BusinessRuntimeError>>,
    },
    RemoveQueue {
        removal: QueueRemoval,
        response: SyncSender<Result<QueueRemoveOutcome, BusinessRuntimeError>>,
    },
    RemoveQueueIndexes {
        indexes: Vec<usize>,
        response: SyncSender<Result<Vec<(usize, QueueItem)>, BusinessRuntimeError>>,
    },
    ClearQueue(SyncSender<Result<usize, BusinessRuntimeError>>),
    QueueContains {
        item: QueueItem,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    QueueSnapshot(SyncSender<Result<Vec<QueueItem>, BusinessRuntimeError>>),
    StateSnapshot(SyncSender<Result<PlaybackRuntimeState, BusinessRuntimeError>>),
    UpdatePlaybackState {
        update: PlaybackStateUpdate,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    CheckSongDedup {
        candidate: SongDedupCandidate,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    RecordSongDedup {
        candidate: SongDedupCandidate,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    ObserveExternalPlayback {
        identity: String,
        now: Instant,
        protect_after: Duration,
        response: SyncSender<Result<ExternalPlaybackObservation, BusinessRuntimeError>>,
    },
    ClearExternalPlaybackTracker(SyncSender<Result<(), BusinessRuntimeError>>),
}

enum ChatListenerRuntimeMessage {
    Snapshot(SyncSender<Result<ChatListenerSnapshot, BusinessRuntimeError>>),
    RequestMode {
        target: ChatListenerMode,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    CompleteMode {
        mode: ChatListenerMode,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    CancelModeRequest {
        target: ChatListenerMode,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    FailModeSwitchToPrimary(SyncSender<Result<(), BusinessRuntimeError>>),
    BeginTemporaryPrimary(SyncSender<Result<(), BusinessRuntimeError>>),
    EndTemporaryPrimary(SyncSender<Result<(), BusinessRuntimeError>>),
    ClaimUnreadTask(SyncSender<Result<bool, BusinessRuntimeError>>),
    FinishUnreadTask {
        processed_message: bool,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    ReleaseUnreadTask(SyncSender<Result<(), BusinessRuntimeError>>),
    FinishInitialUnreadClear(SyncSender<Result<(), BusinessRuntimeError>>),
    FinishHallRound(SyncSender<Result<(), BusinessRuntimeError>>),
}

enum DecisionRuntimeMessage {
    Begin {
        label: String,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout: Duration,
        delivery: SyncSender<DecisionAction>,
        response: SyncSender<Result<u64, BusinessRuntimeError>>,
    },
    #[cfg(test)]
    Snapshot(SyncSender<Result<Option<DecisionSnapshot>, BusinessRuntimeError>>),
    Submit {
        id: u64,
        action: DecisionAction,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    Finish {
        id: u64,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveDeadline {
    token: BusinessDeadlineToken,
    operation_id: BusinessOperationId,
    session_generation: SessionGeneration,
    deadline: Instant,
}

type ActiveIdiomDeadline = ActiveDeadline;
type ActiveCardGameDeadline = ActiveDeadline;
type ActiveUndercoverDeadline = ActiveDeadline;
type ActiveTurtleSoupDeadline = ActiveDeadline;

enum CardGameRuntimeMessage {
    Begin {
        player: String,
        command: LandlordCommand,
        now: Instant,
        response: SyncSender<Result<CardGameCommandStart, BusinessRuntimeError>>,
    },
    Claim {
        key: CardGameEffectKey,
        response: SyncSender<Result<CardGameEffectClaim, BusinessRuntimeError>>,
    },
    Resume {
        key: CardGameEffectKey,
        result: CardGameEffectResult,
        response: SyncSender<Result<CardGameResume, BusinessRuntimeError>>,
    },
    Cancel {
        key: CardGameEffectKey,
        response: SyncSender<Result<CardGameCancel, BusinessRuntimeError>>,
    },
    Tick {
        now: Instant,
        clock_active: bool,
        response: SyncSender<Result<Option<CardGameTimedOutcome>, BusinessRuntimeError>>,
    },
    Abort(SyncSender<Result<bool, BusinessRuntimeError>>),
}

enum UndercoverRuntimeMessage {
    Begin {
        player: String,
        source: UndercoverCommandSource,
        command: UndercoverCommand,
        now: Instant,
        response: SyncSender<Result<UndercoverCommandStart, BusinessRuntimeError>>,
    },
    Claim {
        key: UndercoverEffectKey,
        response: SyncSender<Result<UndercoverEffectClaim, BusinessRuntimeError>>,
    },
    Resume {
        key: UndercoverEffectKey,
        result: UndercoverEffectResult,
        response: SyncSender<Result<UndercoverResume, BusinessRuntimeError>>,
    },
    Cancel {
        key: UndercoverEffectKey,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    Poll {
        now: Instant,
        clock_active: bool,
        response: SyncSender<Result<Option<UndercoverTimedOutcome>, BusinessRuntimeError>>,
    },
    Abort(SyncSender<Result<bool, BusinessRuntimeError>>),
}

enum TurtleSoupRuntimeMessage {
    HallCommand {
        player: String,
        command: TurtleSoupCommand,
        response: SyncSender<TurtleSoupCommandOutcome>,
    },
    FriendCommand {
        player: String,
        command: TurtleSoupCommand,
        response: SyncSender<TurtleSoupCommandOutcome>,
    },
    StartRandom {
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    StartById {
        id: String,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    End {
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    FilterPrimary {
        visible: Vec<TurtleSoupQuestion>,
        suppress_new: bool,
        response: SyncSender<Vec<TurtleSoupQuestion>>,
    },
    StabilizeSecondary {
        observations: Vec<SecondaryOcrObservation>,
        response: SyncSender<SecondaryOcrStability>,
    },
    ClearSecondary,
    Accepts(SyncSender<bool>),
    Submit {
        question: TurtleSoupQuestion,
        response: SyncSender<Result<QuestionSubmitOutcome, BusinessRuntimeError>>,
    },
    Abort {
        reason: String,
    },
    DeliveryCurrent {
        delivery: TurtleSoupDelivery,
        response: SyncSender<bool>,
    },
    DeliverySuccess {
        delivery: TurtleSoupDelivery,
    },
    DeliveryFailure {
        delivery: TurtleSoupDelivery,
        error: String,
    },
    AppendPuzzle {
        submission: TurtleSoupSubmission,
        response: SyncSender<Result<TurtleSoupAppendReceipt, BusinessRuntimeError>>,
    },
}

struct RuntimeChannel {
    sender: SyncSender<RuntimeMessage>,
    state: Mutex<RuntimeChannelState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeChannelState {
    Running,
    Quiescing,
    Stopped,
}

#[derive(Clone)]
pub struct BusinessRuntimeHandle {
    channel: Arc<RuntimeChannel>,
}

pub(crate) struct SchedulerPermit {
    handle: BusinessRuntimeHandle,
    lease: Option<SchedulerLaneLease>,
}

pub(crate) struct DecisionSession {
    handle: BusinessRuntimeHandle,
    id: u64,
    receiver: Receiver<DecisionAction>,
}

impl DecisionSession {
    #[cfg(test)]
    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn wait(
        &self,
        timeout: Duration,
    ) -> Result<Option<DecisionAction>, BusinessRuntimeError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(action) => Ok(Some(action)),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Ok(None),
        }
    }
}

impl Drop for DecisionSession {
    fn drop(&mut self) {
        if let Err(error) = self.handle.finish_decision(self.id) {
            log::debug!("结束 Web 决策失败: {error}");
        }
    }
}

impl Drop for SchedulerPermit {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        if let Err(error) = self.handle.release_scheduler_lane(lease) {
            log::debug!("释放调度通道租约失败: {error}");
        }
    }
}

impl CardGameRuntimePort for BusinessRuntimeHandle {
    fn begin(
        &self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> anyhow::Result<CardGameCommandStart> {
        Ok(self.begin_card_game(player, command, now)?)
    }

    fn claim(&self, key: CardGameEffectKey) -> anyhow::Result<CardGameEffectClaim> {
        Ok(self.claim_card_game_effect(key)?)
    }

    fn resume(
        &self,
        key: CardGameEffectKey,
        result: CardGameEffectResult,
    ) -> anyhow::Result<CardGameResume> {
        Ok(self.resume_card_game(key, result)?)
    }

    fn cancel(&self, key: CardGameEffectKey) -> anyhow::Result<()> {
        let _ = self.cancel_card_game_effect(key)?;
        Ok(())
    }

    fn poll_timed_outcome(
        &self,
        now: Instant,
        clock_active: bool,
    ) -> anyhow::Result<Option<CardGameTimedOutcome>> {
        Ok(self.poll_card_game_timed_outcome(now, clock_active)?)
    }

    fn abort(&self) -> anyhow::Result<bool> {
        Ok(self.abort_card_game()?)
    }
}

impl UndercoverRuntimePort for BusinessRuntimeHandle {
    fn begin(
        &self,
        player: &str,
        source: UndercoverCommandSource,
        command: &UndercoverCommand,
        now: Instant,
    ) -> anyhow::Result<UndercoverCommandStart> {
        Ok(self.begin_undercover(player, source, command, now)?)
    }

    fn claim(&self, key: UndercoverEffectKey) -> anyhow::Result<UndercoverEffectClaim> {
        Ok(self.claim_undercover_effect(key)?)
    }

    fn resume(
        &self,
        key: UndercoverEffectKey,
        result: UndercoverEffectResult,
    ) -> anyhow::Result<UndercoverResume> {
        Ok(self.resume_undercover(key, result)?)
    }

    fn cancel(&self, key: UndercoverEffectKey) -> anyhow::Result<()> {
        Ok(self.cancel_undercover_effect(key)?)
    }

    fn poll_timed_outcome(
        &self,
        now: Instant,
        clock_active: bool,
    ) -> anyhow::Result<Option<UndercoverTimedOutcome>> {
        Ok(self.poll_undercover_timed_outcome(now, clock_active)?)
    }

    fn abort(&self) -> anyhow::Result<bool> {
        Ok(self.abort_undercover()?)
    }
}

impl BusinessRuntimeHandle {
    pub fn submit(&self, event: BusinessEvent) -> Result<(), BusinessRuntimeError> {
        self.send_request(RuntimeMessage::Event(event))
    }

    pub fn snapshot(&self) -> Result<BusinessRuntimeSnapshot, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::Snapshot(response))?;
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }

    pub(crate) fn apply_mutation(
        &self,
        intent: BusinessMutationIntent,
    ) -> Result<BusinessMutationOutcome, BusinessRuntimeError> {
        Ok(match intent {
            BusinessMutationIntent::Administration(intent) => {
                BusinessMutationOutcome::Administration(self.apply_administration_mutation(intent)?)
            }
            BusinessMutationIntent::Hall(intent) => {
                BusinessMutationOutcome::Hall(self.apply_hall_mutation(intent)?)
            }
            BusinessMutationIntent::Playback(intent) => {
                BusinessMutationOutcome::Playback(self.apply_playback_mutation(intent)?)
            }
            BusinessMutationIntent::TurtleSoup(intent) => {
                BusinessMutationOutcome::TurtleSoup(self.apply_turtle_soup_mutation(intent)?)
            }
        })
    }

    fn apply_administration_mutation(
        &self,
        intent: AdministrationMutationIntent,
    ) -> Result<AdministrationMutationOutcome, BusinessRuntimeError> {
        Ok(match intent {
            AdministrationMutationIntent::RequestChatListenerMode(target) => {
                let queued = self.request_chat_listener_mode(target)?;
                let snapshot = self.chat_listener_snapshot()?;
                AdministrationMutationOutcome::ChatListenerModeRequested { queued, snapshot }
            }
            AdministrationMutationIntent::CancelChatListenerModeRequest(target) => {
                self.cancel_chat_listener_mode_request(target)?;
                AdministrationMutationOutcome::ChatListenerModeRequestCancelled
            }
        })
    }

    fn apply_playback_mutation(
        &self,
        intent: PlaybackMutationIntent,
    ) -> Result<PlaybackMutationOutcome, BusinessRuntimeError> {
        Ok(match intent {
            PlaybackMutationIntent::Push(item) => {
                PlaybackMutationOutcome::Pushed(self.push_playback_queue(item)?)
            }
            PlaybackMutationIntent::Remove(removal) => {
                PlaybackMutationOutcome::Removed(self.remove_playback_queue(removal)?)
            }
            PlaybackMutationIntent::Clear => {
                self.clear_playback_queue()?;
                PlaybackMutationOutcome::Cleared
            }
        })
    }

    fn apply_hall_mutation(
        &self,
        intent: HallMutationIntent,
    ) -> Result<HallMutationOutcome, BusinessRuntimeError> {
        Ok(match intent {
            HallMutationIntent::PatchState(patch) => {
                self.patch_hall_state(patch)?;
                HallMutationOutcome::StatePatched
            }
        })
    }

    fn apply_turtle_soup_mutation(
        &self,
        intent: TurtleSoupMutationIntent,
    ) -> Result<TurtleSoupMutationOutcome, BusinessRuntimeError> {
        Ok(match intent {
            TurtleSoupMutationIntent::Start { puzzle_id } => {
                if let Some(id) = puzzle_id {
                    self.start_turtle_soup_by_id(&id)?;
                } else {
                    self.start_turtle_soup_random()?;
                }
                TurtleSoupMutationOutcome::Started(self.turtle_soup_snapshot()?)
            }
            TurtleSoupMutationIntent::End => {
                let ended = self.end_turtle_soup()?;
                let snapshot = self.turtle_soup_snapshot()?;
                TurtleSoupMutationOutcome::Ended { ended, snapshot }
            }
            TurtleSoupMutationIntent::AppendPuzzle(submission) => {
                TurtleSoupMutationOutcome::PuzzleAppended(
                    self.append_turtle_soup_puzzle(submission)?,
                )
            }
        })
    }

    pub(crate) fn enqueue_formal_task(
        &self,
        submission: FormalTaskSubmission,
    ) -> Result<FormalTaskEnqueueOutcome, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::EnqueueFormalTask {
            submission,
            response,
        })
    }

    pub(crate) fn take_next_formal_task(
        &self,
    ) -> Result<Option<FormalTaskLease>, BusinessRuntimeError> {
        self.request(RuntimeMessage::TakeNextFormalTask)
    }

    pub(crate) fn formal_task_contains_dedup_key(
        &self,
        key: FormalTaskDedupKey,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::FormalTaskContainsDedupKey { key, response })
    }

    pub(crate) fn restore_formal_task(
        &self,
        lease: FormalTaskLease,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::RestoreFormalTask { lease, response })
    }

    pub(crate) fn complete_formal_task(
        &self,
        task_id: u64,
        completion: FormalTaskCompletion,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::CompleteFormalTask {
            task_id,
            completion,
            response,
        })
    }

    pub(crate) fn cancel_formal_task(
        &self,
        task_id: u64,
    ) -> Result<FormalTaskCancelOutcome, BusinessRuntimeError> {
        let work =
            self.request(|response| RuntimeMessage::CancelFormalTask { task_id, response })?;
        let Some(work) = work else {
            return Ok(FormalTaskCancelOutcome::NotQueued);
        };
        work.cancel();
        Ok(FormalTaskCancelOutcome::Canceled)
    }

    pub(crate) fn scheduler_snapshot(
        &self,
    ) -> Result<FormalSchedulerSnapshot, BusinessRuntimeError> {
        self.request(RuntimeMessage::FormalSchedulerSnapshot)
    }

    fn release_scheduler_lane(
        &self,
        lease: SchedulerLaneLease,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::ReleaseSchedulerLane { lease, response })
    }

    pub(crate) fn enqueue_deferred_chat(
        &self,
        item: impl Into<DeferredChatItem>,
    ) -> Result<EnqueueOutcome, BusinessRuntimeError> {
        let item = item.into();
        self.request(|response| RuntimeMessage::EnqueueDeferredChat { item, response })
    }

    pub(crate) fn requeue_deferred_chat_front(
        &self,
        item: DeferredChatItem,
    ) -> Result<EnqueueOutcome, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::RequeueDeferredChatFront { item, response })
    }

    pub(crate) fn requeue_deferred_chat_back(
        &self,
        item: DeferredChatItem,
    ) -> Result<EnqueueOutcome, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::RequeueDeferredChatBack { item, response })
    }

    pub(crate) fn take_next_deferred_chat(
        &self,
    ) -> Result<Option<(DeferredChatItem, SchedulerPermit)>, BusinessRuntimeError> {
        let item = self.request(RuntimeMessage::TakeNextDeferredChat)?;
        Ok(item.map(|(item, lease)| {
            (
                item,
                SchedulerPermit {
                    handle: self.clone(),
                    lease: Some(lease),
                },
            )
        }))
    }

    pub(crate) fn enqueue_diagnostic_task(
        &self,
        submission: DiagnosticTaskSubmission,
    ) -> Result<DiagnosticTaskSnapshot, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::EnqueueDiagnosticTask {
            submission,
            response,
        })
    }

    pub(crate) fn take_next_diagnostic_task(
        &self,
    ) -> Result<Option<DiagnosticTaskLease>, BusinessRuntimeError> {
        self.request(RuntimeMessage::TakeNextDiagnosticTask)
    }

    pub(crate) fn complete_diagnostic_task(
        &self,
        task_id: u64,
        completion: DiagnosticTaskCompletion,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::CompleteDiagnosticTask {
            task_id,
            completion,
            response,
        })
    }

    pub(crate) fn diagnostic_task_snapshot(
        &self,
        id: u64,
    ) -> Result<Option<DiagnosticTaskSnapshot>, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::DiagnosticTaskSnapshot { id, response })
    }

    pub(crate) fn operational_snapshot(
        &self,
        now: Instant,
    ) -> Result<BusinessOperationalSnapshot, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::OperationalSnapshot { now, response })
    }

    pub(crate) fn set_commands_enabled(&self, enabled: bool) -> Result<(), BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::SetCommandsEnabled { enabled, response })
    }

    pub(crate) fn configure_idle_exit(
        &self,
        timeout: Duration,
        now: Instant,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::ConfigureIdleExit {
            timeout,
            now,
            response,
        })
    }

    pub(crate) fn record_command_activity(&self, now: Instant) -> Result<(), BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::RecordCommandActivity { now, response })
    }

    pub(crate) fn claim_idle_exit(
        &self,
        now: Instant,
    ) -> Result<Option<Duration>, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::ClaimIdleExit { now, response })
    }

    pub(crate) fn clear_idle_exit(&self) -> Result<(), BusinessRuntimeError> {
        self.request(RuntimeMessage::ClearIdleExit)
    }

    pub(crate) fn chat_listener_snapshot(
        &self,
    ) -> Result<ChatListenerSnapshot, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::Snapshot(response))
        })
    }

    pub(crate) fn request_chat_listener_mode(
        &self,
        target: ChatListenerMode,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::RequestMode {
                target,
                response,
            })
        })
    }

    pub(crate) fn complete_chat_listener_mode(
        &self,
        mode: ChatListenerMode,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::CompleteMode {
                mode,
                response,
            })
        })
    }

    pub(crate) fn cancel_chat_listener_mode_request(
        &self,
        target: ChatListenerMode,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::CancelModeRequest {
                target,
                response,
            })
        })
    }

    pub(crate) fn fail_chat_listener_mode_to_primary(&self) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::FailModeSwitchToPrimary(
                response,
            ))
        })
    }

    pub(crate) fn begin_chat_listener_temporary_primary(&self) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::BeginTemporaryPrimary(
                response,
            ))
        })
    }

    pub(crate) fn end_chat_listener_temporary_primary(&self) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::EndTemporaryPrimary(response))
        })
    }

    pub(crate) fn claim_chat_listener_unread_task(&self) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::ClaimUnreadTask(response))
        })
    }

    pub(crate) fn finish_chat_listener_unread_task(
        &self,
        processed_message: bool,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::FinishUnreadTask {
                processed_message,
                response,
            })
        })
    }

    pub(crate) fn release_chat_listener_unread_task(&self) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::ReleaseUnreadTask(response))
        })
    }

    pub(crate) fn finish_chat_listener_initial_unread_clear(
        &self,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::FinishInitialUnreadClear(
                response,
            ))
        })
    }

    pub(crate) fn finish_chat_listener_hall_round(&self) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::ChatListener(ChatListenerRuntimeMessage::FinishHallRound(response))
        })
    }

    pub(crate) fn begin_decision(
        &self,
        label: impl Into<String>,
        allow_switch_source: bool,
        allow_ai: bool,
        timeout: Duration,
    ) -> Result<DecisionSession, BusinessRuntimeError> {
        let (delivery, receiver) = mpsc::sync_channel(1);
        let id = self.request(|response| {
            RuntimeMessage::Decision(DecisionRuntimeMessage::Begin {
                label: label.into(),
                allow_switch_source,
                allow_ai,
                timeout,
                delivery,
                response,
            })
        })?;
        Ok(DecisionSession {
            handle: self.clone(),
            id,
            receiver,
        })
    }

    #[cfg(test)]
    pub(crate) fn decision_snapshot(
        &self,
    ) -> Result<Option<DecisionSnapshot>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Decision(DecisionRuntimeMessage::Snapshot(response))
        })
    }

    pub(crate) fn submit_decision(
        &self,
        id: u64,
        action: DecisionAction,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Decision(DecisionRuntimeMessage::Submit {
                id,
                action,
                response,
            })
        })
    }

    fn finish_decision(&self, id: u64) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Decision(DecisionRuntimeMessage::Finish { id, response })
        })
    }

    pub(crate) fn push_playback_queue(
        &self,
        item: QueueItem,
    ) -> Result<QueuePushOutcome, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::PushQueue { item, response })
        })
    }

    pub(crate) fn remove_playback_queue(
        &self,
        removal: QueueRemoval,
    ) -> Result<QueueRemoveOutcome, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::RemoveQueue { removal, response })
        })
    }

    pub(crate) fn remove_playback_queue_indexes(
        &self,
        indexes: Vec<usize>,
    ) -> Result<Vec<(usize, QueueItem)>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::RemoveQueueIndexes {
                indexes,
                response,
            })
        })
    }

    pub(crate) fn clear_playback_queue(&self) -> Result<usize, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::ClearQueue(response))
        })
    }

    pub(crate) fn playback_queue_snapshot(&self) -> Result<Vec<QueueItem>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::QueueSnapshot(response))
        })
    }

    pub(crate) fn playback_queue_contains(
        &self,
        item: QueueItem,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::QueueContains { item, response })
        })
    }

    pub(crate) fn patch_hall_state(
        &self,
        patch: HallStatePatch,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Hall(HallRuntimeMessage::PatchState { patch, response })
        })
    }

    pub(crate) fn hall_state_snapshot(&self) -> Result<HallRuntimeState, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::Hall(HallRuntimeMessage::StateSnapshot(response)))
    }

    pub(crate) fn update_hall_remaining_minutes(
        &self,
        minutes: u32,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Hall(HallRuntimeMessage::UpdateRemainingMinutes { minutes, response })
        })
    }

    pub(crate) fn clear_hall_remaining_minutes(&self) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Hall(HallRuntimeMessage::ClearRemainingMinutes(response))
        })
    }

    pub(crate) fn clear_hall_countdown_cache(&self) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Hall(HallRuntimeMessage::ClearCountdownCache(response))
        })
    }

    pub(crate) fn playback_state_snapshot(
        &self,
    ) -> Result<PlaybackRuntimeState, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::StateSnapshot(response))
        })
    }

    pub(crate) fn update_playback_state(
        &self,
        update: PlaybackStateUpdate,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::UpdatePlaybackState {
                update,
                response,
            })
        })
    }

    pub(crate) fn song_dedup_limited(
        &self,
        candidate: SongDedupCandidate,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::CheckSongDedup {
                candidate,
                response,
            })
        })
    }

    pub(crate) fn record_song_dedup(
        &self,
        candidate: SongDedupCandidate,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::RecordSongDedup {
                candidate,
                response,
            })
        })
    }

    pub(crate) fn observe_external_playback(
        &self,
        identity: String,
        now: Instant,
        protect_after: Duration,
    ) -> Result<ExternalPlaybackObservation, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::ObserveExternalPlayback {
                identity,
                now,
                protect_after,
                response,
            })
        })
    }

    pub(crate) fn clear_external_playback_tracker(&self) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::ClearExternalPlaybackTracker(
                response,
            ))
        })
    }

    pub(crate) fn active_entertainment(
        &self,
    ) -> Result<Option<EntertainmentKind>, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::ActiveEntertainment(response))?;
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }

    pub(crate) fn invite_should_accept(
        &self,
        sequence: Option<u32>,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request_value(|response| RuntimeMessage::InviteShouldAccept { sequence, response })
    }

    pub(crate) fn begin_invite(
        &self,
        request: InviteRequest,
    ) -> Result<InviteStart, BusinessRuntimeError> {
        self.request_value(|response| RuntimeMessage::BeginInvite { request, response })
    }

    fn acquire_moderation_workflow(
        &self,
        key: ModerationWorkflowKey,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request_value(|response| RuntimeMessage::AcquireModerationWorkflow { key, response })
    }

    fn release_moderation_workflow(
        &self,
        key: ModerationWorkflowKey,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request_value(|response| RuntimeMessage::ReleaseModerationWorkflow { key, response })
    }

    #[cfg(test)]
    fn contains_moderation_workflow(
        &self,
        key: ModerationWorkflowKey,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request_value(|response| RuntimeMessage::ContainsModerationWorkflow { key, response })
    }

    pub fn handle_idiom_chain(
        &self,
        player: &str,
        command: &IdiomChainCommand,
    ) -> Result<IdiomChainOutcome, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::HandleIdiomChain {
            player: player.to_string(),
            command: command.clone(),
            response,
        })
    }

    pub fn explain_idiom_chain(
        &self,
        player: &str,
        command: &IdiomChainCommand,
    ) -> Result<IdiomChainOutcome, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::ExplainIdiomChain {
            player: player.to_string(),
            command: command.clone(),
            response,
        })
    }

    pub fn abort_idiom_chain(&self) -> Result<bool, BusinessRuntimeError> {
        self.request(RuntimeMessage::AbortIdiomChain)
    }

    pub fn expire_idiom_chain(&self) -> Result<bool, BusinessRuntimeError> {
        self.request(RuntimeMessage::ExpireIdiomChain)
    }

    pub fn begin_card_game(
        &self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<CardGameCommandStart, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::CardGame(CardGameRuntimeMessage::Begin {
                player: player.to_string(),
                command: command.clone(),
                now,
                response,
            })
        })
    }

    pub fn claim_card_game_effect(
        &self,
        key: CardGameEffectKey,
    ) -> Result<CardGameEffectClaim, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::CardGame(CardGameRuntimeMessage::Claim { key, response })
        })
    }

    pub fn resume_card_game(
        &self,
        key: CardGameEffectKey,
        result: CardGameEffectResult,
    ) -> Result<CardGameResume, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::CardGame(CardGameRuntimeMessage::Resume {
                key,
                result,
                response,
            })
        })
    }

    pub fn cancel_card_game_effect(
        &self,
        key: CardGameEffectKey,
    ) -> Result<CardGameCancel, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::CardGame(CardGameRuntimeMessage::Cancel { key, response })
        })
    }

    pub fn poll_card_game_timed_outcome(
        &self,
        now: Instant,
        clock_active: bool,
    ) -> Result<Option<CardGameTimedOutcome>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::CardGame(CardGameRuntimeMessage::Tick {
                now,
                clock_active,
                response,
            })
        })
    }

    pub fn abort_card_game(&self) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::CardGame(CardGameRuntimeMessage::Abort(response)))
    }

    pub fn begin_undercover(
        &self,
        player: &str,
        source: UndercoverCommandSource,
        command: &UndercoverCommand,
        now: Instant,
    ) -> Result<UndercoverCommandStart, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Undercover(UndercoverRuntimeMessage::Begin {
                player: player.to_string(),
                source,
                command: command.clone(),
                now,
                response,
            })
        })
    }

    pub fn claim_undercover_effect(
        &self,
        key: UndercoverEffectKey,
    ) -> Result<UndercoverEffectClaim, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Undercover(UndercoverRuntimeMessage::Claim { key, response })
        })
    }

    pub fn resume_undercover(
        &self,
        key: UndercoverEffectKey,
        result: UndercoverEffectResult,
    ) -> Result<UndercoverResume, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Undercover(UndercoverRuntimeMessage::Resume {
                key,
                result,
                response,
            })
        })
    }

    pub fn cancel_undercover_effect(
        &self,
        key: UndercoverEffectKey,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Undercover(UndercoverRuntimeMessage::Cancel { key, response })
        })
    }

    pub fn abort_undercover(&self) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Undercover(UndercoverRuntimeMessage::Abort(response))
        })
    }

    pub fn poll_undercover_timed_outcome(
        &self,
        now: Instant,
        clock_active: bool,
    ) -> Result<Option<UndercoverTimedOutcome>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Undercover(UndercoverRuntimeMessage::Poll {
                now,
                clock_active,
                response,
            })
        })
    }

    pub fn undercover_snapshot(&self) -> Result<UndercoverSnapshot, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::UndercoverSnapshot(response))?;
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?
    }

    pub(crate) fn refresh_turtle_soup_deadline(
        &self,
        now: Instant,
        clock_active: bool,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::RefreshTurtleSoup {
            now,
            clock_active,
            response,
        })
    }

    pub(crate) fn turtle_soup_snapshot(&self) -> Result<TurtleSoupSnapshot, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::TurtleSoupSnapshot(response))?;
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?
    }

    pub(crate) fn handle_turtle_soup_hall_command(
        &self,
        player: &str,
        command: &TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome, BusinessRuntimeError> {
        self.request_value(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::HallCommand {
                player: player.to_string(),
                command: command.clone(),
                response,
            })
        })
    }

    pub(crate) fn handle_turtle_soup_friend_command(
        &self,
        player: &str,
        command: &TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome, BusinessRuntimeError> {
        self.request_value(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::FriendCommand {
                player: player.to_string(),
                command: command.clone(),
                response,
            })
        })
    }

    pub(crate) fn start_turtle_soup_random(&self) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::StartRandom { response })
        })
    }

    pub(crate) fn start_turtle_soup_by_id(&self, id: &str) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::StartById {
                id: id.to_string(),
                response,
            })
        })
    }

    pub(crate) fn end_turtle_soup(&self) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::End { response })
        })
    }

    pub(crate) fn filter_turtle_soup_primary_questions(
        &self,
        visible: Vec<TurtleSoupQuestion>,
        suppress_new: bool,
    ) -> Result<Vec<TurtleSoupQuestion>, BusinessRuntimeError> {
        self.request_value(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::FilterPrimary {
                visible,
                suppress_new,
                response,
            })
        })
    }

    pub(crate) fn stabilize_turtle_soup_secondary(
        &self,
        observations: Vec<SecondaryOcrObservation>,
    ) -> Result<SecondaryOcrStability, BusinessRuntimeError> {
        self.request_value(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::StabilizeSecondary {
                observations,
                response,
            })
        })
    }

    pub(crate) fn clear_turtle_soup_secondary_stability(&self) -> Result<(), BusinessRuntimeError> {
        self.send_request(RuntimeMessage::TurtleSoup(
            TurtleSoupRuntimeMessage::ClearSecondary,
        ))
    }

    pub(crate) fn turtle_soup_accepts_questions(&self) -> Result<bool, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send_query(RuntimeMessage::TurtleSoup(
            TurtleSoupRuntimeMessage::Accepts(response),
        ))?;
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }

    pub(crate) fn submit_turtle_soup_question(
        &self,
        question: TurtleSoupQuestion,
    ) -> Result<QuestionSubmitOutcome, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::Submit { question, response })
        })
    }

    pub(crate) fn abort_turtle_soup(&self, reason: &str) -> Result<(), BusinessRuntimeError> {
        self.send_request(RuntimeMessage::TurtleSoup(
            TurtleSoupRuntimeMessage::Abort {
                reason: reason.to_string(),
            },
        ))
    }

    pub(crate) fn turtle_soup_delivery_is_current(&self, delivery: TurtleSoupDelivery) -> bool {
        let (response, receiver) = mpsc::sync_channel(1);
        if self
            .send_query(RuntimeMessage::TurtleSoup(
                TurtleSoupRuntimeMessage::DeliveryCurrent { delivery, response },
            ))
            .is_err()
        {
            return false;
        }
        receiver.recv().unwrap_or(false)
    }

    pub(crate) fn turtle_soup_delivery_success(&self, delivery: TurtleSoupDelivery) {
        let _ = self.send_request(RuntimeMessage::TurtleSoup(
            TurtleSoupRuntimeMessage::DeliverySuccess { delivery },
        ));
    }

    pub(crate) fn turtle_soup_delivery_failure(
        &self,
        delivery: TurtleSoupDelivery,
        error: &anyhow::Error,
    ) {
        let _ = self.send_request(RuntimeMessage::TurtleSoup(
            TurtleSoupRuntimeMessage::DeliveryFailure {
                delivery,
                error: error.to_string(),
            },
        ));
    }

    pub(crate) fn append_turtle_soup_puzzle(
        &self,
        submission: TurtleSoupSubmission,
    ) -> Result<TurtleSoupAppendReceipt, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::AppendPuzzle {
                submission,
                response,
            })
        })
    }

    fn request<T>(
        &self,
        message: impl FnOnce(SyncSender<Result<T, BusinessRuntimeError>>) -> RuntimeMessage,
    ) -> Result<T, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send_request(message(response))?;
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?
    }

    fn request_value<T>(
        &self,
        message: impl FnOnce(SyncSender<T>) -> RuntimeMessage,
    ) -> Result<T, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send_request(message(response))?;
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }

    fn send_request(&self, message: RuntimeMessage) -> Result<(), BusinessRuntimeError> {
        let state = self
            .channel
            .state
            .lock()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
        match *state {
            RuntimeChannelState::Running => {}
            RuntimeChannelState::Quiescing => return Err(BusinessRuntimeError::Quiescing),
            RuntimeChannelState::Stopped => return Err(BusinessRuntimeError::RuntimeStopped),
        }
        self.channel
            .sender
            .send(message)
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }

    fn send_query(&self, message: RuntimeMessage) -> Result<(), BusinessRuntimeError> {
        let state = self
            .channel
            .state
            .lock()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
        if *state == RuntimeChannelState::Stopped {
            return Err(BusinessRuntimeError::RuntimeStopped);
        }
        self.channel
            .sender
            .send(message)
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }
}

impl ModerationWorkflowLedger for BusinessRuntimeHandle {
    fn acquire(&self, key: ModerationWorkflowKey) -> anyhow::Result<bool> {
        self.acquire_moderation_workflow(key)
            .map_err(anyhow::Error::from)
    }

    fn release(&self, key: ModerationWorkflowKey) -> anyhow::Result<bool> {
        self.release_moderation_workflow(key)
            .map_err(anyhow::Error::from)
    }

    #[cfg(test)]
    fn contains(&self, key: ModerationWorkflowKey) -> anyhow::Result<bool> {
        self.contains_moderation_workflow(key)
            .map_err(anyhow::Error::from)
    }
}

#[derive(Clone)]
pub struct BusinessRuntimeEventSink {
    channel: Arc<RuntimeChannel>,
}

impl BusinessRuntimeEventSink {
    pub fn submit(&self, event: BusinessEvent) -> Result<(), BusinessRuntimeError> {
        let state = self
            .channel
            .state
            .lock()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
        if *state == RuntimeChannelState::Stopped {
            return Err(BusinessRuntimeError::RuntimeStopped);
        }
        self.channel
            .sender
            .send(RuntimeMessage::Event(event))
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }
}

struct TurtleSoupBusinessEventPort {
    sink: BusinessRuntimeEventSink,
}

impl TurtleSoupAiCompletionPort for TurtleSoupBusinessEventPort {
    fn submit(&self, completion: TurtleSoupAiCompletion) {
        if let Err(error) = self
            .sink
            .submit(BusinessEvent::TurtleSoupAiCompleted(completion))
        {
            log::debug!("业务运行时已停止，丢弃海龟汤 AI 裁决结果: {error}");
        }
    }
}

pub struct BusinessRuntime {
    handle: BusinessRuntimeHandle,
    worker: Option<JoinHandle<()>>,
    turtle_soup_workers: Option<TurtleSoupWorkerRuntime>,
}

pub(crate) struct BusinessRuntimeWorker {
    idiom_chain: IdiomChainService,
    card_games: CardGameService,
    undercover: UndercoverRuntimeService,
    turtle_soup: Option<TurtleSoupService>,
    hall: Option<HallStateService>,
    playback: Option<PlaybackService>,
    invite: InviteService,
    timer: Option<TimerRuntimeHandle<BusinessDeadlineToken>>,
    state_sink: Option<Arc<dyn BusinessStateSink>>,
    clock: Arc<dyn Clock>,
}

impl BusinessRuntimeWorker {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
        undercover: UndercoverRuntimeService,
        turtle_soup: TurtleSoupService,
        hall: HallStateService,
        playback: PlaybackService,
        invite: InviteService,
        timer: TimerRuntimeHandle<BusinessDeadlineToken>,
        state_sink: Arc<dyn BusinessStateSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            idiom_chain,
            card_games,
            undercover,
            turtle_soup: Some(turtle_soup),
            hall: Some(hall),
            playback: Some(playback),
            invite,
            timer: Some(timer),
            state_sink: Some(state_sink),
            clock,
        }
    }

    #[cfg(test)]
    fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

impl BusinessRuntime {
    #[cfg(test)]
    pub(crate) fn start(
        queue_capacity: usize,
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
    ) -> Result<Self, BusinessRuntimeError> {
        Self::start_with_clock(
            queue_capacity,
            idiom_chain,
            card_games,
            Arc::new(SystemClock),
        )
    }

    #[cfg(test)]
    fn start_with_clock(
        queue_capacity: usize,
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, BusinessRuntimeError> {
        Self::start_internal(
            queue_capacity,
            BusinessRuntimeWorker {
                idiom_chain,
                card_games,
                undercover: default_undercover_service(),
                turtle_soup: None,
                hall: None,
                playback: None,
                invite: InviteService::new(),
                timer: None,
                state_sink: None,
                clock: Arc::new(SystemClock),
            }
            .with_clock(clock),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_playback(
        queue_capacity: usize,
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
        hall: HallStateService,
        playback: PlaybackService,
    ) -> Result<Self, BusinessRuntimeError> {
        Self::start_internal(
            queue_capacity,
            BusinessRuntimeWorker {
                idiom_chain,
                card_games,
                undercover: default_undercover_service(),
                turtle_soup: None,
                hall: Some(hall),
                playback: Some(playback),
                invite: InviteService::new(),
                timer: None,
                state_sink: None,
                clock: Arc::new(SystemClock),
            },
        )
    }

    pub(crate) fn start_with_timer_and_modules_and_state_sink(
        queue_capacity: usize,
        worker_config: BusinessRuntimeWorker,
    ) -> Result<Self, BusinessRuntimeError> {
        Self::start_internal(queue_capacity, worker_config)
    }

    fn start_internal(
        queue_capacity: usize,
        mut worker_config: BusinessRuntimeWorker,
    ) -> Result<Self, BusinessRuntimeError> {
        if queue_capacity == 0 {
            return Err(BusinessRuntimeError::ZeroQueueCapacity);
        }
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let channel = Arc::new(RuntimeChannel {
            sender,
            state: Mutex::new(RuntimeChannelState::Running),
        });
        let event_sink = BusinessRuntimeEventSink {
            channel: channel.clone(),
        };
        let mut turtle_soup_workers = if let Some(service) = worker_config.turtle_soup.as_mut() {
            service
                .start_workers_with_port(Arc::new(TurtleSoupBusinessEventPort { sink: event_sink }))
        } else {
            None
        };
        let worker = thread::Builder::new()
            .name("business-runtime".to_string())
            .spawn(move || run_business_runtime(receiver, worker_config))
            .map_err(|_| {
                if let Some(workers) = turtle_soup_workers.as_mut() {
                    workers.shutdown();
                }
                BusinessRuntimeError::RuntimeStopped
            })?;
        Ok(Self {
            handle: BusinessRuntimeHandle { channel },
            worker: Some(worker),
            turtle_soup_workers,
        })
    }

    pub fn handle(&self) -> BusinessRuntimeHandle {
        self.handle.clone()
    }

    pub fn event_sink(&self) -> BusinessRuntimeEventSink {
        BusinessRuntimeEventSink {
            channel: self.handle.channel.clone(),
        }
    }

    pub fn prepare_shutdown(&self) -> Result<BusinessRuntimeSnapshot, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        {
            let mut state = self
                .handle
                .channel
                .state
                .lock()
                .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
            let message = match *state {
                RuntimeChannelState::Running => {
                    *state = RuntimeChannelState::Quiescing;
                    RuntimeMessage::PrepareShutdown(response)
                }
                RuntimeChannelState::Quiescing => RuntimeMessage::Snapshot(response),
                RuntimeChannelState::Stopped => return Err(BusinessRuntimeError::RuntimeStopped),
            };
            self.handle
                .channel
                .sender
                .send(message)
                .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
        }
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }

    pub fn shutdown(mut self) -> Result<BusinessRuntimeSnapshot, BusinessRuntimeError> {
        self.stop_worker()
    }

    pub(crate) fn stop_external_workers(&mut self) {
        if let Some(mut workers) = self.turtle_soup_workers.take() {
            workers.shutdown();
        }
    }

    fn stop_worker(&mut self) -> Result<BusinessRuntimeSnapshot, BusinessRuntimeError> {
        let Some(worker) = self.worker.take() else {
            if let Some(mut workers) = self.turtle_soup_workers.take() {
                workers.shutdown();
            }
            return Err(BusinessRuntimeError::RuntimeStopped);
        };
        let (response, receiver) = mpsc::sync_channel(1);
        let sent = {
            let mut state = self
                .handle
                .channel
                .state
                .lock()
                .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
            if *state == RuntimeChannelState::Stopped {
                return Err(BusinessRuntimeError::RuntimeStopped);
            }
            *state = RuntimeChannelState::Stopped;
            self.handle
                .channel
                .sender
                .send(RuntimeMessage::Shutdown(response))
                .is_ok()
        };
        let snapshot = sent.then(|| receiver.recv().ok()).flatten();
        let worker_result = worker
            .join()
            .map_err(|_| BusinessRuntimeError::WorkerPanicked);
        self.stop_external_workers();
        worker_result?;
        snapshot.ok_or(BusinessRuntimeError::RuntimeStopped)
    }
}

#[cfg(test)]
fn default_undercover_service() -> UndercoverRuntimeService {
    UndercoverRuntimeService::new(UndercoverConfig::default())
}

impl Drop for BusinessRuntime {
    fn drop(&mut self) {
        let _ = self.stop_worker();
    }
}

fn idiom_deadline_token() -> BusinessDeadlineToken {
    BusinessDeadlineToken::from(IdiomChainDeadlineToken::new(
        IDIOM_DEADLINE_TOKEN_ID,
        IdiomChainDeadlineKind::SessionIdle,
    ))
}

fn sync_idiom_deadline(
    idiom_chain: &IdiomChainService,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active: &mut Option<ActiveIdiomDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    generation: &mut SessionGeneration,
) -> Result<(), BusinessRuntimeError> {
    let Some(timer) = timer else {
        *active = None;
        return Ok(());
    };

    match (idiom_chain.idle_deadline(), active.as_ref()) {
        (None, None) => Ok(()),
        (None, Some(previous)) => {
            timer
                .cancel(DeadlineCancellation::new(
                    previous.token.clone(),
                    previous.operation_id,
                    previous.session_generation,
                ))
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            *active = None;
            Ok(())
        }
        (Some(deadline), None) => {
            let operation_id = operation_ids
                .allocate()
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            *generation = generation.checked_next().ok_or_else(|| {
                BusinessRuntimeError::TimerOperationFailed(
                    "idiom chain session generation exhausted".to_string(),
                )
            })?;
            let token = idiom_deadline_token();
            timer
                .schedule(DeadlineSchedule::new(
                    token.clone(),
                    operation_id,
                    *generation,
                    deadline,
                ))
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            *active = Some(ActiveIdiomDeadline {
                token,
                operation_id,
                session_generation: *generation,
                deadline,
            });
            Ok(())
        }
        (Some(deadline), Some(previous)) if previous.deadline == deadline => Ok(()),
        (Some(deadline), Some(previous)) => {
            let operation_id = operation_ids
                .allocate()
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            let schedule = DeadlineSchedule::new(
                previous.token.clone(),
                operation_id,
                previous.session_generation,
                deadline,
            );
            timer
                .reschedule(schedule)
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            *active = Some(ActiveIdiomDeadline {
                token: previous.token.clone(),
                operation_id,
                session_generation: previous.session_generation,
                deadline,
            });
            Ok(())
        }
    }
}

fn card_game_deadline_token(kind: CardGameDeadlineKind) -> BusinessDeadlineToken {
    BusinessDeadlineToken::from(CardGameDeadlineToken::new(
        CARD_GAME_DEADLINE_TOKEN_ID,
        kind,
    ))
}

fn sync_card_game_deadline(
    card_games: &CardGameService,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active: &mut Option<ActiveCardGameDeadline>,
    pending_cancellations: &mut Vec<ActiveCardGameDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    now: Instant,
    clock_active: bool,
) -> Result<(), BusinessRuntimeError> {
    let Some(timer) = timer else {
        *active = None;
        pending_cancellations.clear();
        return Ok(());
    };
    let desired = card_games.next_deadline(now, clock_active);
    let Some((kind, deadline)) = desired else {
        if let Some(previous) = active.take() {
            let cancellation = DeadlineCancellation::new(
                previous.token.clone(),
                previous.operation_id,
                previous.session_generation,
            );
            if let Err(error) = timer.cancel(cancellation) {
                *active = Some(previous);
                return Err(BusinessRuntimeError::TimerOperationFailed(
                    error.to_string(),
                ));
            }
            pending_cancellations.push(previous);
        }
        return Ok(());
    };

    let token = card_game_deadline_token(kind);
    let session_generation = card_games.session_generation();
    if let Some(previous) = active.as_ref() {
        if previous.token == token
            && previous.session_generation == session_generation
            && previous.deadline == deadline
        {
            return Ok(());
        }
        if previous.token == token {
            let previous = active
                .take()
                .expect("active card deadline remains while replacing it");
            let cancellation = DeadlineCancellation::new(
                previous.token.clone(),
                previous.operation_id,
                previous.session_generation,
            );
            if let Err(error) = timer.cancel(cancellation) {
                *active = Some(previous);
                return Err(BusinessRuntimeError::TimerOperationFailed(
                    error.to_string(),
                ));
            }
            pending_cancellations.push(previous);
            return Ok(());
        }
        let previous = active
            .take()
            .expect("active card deadline exists while replacing its token");
        let cancellation = DeadlineCancellation::new(
            previous.token.clone(),
            previous.operation_id,
            previous.session_generation,
        );
        if let Err(error) = timer.cancel(cancellation) {
            *active = Some(previous);
            return Err(BusinessRuntimeError::TimerOperationFailed(
                error.to_string(),
            ));
        }
        pending_cancellations.push(previous);
    }
    schedule_card_game_deadline(
        timer,
        active,
        operation_ids,
        token,
        session_generation,
        deadline,
    )
}

fn schedule_card_game_deadline(
    timer: &TimerRuntimeHandle<BusinessDeadlineToken>,
    active: &mut Option<ActiveCardGameDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    token: BusinessDeadlineToken,
    session_generation: SessionGeneration,
    deadline: Instant,
) -> Result<(), BusinessRuntimeError> {
    let operation_id = operation_ids
        .allocate()
        .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
    timer
        .schedule(DeadlineSchedule::new(
            token.clone(),
            operation_id,
            session_generation,
            deadline,
        ))
        .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
    *active = Some(ActiveCardGameDeadline {
        token,
        operation_id,
        session_generation,
        deadline,
    });
    Ok(())
}

fn undercover_deadline_token(kind: UndercoverDeadlineKind) -> BusinessDeadlineToken {
    BusinessDeadlineToken::from(UndercoverDeadlineToken::new(
        UNDERCOVER_DEADLINE_TOKEN_ID,
        kind,
    ))
}

fn turtle_soup_deadline_token(kind: TurtleSoupDeadlineKind) -> BusinessDeadlineToken {
    BusinessDeadlineToken::from(TurtleSoupDeadlineToken::new(
        TURTLE_SOUP_DEADLINE_TOKEN_ID,
        kind,
    ))
}

fn sync_turtle_soup_deadline(
    turtle_soup: Option<&TurtleSoupService>,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active: &mut Option<ActiveTurtleSoupDeadline>,
    pending_cancellations: &mut Vec<ActiveTurtleSoupDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    now: Instant,
    clock_active: bool,
) -> Result<(), BusinessRuntimeError> {
    let Some(timer) = timer else {
        *active = None;
        pending_cancellations.clear();
        return Ok(());
    };
    let desired = turtle_soup.and_then(|service| service.next_deadline(now, clock_active));
    let Some((kind, deadline)) = desired else {
        if let Some(previous) = active.take() {
            let cancellation = DeadlineCancellation::new(
                previous.token.clone(),
                previous.operation_id,
                previous.session_generation,
            );
            if let Err(error) = timer.cancel(cancellation) {
                *active = Some(previous);
                return Err(BusinessRuntimeError::TimerOperationFailed(
                    error.to_string(),
                ));
            }
            pending_cancellations.push(previous);
        }
        return Ok(());
    };
    let service = turtle_soup.expect("deadline exists only with turtle soup service");
    let token = turtle_soup_deadline_token(kind);
    let session_generation = service.session_generation();
    if let Some(previous) = active.as_ref() {
        if previous.token == token
            && previous.session_generation == session_generation
            && previous.deadline == deadline
        {
            return Ok(());
        }
        if previous.token == token {
            let previous = active
                .take()
                .expect("active turtle soup deadline remains while replacing it");
            let cancellation = DeadlineCancellation::new(
                previous.token.clone(),
                previous.operation_id,
                previous.session_generation,
            );
            if let Err(error) = timer.cancel(cancellation) {
                *active = Some(previous);
                return Err(BusinessRuntimeError::TimerOperationFailed(
                    error.to_string(),
                ));
            }
            pending_cancellations.push(previous);
            return Ok(());
        }
        let previous = active
            .take()
            .expect("active turtle soup deadline exists while replacing its token");
        let cancellation = DeadlineCancellation::new(
            previous.token.clone(),
            previous.operation_id,
            previous.session_generation,
        );
        if let Err(error) = timer.cancel(cancellation) {
            *active = Some(previous);
            return Err(BusinessRuntimeError::TimerOperationFailed(
                error.to_string(),
            ));
        }
        pending_cancellations.push(previous);
    }
    let operation_id = operation_ids
        .allocate()
        .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
    timer
        .schedule(DeadlineSchedule::new(
            token.clone(),
            operation_id,
            session_generation,
            deadline,
        ))
        .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
    *active = Some(ActiveTurtleSoupDeadline {
        token,
        operation_id,
        session_generation,
        deadline,
    });
    Ok(())
}

fn sync_undercover_deadline(
    undercover: &UndercoverRuntimeService,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active: &mut Option<ActiveUndercoverDeadline>,
    pending_cancellations: &mut Vec<ActiveUndercoverDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    now: Instant,
    clock_active: bool,
) -> Result<(), BusinessRuntimeError> {
    let Some(timer) = timer else {
        *active = None;
        pending_cancellations.clear();
        return Ok(());
    };
    let desired = undercover.next_deadline(now, clock_active);
    let Some((kind, deadline)) = desired else {
        if let Some(previous) = active.take() {
            let cancellation = DeadlineCancellation::new(
                previous.token.clone(),
                previous.operation_id,
                previous.session_generation,
            );
            if let Err(error) = timer.cancel(cancellation) {
                *active = Some(previous);
                return Err(BusinessRuntimeError::TimerOperationFailed(
                    error.to_string(),
                ));
            }
            pending_cancellations.push(previous);
        }
        return Ok(());
    };
    let token = undercover_deadline_token(kind);
    let session_generation = undercover.session_generation();
    if let Some(previous) = active.as_ref() {
        if previous.token == token
            && previous.session_generation == session_generation
            && previous.deadline == deadline
        {
            return Ok(());
        }
        if previous.token == token {
            let previous = active
                .take()
                .expect("active undercover deadline remains while replacing it");
            let cancellation = DeadlineCancellation::new(
                previous.token.clone(),
                previous.operation_id,
                previous.session_generation,
            );
            if let Err(error) = timer.cancel(cancellation) {
                *active = Some(previous);
                return Err(BusinessRuntimeError::TimerOperationFailed(
                    error.to_string(),
                ));
            }
            pending_cancellations.push(previous);
            return Ok(());
        }
        let previous = active
            .take()
            .expect("active undercover deadline exists while replacing its token");
        let cancellation = DeadlineCancellation::new(
            previous.token.clone(),
            previous.operation_id,
            previous.session_generation,
        );
        if let Err(error) = timer.cancel(cancellation) {
            *active = Some(previous);
            return Err(BusinessRuntimeError::TimerOperationFailed(
                error.to_string(),
            ));
        }
        pending_cancellations.push(previous);
    }
    let operation_id = operation_ids
        .allocate()
        .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
    timer
        .schedule(DeadlineSchedule::new(
            token.clone(),
            operation_id,
            session_generation,
            deadline,
        ))
        .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
    *active = Some(ActiveUndercoverDeadline {
        token,
        operation_id,
        session_generation,
        deadline,
    });
    Ok(())
}

// The deadline bridge passes each module's state and correlation lanes explicitly so the
// orchestrator cannot accidentally share one module's pending effects with another.
#[allow(clippy::too_many_arguments)]
fn handle_business_timer(
    event: BusinessDeadlineEvent,
    entertainment: &mut EntertainmentState,
    idiom_chain: &mut IdiomChainService,
    card_games: &mut CardGameService,
    undercover: &mut UndercoverRuntimeService,
    mut turtle_soup: Option<&mut TurtleSoupService>,
    deferred_chat: &mut DeferredChatQueue,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active_idiom_deadline: &mut Option<ActiveIdiomDeadline>,
    active_card_game_deadline: &mut Option<ActiveCardGameDeadline>,
    active_undercover_deadline: &mut Option<ActiveUndercoverDeadline>,
    active_turtle_soup_deadline: &mut Option<ActiveTurtleSoupDeadline>,
    pending_card_game_cancellations: &mut Vec<ActiveCardGameDeadline>,
    pending_undercover_cancellations: &mut Vec<ActiveUndercoverDeadline>,
    pending_turtle_soup_cancellations: &mut Vec<ActiveTurtleSoupDeadline>,
    pending_card_game_outcomes: &mut std::collections::VecDeque<CardGameTimedOutcome>,
    pending_undercover_outcomes: &mut std::collections::VecDeque<UndercoverTimedOutcome>,
    operation_ids: &BusinessOperationIdAllocator,
    generation: &mut SessionGeneration,
    clock_active: bool,
    clock: &dyn Clock,
) -> Result<(), BusinessRuntimeError> {
    if let BusinessDeadlineEvent::TurtleSoup(timer_event) = &event {
        match timer_event {
            TimerRuntimeEvent::DeadlineExpired(expired) => {
                let Some(previous) = active_turtle_soup_deadline.take() else {
                    return Ok(());
                };
                let token = BusinessDeadlineToken::from(expired.token().clone());
                if previous.token != token
                    || previous.operation_id != expired.operation_id()
                    || previous.session_generation != expired.session_generation()
                {
                    *active_turtle_soup_deadline = Some(previous);
                    return Ok(());
                }
                if let Some(service) = turtle_soup.as_deref_mut() {
                    service.handle_deadline(
                        entertainment,
                        *expired.token().kind(),
                        clock.now(),
                        deferred_chat,
                    );
                }
                return sync_turtle_soup_deadline(
                    turtle_soup.as_deref(),
                    timer,
                    active_turtle_soup_deadline,
                    pending_turtle_soup_cancellations,
                    operation_ids,
                    clock.now(),
                    clock_active,
                );
            }
            TimerRuntimeEvent::CommandCompleted(completed) => {
                let token = BusinessDeadlineToken::from(completed.token().clone());
                if completed.command() == TimerCommandKind::Cancel {
                    let Some(index) =
                        pending_turtle_soup_cancellations
                            .iter()
                            .position(|pending| {
                                pending.token == token
                                    && pending.operation_id == completed.operation_id()
                                    && pending.session_generation == completed.session_generation()
                            })
                    else {
                        return Ok(());
                    };
                    let previous = pending_turtle_soup_cancellations.remove(index);
                    if completed.result().is_err() {
                        if active_turtle_soup_deadline.is_none() {
                            *active_turtle_soup_deadline = Some(previous);
                        } else {
                            let Some(timer) = timer else {
                                return Ok(());
                            };
                            timer
                                .cancel(DeadlineCancellation::new(
                                    previous.token.clone(),
                                    previous.operation_id,
                                    previous.session_generation,
                                ))
                                .map_err(|error| {
                                    BusinessRuntimeError::TimerOperationFailed(error.to_string())
                                })?;
                            pending_turtle_soup_cancellations.push(previous);
                        }
                    }
                    return sync_turtle_soup_deadline(
                        turtle_soup.as_deref(),
                        timer,
                        active_turtle_soup_deadline,
                        pending_turtle_soup_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    );
                }
                let Some(active) = active_turtle_soup_deadline.as_ref() else {
                    return Ok(());
                };
                if active.token != token
                    || active.operation_id != completed.operation_id()
                    || active.session_generation != completed.session_generation()
                {
                    return Ok(());
                }
                if completed.result().is_err() {
                    *active_turtle_soup_deadline = None;
                    return sync_turtle_soup_deadline(
                        turtle_soup.as_deref(),
                        timer,
                        active_turtle_soup_deadline,
                        pending_turtle_soup_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    );
                }
                return Ok(());
            }
        }
    }
    if let BusinessDeadlineEvent::Undercover(timer_event) = &event {
        match timer_event {
            TimerRuntimeEvent::DeadlineExpired(expired) => {
                let Some(previous) = active_undercover_deadline.take() else {
                    return Ok(());
                };
                let token = BusinessDeadlineToken::from(expired.token().clone());
                if previous.token != token
                    || previous.operation_id != expired.operation_id()
                    || previous.session_generation != expired.session_generation()
                {
                    *active_undercover_deadline = Some(previous);
                    return Ok(());
                }
                let outcome_operation = operation_ids.allocate().map_err(|error| {
                    BusinessRuntimeError::UndercoverOperationFailed(error.to_string())
                })?;
                if let Some(outcome) = undercover
                    .handle_deadline(
                        entertainment,
                        *expired.token().kind(),
                        clock.now(),
                        outcome_operation,
                    )
                    .map_err(undercover_operation_failed)?
                {
                    pending_undercover_outcomes.push_back(outcome);
                }
                return sync_undercover_deadline(
                    undercover,
                    timer,
                    active_undercover_deadline,
                    pending_undercover_cancellations,
                    operation_ids,
                    clock.now(),
                    clock_active,
                );
            }
            TimerRuntimeEvent::CommandCompleted(completed) => {
                let token = BusinessDeadlineToken::from(completed.token().clone());
                if completed.command() == TimerCommandKind::Cancel {
                    let Some(index) = pending_undercover_cancellations.iter().position(|pending| {
                        pending.token == token
                            && pending.operation_id == completed.operation_id()
                            && pending.session_generation == completed.session_generation()
                    }) else {
                        return Ok(());
                    };
                    let previous = pending_undercover_cancellations.remove(index);
                    if completed.result().is_err() {
                        if active_undercover_deadline.is_none() {
                            *active_undercover_deadline = Some(previous);
                        } else {
                            let Some(timer) = timer else {
                                return Ok(());
                            };
                            timer
                                .cancel(DeadlineCancellation::new(
                                    previous.token.clone(),
                                    previous.operation_id,
                                    previous.session_generation,
                                ))
                                .map_err(|error| {
                                    BusinessRuntimeError::TimerOperationFailed(error.to_string())
                                })?;
                            pending_undercover_cancellations.push(previous);
                        }
                    }
                    return sync_undercover_deadline(
                        undercover,
                        timer,
                        active_undercover_deadline,
                        pending_undercover_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    );
                }
                let Some(active) = active_undercover_deadline.as_ref() else {
                    return Ok(());
                };
                if active.token != token
                    || active.operation_id != completed.operation_id()
                    || active.session_generation != completed.session_generation()
                {
                    return Ok(());
                }
                if completed.result().is_err() {
                    *active_undercover_deadline = None;
                    return sync_undercover_deadline(
                        undercover,
                        timer,
                        active_undercover_deadline,
                        pending_undercover_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    );
                }
                return Ok(());
            }
        }
    }
    if let BusinessDeadlineEvent::CardGame(timer_event) = &event {
        match timer_event {
            TimerRuntimeEvent::DeadlineExpired(expired) => {
                let Some(previous) = active_card_game_deadline.take() else {
                    return Ok(());
                };
                let token = BusinessDeadlineToken::from(expired.token().clone());
                if previous.token != token
                    || previous.operation_id != expired.operation_id()
                    || previous.session_generation != expired.session_generation()
                {
                    *active_card_game_deadline = Some(previous);
                    return Ok(());
                }
                let handled_at = clock.now();
                if let Some(outcome) = card_games
                    .handle_deadline(entertainment, *expired.token().kind(), handled_at)
                    .map_err(card_game_operation_failed)?
                {
                    pending_card_game_outcomes.push_back(outcome);
                }
                return sync_card_game_deadline(
                    card_games,
                    timer,
                    active_card_game_deadline,
                    pending_card_game_cancellations,
                    operation_ids,
                    handled_at,
                    clock_active,
                );
            }
            TimerRuntimeEvent::CommandCompleted(completed) => {
                let token = BusinessDeadlineToken::from(completed.token().clone());
                if completed.command() == TimerCommandKind::Cancel {
                    let Some(index) = pending_card_game_cancellations.iter().position(|pending| {
                        pending.token == token
                            && pending.operation_id == completed.operation_id()
                            && pending.session_generation == completed.session_generation()
                    }) else {
                        return Ok(());
                    };
                    let previous = pending_card_game_cancellations.remove(index);
                    if completed.result().is_err() {
                        if active_card_game_deadline.is_none() {
                            *active_card_game_deadline = Some(previous);
                        } else {
                            let Some(timer) = timer else {
                                return Ok(());
                            };
                            timer
                                .cancel(DeadlineCancellation::new(
                                    previous.token.clone(),
                                    previous.operation_id,
                                    previous.session_generation,
                                ))
                                .map_err(|error| {
                                    BusinessRuntimeError::TimerOperationFailed(error.to_string())
                                })?;
                            pending_card_game_cancellations.push(previous);
                        }
                    }
                    return sync_card_game_deadline(
                        card_games,
                        timer,
                        active_card_game_deadline,
                        pending_card_game_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    );
                }

                let Some(active) = active_card_game_deadline.as_ref() else {
                    return Ok(());
                };
                if active.token != token
                    || active.operation_id != completed.operation_id()
                    || active.session_generation != completed.session_generation()
                {
                    return Ok(());
                }
                if completed.result().is_err() {
                    *active_card_game_deadline = None;
                    return sync_card_game_deadline(
                        card_games,
                        timer,
                        active_card_game_deadline,
                        pending_card_game_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    );
                }
                return Ok(());
            }
        }
    }
    let BusinessDeadlineEvent::IdiomChain(TimerRuntimeEvent::DeadlineExpired(expired)) = event
    else {
        return Ok(());
    };
    let Some(active) = active_idiom_deadline.as_ref() else {
        return Ok(());
    };
    let token = BusinessDeadlineToken::IdiomChain(expired.token().clone());
    if active.token != token
        || active.operation_id != expired.operation_id()
        || active.session_generation != expired.session_generation()
    {
        return Ok(());
    }

    *active_idiom_deadline = None;
    if idiom_chain
        .expire_idle_at(entertainment, expired.emitted_at())
        .map_err(idiom_chain_operation_failed)?
    {
        log::info!("成语接龙已因计时运行时期限到期结束，娱乐互斥已释放");
    }
    sync_idiom_deadline(
        idiom_chain,
        timer,
        active_idiom_deadline,
        operation_ids,
        generation,
    )
}

fn run_business_runtime(receiver: Receiver<RuntimeMessage>, worker_config: BusinessRuntimeWorker) {
    let BusinessRuntimeWorker {
        mut idiom_chain,
        mut card_games,
        mut undercover,
        mut turtle_soup,
        mut hall,
        mut playback,
        mut invite,
        timer,
        state_sink,
        clock,
    } = worker_config;
    let mut snapshot = BusinessRuntimeSnapshot::default();
    let mut entertainment = EntertainmentState::new();
    let mut formal_scheduler = FormalScheduler::new();
    let mut deferred_chat = DeferredChatQueue::new(DEFERRED_CHAT_CAPACITY);
    let mut chat_listener = ChatListenerState::new();
    let mut decision = DecisionState::new();
    let mut operational = OperationalState::new();
    let mut active_idiom_deadline = None;
    let mut active_card_game_deadline = None;
    let mut active_undercover_deadline = None;
    let mut active_turtle_soup_deadline = None;
    let mut pending_card_game_cancellations = Vec::new();
    let mut pending_undercover_cancellations = Vec::new();
    let mut pending_turtle_soup_cancellations = Vec::new();
    let mut pending_card_game_outcomes = std::collections::VecDeque::new();
    let mut pending_undercover_outcomes = std::collections::VecDeque::new();
    let mut entertainment_clock_active = true;
    let mut moderation_workflows = HashSet::new();
    let operation_ids = BusinessOperationIdAllocator::new();
    let mut session_generation = SessionGeneration::INITIAL;
    publish_business_state(
        &state_sink,
        turtle_soup.as_ref(),
        &undercover,
        clock.as_ref(),
    );
    publish_playback_queue(&state_sink, playback.as_ref());
    publish_hall_state(&state_sink, hall.as_ref());
    publish_scheduler_state(&state_sink, &formal_scheduler);
    publish_diagnostic_state(&state_sink, &formal_scheduler);
    publish_chat_listener_state(&state_sink, &chat_listener);
    publish_decision_state(&state_sink, &mut decision);
    publish_operational_state(&state_sink, &operational, clock.now());
    while let Ok(message) = receiver.recv() {
        match message {
            RuntimeMessage::EnqueueFormalTask {
                submission,
                response,
            } => {
                let outcome = formal_scheduler.enqueue(submission);
                publish_scheduler_state(&state_sink, &formal_scheduler);
                let _ = response.send(Ok(outcome));
            }
            RuntimeMessage::FormalSchedulerSnapshot(response) => {
                let _ = response.send(Ok(formal_scheduler.snapshot()));
            }
            RuntimeMessage::FormalTaskContainsDedupKey { key, response } => {
                let _ = response.send(Ok(formal_scheduler.contains_dedup_key(&key)));
            }
            RuntimeMessage::TakeNextFormalTask(response) => {
                let task = formal_scheduler.take_next();
                if task.is_some() {
                    publish_scheduler_state(&state_sink, &formal_scheduler);
                }
                let _ = response.send(Ok(task));
            }
            RuntimeMessage::RestoreFormalTask { lease, response } => {
                let result = formal_scheduler.restore(lease).map_err(|error| {
                    BusinessRuntimeError::SchedulerOperationFailed(error.to_string())
                });
                if result.is_ok() {
                    publish_scheduler_state(&state_sink, &formal_scheduler);
                }
                let _ = response.send(result);
            }
            RuntimeMessage::CompleteFormalTask {
                task_id,
                completion,
                response,
            } => {
                let result = formal_scheduler
                    .complete(task_id, completion)
                    .map_err(|error| {
                        BusinessRuntimeError::SchedulerOperationFailed(error.to_string())
                    });
                if result.is_ok() {
                    publish_scheduler_state(&state_sink, &formal_scheduler);
                }
                let _ = response.send(result);
            }
            RuntimeMessage::CancelFormalTask { task_id, response } => {
                let work = formal_scheduler.cancel_queued(task_id);
                if work.is_some() {
                    publish_scheduler_state(&state_sink, &formal_scheduler);
                }
                let _ = response.send(Ok(work));
            }
            RuntimeMessage::ReleaseSchedulerLane { lease, response } => {
                let result = formal_scheduler.release_lane(lease).map_err(|error| {
                    BusinessRuntimeError::SchedulerOperationFailed(error.to_string())
                });
                if result.is_ok() {
                    publish_scheduler_state(&state_sink, &formal_scheduler);
                }
                let _ = response.send(result);
            }
            RuntimeMessage::EnqueueDeferredChat { item, response } => {
                let result = deferred_chat.enqueue(item).map_err(|error| {
                    BusinessRuntimeError::SchedulerOperationFailed(error.to_string())
                });
                let _ = response.send(result);
            }
            RuntimeMessage::RequeueDeferredChatFront { item, response } => {
                let _ = response.send(Ok(deferred_chat.requeue_front(item)));
            }
            RuntimeMessage::RequeueDeferredChatBack { item, response } => {
                let _ = response.send(Ok(deferred_chat.requeue_back(item)));
            }
            RuntimeMessage::TakeNextDeferredChat(response) => {
                let result = if deferred_chat.is_empty() {
                    Ok(None)
                } else {
                    formal_scheduler
                        .try_acquire_lane(SchedulerLane::Deferred)
                        .map_err(|error| {
                            BusinessRuntimeError::SchedulerOperationFailed(error.to_string())
                        })
                        .map(|lease| {
                            lease.map(|lease| {
                                let item = deferred_chat
                                    .take_next()
                                    .expect("deferred queue was checked as non-empty");
                                (item, lease)
                            })
                        })
                };
                if result.as_ref().is_ok_and(Option::is_some) {
                    publish_scheduler_state(&state_sink, &formal_scheduler);
                }
                let _ = response.send(result);
            }
            RuntimeMessage::EnqueueDiagnosticTask {
                submission,
                response,
            } => {
                let result = formal_scheduler
                    .enqueue_diagnostic(submission)
                    .map_err(|error| {
                        BusinessRuntimeError::SchedulerOperationFailed(error.to_string())
                    });
                if result.is_ok() {
                    publish_diagnostic_state(&state_sink, &formal_scheduler);
                }
                let _ = response.send(result);
            }
            RuntimeMessage::TakeNextDiagnosticTask(response) => {
                let task = if deferred_chat.is_empty() {
                    formal_scheduler.take_next_diagnostic()
                } else {
                    None
                };
                if task.is_some() {
                    publish_scheduler_state(&state_sink, &formal_scheduler);
                    publish_diagnostic_state(&state_sink, &formal_scheduler);
                }
                let _ = response.send(Ok(task));
            }
            RuntimeMessage::CompleteDiagnosticTask {
                task_id,
                completion,
                response,
            } => {
                let result = formal_scheduler
                    .complete_diagnostic(task_id, completion)
                    .map_err(|error| {
                        BusinessRuntimeError::SchedulerOperationFailed(error.to_string())
                    });
                if result.is_ok() {
                    publish_scheduler_state(&state_sink, &formal_scheduler);
                    publish_diagnostic_state(&state_sink, &formal_scheduler);
                }
                let _ = response.send(result);
            }
            RuntimeMessage::DiagnosticTaskSnapshot { id, response } => {
                let _ = response.send(Ok(formal_scheduler.diagnostic_task_snapshot(id)));
            }
            RuntimeMessage::OperationalSnapshot { now, response } => {
                let _ = response.send(Ok(operational.snapshot(now)));
            }
            RuntimeMessage::SetCommandsEnabled { enabled, response } => {
                operational.commands_enabled = enabled;
                publish_operational_state(&state_sink, &operational, clock.now());
                let _ = response.send(Ok(()));
            }
            RuntimeMessage::ConfigureIdleExit {
                timeout,
                now,
                response,
            } => {
                operational.configure_idle_exit(timeout, now);
                publish_operational_state(&state_sink, &operational, now);
                let _ = response.send(Ok(()));
            }
            RuntimeMessage::RecordCommandActivity { now, response } => {
                operational.record_command_activity(now);
                publish_operational_state(&state_sink, &operational, now);
                let _ = response.send(Ok(()));
            }
            RuntimeMessage::ClaimIdleExit { now, response } => {
                let claimed = operational.claim_idle_exit(
                    now,
                    formal_scheduler.snapshot().is_idle() && deferred_chat.is_empty(),
                );
                publish_operational_state(&state_sink, &operational, now);
                let _ = response.send(Ok(claimed));
            }
            RuntimeMessage::ClearIdleExit(response) => {
                operational.idle_exit = None;
                publish_operational_state(&state_sink, &operational, clock.now());
                let _ = response.send(Ok(()));
            }
            RuntimeMessage::ChatListener(message) => {
                handle_chat_listener_message(&mut chat_listener, message, &state_sink);
            }
            RuntimeMessage::Decision(message) => {
                handle_decision_message(&mut decision, message, &state_sink);
            }
            RuntimeMessage::Event(event) => match event {
                BusinessEvent::Timer(timer_event) => {
                    snapshot.apply(BusinessEvent::Timer(timer_event.clone()));
                    if let Err(error) = handle_business_timer(
                        timer_event,
                        &mut entertainment,
                        &mut idiom_chain,
                        &mut card_games,
                        &mut undercover,
                        turtle_soup.as_mut(),
                        &mut deferred_chat,
                        timer.as_ref(),
                        &mut active_idiom_deadline,
                        &mut active_card_game_deadline,
                        &mut active_undercover_deadline,
                        &mut active_turtle_soup_deadline,
                        &mut pending_card_game_cancellations,
                        &mut pending_undercover_cancellations,
                        &mut pending_turtle_soup_cancellations,
                        &mut pending_card_game_outcomes,
                        &mut pending_undercover_outcomes,
                        &operation_ids,
                        &mut session_generation,
                        entertainment_clock_active,
                        clock.as_ref(),
                    ) {
                        log::error!("业务运行时处理计时事件失败: {error}");
                    }
                }
                BusinessEvent::TurtleSoupAiCompleted(completion) => {
                    if let Some(service) = turtle_soup.as_mut() {
                        service.apply_ai_completion(
                            &mut entertainment,
                            completion,
                            &mut deferred_chat,
                        );
                        if let Err(error) = sync_turtle_soup_deadline(
                            Some(&*service),
                            timer.as_ref(),
                            &mut active_turtle_soup_deadline,
                            &mut pending_turtle_soup_cancellations,
                            &operation_ids,
                            clock.now(),
                            entertainment_clock_active,
                        ) {
                            log::error!("处理海龟汤 AI 裁决后同步期限失败: {error}");
                        }
                    }
                }
                other => snapshot.apply(other),
            },
            RuntimeMessage::HandleIdiomChain {
                player,
                command,
                response,
            } => {
                let result = idiom_chain
                    .handle_at(&mut entertainment, &player, &command, clock.now())
                    .map_err(idiom_chain_operation_failed)
                    .and_then(|outcome| {
                        sync_idiom_deadline(
                            &idiom_chain,
                            timer.as_ref(),
                            &mut active_idiom_deadline,
                            &operation_ids,
                            &mut session_generation,
                        )?;
                        Ok(outcome)
                    });
                let _ = response.send(result);
            }
            RuntimeMessage::ExplainIdiomChain {
                player,
                command,
                response,
            } => {
                let result = idiom_chain
                    .explain_at(&player, &command, clock.now())
                    .map_err(idiom_chain_operation_failed)
                    .and_then(|outcome| {
                        sync_idiom_deadline(
                            &idiom_chain,
                            timer.as_ref(),
                            &mut active_idiom_deadline,
                            &operation_ids,
                            &mut session_generation,
                        )?;
                        Ok(outcome)
                    });
                let _ = response.send(result);
            }
            RuntimeMessage::AbortIdiomChain(response) => {
                let result = idiom_chain
                    .abort(&mut entertainment)
                    .map_err(idiom_chain_operation_failed)
                    .and_then(|aborted| {
                        sync_idiom_deadline(
                            &idiom_chain,
                            timer.as_ref(),
                            &mut active_idiom_deadline,
                            &operation_ids,
                            &mut session_generation,
                        )?;
                        Ok(aborted)
                    });
                let _ = response.send(result);
            }
            RuntimeMessage::ExpireIdiomChain(response) => {
                let result = idiom_chain
                    .expire_idle_at(&mut entertainment, clock.now())
                    .map_err(idiom_chain_operation_failed)
                    .and_then(|expired| {
                        sync_idiom_deadline(
                            &idiom_chain,
                            timer.as_ref(),
                            &mut active_idiom_deadline,
                            &operation_ids,
                            &mut session_generation,
                        )?;
                        Ok(expired)
                    });
                let _ = response.send(result);
            }
            RuntimeMessage::CardGame(message) => {
                if let Err(error) = handle_card_game_message(
                    &mut card_games,
                    &mut entertainment,
                    message,
                    timer.as_ref(),
                    &mut active_card_game_deadline,
                    &mut pending_card_game_cancellations,
                    &operation_ids,
                    &mut pending_card_game_outcomes,
                    &mut entertainment_clock_active,
                    clock.as_ref(),
                ) {
                    log::error!("业务运行时处理牌局消息失败: {error}");
                }
            }
            RuntimeMessage::Undercover(message) => {
                if let Err(error) = handle_undercover_message(
                    &mut undercover,
                    &mut entertainment,
                    message,
                    timer.as_ref(),
                    &mut active_undercover_deadline,
                    &mut pending_undercover_cancellations,
                    &operation_ids,
                    &mut pending_undercover_outcomes,
                    &mut entertainment_clock_active,
                    clock.as_ref(),
                ) {
                    log::error!("业务运行时处理谁是卧底消息失败: {error}");
                }
            }
            RuntimeMessage::UndercoverSnapshot(response) => {
                let _ = response.send(Ok(undercover.snapshot(clock.now())));
            }
            RuntimeMessage::TurtleSoup(message) => {
                if let Err(error) = handle_turtle_soup_message(
                    TurtleSoupHandlerContext {
                        turtle_soup: turtle_soup.as_mut(),
                        entertainment: &mut entertainment,
                        deferred_chat: &mut deferred_chat,
                        timer: timer.as_ref(),
                        active_deadline: &mut active_turtle_soup_deadline,
                        pending_cancellations: &mut pending_turtle_soup_cancellations,
                        operation_ids: &operation_ids,
                        clock_active: entertainment_clock_active,
                        clock: clock.as_ref(),
                    },
                    message,
                ) {
                    log::error!("业务运行时处理海龟汤消息失败: {error}");
                }
            }
            RuntimeMessage::RefreshTurtleSoup {
                now,
                clock_active,
                response,
            } => {
                let result = sync_turtle_soup_deadline(
                    turtle_soup.as_ref(),
                    timer.as_ref(),
                    &mut active_turtle_soup_deadline,
                    &mut pending_turtle_soup_cancellations,
                    &operation_ids,
                    now,
                    clock_active,
                );
                let _ = response.send(result);
            }
            RuntimeMessage::TurtleSoupSnapshot(response) => {
                let result = turtle_soup
                    .as_ref()
                    .map(|service| service.snapshot())
                    .ok_or(BusinessRuntimeError::RuntimeStopped);
                let _ = response.send(result);
            }
            RuntimeMessage::InviteShouldAccept { sequence, response } => {
                let _ = response.send(invite.should_accept(sequence));
            }
            RuntimeMessage::BeginInvite { request, response } => {
                let _ = response.send(invite.begin(request));
            }
            RuntimeMessage::AcquireModerationWorkflow { key, response } => {
                let _ = response.send(moderation_workflows.insert(key));
            }
            RuntimeMessage::ReleaseModerationWorkflow { key, response } => {
                let _ = response.send(moderation_workflows.remove(&key));
            }
            #[cfg(test)]
            RuntimeMessage::ContainsModerationWorkflow { key, response } => {
                let _ = response.send(moderation_workflows.contains(&key));
            }
            RuntimeMessage::Hall(message) => match message {
                HallRuntimeMessage::PatchState { patch, response } => {
                    let result = hall
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| service.patch(patch).map_err(hall_operation_failed));
                    if result.is_ok() {
                        publish_hall_state(&state_sink, hall.as_ref());
                    }
                    let _ = response.send(result);
                }
                HallRuntimeMessage::StateSnapshot(response) => {
                    let result = hall
                        .as_ref()
                        .map(HallStateService::snapshot)
                        .ok_or(BusinessRuntimeError::RuntimeStopped);
                    let _ = response.send(result);
                }
                HallRuntimeMessage::UpdateRemainingMinutes { minutes, response } => {
                    let result = hall
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| {
                            service
                                .update_remaining_minutes(minutes)
                                .map_err(hall_operation_failed)
                        });
                    if result.is_ok() {
                        publish_hall_state(&state_sink, hall.as_ref());
                    }
                    let _ = response.send(result);
                }
                HallRuntimeMessage::ClearRemainingMinutes(response) => {
                    let result = hall
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| {
                            service
                                .clear_remaining_minutes()
                                .map_err(hall_operation_failed)
                        });
                    if result.is_ok() {
                        publish_hall_state(&state_sink, hall.as_ref());
                    }
                    let _ = response.send(result);
                }
                HallRuntimeMessage::ClearCountdownCache(response) => {
                    let result = hall
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| {
                            service
                                .clear_countdown_cache()
                                .map_err(hall_operation_failed)
                        });
                    if result.as_ref().is_ok_and(|cleared| *cleared) {
                        publish_hall_state(&state_sink, hall.as_ref());
                    }
                    let _ = response.send(result);
                }
            },
            RuntimeMessage::Playback(message) => match message {
                PlaybackRuntimeMessage::PushQueue { item, response } => {
                    let result = playback
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| {
                            service.push_queue(item).map_err(playback_operation_failed)
                        });
                    if result.is_ok() {
                        publish_playback_queue(&state_sink, playback.as_ref());
                    }
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::RemoveQueue { removal, response } => {
                    let result = playback
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| {
                            service
                                .remove_queue(removal)
                                .map_err(playback_operation_failed)
                        });
                    if matches!(result, Ok(QueueRemoveOutcome::Removed { .. })) {
                        publish_playback_queue(&state_sink, playback.as_ref());
                    }
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::RemoveQueueIndexes { indexes, response } => {
                    let result = playback
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| {
                            service
                                .remove_queue_indexes(indexes)
                                .map_err(playback_operation_failed)
                        });
                    if result.as_ref().is_ok_and(|removed| !removed.is_empty()) {
                        publish_playback_queue(&state_sink, playback.as_ref());
                    }
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::ClearQueue(response) => {
                    let result = playback
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| {
                            service.clear_queue().map_err(playback_operation_failed)
                        });
                    if result.as_ref().is_ok_and(|count| *count > 0) {
                        publish_playback_queue(&state_sink, playback.as_ref());
                    }
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::QueueContains { item, response } => {
                    let result = playback
                        .as_ref()
                        .map(|service| service.queue_contains(&item))
                        .ok_or(BusinessRuntimeError::RuntimeStopped);
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::QueueSnapshot(response) => {
                    let result = playback
                        .as_ref()
                        .map(PlaybackService::queue_snapshot)
                        .ok_or(BusinessRuntimeError::RuntimeStopped);
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::StateSnapshot(response) => {
                    let result = playback
                        .as_ref()
                        .map(PlaybackService::playback_state_snapshot)
                        .ok_or(BusinessRuntimeError::RuntimeStopped);
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::UpdatePlaybackState { update, response } => {
                    let result = playback
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| {
                            service
                                .apply_playback_state_update(update)
                                .map_err(playback_operation_failed)
                        });
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::CheckSongDedup {
                    candidate,
                    response,
                } => {
                    let result = playback
                        .as_ref()
                        .map(|service| service.song_dedup_limited(&candidate))
                        .ok_or(BusinessRuntimeError::RuntimeStopped);
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::RecordSongDedup {
                    candidate,
                    response,
                } => {
                    let result = playback
                        .as_mut()
                        .ok_or(BusinessRuntimeError::RuntimeStopped)
                        .and_then(|service| {
                            service
                                .record_song_dedup(candidate)
                                .map_err(playback_operation_failed)
                        });
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::ObserveExternalPlayback {
                    identity,
                    now,
                    protect_after,
                    response,
                } => {
                    let result = playback
                        .as_mut()
                        .map(|service| {
                            service.observe_external_playback(&identity, now, protect_after)
                        })
                        .ok_or(BusinessRuntimeError::RuntimeStopped);
                    let _ = response.send(result);
                }
                PlaybackRuntimeMessage::ClearExternalPlaybackTracker(response) => {
                    let result = playback
                        .as_mut()
                        .map(|service| service.clear_external_playback_tracker())
                        .ok_or(BusinessRuntimeError::RuntimeStopped);
                    let _ = response.send(result);
                }
            },
            RuntimeMessage::ActiveEntertainment(response) => {
                let _ = response.send(entertainment.active());
            }
            RuntimeMessage::Snapshot(response) => {
                let _ = response.send(snapshot);
            }
            RuntimeMessage::PrepareShutdown(response) => {
                abort_business_modules(
                    &mut entertainment,
                    &mut idiom_chain,
                    &mut card_games,
                    &mut undercover,
                    turtle_soup.as_mut(),
                    timer.as_ref(),
                    &mut active_idiom_deadline,
                    &mut active_card_game_deadline,
                    &mut active_undercover_deadline,
                    &mut active_turtle_soup_deadline,
                    &mut pending_card_game_cancellations,
                    &mut pending_undercover_cancellations,
                    &mut pending_turtle_soup_cancellations,
                    &operation_ids,
                    &mut session_generation,
                    &mut pending_card_game_outcomes,
                    &mut pending_undercover_outcomes,
                    entertainment_clock_active,
                    clock.as_ref(),
                );
                snapshot.quiescing = true;
                let _ = response.send(snapshot);
            }
            RuntimeMessage::Shutdown(response) => {
                abort_business_modules(
                    &mut entertainment,
                    &mut idiom_chain,
                    &mut card_games,
                    &mut undercover,
                    turtle_soup.as_mut(),
                    timer.as_ref(),
                    &mut active_idiom_deadline,
                    &mut active_card_game_deadline,
                    &mut active_undercover_deadline,
                    &mut active_turtle_soup_deadline,
                    &mut pending_card_game_cancellations,
                    &mut pending_undercover_cancellations,
                    &mut pending_turtle_soup_cancellations,
                    &operation_ids,
                    &mut session_generation,
                    &mut pending_card_game_outcomes,
                    &mut pending_undercover_outcomes,
                    entertainment_clock_active,
                    clock.as_ref(),
                );
                let _ = response.send(snapshot);
                break;
            }
        }
        publish_business_state(
            &state_sink,
            turtle_soup.as_ref(),
            &undercover,
            clock.as_ref(),
        );
    }
}

fn publish_business_state(
    sink: &Option<Arc<dyn BusinessStateSink>>,
    turtle_soup: Option<&TurtleSoupService>,
    undercover: &UndercoverRuntimeService,
    clock: &dyn Clock,
) {
    let Some(sink) = sink.as_ref() else {
        return;
    };
    if let Some(turtle_soup) = turtle_soup {
        sink.publish_turtle_soup(turtle_soup.snapshot().redacted_for_monitor());
    }
    sink.publish_undercover(undercover.snapshot(clock.now()));
}

#[allow(clippy::too_many_arguments)]
fn abort_business_modules(
    entertainment: &mut EntertainmentState,
    idiom_chain: &mut IdiomChainService,
    card_games: &mut CardGameService,
    undercover: &mut UndercoverRuntimeService,
    mut turtle_soup: Option<&mut TurtleSoupService>,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active_idiom_deadline: &mut Option<ActiveIdiomDeadline>,
    active_card_game_deadline: &mut Option<ActiveCardGameDeadline>,
    active_undercover_deadline: &mut Option<ActiveUndercoverDeadline>,
    active_turtle_soup_deadline: &mut Option<ActiveTurtleSoupDeadline>,
    pending_card_game_cancellations: &mut Vec<ActiveCardGameDeadline>,
    pending_undercover_cancellations: &mut Vec<ActiveUndercoverDeadline>,
    pending_turtle_soup_cancellations: &mut Vec<ActiveTurtleSoupDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    session_generation: &mut SessionGeneration,
    pending_card_game_outcomes: &mut std::collections::VecDeque<CardGameTimedOutcome>,
    pending_undercover_outcomes: &mut std::collections::VecDeque<UndercoverTimedOutcome>,
    clock_active: bool,
    clock: &dyn Clock,
) {
    if let Err(error) = card_games.abort(entertainment) {
        log::error!("业务运行时关闭时无法中止牌局: {error:#}");
    }
    if let Err(error) = idiom_chain.abort(entertainment) {
        log::error!("业务运行时关闭时无法中止成语接龙: {error:#}");
    }
    let _ = undercover.abort(entertainment);
    if let Some(turtle_soup) = turtle_soup.as_deref_mut() {
        turtle_soup.abort_for_context_loss(entertainment, "业务运行时关闭");
    }
    if let Err(error) = sync_idiom_deadline(
        idiom_chain,
        timer,
        active_idiom_deadline,
        operation_ids,
        session_generation,
    ) {
        log::error!("业务运行时关闭时无法撤销成语接龙期限: {error}");
    }
    if let Err(error) = sync_card_game_deadline(
        card_games,
        timer,
        active_card_game_deadline,
        pending_card_game_cancellations,
        operation_ids,
        clock.now(),
        clock_active,
    ) {
        log::error!("业务运行时关闭时无法撤销牌局期限: {error}");
    }
    if let Err(error) = sync_undercover_deadline(
        undercover,
        timer,
        active_undercover_deadline,
        pending_undercover_cancellations,
        operation_ids,
        clock.now(),
        clock_active,
    ) {
        log::error!("业务运行时关闭时无法撤销谁是卧底期限: {error}");
    }
    if let Err(error) = sync_turtle_soup_deadline(
        turtle_soup.as_deref(),
        timer,
        active_turtle_soup_deadline,
        pending_turtle_soup_cancellations,
        operation_ids,
        clock.now(),
        clock_active,
    ) {
        log::error!("业务运行时关闭时无法撤销海龟汤期限: {error}");
    }
    pending_card_game_outcomes.clear();
    pending_card_game_cancellations.clear();
    pending_undercover_outcomes.clear();
    pending_undercover_cancellations.clear();
    pending_turtle_soup_cancellations.clear();
}

#[allow(clippy::too_many_arguments)]
fn handle_card_game_message(
    card_games: &mut CardGameService,
    entertainment: &mut EntertainmentState,
    message: CardGameRuntimeMessage,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active_deadline: &mut Option<ActiveCardGameDeadline>,
    pending_cancellations: &mut Vec<ActiveCardGameDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    pending_outcomes: &mut std::collections::VecDeque<CardGameTimedOutcome>,
    clock_active: &mut bool,
    clock: &dyn Clock,
) -> Result<(), BusinessRuntimeError> {
    match message {
        CardGameRuntimeMessage::Begin {
            player,
            command,
            now,
            response,
        } => {
            let result = card_games
                .begin_command(entertainment, &player, &command, now)
                .map_err(card_game_operation_failed)
                .and_then(|result| {
                    sync_card_game_deadline(
                        card_games,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        now,
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(result);
        }
        CardGameRuntimeMessage::Claim { key, response } => {
            let result = card_games
                .claim(key)
                .map_err(card_game_operation_failed)
                .and_then(|result| {
                    sync_card_game_deadline(
                        card_games,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(result);
        }
        CardGameRuntimeMessage::Resume {
            key,
            result,
            response,
        } => {
            let response_result = card_games
                .resume(entertainment, key, result)
                .map_err(card_game_operation_failed)
                .and_then(|result| {
                    sync_card_game_deadline(
                        card_games,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(response_result);
        }
        CardGameRuntimeMessage::Cancel { key, response } => {
            let result = card_games
                .cancel(entertainment, key)
                .map_err(card_game_operation_failed)
                .and_then(|result| {
                    sync_card_game_deadline(
                        card_games,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(result);
        }
        CardGameRuntimeMessage::Tick {
            now,
            clock_active: requested_clock_active,
            response,
        } => {
            *clock_active = requested_clock_active;
            let result = match timer {
                Some(timer) => {
                    card_games.sync_clock(now, requested_clock_active);
                    sync_card_game_deadline(
                        card_games,
                        Some(timer),
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        now,
                        requested_clock_active,
                    )
                    .map(|()| pending_outcomes.pop_front())
                }
                None => {
                    #[cfg(test)]
                    {
                        card_games
                            .tick(entertainment, now, requested_clock_active)
                            .map_err(card_game_operation_failed)
                    }
                    #[cfg(not(test))]
                    {
                        Err(BusinessRuntimeError::TimerOperationFailed(
                            "牌局运行时缺少业务计时器".to_string(),
                        ))
                    }
                }
            };
            let _ = response.send(result);
        }
        CardGameRuntimeMessage::Abort(response) => {
            let result = card_games
                .abort(entertainment)
                .map_err(card_game_operation_failed)
                .and_then(|result| {
                    sync_card_game_deadline(
                        card_games,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(result);
        }
    }
    Ok(())
}

struct TurtleSoupHandlerContext<'a> {
    turtle_soup: Option<&'a mut TurtleSoupService>,
    entertainment: &'a mut EntertainmentState,
    deferred_chat: &'a mut DeferredChatQueue,
    timer: Option<&'a TimerRuntimeHandle<BusinessDeadlineToken>>,
    active_deadline: &'a mut Option<ActiveTurtleSoupDeadline>,
    pending_cancellations: &'a mut Vec<ActiveTurtleSoupDeadline>,
    operation_ids: &'a BusinessOperationIdAllocator,
    clock_active: bool,
    clock: &'a dyn Clock,
}

fn handle_turtle_soup_message(
    context: TurtleSoupHandlerContext<'_>,
    message: TurtleSoupRuntimeMessage,
) -> Result<(), BusinessRuntimeError> {
    let TurtleSoupHandlerContext {
        turtle_soup,
        entertainment,
        deferred_chat,
        timer,
        active_deadline,
        pending_cancellations,
        operation_ids,
        clock_active,
        clock,
    } = context;
    let Some(service) = turtle_soup else {
        match message {
            TurtleSoupRuntimeMessage::HallCommand { response, .. }
            | TurtleSoupRuntimeMessage::FriendCommand { response, .. } => {
                let _ = response.send(TurtleSoupCommandOutcome {
                    action: "unavailable",
                    immediate_reply: Some("海龟汤运行时不可用".to_string()),
                });
            }
            TurtleSoupRuntimeMessage::StartRandom { response }
            | TurtleSoupRuntimeMessage::StartById { response, .. } => {
                let _ = response.send(Err(BusinessRuntimeError::RuntimeStopped));
            }
            TurtleSoupRuntimeMessage::End { response } => {
                let _ = response.send(Err(BusinessRuntimeError::RuntimeStopped));
            }
            TurtleSoupRuntimeMessage::FilterPrimary { response, .. } => {
                let _ = response.send(Vec::new());
            }
            TurtleSoupRuntimeMessage::StabilizeSecondary { response, .. } => {
                let _ = response.send(SecondaryOcrStability::Pending);
            }
            TurtleSoupRuntimeMessage::ClearSecondary
            | TurtleSoupRuntimeMessage::Abort { .. }
            | TurtleSoupRuntimeMessage::DeliverySuccess { .. }
            | TurtleSoupRuntimeMessage::DeliveryFailure { .. } => {}
            TurtleSoupRuntimeMessage::AppendPuzzle { response, .. } => {
                let _ = response.send(Err(BusinessRuntimeError::RuntimeStopped));
            }
            TurtleSoupRuntimeMessage::Accepts(response) => {
                let _ = response.send(false);
            }
            TurtleSoupRuntimeMessage::Submit { response, .. } => {
                let _ = response.send(Err(BusinessRuntimeError::RuntimeStopped));
            }
            TurtleSoupRuntimeMessage::DeliveryCurrent { response, .. } => {
                let _ = response.send(false);
            }
        }
        return Ok(());
    };

    match message {
        TurtleSoupRuntimeMessage::HallCommand {
            player,
            command,
            response,
        } => {
            let outcome =
                service.handle_hall_command(entertainment, &player, &command, deferred_chat);
            if let Err(error) = sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            ) {
                log::error!("处理海龟汤大厅命令后同步期限失败: {error}");
            }
            let _ = response.send(outcome);
        }
        TurtleSoupRuntimeMessage::FriendCommand {
            player,
            command,
            response,
        } => {
            let outcome =
                service.handle_friend_command(entertainment, &player, &command, deferred_chat);
            if let Err(error) = sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            ) {
                log::error!("处理海龟汤好友命令后同步期限失败: {error}");
            }
            let _ = response.send(outcome);
        }
        TurtleSoupRuntimeMessage::StartRandom { response } => {
            let result = service
                .start_random_from_web(entertainment, deferred_chat)
                .map_err(turtle_soup_operation_failed)
                .and_then(|()| {
                    sync_turtle_soup_deadline(
                        Some(&*service),
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    )
                });
            let _ = response.send(result);
        }
        TurtleSoupRuntimeMessage::StartById { id, response } => {
            let result = service
                .start_by_id_from_web(entertainment, &id, deferred_chat)
                .map_err(turtle_soup_operation_failed)
                .and_then(|()| {
                    sync_turtle_soup_deadline(
                        Some(&*service),
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    )
                });
            let _ = response.send(result);
        }
        TurtleSoupRuntimeMessage::End { response } => {
            let result = service
                .end_from_web(entertainment, deferred_chat)
                .map_err(turtle_soup_operation_failed)
                .and_then(|ended| {
                    sync_turtle_soup_deadline(
                        Some(&*service),
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    )?;
                    Ok(ended)
                });
            let _ = response.send(result);
        }
        TurtleSoupRuntimeMessage::FilterPrimary {
            visible,
            suppress_new,
            response,
        } => {
            let _ = response.send(service.filter_new_primary_questions(visible, suppress_new));
        }
        TurtleSoupRuntimeMessage::StabilizeSecondary {
            observations,
            response,
        } => {
            let _ = response.send(service.stabilize_secondary_ocr(observations));
        }
        TurtleSoupRuntimeMessage::ClearSecondary => service.clear_secondary_ocr_stability(),
        TurtleSoupRuntimeMessage::Accepts(response) => {
            let _ = response.send(service.accepts_questions());
        }
        TurtleSoupRuntimeMessage::Submit { question, response } => {
            let result = service
                .submit_question(question)
                .map_err(turtle_soup_operation_failed)
                .and_then(|outcome| {
                    sync_turtle_soup_deadline(
                        Some(&*service),
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        clock_active,
                    )?;
                    Ok(outcome)
                });
            let _ = response.send(result);
        }
        TurtleSoupRuntimeMessage::Abort { reason } => {
            service.abort_for_context_loss(entertainment, &reason);
            sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            )?;
        }
        TurtleSoupRuntimeMessage::DeliveryCurrent { delivery, response } => {
            let _ = response.send(service.delivery_is_current(delivery));
        }
        TurtleSoupRuntimeMessage::DeliverySuccess { delivery } => {
            service.handle_delivery_success(entertainment, delivery);
            sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            )?;
        }
        TurtleSoupRuntimeMessage::DeliveryFailure { delivery, error } => {
            let error = anyhow::anyhow!(error);
            service.handle_delivery_failure(entertainment, delivery, &error);
            sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            )?;
        }
        TurtleSoupRuntimeMessage::AppendPuzzle {
            submission,
            response,
        } => {
            let result = service
                .append_puzzle(submission)
                .map_err(turtle_soup_operation_failed);
            let _ = response.send(result);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_undercover_message(
    undercover: &mut UndercoverRuntimeService,
    entertainment: &mut EntertainmentState,
    message: UndercoverRuntimeMessage,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active_deadline: &mut Option<ActiveUndercoverDeadline>,
    pending_cancellations: &mut Vec<ActiveUndercoverDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    pending_outcomes: &mut std::collections::VecDeque<UndercoverTimedOutcome>,
    clock_active: &mut bool,
    clock: &dyn Clock,
) -> Result<(), BusinessRuntimeError> {
    match message {
        UndercoverRuntimeMessage::Begin {
            player,
            source,
            command,
            now,
            response,
        } => {
            let operation_id = operation_ids.allocate().map_err(|error| {
                BusinessRuntimeError::UndercoverOperationFailed(error.to_string())
            })?;
            let result = undercover
                .begin_command(entertainment, &player, source, &command, now, operation_id)
                .map_err(undercover_operation_failed)
                .and_then(|result| {
                    sync_undercover_deadline(
                        undercover,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        now,
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(result);
        }
        UndercoverRuntimeMessage::Claim { key, response } => {
            let result = undercover
                .claim(key)
                .map_err(undercover_operation_failed)
                .and_then(|result| {
                    sync_undercover_deadline(
                        undercover,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(result);
        }
        UndercoverRuntimeMessage::Resume {
            key,
            result: effect_result,
            response,
        } => {
            let result = undercover
                .resume(entertainment, key, effect_result)
                .map_err(undercover_operation_failed)
                .and_then(|result| {
                    sync_undercover_deadline(
                        undercover,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(result);
        }
        UndercoverRuntimeMessage::Cancel { key, response } => {
            let result = undercover
                .cancel(entertainment, key)
                .map_err(undercover_operation_failed)
                .and_then(|result| {
                    sync_undercover_deadline(
                        undercover,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        clock.now(),
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(result);
        }
        UndercoverRuntimeMessage::Poll {
            now,
            clock_active: requested_clock_active,
            response,
        } => {
            *clock_active = requested_clock_active;
            let result = sync_undercover_deadline(
                undercover,
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                now,
                requested_clock_active,
            )
            .map(|()| pending_outcomes.pop_front());
            let _ = response.send(result);
        }
        UndercoverRuntimeMessage::Abort(response) => {
            let aborted = undercover.abort(entertainment);
            let result = sync_undercover_deadline(
                undercover,
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                *clock_active,
            )
            .map(|()| aborted);
            let _ = response.send(result);
        }
    }
    Ok(())
}

fn idiom_chain_operation_failed(error: anyhow::Error) -> BusinessRuntimeError {
    BusinessRuntimeError::IdiomChainOperationFailed(format!("{error:#}"))
}

fn card_game_operation_failed(error: anyhow::Error) -> BusinessRuntimeError {
    BusinessRuntimeError::CardGameOperationFailed(format!("{error:#}"))
}

fn undercover_operation_failed(error: anyhow::Error) -> BusinessRuntimeError {
    BusinessRuntimeError::UndercoverOperationFailed(format!("{error:#}"))
}

fn turtle_soup_operation_failed(error: anyhow::Error) -> BusinessRuntimeError {
    BusinessRuntimeError::TurtleSoupOperationFailed(format!("{error:#}"))
}

fn hall_operation_failed(error: anyhow::Error) -> BusinessRuntimeError {
    BusinessRuntimeError::HallOperationFailed(format!("{error:#}"))
}

fn playback_operation_failed(error: anyhow::Error) -> BusinessRuntimeError {
    BusinessRuntimeError::PlaybackOperationFailed(format!("{error:#}"))
}

fn publish_playback_queue(
    state_sink: &Option<Arc<dyn BusinessStateSink>>,
    playback: Option<&PlaybackService>,
) {
    if let (Some(state_sink), Some(playback)) = (state_sink, playback) {
        state_sink.publish_playback_queue(playback.queue_snapshot());
    }
}

fn publish_hall_state(
    state_sink: &Option<Arc<dyn BusinessStateSink>>,
    hall: Option<&HallStateService>,
) {
    if let (Some(state_sink), Some(hall)) = (state_sink, hall) {
        state_sink.publish_hall_remaining_minutes(hall.snapshot().remaining_minutes_now());
    }
}

fn publish_scheduler_state(
    state_sink: &Option<Arc<dyn BusinessStateSink>>,
    scheduler: &FormalScheduler,
) {
    if let Some(state_sink) = state_sink {
        state_sink.publish_scheduler(scheduler.snapshot());
    }
}

fn publish_diagnostic_state(
    state_sink: &Option<Arc<dyn BusinessStateSink>>,
    scheduler: &FormalScheduler,
) {
    if let Some(state_sink) = state_sink {
        state_sink.publish_diagnostics(scheduler.diagnostic_snapshot());
    }
}

fn publish_operational_state(
    state_sink: &Option<Arc<dyn BusinessStateSink>>,
    operational: &OperationalState,
    now: Instant,
) {
    if let Some(state_sink) = state_sink {
        state_sink.publish_operational(operational.snapshot(now));
    }
}

fn publish_chat_listener_state(
    state_sink: &Option<Arc<dyn BusinessStateSink>>,
    chat_listener: &ChatListenerState,
) {
    if let Some(state_sink) = state_sink {
        state_sink.publish_chat_listener(chat_listener.snapshot());
    }
}

fn handle_chat_listener_message(
    state: &mut ChatListenerState,
    message: ChatListenerRuntimeMessage,
    state_sink: &Option<Arc<dyn BusinessStateSink>>,
) {
    let mutated = match message {
        ChatListenerRuntimeMessage::Snapshot(response) => {
            let _ = response.send(Ok(state.snapshot()));
            false
        }
        ChatListenerRuntimeMessage::RequestMode { target, response } => {
            let changed = state.request_mode(target);
            let _ = response.send(Ok(changed));
            changed
        }
        ChatListenerRuntimeMessage::CompleteMode { mode, response } => {
            state.complete_mode_switch(mode);
            let _ = response.send(Ok(()));
            true
        }
        ChatListenerRuntimeMessage::CancelModeRequest { target, response } => {
            state.cancel_mode_request(target);
            let _ = response.send(Ok(()));
            true
        }
        ChatListenerRuntimeMessage::FailModeSwitchToPrimary(response) => {
            state.fail_mode_switch_to_primary();
            let _ = response.send(Ok(()));
            true
        }
        ChatListenerRuntimeMessage::BeginTemporaryPrimary(response) => {
            state.begin_temporary_primary();
            let _ = response.send(Ok(()));
            true
        }
        ChatListenerRuntimeMessage::EndTemporaryPrimary(response) => {
            state.end_temporary_primary();
            let _ = response.send(Ok(()));
            true
        }
        ChatListenerRuntimeMessage::ClaimUnreadTask(response) => {
            let claimed = state.claim_unread_task();
            let _ = response.send(Ok(claimed));
            claimed
        }
        ChatListenerRuntimeMessage::FinishUnreadTask {
            processed_message,
            response,
        } => {
            state.finish_unread_task(processed_message);
            let _ = response.send(Ok(()));
            true
        }
        ChatListenerRuntimeMessage::ReleaseUnreadTask(response) => {
            state.release_unread_task();
            let _ = response.send(Ok(()));
            true
        }
        ChatListenerRuntimeMessage::FinishInitialUnreadClear(response) => {
            state.finish_initial_unread_clear();
            let _ = response.send(Ok(()));
            true
        }
        ChatListenerRuntimeMessage::FinishHallRound(response) => {
            state.finish_hall_round();
            let _ = response.send(Ok(()));
            true
        }
    };
    if mutated {
        publish_chat_listener_state(state_sink, state);
    }
}

fn publish_decision_state(
    state_sink: &Option<Arc<dyn BusinessStateSink>>,
    decision: &mut DecisionState,
) {
    if let Some(state_sink) = state_sink {
        state_sink.publish_decision(decision.snapshot());
    }
}

fn handle_decision_message(
    state: &mut DecisionState,
    message: DecisionRuntimeMessage,
    state_sink: &Option<Arc<dyn BusinessStateSink>>,
) {
    match message {
        DecisionRuntimeMessage::Begin {
            label,
            allow_switch_source,
            allow_ai,
            timeout,
            delivery,
            response,
        } => {
            let result = state
                .begin(label, allow_switch_source, allow_ai, timeout, delivery)
                .map_err(BusinessRuntimeError::DecisionOperationFailed);
            let _ = response.send(result);
        }
        #[cfg(test)]
        DecisionRuntimeMessage::Snapshot(response) => {
            let _ = response.send(Ok(state.snapshot()));
        }
        DecisionRuntimeMessage::Submit {
            id,
            action,
            response,
        } => {
            let result = state
                .submit(id, action)
                .map_err(BusinessRuntimeError::DecisionOperationFailed);
            let _ = response.send(result);
        }
        DecisionRuntimeMessage::Finish { id, response } => {
            state.finish(id);
            let _ = response.send(Ok(()));
        }
    }
    publish_decision_state(state_sink, state);
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::features::card_games::{
        CardGameDeadlineKind, CardGameDeadlineToken, CardGameEffect, CardGameEffectLane,
        CardGameEffectRequest, CardGameLateResult, LandlordConfig,
    };
    use crate::features::entertainment::{EntertainmentKind, EntertainmentState};
    use crate::features::idiom_chain::{
        IdiomChainDeadlineKind, IdiomChainDeadlineToken, IdiomChainMode,
    };
    use crate::features::turtle_soup::{TurtleSoupDeadlineKind, TurtleSoupDeadlineToken};
    use crate::features::undercover::{UndercoverDeadlineKind, UndercoverDeadlineToken};
    use crate::observation::chat::ChatObservationLedger;
    use crate::observation::shared::{ObservationGapKind, SharedObservationStream};
    use crate::runtime::clock::ManualClock;
    use crate::runtime::deadline::{BusinessDeadlineEvent, BusinessDeadlineToken};
    use crate::runtime::deferred_chat::{DeferredChatMessage, DeferredChatTarget};
    use crate::runtime::identity::{BusinessOperationId, SessionGeneration};
    use crate::runtime::scheduler::{
        DiagnosticTaskCompletion, DiagnosticTaskSubmission, DiagnosticTaskWork, FormalTaskDedupKey,
        FormalTaskReceipt,
    };
    use crate::runtime::timer::{DeadlineSchedule, TimerCore, TimerRuntime, TimerRuntimeEvent};

    struct TestFormalWork;

    impl FormalTaskWork for TestFormalWork {
        fn execute(self: Box<Self>) -> anyhow::Result<String> {
            Ok("done".to_string())
        }

        fn cancel(self: Box<Self>) {}
    }

    struct CancelAwareFormalWork(Arc<AtomicBool>);

    impl FormalTaskWork for CancelAwareFormalWork {
        fn execute(self: Box<Self>) -> anyhow::Result<String> {
            Ok("done".to_string())
        }

        fn cancel(self: Box<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct TestDiagnosticWork;

    impl DiagnosticTaskWork for TestDiagnosticWork {
        fn execute(self: Box<Self>) -> anyhow::Result<String> {
            Ok("diagnostic-result".to_string())
        }
    }

    fn formal_submission(label: &str, dedup_key: Option<&str>) -> FormalTaskSubmission {
        FormalTaskSubmission::new(
            label,
            dedup_key.map(FormalTaskDedupKey::new),
            false,
            Box::new(TestFormalWork),
        )
    }

    fn idiom_service(idle_timeout: Option<Duration>) -> IdiomChainService {
        IdiomChainService::from_entries_for_test(
            &["画蛇添足", "足智多谋", "谋事在人", "人山人海"],
            idle_timeout,
        )
    }

    fn runtime(queue_capacity: usize) -> BusinessRuntime {
        BusinessRuntime::start(
            queue_capacity,
            idiom_service(Some(Duration::from_secs(300))),
            CardGameService::new(LandlordConfig::default()),
        )
        .unwrap()
    }

    fn runtime_with_entertainment(queue_capacity: usize) -> BusinessRuntime {
        BusinessRuntime::start(
            queue_capacity,
            idiom_service(Some(Duration::from_secs(300))),
            CardGameService::new(LandlordConfig::default()),
        )
        .unwrap()
    }

    #[test]
    fn formal_scheduler_enqueues_in_order_and_rejects_queued_duplicate() {
        let runtime = runtime(8);
        let handle = runtime.handle();

        let first = handle
            .enqueue_formal_task(formal_submission("first", Some("same")))
            .unwrap();
        let second = handle
            .enqueue_formal_task(formal_submission("second", Some("other")))
            .unwrap();
        let duplicate = handle
            .enqueue_formal_task(formal_submission("duplicate", Some("same")))
            .unwrap();

        assert!(matches!(
            first,
            FormalTaskEnqueueOutcome::Queued(FormalTaskReceipt {
                task_id: 1,
                position: 1
            })
        ));
        assert!(matches!(
            second,
            FormalTaskEnqueueOutcome::Queued(FormalTaskReceipt {
                task_id: 2,
                position: 2
            })
        ));
        assert_eq!(duplicate, FormalTaskEnqueueOutcome::Duplicate);
        assert_eq!(
            handle.scheduler_snapshot().unwrap().pending_labels(),
            &["first".to_string(), "second".to_string()]
        );

        runtime.shutdown().unwrap();
    }

    #[test]
    fn formal_scheduler_restores_original_turn_and_cancels_only_queued_work() {
        let runtime = runtime(8);
        let handle = runtime.handle();
        let canceled = Arc::new(AtomicBool::new(false));
        let first = handle
            .enqueue_formal_task(formal_submission("first", Some("first")))
            .unwrap();
        let second = handle
            .enqueue_formal_task(FormalTaskSubmission::new(
                "second",
                Some(FormalTaskDedupKey::new("second")),
                false,
                Box::new(CancelAwareFormalWork(canceled.clone())),
            ))
            .unwrap();
        let FormalTaskEnqueueOutcome::Queued(first) = first else {
            panic!("first task should be queued");
        };
        let FormalTaskEnqueueOutcome::Queued(second) = second else {
            panic!("second task should be queued");
        };

        let active = handle
            .take_next_formal_task()
            .unwrap()
            .expect("first task should start");
        assert_eq!(active.task_id(), first.task_id);
        assert_eq!(
            handle.cancel_formal_task(first.task_id).unwrap(),
            FormalTaskCancelOutcome::NotQueued
        );

        handle.restore_formal_task(active).unwrap();
        assert_eq!(
            handle.scheduler_snapshot().unwrap().pending_labels(),
            &["first".to_string(), "second".to_string()]
        );

        let active = handle
            .take_next_formal_task()
            .unwrap()
            .expect("restored task should start first");
        let task_id = active.task_id();
        let result = active.execute().unwrap();
        handle
            .complete_formal_task(task_id, FormalTaskCompletion::Succeeded(result))
            .unwrap();
        assert_eq!(
            handle.cancel_formal_task(second.task_id).unwrap(),
            FormalTaskCancelOutcome::Canceled
        );
        assert!(canceled.load(Ordering::SeqCst));

        let snapshot = handle.scheduler_snapshot().unwrap();
        assert!(snapshot.pending_labels().is_empty());
        assert_eq!(snapshot.tasks()[0].id, second.task_id);
        assert_eq!(snapshot.tasks()[0].status, "canceled");
        assert_eq!(snapshot.tasks()[1].id, first.task_id);
        assert_eq!(snapshot.tasks()[1].status, "completed");

        runtime.shutdown().unwrap();
    }

    #[test]
    fn business_runtime_owns_command_availability_and_idle_exit_claiming() {
        let runtime = runtime(8);
        let handle = runtime.handle();
        let started_at = Instant::now();

        assert!(
            handle
                .operational_snapshot(started_at)
                .unwrap()
                .commands_enabled()
        );
        handle.set_commands_enabled(false).unwrap();
        assert!(
            !handle
                .operational_snapshot(started_at)
                .unwrap()
                .commands_enabled()
        );

        handle
            .configure_idle_exit(Duration::from_secs(120), started_at)
            .unwrap();
        handle
            .record_command_activity(started_at + Duration::from_secs(30))
            .unwrap();
        assert_eq!(
            handle
                .operational_snapshot(started_at + Duration::from_secs(40))
                .unwrap()
                .idle_exit_remaining_seconds(),
            Some(110)
        );

        let queued = handle
            .enqueue_formal_task(formal_submission("busy", Some("busy")))
            .unwrap();
        assert_eq!(
            handle
                .claim_idle_exit(started_at + Duration::from_secs(151))
                .unwrap(),
            None
        );
        let FormalTaskEnqueueOutcome::Queued(receipt) = queued else {
            panic!("formal task should be queued");
        };
        assert_eq!(
            handle.cancel_formal_task(receipt.task_id).unwrap(),
            FormalTaskCancelOutcome::Canceled
        );
        assert_eq!(
            handle
                .claim_idle_exit(started_at + Duration::from_secs(151))
                .unwrap(),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            handle
                .operational_snapshot(started_at + Duration::from_secs(151))
                .unwrap()
                .idle_exit_remaining_seconds(),
            None
        );

        runtime.shutdown().unwrap();
    }

    #[test]
    fn diagnostic_tasks_wait_behind_formal_tasks_and_keep_owned_history() {
        let runtime = runtime(8);
        let handle = runtime.handle();
        let diagnostic = handle
            .enqueue_diagnostic_task(DiagnosticTaskSubmission::new(
                "OCR 诊断",
                Box::new(TestDiagnosticWork),
            ))
            .unwrap();
        handle
            .enqueue_formal_task(formal_submission("formal", Some("formal")))
            .unwrap();

        assert!(handle.take_next_diagnostic_task().unwrap().is_none());
        let formal = handle.take_next_formal_task().unwrap().unwrap();
        let formal_id = formal.task_id();
        let formal_result = formal.execute().unwrap();
        handle
            .complete_formal_task(formal_id, FormalTaskCompletion::Succeeded(formal_result))
            .unwrap();

        let task = handle.take_next_diagnostic_task().unwrap().unwrap();
        assert_eq!(task.task_id(), diagnostic.id);
        let task_id = task.task_id();
        let result = task.execute().unwrap();
        handle
            .complete_diagnostic_task(task_id, DiagnosticTaskCompletion::Succeeded(result))
            .unwrap();

        let snapshot = handle
            .diagnostic_task_snapshot(diagnostic.id)
            .unwrap()
            .expect("diagnostic history");
        assert_eq!(snapshot.id, diagnostic.id);
        assert_eq!(snapshot.status, "completed");
        assert_eq!(snapshot.result.as_deref(), Some("diagnostic-result"));

        runtime.shutdown().unwrap();
    }

    #[test]
    fn deferred_chat_waits_behind_formal_work_and_ahead_of_diagnostics() {
        let runtime = runtime(8);
        let handle = runtime.handle();
        handle
            .enqueue_diagnostic_task(DiagnosticTaskSubmission::new(
                "diagnostic",
                Box::new(TestDiagnosticWork),
            ))
            .unwrap();
        handle
            .enqueue_deferred_chat(DeferredChatMessage {
                text: "deferred".to_string(),
                target: DeferredChatTarget::Primary,
            })
            .unwrap();
        handle
            .enqueue_formal_task(formal_submission("formal", Some("formal-priority")))
            .unwrap();

        assert!(handle.take_next_deferred_chat().unwrap().is_none());
        assert!(handle.take_next_diagnostic_task().unwrap().is_none());
        let formal = handle.take_next_formal_task().unwrap().unwrap();
        let formal_id = formal.task_id();
        handle
            .complete_formal_task(
                formal_id,
                FormalTaskCompletion::Succeeded(formal.execute().unwrap()),
            )
            .unwrap();

        assert!(handle.take_next_diagnostic_task().unwrap().is_none());
        let (item, permit) = handle
            .take_next_deferred_chat()
            .unwrap()
            .expect("deferred work");
        assert!(matches!(
            item,
            crate::runtime::deferred_chat::DeferredChatItem::Message(_)
        ));
        drop(permit);
        assert!(handle.take_next_diagnostic_task().unwrap().is_some());

        runtime.shutdown().unwrap();
    }

    #[test]
    fn business_runtime_owns_chat_listener_mode_holds_and_unread_claims() {
        let runtime = runtime(16);
        let handle = runtime.handle();

        assert!(
            handle
                .request_chat_listener_mode(ChatListenerMode::Secondary)
                .unwrap()
        );
        assert_eq!(
            handle.chat_listener_snapshot().unwrap().pending_mode,
            Some(ChatListenerMode::Secondary)
        );
        handle
            .complete_chat_listener_mode(ChatListenerMode::Secondary)
            .unwrap();
        assert!(
            handle
                .chat_listener_snapshot()
                .unwrap()
                .initial_unread_clear
        );
        assert!(handle.claim_chat_listener_unread_task().unwrap());
        assert!(!handle.claim_chat_listener_unread_task().unwrap());
        handle.finish_chat_listener_unread_task(true).unwrap();
        handle.finish_chat_listener_initial_unread_clear().unwrap();
        assert!(handle.chat_listener_snapshot().unwrap().hall_round_required);

        handle.begin_chat_listener_temporary_primary().unwrap();
        handle.begin_chat_listener_temporary_primary().unwrap();
        assert!(handle.chat_listener_snapshot().unwrap().temporary_primary);
        handle.end_chat_listener_temporary_primary().unwrap();
        assert!(handle.chat_listener_snapshot().unwrap().temporary_primary);
        handle.end_chat_listener_temporary_primary().unwrap();
        assert!(!handle.chat_listener_snapshot().unwrap().temporary_primary);

        handle.finish_chat_listener_hall_round().unwrap();
        assert!(!handle.chat_listener_snapshot().unwrap().hall_round_required);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn business_runtime_owns_decision_validation_while_waiting_stays_external() {
        let runtime = runtime(16);
        let handle = runtime.handle();
        let session = handle
            .begin_decision("候选确认", true, false, Duration::from_secs(1))
            .unwrap();
        let snapshot = handle
            .decision_snapshot()
            .unwrap()
            .expect("active decision");

        assert_eq!(snapshot.id, session.id());
        assert_eq!(
            handle.submit_decision(snapshot.id, DecisionAction::Ai),
            Err(BusinessRuntimeError::DecisionOperationFailed(
                "当前决策不允许切换 AI".to_string()
            ))
        );
        handle
            .submit_decision(snapshot.id, DecisionAction::SwitchSource)
            .unwrap();
        assert_eq!(
            session.wait(Duration::from_millis(10)).unwrap(),
            Some(DecisionAction::SwitchSource)
        );
        drop(session);
        assert!(handle.decision_snapshot().unwrap().is_none());

        runtime.shutdown().unwrap();
    }

    fn playback_service(
        queue: crate::features::playback::PersistentQueue,
        playback_state: crate::features::playback::PersistentPlaybackState,
    ) -> crate::features::playback::PlaybackService {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let history = crate::features::playback::PersistentSongDedupHistory::load(
            std::env::temp_dir().join(format!("mwm-business-dedup-{suffix}.json")),
            Arc::new(SystemClock),
        )
        .unwrap();
        crate::features::playback::PlaybackService::new(
            queue,
            playback_state,
            history,
            crate::features::playback::SongDedupConfig::default(),
        )
    }

    fn hall_service(path: std::path::PathBuf) -> HallStateService {
        let clock = Arc::new(SystemClock);
        HallStateService::new_with_time(
            crate::features::hall::PersistentHallState::load(path).expect("hall state"),
            clock.clone(),
            clock,
        )
    }

    fn suspended(start: CardGameCommandStart) -> CardGameEffectRequest {
        match start {
            CardGameCommandStart::Suspended(request) => request,
            CardGameCommandStart::Completed(_) => panic!("expected suspended card game effect"),
        }
    }

    #[test]
    fn real_channel_applies_completion_events_in_order() {
        let runtime = runtime(4);
        let handle = runtime.handle();
        let mut ledger = ChatObservationLedger::new();
        let first = ledger.begin_frame(Instant::now());
        let second = ledger.begin_frame(Instant::now());
        let blocked = ledger.complete_success(second.id()).unwrap();
        let advance = ledger.complete_failure(first.id(), "failed").unwrap();

        handle
            .submit(BusinessEvent::CompletionAdvance(blocked))
            .unwrap();
        handle
            .submit(BusinessEvent::CompletionAdvance(advance))
            .unwrap();
        let snapshot = handle.snapshot().unwrap();

        assert_eq!(
            snapshot.latest_watermark().unwrap().completed_through,
            second.id()
        );
        assert_eq!(snapshot.terminal_failure_count(), 1);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn completion_gap_is_counted_without_inventing_a_watermark() {
        let runtime = runtime(2);
        let handle = runtime.handle();
        let mut stream = SharedObservationStream::<()>::new(NonZeroUsize::new(1).unwrap());
        let gap = stream.mark_gap(ObservationGapKind::HistoryEvicted);

        handle.submit(BusinessEvent::CompletionGap(gap)).unwrap();
        let snapshot = handle.snapshot().unwrap();

        assert_eq!(snapshot.completion_gap_count(), 1);
        assert_eq!(snapshot.latest_watermark(), None);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn real_channel_counts_routed_deadlines_without_running_game_logic() {
        let runtime = runtime(8);
        let handle = runtime.handle();
        let event_sink = runtime.event_sink();
        let deadline = Instant::now();
        let tokens = [
            BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                1,
                CardGameDeadlineKind::LobbyExpiry,
            )),
            BusinessDeadlineToken::from(UndercoverDeadlineToken::new(
                2,
                UndercoverDeadlineKind::LobbyIdle,
            )),
            BusinessDeadlineToken::from(TurtleSoupDeadlineToken::new(
                3,
                TurtleSoupDeadlineKind::SessionIdle,
            )),
            BusinessDeadlineToken::from(IdiomChainDeadlineToken::new(
                4,
                IdiomChainDeadlineKind::SessionIdle,
            )),
        ];
        let mut timer = TimerCore::new();
        for (index, token) in tokens.into_iter().enumerate() {
            timer
                .schedule(DeadlineSchedule::new(
                    token,
                    BusinessOperationId::new(index as u64 + 1),
                    SessionGeneration::INITIAL,
                    deadline,
                ))
                .unwrap();
        }

        for event in timer.drain_expired(deadline).unwrap() {
            event_sink
                .submit(BusinessEvent::Timer(BusinessDeadlineEvent::from(
                    TimerRuntimeEvent::DeadlineExpired(event),
                )))
                .unwrap();
        }
        let snapshot = handle.snapshot().unwrap();

        assert_eq!(snapshot.timer_counts().card_game(), 1);
        assert_eq!(snapshot.timer_counts().undercover(), 1);
        assert_eq!(snapshot.timer_counts().turtle_soup(), 1);
        assert_eq!(snapshot.timer_counts().idiom_chain(), 1);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_drains_prior_events_and_stops_cloned_handles() {
        let runtime = runtime(1);
        let handle = runtime.handle();
        let mut ledger = ChatObservationLedger::new();
        let frame = ledger.begin_frame(Instant::now());
        handle
            .submit(BusinessEvent::CompletionAdvance(
                ledger.complete_failure(frame.id(), "failed").unwrap(),
            ))
            .unwrap();

        let final_snapshot = runtime.shutdown().unwrap();

        assert_eq!(final_snapshot.terminal_failure_count(), 1);
        assert_eq!(handle.snapshot(), Err(BusinessRuntimeError::RuntimeStopped));
    }

    #[test]
    fn prepare_shutdown_rejects_new_work_but_drains_internal_events_before_finish() {
        let runtime = runtime_with_entertainment(8);
        let handle = runtime.handle();
        let event_sink = runtime.event_sink();
        handle
            .handle_idiom_chain(
                "Alice",
                &IdiomChainCommand::Start {
                    idiom: "画蛇添足".to_string(),
                    mode: IdiomChainMode::Exact,
                },
            )
            .unwrap();

        let prepared = runtime.prepare_shutdown().unwrap();

        assert!(prepared.is_quiescing());
        assert_eq!(handle.active_entertainment().unwrap(), None);
        assert_eq!(
            handle.handle_idiom_chain("Bob", &IdiomChainCommand::Status),
            Err(BusinessRuntimeError::Quiescing)
        );
        let mut stream = SharedObservationStream::<()>::new(NonZeroUsize::new(1).unwrap());
        let gap = stream.mark_gap(ObservationGapKind::HistoryEvicted);
        assert_eq!(
            handle.submit(BusinessEvent::CompletionGap(gap.clone())),
            Err(BusinessRuntimeError::Quiescing)
        );
        event_sink
            .submit(BusinessEvent::CompletionGap(gap))
            .unwrap();
        let deadline = Instant::now();
        let mut timer = TimerCore::new();
        timer
            .schedule(DeadlineSchedule::new(
                BusinessDeadlineToken::from(IdiomChainDeadlineToken::new(
                    1,
                    IdiomChainDeadlineKind::SessionIdle,
                )),
                BusinessOperationId::new(1),
                SessionGeneration::INITIAL,
                deadline,
            ))
            .unwrap();
        let expired = timer.drain_expired(deadline).unwrap().pop().unwrap();
        event_sink
            .submit(BusinessEvent::Timer(BusinessDeadlineEvent::from(
                TimerRuntimeEvent::DeadlineExpired(expired),
            )))
            .unwrap();

        let final_snapshot = runtime.shutdown().unwrap();

        assert!(final_snapshot.is_quiescing());
        assert_eq!(final_snapshot.completion_gap_count(), 1);
        assert_eq!(final_snapshot.timer_counts().idiom_chain(), 1);
        assert_eq!(
            event_sink.submit(BusinessEvent::CompletionGap(
                SharedObservationStream::<()>::new(NonZeroUsize::new(1).unwrap())
                    .mark_gap(ObservationGapKind::HistoryEvicted),
            )),
            Err(BusinessRuntimeError::RuntimeStopped)
        );
    }

    #[test]
    fn card_game_begin_claim_and_resume_share_worker_owned_state() {
        let runtime = runtime_with_entertainment(8);
        let handle = runtime.handle();
        let verification = suspended(
            handle
                .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
                .unwrap(),
        );

        assert_eq!(verification.lane, CardGameEffectLane::Formal);
        assert!(matches!(
            verification.effect,
            CardGameEffect::FriendVerify { ref player, .. } if player == "甲"
        ));
        assert_eq!(
            handle.claim_card_game_effect(verification.key).unwrap(),
            CardGameEffectClaim::Claimed
        );
        let hall = match handle
            .resume_card_game(
                verification.key,
                CardGameEffectResult::FriendVerify(Ok(true)),
            )
            .unwrap()
        {
            CardGameResume::Suspended(request) => request,
            other => panic!("verified start should announce the lobby: {other:?}"),
        };
        assert_eq!(hall.lane, CardGameEffectLane::Formal);
        assert_eq!(
            handle.claim_card_game_effect(hall.key).unwrap(),
            CardGameEffectClaim::Claimed
        );
        assert!(matches!(
            handle
                .resume_card_game(hall.key, CardGameEffectResult::HallDelivery(Ok(())))
                .unwrap(),
            CardGameResume::Completed(_)
        ));
        assert_eq!(
            handle.active_entertainment().unwrap(),
            Some(EntertainmentKind::Landlord)
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn card_game_claim_distinguishes_queued_cancel_from_claimed_work() {
        let runtime = runtime(8);
        let handle = runtime.handle();
        let first = suspended(
            handle
                .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
                .unwrap(),
        );

        assert!(matches!(
            handle.cancel_card_game_effect(first.key).unwrap(),
            CardGameCancel::Cancelled(_)
        ));
        assert!(matches!(
            handle.claim_card_game_effect(first.key).unwrap(),
            CardGameEffectClaim::Late(CardGameLateResult { key }) if key == first.key
        ));

        let second = suspended(
            handle
                .begin_card_game("乙", &LandlordCommand::Start, Instant::now())
                .unwrap(),
        );
        assert_eq!(
            handle.claim_card_game_effect(second.key).unwrap(),
            CardGameEffectClaim::Claimed
        );
        assert!(matches!(
            handle.claim_card_game_effect(second.key).unwrap(),
            CardGameEffectClaim::Late(_)
        ));
        assert!(matches!(
            handle.cancel_card_game_effect(second.key).unwrap(),
            CardGameCancel::Late(_)
        ));
        assert!(matches!(
            handle
                .resume_card_game(second.key, CardGameEffectResult::FriendVerify(Ok(false)),)
                .unwrap(),
            CardGameResume::Suspended(_)
        ));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn old_card_game_generation_is_late_without_exposing_ui_work() {
        let runtime = runtime(8);
        let handle = runtime.handle();
        let old = suspended(
            handle
                .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
                .unwrap(),
        );
        assert!(handle.abort_card_game().unwrap());
        let current = suspended(
            handle
                .begin_card_game("乙", &LandlordCommand::Start, Instant::now())
                .unwrap(),
        );

        assert!(matches!(
            handle.claim_card_game_effect(old.key).unwrap(),
            CardGameEffectClaim::Late(_)
        ));
        assert_eq!(
            handle.claim_card_game_effect(current.key).unwrap(),
            CardGameEffectClaim::Claimed
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn abort_remains_responsive_while_claimed_ui_work_is_slow() {
        let runtime = runtime(8);
        let handle = runtime.handle();
        let request = suspended(
            handle
                .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
                .unwrap(),
        );
        let worker_handle = handle.clone();
        let (claimed_sender, claimed_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            assert_eq!(
                worker_handle.claim_card_game_effect(request.key).unwrap(),
                CardGameEffectClaim::Claimed
            );
            claimed_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            worker_handle
                .resume_card_game(request.key, CardGameEffectResult::FriendVerify(Ok(true)))
        });
        claimed_receiver.recv().unwrap();

        assert!(handle.abort_card_game().unwrap());
        release_sender.send(()).unwrap();
        assert!(matches!(
            worker.join().unwrap().unwrap(),
            CardGameResume::Late(_)
        ));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn card_game_effect_chains_preserve_their_scheduling_lane() {
        let runtime = runtime(8);
        let handle = runtime.handle();
        let verification = suspended(
            handle
                .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
                .unwrap(),
        );
        handle.claim_card_game_effect(verification.key).unwrap();
        let hall = match handle
            .resume_card_game(
                verification.key,
                CardGameEffectResult::FriendVerify(Ok(true)),
            )
            .unwrap()
        {
            CardGameResume::Suspended(request) => request,
            other => panic!("verified start should continue: {other:?}"),
        };
        assert_eq!(hall.lane, CardGameEffectLane::Formal);
        handle.claim_card_game_effect(hall.key).unwrap();
        handle
            .resume_card_game(hall.key, CardGameEffectResult::HallDelivery(Ok(())))
            .unwrap();

        let status = suspended(
            handle
                .begin_card_game("甲", &LandlordCommand::Status, Instant::now())
                .unwrap(),
        );
        assert_eq!(status.lane, CardGameEffectLane::Deferred);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_stops_card_game_requests_from_all_handle_clones() {
        let runtime = runtime_with_entertainment(4);
        let first = runtime.handle();
        let second = first.clone();
        let request = suspended(
            first
                .begin_card_game("甲", &LandlordCommand::Start, Instant::now())
                .unwrap(),
        );

        runtime.shutdown().unwrap();

        assert_eq!(
            first.claim_card_game_effect(request.key),
            Err(BusinessRuntimeError::RuntimeStopped)
        );
        assert_eq!(
            second.abort_card_game(),
            Err(BusinessRuntimeError::RuntimeStopped)
        );
    }

    #[test]
    fn shutdown_aborts_active_idiom_chain_and_releases_entertainment() {
        let runtime = BusinessRuntime::start(
            4,
            idiom_service(Some(Duration::from_secs(300))),
            CardGameService::new(LandlordConfig::default()),
        )
        .unwrap();
        runtime
            .handle()
            .handle_idiom_chain(
                "Alice",
                &IdiomChainCommand::Start {
                    idiom: "画蛇添足".to_string(),
                    mode: IdiomChainMode::Exact,
                },
            )
            .unwrap();

        runtime.prepare_shutdown().unwrap();
        assert_eq!(runtime.handle().active_entertainment().unwrap(), None);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn idiom_chain_requests_share_worker_owned_state() {
        let runtime = BusinessRuntime::start(
            4,
            idiom_service(Some(Duration::from_secs(300))),
            CardGameService::new(LandlordConfig::default()),
        )
        .unwrap();
        let handle = runtime.handle();

        let started = handle
            .handle_idiom_chain(
                "Alice",
                &IdiomChainCommand::Start {
                    idiom: "画蛇添足".to_string(),
                    mode: IdiomChainMode::Exact,
                },
            )
            .unwrap();
        let submitted = handle
            .handle_idiom_chain("Bob", &IdiomChainCommand::Submit("足智多谋".to_string()))
            .unwrap();
        let explained = handle
            .explain_idiom_chain(
                "Carol",
                &IdiomChainCommand::Explain(Some("足智多谋".to_string())),
            )
            .unwrap();

        assert_eq!(started.action, "started");
        assert_eq!(submitted.action, "accepted");
        assert_eq!(explained.action, "missing-explanation");
        assert!(explained.explanation.is_none());
        assert!(!handle.expire_idiom_chain().unwrap());
        assert!(handle.abort_idiom_chain().unwrap());
        assert_eq!(handle.active_entertainment().unwrap(), None);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn idiom_chain_idle_expiration_runs_on_the_business_worker() {
        let runtime = BusinessRuntime::start(
            2,
            idiom_service(Some(Duration::ZERO)),
            CardGameService::new(LandlordConfig::default()),
        )
        .unwrap();
        let handle = runtime.handle();

        let started = handle
            .handle_idiom_chain(
                "Alice",
                &IdiomChainCommand::Start {
                    idiom: "画蛇添足".to_string(),
                    mode: IdiomChainMode::Exact,
                },
            )
            .unwrap();

        assert_eq!(started.action, "started");
        assert!(handle.expire_idiom_chain().unwrap());
        assert_eq!(handle.active_entertainment().unwrap(), None);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn worker_owned_idiom_time_uses_the_injected_clock() {
        let clock = Arc::new(ManualClock::new(Instant::now()));
        let runtime = BusinessRuntime::start_with_clock(
            2,
            idiom_service(Some(Duration::from_secs(30))),
            CardGameService::new(LandlordConfig::default()),
            clock.clone(),
        )
        .unwrap();
        let handle = runtime.handle();
        handle
            .handle_idiom_chain(
                "Alice",
                &IdiomChainCommand::Start {
                    idiom: "画蛇添足".to_string(),
                    mode: IdiomChainMode::Exact,
                },
            )
            .unwrap();

        clock.advance(Duration::from_secs(29)).unwrap();
        assert!(!handle.expire_idiom_chain().unwrap());
        clock.advance(Duration::from_secs(1)).unwrap();
        assert!(handle.expire_idiom_chain().unwrap());

        runtime.shutdown().unwrap();
    }

    #[test]
    fn matching_idiom_deadline_event_expires_the_owned_session() {
        let mut entertainment = EntertainmentState::new();
        let mut idiom_chain = idiom_service(Some(Duration::ZERO));
        let started = idiom_chain
            .handle(
                &mut entertainment,
                "Alice",
                &IdiomChainCommand::Start {
                    idiom: "画蛇添足".to_string(),
                    mode: IdiomChainMode::Exact,
                },
            )
            .unwrap();
        assert_eq!(started.action, "started");

        let deadline = idiom_chain.idle_deadline().unwrap();
        let operation_id = BusinessOperationId::new(1);
        let session_generation = SessionGeneration::new(1);
        let token = idiom_deadline_token();
        let mut timer = TimerCore::new();
        timer
            .schedule(DeadlineSchedule::new(
                token.clone(),
                operation_id,
                session_generation,
                deadline,
            ))
            .unwrap();
        let expired = timer.drain_expired(deadline).unwrap().pop().unwrap();
        let event = BusinessDeadlineEvent::from(TimerRuntimeEvent::DeadlineExpired(expired));
        let mut active = Some(ActiveIdiomDeadline {
            token,
            operation_id,
            session_generation,
            deadline,
        });
        let operation_ids = BusinessOperationIdAllocator::new();
        let mut next_generation = session_generation;
        let mut card_games = CardGameService::new(LandlordConfig::default());
        let mut undercover = default_undercover_service();
        let mut turtle_active = None;
        let mut card_active = None;
        let mut undercover_active = None;
        let mut pending_turtle_cancellations = Vec::new();
        let mut pending_card_cancellations = Vec::new();
        let mut pending_undercover_cancellations = Vec::new();
        let mut pending_card_outcomes = std::collections::VecDeque::new();
        let mut pending_undercover_outcomes = std::collections::VecDeque::new();
        let mut deferred_chat = DeferredChatQueue::new(DEFERRED_CHAT_CAPACITY);

        handle_business_timer(
            event,
            &mut entertainment,
            &mut idiom_chain,
            &mut card_games,
            &mut undercover,
            None,
            &mut deferred_chat,
            None,
            &mut active,
            &mut card_active,
            &mut undercover_active,
            &mut turtle_active,
            &mut pending_card_cancellations,
            &mut pending_undercover_cancellations,
            &mut pending_turtle_cancellations,
            &mut pending_card_outcomes,
            &mut pending_undercover_outcomes,
            &operation_ids,
            &mut next_generation,
            true,
            &SystemClock,
        )
        .unwrap();

        assert!(active.is_none());
        assert_eq!(entertainment.active(), None);
        assert_eq!(idiom_chain.idle_deadline(), None);
    }

    #[test]
    fn card_deadline_cancel_submission_failure_restores_the_old_deadline() {
        let (event_sender, _event_receiver) = mpsc::sync_channel(4);
        let timer_runtime = TimerRuntime::start(4, event_sender).unwrap();
        let timer = timer_runtime.handle();
        let card_games = CardGameService::new(LandlordConfig::default());
        let previous = ActiveCardGameDeadline {
            token: card_game_deadline_token(CardGameDeadlineKind::LobbyExpiry),
            operation_id: BusinessOperationId::new(17),
            session_generation: SessionGeneration::INITIAL,
            deadline: Instant::now(),
        };
        let mut active = Some(previous.clone());
        let mut pending = Vec::new();
        let operation_ids = BusinessOperationIdAllocator::new();

        timer_runtime.shutdown().unwrap();
        let result = sync_card_game_deadline(
            &card_games,
            Some(&timer),
            &mut active,
            &mut pending,
            &operation_ids,
            Instant::now(),
            true,
        );

        assert!(matches!(
            result,
            Err(BusinessRuntimeError::TimerOperationFailed(message))
                if message.contains("timer runtime is stopped")
        ));
        assert_eq!(active, Some(previous));
        assert!(pending.is_empty());
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert!(matches!(
            BusinessRuntime::start(
                0,
                idiom_service(None),
                CardGameService::new(LandlordConfig::default()),
            ),
            Err(BusinessRuntimeError::ZeroQueueCapacity)
        ));
    }

    #[test]
    fn playback_queue_mutations_run_on_the_business_owner() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mwm-business-queue-{suffix}.json"));
        let playback_state_path = path.with_extension("playback-state.json");
        let hall_state_path = path.with_extension("hall-state.json");
        let queue = crate::features::playback::PersistentQueue::load(path, 4).unwrap();
        let playback_state =
            crate::features::playback::PersistentPlaybackState::load(playback_state_path).unwrap();
        let runtime = BusinessRuntime::start_with_playback(
            4,
            idiom_service(None),
            CardGameService::new(LandlordConfig::default()),
            hall_service(hall_state_path),
            playback_service(queue, playback_state),
        )
        .unwrap();
        let handle = runtime.handle();

        let pushed = handle
            .push_playback_queue(QueueItem {
                keyword: "晴天".to_string(),
                source: "qqmusic".to_string(),
                uri: "fuo://qqmusic/songs/1".to_string(),
                ..QueueItem::default()
            })
            .unwrap();
        let snapshot = handle.playback_queue_snapshot().unwrap();

        assert!(pushed.accepted);
        assert_eq!(pushed.size, 1);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].keyword, "晴天");
        runtime.shutdown().unwrap();
    }

    #[test]
    fn hall_state_patch_is_applied_by_the_business_owner() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("mwm-business-state-{suffix}"));
        let queue =
            crate::features::playback::PersistentQueue::load(directory.join("queue.json"), 4)
                .unwrap();
        let playback_state = crate::features::playback::PersistentPlaybackState::load(
            directory.join("playback-state.json"),
        )
        .unwrap();
        let runtime = BusinessRuntime::start_with_playback(
            4,
            idiom_service(None),
            CardGameService::new(LandlordConfig::default()),
            hall_service(directory.join("hall-state.json")),
            playback_service(queue, playback_state),
        )
        .unwrap();
        let handle = runtime.handle();

        handle
            .patch_hall_state(HallStatePatch {
                remaining_minutes: Some(Some(42)),
                remaining_updated_at: Some(Some(1234)),
                expiring_warning_sent: Some(true),
            })
            .unwrap();
        let snapshot = handle.hall_state_snapshot().unwrap();

        assert_eq!(snapshot.remaining_minutes, Some(42));
        assert_eq!(snapshot.remaining_updated_at, Some(1234));
        assert!(snapshot.expiring_warning_sent);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn concurrent_invite_sequence_reservations_have_one_winner() {
        let runtime = runtime(4);
        let handle = runtime.handle();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers = ["甲", "乙"].map(|username| {
            let handle = handle.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                matches!(
                    handle
                        .begin_invite(InviteRequest::new(username, Some(9), None))
                        .unwrap(),
                    InviteStart::Ready(_)
                )
            })
        });
        barrier.wait();

        let ready = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|ready| *ready)
            .count();

        assert_eq!(ready, 1);
        assert!(!handle.invite_should_accept(Some(9)).unwrap());
        runtime.shutdown().unwrap();
    }
}
