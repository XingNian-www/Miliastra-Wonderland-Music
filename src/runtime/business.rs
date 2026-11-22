use std::collections::HashSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

mod entertainment;
mod hall;
mod playback;

use entertainment::EntertainmentRuntimeState as EntertainmentComponent;
use hall::HallRuntimeState as HallComponent;
use miliastra_playback::{PlayableTrack, TrackKey};
use playback::PlaybackRuntimeState as PlaybackComponent;

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
use crate::features::moderation::{
    ModerationWorkflowKey, ModerationWorkflowLedger, ModerationWorkflowRegistry,
    ModerationWorkflowToken,
};
#[cfg(test)]
use crate::features::number_bomb::NumberBombConfig;
use crate::features::number_bomb::{
    NumberBombCommand, NumberBombCommandOutcome, NumberBombService, NumberBombTimeout,
};
use crate::features::playback::{
    ActivePlaybackIdentity, ExternalPlaybackObservation, PlaybackMutationIntent,
    PlaybackMutationOutcome, PlaybackObservation, PlaybackRuntimeState, PlaybackService,
    PlaybackSessionBinding, PlaybackStateUpdate, QueueItem, QueuePushOutcome, QueueRemoval,
    QueueRemoveOutcome, SessionReconciliation, SongDedupCandidate, SongRequestLeaderboardEntry,
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
use crate::runtime::deadline::{BusinessDeadlineEvent, BusinessDeadlineToken};
use crate::runtime::decision::{DecisionAction, DecisionSnapshot, DecisionState};
#[cfg(test)]
use crate::runtime::task_engine::TaskEngineHandle;
use crate::runtime::task_engine::{BusinessTaskPort, TaskEngineError};
use miliastra_kernel::clock::Clock;
#[cfg(test)]
use miliastra_kernel::clock::SystemClock;
use miliastra_kernel::identity::{
    BusinessOperationId, BusinessOperationIdAllocator, SessionGeneration,
};
use miliastra_kernel::timer::{
    DeadlineCancellation, DeadlineSchedule, TimerCommandKind, TimerRuntimeEvent, TimerRuntimeHandle,
};

const IDIOM_DEADLINE_TOKEN_ID: u64 = 1;
const CARD_GAME_DEADLINE_TOKEN_ID: u64 = 1;
const UNDERCOVER_DEADLINE_TOKEN_ID: u64 = 1;
const TURTLE_SOUP_DEADLINE_TOKEN_ID: u64 = 1;

/// 关停时等待 worker 退出（回包 + 线程收敛）的上限。
///
/// 演员可能正阻塞在某个同步端口调用里（SQLite 写、`player_io` 的 `sync_channel`
/// 满、AI 调用……），这些阻塞没有取消点。`join()` 无超时会把退出流程和配置重载
/// 一起挂死，进程只能强杀；超过这个期限就记 error 并放弃 join（detach），
/// 让主流程继续关停其它子系统。见 `technical-docs/archive/.../concurrency.md`（P2-12）。
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// 从 `catch_unwind` 的载荷里取可读的 panic 文案。
fn panic_detail(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

/// `deadline` 之前的剩余时间；已过期则返回零（`recv_timeout(0)` 立即超时）。
fn remaining_until(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

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
    TurtleSoup(Box<TurtleSoupMutationOutcome>),
}

/// 业务公开状态的窄投影接口。实现只能保留已经脱敏的快照，业务内部不依赖监控展示结构。
pub(crate) trait BusinessStateSink: Send + Sync {
    fn publish_turtle_soup(&self, snapshot: TurtleSoupSnapshot);
    fn publish_undercover(&self, snapshot: UndercoverSnapshot);
    fn publish_playback_queue(&self, _queue: Vec<QueueItem>) {}
    fn publish_hall_remaining_minutes(&self, _minutes: Option<u32>, _updated_at: Option<u64>) {}
    fn publish_chat_listener(&self, _snapshot: ChatListenerSnapshot) {}
    fn publish_decision(&self, _snapshot: Option<DecisionSnapshot>) {}
    fn publish_operational(&self, _snapshot: BusinessOperationalSnapshot) {}
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
                log::warn!("计时运行时上报了一次命令失败，失败计数已累加，本局继续");
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
            schedule.last_command_at = schedule.last_command_at.max(now);
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
    /// 停机超时：worker 在 [`WORKER_SHUTDOWN_TIMEOUT`] 内没有退出，已被脱管。
    ///
    /// 与 [`Self::WorkerPanicked`] 分开，因为处置方式相反——panic 是线程已经没了，
    /// 超时是线程还活着（可能仍持有玩法租约），调用方/日志需要能区分。
    WorkerShutdownTimedOut,
    IdiomChainOperationFailed(String),
    CardGameOperationFailed(String),
    UndercoverOperationFailed(String),
    NumberBombOperationFailed(String),
    TurtleSoupOperationFailed(String),
    HallOperationFailed(String),
    PlaybackOperationFailed(String),
    TimerOperationFailed(String),
    TaskEngineOperationFailed(String),
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
            Self::WorkerShutdownTimedOut => formatter
                .write_str("business runtime worker did not stop before the shutdown deadline"),
            Self::IdiomChainOperationFailed(message) => {
                write!(formatter, "idiom chain operation failed: {message}")
            }
            Self::CardGameOperationFailed(message) => {
                write!(formatter, "card game operation failed: {message}")
            }
            Self::UndercoverOperationFailed(message) => {
                write!(formatter, "undercover operation failed: {message}")
            }
            Self::NumberBombOperationFailed(message) => {
                write!(formatter, "number bomb operation failed: {message}")
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
            Self::TaskEngineOperationFailed(message) => {
                write!(formatter, "task engine operation failed: {message}")
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
        observed_at: Option<Instant>,
        response: SyncSender<Result<IdiomChainOutcome, BusinessRuntimeError>>,
    },
    ExplainIdiomChain {
        player: String,
        command: IdiomChainCommand,
        observed_at: Option<Instant>,
        response: SyncSender<Result<IdiomChainOutcome, BusinessRuntimeError>>,
    },
    AbortIdiomChain(SyncSender<Result<bool, BusinessRuntimeError>>),
    ExpireIdiomChain(SyncSender<Result<bool, BusinessRuntimeError>>),
    CardGame(CardGameRuntimeMessage),
    Undercover(UndercoverRuntimeMessage),
    UndercoverSnapshot(SyncSender<Result<UndercoverSnapshot, BusinessRuntimeError>>),
    NumberBomb(NumberBombRuntimeMessage),
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
        direct: bool,
        response: SyncSender<Option<Arc<ModerationWorkflowToken>>>,
    },
    ReleaseModerationWorkflow {
        key: ModerationWorkflowKey,
        token: Arc<ModerationWorkflowToken>,
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
    PushQueueNext {
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
    RemovePlaylistAutoItems(SyncSender<Result<usize, BusinessRuntimeError>>),
    QueueContains {
        item: QueueItem,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    QueueSnapshot(SyncSender<Result<Vec<QueueItem>, BusinessRuntimeError>>),
    PlaybackPoolSnapshot(SyncSender<Result<Vec<PlayableTrack>, BusinessRuntimeError>>),
    PlaybackPoolAvailable(SyncSender<Result<bool, BusinessRuntimeError>>),
    RemovePoolTrack {
        key: TrackKey,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    RecordPlaybackPoolTrack {
        track: PlayableTrack,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    PickPlaybackPoolTrack {
        excluded: HashSet<TrackKey>,
        response: SyncSender<Result<Option<PlayableTrack>, BusinessRuntimeError>>,
    },
    StateSnapshot(SyncSender<Result<PlaybackRuntimeState, BusinessRuntimeError>>),
    UpdatePlaybackState {
        update: PlaybackStateUpdate,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    RecordObservationIfActive {
        expected: ActivePlaybackIdentity,
        observation: PlaybackObservation,
        immediate: bool,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    ConfirmPlaybackAndDequeue {
        update: PlaybackStateUpdate,
        queue_item_id: Option<u64>,
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
        identity: TrackKey,
        now: Instant,
        protect_after: Duration,
        response: SyncSender<Result<ExternalPlaybackObservation, BusinessRuntimeError>>,
    },
    ClearExternalPlaybackTracker(SyncSender<Result<(), BusinessRuntimeError>>),
    ReconcilePlayerSession {
        binding: Option<PlaybackSessionBinding>,
        response: SyncSender<Result<SessionReconciliation, BusinessRuntimeError>>,
    },
    InspectPlayerSession {
        binding: Option<PlaybackSessionBinding>,
        response: SyncSender<Result<SessionReconciliation, BusinessRuntimeError>>,
    },
    ClaimTerminalOutcome {
        request_id: u64,
        outcome: String,
        handled_at_ms: u64,
        response: SyncSender<Result<bool, BusinessRuntimeError>>,
    },
    RecordPlaybackAttempt {
        provider: String,
        locator: String,
        started_at_ms: u64,
        result: String,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    RecordControlOperation {
        operation: String,
        requested_at_ms: u64,
        completed: bool,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    },
    SongRequestLeaderboard {
        since_ms: Option<u64>,
        limit: usize,
        response: SyncSender<Result<Vec<SongRequestLeaderboardEntry>, BusinessRuntimeError>>,
    },
}

enum ChatListenerRuntimeMessage {
    Snapshot(SyncSender<Result<ChatListenerSnapshot, BusinessRuntimeError>>),
    RequestMode {
        target: ChatListenerMode,
        // 一次性返回请求结果与最新快照，避免「请求 + 单独快照」两段式
        // 造成跨 actor 间隙的不一致。
        response: SyncSender<Result<(bool, ChatListenerSnapshot), BusinessRuntimeError>>,
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
        // 面向玩家的展示名。`player` 是身份键（已登记身份为 UUID），
        // 只参与比较；展示名只参与渲染。两者必须分开，否则 UUID 会进文案。
        player_display: String,
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
        player_display: String,
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

enum NumberBombRuntimeMessage {
    Handle {
        player: String,
        player_display: String,
        command: NumberBombCommand,
        now: Instant,
        response: SyncSender<Result<NumberBombCommandOutcome, BusinessRuntimeError>>,
    },
    Poll {
        now: Instant,
        clock_active: bool,
        response: SyncSender<Result<Option<NumberBombTimeout>, BusinessRuntimeError>>,
    },
    Abort(SyncSender<Result<bool, BusinessRuntimeError>>),
}

enum TurtleSoupRuntimeMessage {
    HallCommand {
        /// 展示名。海龟汤的命令方只用它渲染「开局者」与「好友结束」文案，
        /// 内部键（UUID）不得从这里进来。
        player_display: String,
        command: TurtleSoupCommand,
        response: SyncSender<TurtleSoupCommandOutcome>,
    },
    FriendCommand {
        player_display: String,
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
        observed_at: Instant,
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

    /// 主动关闭 Web 决策：聊天侧已经做出决定时立即失效，
    /// 避免并发 submit 在窗口退出前抢到「假成功」。
    pub(crate) fn close(&self) {
        if let Err(error) = self.handle.finish_decision(self.id) {
            log::debug!("Web 决策未能正常关闭，窗口可能延后释放：{error}");
        }
    }
}

impl Drop for DecisionSession {
    fn drop(&mut self) {
        if let Err(error) = self.handle.finish_decision(self.id) {
            log::debug!("Web 决策未能正常结束，窗口可能延后释放：{error}");
        }
    }
}

impl CardGameRuntimePort for BusinessRuntimeHandle {
    fn begin_with_display(
        &self,
        player: &str,
        player_display: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> anyhow::Result<CardGameCommandStart> {
        Ok(self.begin_card_game_with_display(player, player_display, command, now)?)
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
        player_display: &str,
        source: UndercoverCommandSource,
        command: &UndercoverCommand,
        now: Instant,
    ) -> anyhow::Result<UndercoverCommandStart> {
        Ok(self.begin_undercover(player, player_display, source, command, now)?)
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
            BusinessMutationIntent::TurtleSoup(intent) => BusinessMutationOutcome::TurtleSoup(
                Box::new(self.apply_turtle_soup_mutation(intent)?),
            ),
        })
    }

    fn apply_administration_mutation(
        &self,
        intent: AdministrationMutationIntent,
    ) -> Result<AdministrationMutationOutcome, BusinessRuntimeError> {
        Ok(match intent {
            AdministrationMutationIntent::RequestChatListenerMode(target) => {
                // 请求与快照在同一 actor 消息内返回，消除两段式间隙。
                let (queued, snapshot) = self.request_chat_listener_mode(target)?;
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
                PlaybackMutationOutcome::Pushed(self.push_playback_queue(*item)?)
            }
            PlaybackMutationIntent::Remove(removal) => {
                PlaybackMutationOutcome::Removed(self.remove_playback_queue(removal)?)
            }
            PlaybackMutationIntent::Clear => {
                self.clear_playback_queue()?;
                PlaybackMutationOutcome::Cleared
            }
            PlaybackMutationIntent::RemovePoolTrack(key) => {
                PlaybackMutationOutcome::PoolTrackRemoved(self.remove_playback_pool_track(key)?)
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
    ) -> Result<(bool, ChatListenerSnapshot), BusinessRuntimeError> {
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

    pub(crate) fn push_playback_queue_next(
        &self,
        item: QueueItem,
    ) -> Result<QueuePushOutcome, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::PushQueueNext { item, response })
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

    pub(crate) fn remove_playlist_auto_items(&self) -> Result<usize, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::RemovePlaylistAutoItems(response))
        })
    }

    pub(crate) fn playback_queue_snapshot(&self) -> Result<Vec<QueueItem>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::QueueSnapshot(response))
        })
    }

    pub(crate) fn playback_pool_snapshot(
        &self,
    ) -> Result<Vec<PlayableTrack>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::PlaybackPoolSnapshot(response))
        })
    }

    pub(crate) fn playback_pool_available(&self) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::PlaybackPoolAvailable(response))
        })
    }

    /// 从播放池删除指定歌曲(不再参与随机播放)。
    pub(crate) fn remove_playback_pool_track(
        &self,
        key: TrackKey,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::RemovePoolTrack { key, response })
        })
    }

    pub(crate) fn record_playback_pool_track(
        &self,
        track: PlayableTrack,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::RecordPlaybackPoolTrack {
                track,
                response,
            })
        })
    }

    /// 点歌人排行榜查询。
    pub(crate) fn song_request_leaderboard(
        &self,
        since_ms: Option<u64>,
        limit: usize,
    ) -> Result<Vec<SongRequestLeaderboardEntry>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::SongRequestLeaderboard {
                since_ms,
                limit,
                response,
            })
        })
    }

    pub(crate) fn pick_playback_pool_track(
        &self,
        excluded: &HashSet<TrackKey>,
    ) -> Result<Option<PlayableTrack>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::PickPlaybackPoolTrack {
                excluded: excluded.clone(),
                response,
            })
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

    /// 原子确认播放成功并从队列删除对应项（同一笔持久化），
    /// 消除「确认成功、出队未持久化」的崩溃窗口。
    pub(crate) fn confirm_playback_and_dequeue(
        &self,
        update: PlaybackStateUpdate,
        queue_item_id: Option<u64>,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::ConfirmPlaybackAndDequeue {
                update,
                queue_item_id,
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
        identity: TrackKey,
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

    pub(crate) fn reconcile_player_session(
        &self,
        binding: Option<PlaybackSessionBinding>,
    ) -> Result<SessionReconciliation, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::ReconcilePlayerSession {
                binding,
                response,
            })
        })
    }

    pub(crate) fn record_observation_if_active(
        &self,
        expected: ActivePlaybackIdentity,
        observation: PlaybackObservation,
        immediate: bool,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::RecordObservationIfActive {
                expected,
                observation,
                immediate,
                response,
            })
        })
    }

    pub(crate) fn inspect_player_session(
        &self,
        binding: Option<PlaybackSessionBinding>,
    ) -> Result<SessionReconciliation, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::InspectPlayerSession {
                binding,
                response,
            })
        })
    }

    pub(crate) fn claim_terminal_outcome(
        &self,
        request_id: u64,
        outcome: String,
        handled_at_ms: u64,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::ClaimTerminalOutcome {
                request_id,
                outcome,
                handled_at_ms,
                response,
            })
        })
    }

    pub(crate) fn record_playback_attempt(
        &self,
        provider: String,
        locator: String,
        started_at_ms: u64,
        result: String,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::RecordPlaybackAttempt {
                provider,
                locator,
                started_at_ms,
                result,
                response,
            })
        })
    }

    pub(crate) fn record_control_operation(
        &self,
        operation: String,
        requested_at_ms: u64,
        completed: bool,
    ) -> Result<(), BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Playback(PlaybackRuntimeMessage::RecordControlOperation {
                operation,
                requested_at_ms,
                completed,
                response,
            })
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
        direct: bool,
    ) -> Result<Option<Arc<ModerationWorkflowToken>>, BusinessRuntimeError> {
        self.request_value(|response| RuntimeMessage::AcquireModerationWorkflow {
            key,
            direct,
            response,
        })
    }

    fn release_moderation_workflow(
        &self,
        key: ModerationWorkflowKey,
        token: Arc<ModerationWorkflowToken>,
    ) -> Result<bool, BusinessRuntimeError> {
        self.request_value(|response| RuntimeMessage::ReleaseModerationWorkflow {
            key,
            token,
            response,
        })
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
        self.handle_idiom_chain_with_time(player, command, None)
    }

    pub fn handle_idiom_chain_at(
        &self,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Instant,
    ) -> Result<IdiomChainOutcome, BusinessRuntimeError> {
        self.handle_idiom_chain_with_time(player, command, Some(observed_at))
    }

    fn handle_idiom_chain_with_time(
        &self,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Option<Instant>,
    ) -> Result<IdiomChainOutcome, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::HandleIdiomChain {
            player: player.to_string(),
            command: command.clone(),
            observed_at,
            response,
        })
    }

    pub fn explain_idiom_chain(
        &self,
        player: &str,
        command: &IdiomChainCommand,
    ) -> Result<IdiomChainOutcome, BusinessRuntimeError> {
        self.explain_idiom_chain_with_time(player, command, None)
    }

    pub fn explain_idiom_chain_at(
        &self,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Instant,
    ) -> Result<IdiomChainOutcome, BusinessRuntimeError> {
        self.explain_idiom_chain_with_time(player, command, Some(observed_at))
    }

    fn explain_idiom_chain_with_time(
        &self,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Option<Instant>,
    ) -> Result<IdiomChainOutcome, BusinessRuntimeError> {
        self.request(|response| RuntimeMessage::ExplainIdiomChain {
            player: player.to_string(),
            command: command.clone(),
            observed_at,
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
        // 兼容入口：展示名与身份键相同（测试与未登记玩家）。
        self.begin_card_game_with_display(player, player, command, now)
    }

    /// 展示名与内部键分开的入口；生产路径走这条。
    pub fn begin_card_game_with_display(
        &self,
        player: &str,
        player_display: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<CardGameCommandStart, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::CardGame(CardGameRuntimeMessage::Begin {
                player: player.to_string(),
                player_display: player_display.to_string(),
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
        player_display: &str,
        source: UndercoverCommandSource,
        command: &UndercoverCommand,
        now: Instant,
    ) -> Result<UndercoverCommandStart, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::Undercover(UndercoverRuntimeMessage::Begin {
                player: player.to_string(),
                player_display: player_display.to_string(),
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

    /// 数字炸弹命令只走单写者业务线程：加入/回合/超时都依赖同一份状态。
    pub(crate) fn handle_number_bomb(
        &self,
        player: &str,
        player_display: &str,
        command: &NumberBombCommand,
        now: Instant,
    ) -> Result<NumberBombCommandOutcome, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::NumberBomb(NumberBombRuntimeMessage::Handle {
                player: player.to_string(),
                player_display: player_display.to_string(),
                command: command.clone(),
                now,
                response,
            })
        })
    }

    pub(crate) fn poll_number_bomb_timeout(
        &self,
        now: Instant,
        clock_active: bool,
    ) -> Result<Option<NumberBombTimeout>, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::NumberBomb(NumberBombRuntimeMessage::Poll {
                now,
                clock_active,
                response,
            })
        })
    }

    pub(crate) fn abort_number_bomb(&self) -> Result<bool, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::NumberBomb(NumberBombRuntimeMessage::Abort(response))
        })
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
        player_display: &str,
        command: &TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome, BusinessRuntimeError> {
        self.request_value(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::HallCommand {
                player_display: player_display.to_string(),
                command: command.clone(),
                response,
            })
        })
    }

    pub(crate) fn handle_turtle_soup_friend_command(
        &self,
        player_display: &str,
        command: &TurtleSoupCommand,
    ) -> Result<TurtleSoupCommandOutcome, BusinessRuntimeError> {
        self.request_value(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::FriendCommand {
                player_display: player_display.to_string(),
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

    // Linux 非测试库构建下，本方法只从 Windows 生产入口可达；生产与测试构建仍然检查。
    #[cfg_attr(all(not(target_os = "windows"), not(test)), allow(dead_code))]
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

    // Linux 非测试库构建下，本方法只从 Windows 生产入口可达；生产与测试构建仍然检查。
    #[cfg_attr(all(not(target_os = "windows"), not(test)), allow(dead_code))]
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

    pub(crate) fn submit_turtle_soup_question_at(
        &self,
        question: TurtleSoupQuestion,
        observed_at: Instant,
    ) -> Result<QuestionSubmitOutcome, BusinessRuntimeError> {
        self.request(|response| {
            RuntimeMessage::TurtleSoup(TurtleSoupRuntimeMessage::Submit {
                question,
                observed_at,
                response,
            })
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
        {
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
        }
        // 锁外 send:队列打满时不再持锁阻塞,避免拖住同锁的 shutdown/stop_worker。
        self.channel
            .sender
            .send(message)
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }

    fn send_query(&self, message: RuntimeMessage) -> Result<(), BusinessRuntimeError> {
        {
            let state = self
                .channel
                .state
                .lock()
                .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
            if *state == RuntimeChannelState::Stopped {
                return Err(BusinessRuntimeError::RuntimeStopped);
            }
        }
        // 锁外 send,同上:避免持锁阻塞 shutdown/stop_worker。
        self.channel
            .sender
            .send(message)
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }
}

impl ModerationWorkflowLedger for BusinessRuntimeHandle {
    fn acquire(
        &self,
        key: ModerationWorkflowKey,
        direct: bool,
    ) -> anyhow::Result<Option<Arc<ModerationWorkflowToken>>> {
        self.acquire_moderation_workflow(key, direct)
            .map_err(anyhow::Error::from)
    }

    fn release(
        &self,
        key: ModerationWorkflowKey,
        token: Arc<ModerationWorkflowToken>,
    ) -> anyhow::Result<bool> {
        self.release_moderation_workflow(key, token)
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
        // 锁外 send：队列打满时不能持锁阻塞。
        //
        // 与 `send_request` / `send_query` 对齐——那两处都用内层 block 提前释放
        // guard，注释也写明「避免拖住同锁的 shutdown/stop_worker」。这里原来漏了，
        // 于是队列满时 submit 持锁等空位、stop_worker 等同一把锁，两条链互卡，
        // 关闭流程挂住而 join 无超时，进程退不掉。
        {
            let state = self
                .channel
                .state
                .lock()
                .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
            if *state == RuntimeChannelState::Stopped {
                return Err(BusinessRuntimeError::RuntimeStopped);
            }
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
            log::debug!(
                "业务运行时已停止，本次海龟汤 AI 裁决结果被丢弃，不影响已结束的对局：{error}"
            );
        }
    }
}

pub struct BusinessRuntime {
    handle: BusinessRuntimeHandle,
    worker: Option<JoinHandle<()>>,
    /// worker 线程跑完主循环后投递的回执；`stop_worker` 用它做有界等待。
    finished: Receiver<()>,
    turtle_soup_workers: Option<TurtleSoupWorkerRuntime>,
}

pub(crate) struct BusinessRuntimeWorker {
    idiom_chain: IdiomChainService,
    card_games: CardGameService,
    undercover: UndercoverRuntimeService,
    number_bomb: NumberBombService,
    turtle_soup: Option<TurtleSoupService>,
    hall: Option<HallStateService>,
    playback: Option<PlaybackService>,
    invite: InviteService,
    task_port: BusinessTaskPort,
    timer: Option<TimerRuntimeHandle<BusinessDeadlineToken>>,
    state_sink: Option<Arc<dyn BusinessStateSink>>,
    clock: Arc<dyn Clock>,
    /// 仅测试：本条运行时关停时，在 `abort_all` 之前注入一次 panic。
    ///
    /// 停机顺序（`should_stop` 必须先于清理置位）没法从外部观察——它只在
    /// 「清理 panic 且被吞掉」时才有可观测差异，所以用注入点把它钉死。
    #[cfg(test)]
    abort_all_probe: Option<Arc<AbortAllPanicProbe>>,
}

impl BusinessRuntimeWorker {
    /// 仅测试：让本条运行时在关停清理时 panic 一次。
    #[cfg(test)]
    pub(crate) fn with_abort_all_panic_injection(mut self) -> Self {
        self.abort_all_probe = Some(AbortAllPanicProbe::armed());
        self
    }

    /// 仅测试：拿一份注入探针的句柄，用于断言「panic 真的发生过」。
    ///
    /// 刻意用 clone 而不是 take：探针必须**留在 worker 里**才会被关停路径消费，
    /// 取走之后注入就永远不会触发（测试会退化成真空）。
    #[cfg(test)]
    pub(crate) fn abort_all_probe(&self) -> Option<Arc<AbortAllPanicProbe>> {
        self.abort_all_probe.clone()
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
        undercover: UndercoverRuntimeService,
        number_bomb: NumberBombService,
        turtle_soup: TurtleSoupService,
        hall: HallStateService,
        playback: PlaybackService,
        invite: InviteService,
        task_port: BusinessTaskPort,
        timer: TimerRuntimeHandle<BusinessDeadlineToken>,
        state_sink: Arc<dyn BusinessStateSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            idiom_chain,
            card_games,
            undercover,
            number_bomb,
            turtle_soup: Some(turtle_soup),
            hall: Some(hall),
            playback: Some(playback),
            invite,
            task_port,
            timer: Some(timer),
            state_sink: Some(state_sink),
            clock,
            #[cfg(test)]
            abort_all_probe: None,
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
        Self::start_with_clock_and_task_engine(
            queue_capacity,
            idiom_chain,
            card_games,
            clock,
            TaskEngineHandle::new(None),
        )
    }

    #[cfg(test)]
    pub(crate) fn start_with_task_engine(
        queue_capacity: usize,
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
        task_engine: TaskEngineHandle,
    ) -> Result<Self, BusinessRuntimeError> {
        Self::start_with_clock_and_task_engine(
            queue_capacity,
            idiom_chain,
            card_games,
            Arc::new(SystemClock),
            task_engine,
        )
    }

    #[cfg(test)]
    fn start_with_clock_and_task_engine(
        queue_capacity: usize,
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
        clock: Arc<dyn Clock>,
        task_engine: TaskEngineHandle,
    ) -> Result<Self, BusinessRuntimeError> {
        Self::start_internal(
            queue_capacity,
            BusinessRuntimeWorker {
                idiom_chain,
                card_games,
                undercover: default_undercover_service(),
                number_bomb: NumberBombService::new(NumberBombConfig::default()),
                turtle_soup: None,
                hall: None,
                playback: None,
                invite: InviteService::new(),
                task_port: task_engine.business_port(),
                timer: None,
                state_sink: None,
                clock: Arc::new(SystemClock),
                #[cfg(test)]
                abort_all_probe: None,
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
                number_bomb: NumberBombService::new(NumberBombConfig::default()),
                turtle_soup: None,
                hall: Some(hall),
                playback: Some(playback),
                invite: InviteService::new(),
                task_port: TaskEngineHandle::new(None).business_port(),
                timer: None,
                state_sink: None,
                clock: Arc::new(SystemClock),
                #[cfg(test)]
                abort_all_probe: None,
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
        let (finished_sender, finished) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("business-runtime".to_string())
            .spawn(move || {
                run_business_runtime(receiver, worker_config);
                // 只为「线程真的跑完了」这一件事服务：`join()` 没有超时，
                // 关停侧靠这条回执判断能否安全 join（见 `stop_worker`）。
                let _ = finished_sender.send(());
            })
            .map_err(|_| {
                if let Some(workers) = turtle_soup_workers.as_mut() {
                    workers.shutdown();
                }
                BusinessRuntimeError::RuntimeStopped
            })?;
        Ok(Self {
            handle: BusinessRuntimeHandle { channel },
            worker: Some(worker),
            finished,
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
        // 锁内只改状态并挑选消息类型，**出锁再 send**。
        //
        // 队列是有界 channel，满时 send 会阻塞；持着 `state` 锁阻塞会拖住
        // send_request / send_query（它们要先抢同一把锁），纯查询调用也跟着卡，
        // 关闭期出现确定性卡顿与优先级反转。
        let message = {
            let mut state = self
                .handle
                .channel
                .state
                .lock()
                .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
            match *state {
                RuntimeChannelState::Running => {
                    *state = RuntimeChannelState::Quiescing;
                    RuntimeMessage::PrepareShutdown(response)
                }
                RuntimeChannelState::Quiescing => RuntimeMessage::Snapshot(response),
                RuntimeChannelState::Stopped => return Err(BusinessRuntimeError::RuntimeStopped),
            }
        };
        self.handle
            .channel
            .sender
            .send(message)
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
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
        // 同 prepare_shutdown：锁内只改状态，出锁再 send，避免队列满时
        // 持锁阻塞、把并发请求与另一次 shutdown 一起拖住。
        {
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
        }
        // 整段关停共用**一个**期限：三段各自等满 3s 会让退出最多挂 9s，
        // 而验收要求是「worker 卡住时 ~3s 内返回」。剩余时间逐段递减。
        let deadline = Instant::now() + WORKER_SHUTDOWN_TIMEOUT;
        // 入队也可能无限阻塞：队列满时 `send` 会一直等，而卡住的正是消费者。
        // 用 `try_send` + 期限重试，把「投递关停指令」也纳入有界等待。
        let sent = self.send_shutdown_bounded(RuntimeMessage::Shutdown(response), deadline);
        let snapshot = if sent {
            match receiver.recv_timeout(remaining_until(deadline)) {
                Ok(snapshot) => Some(snapshot),
                Err(RecvTimeoutError::Timeout) => {
                    log::error!(
                        "业务运行时在 {WORKER_SHUTDOWN_TIMEOUT:?} 内没有回应关停（步骤：等待停机快照），本次放弃等待并脱管该线程"
                    );
                    None
                }
                Err(RecvTimeoutError::Disconnected) => None,
            }
        } else {
            None
        };
        // 有界收敛：先用 `finished` 回执判断线程是否真的跑完，再决定能否 `join()`。
        // `join()` 本身没有期限，直接调它会把关停流程和配置重载一起挂死
        // （`concurrency.md` P2-12）；超时就记 error 并脱管线程，让主流程继续。
        let worker_result = match self.finished.recv_timeout(remaining_until(deadline)) {
            Ok(()) => worker
                .join()
                .map_err(|_| BusinessRuntimeError::WorkerPanicked),
            Err(RecvTimeoutError::Disconnected) => {
                // 回执通道断了说明线程已经结束（只是没走到最后一行），`join` 会立刻返回。
                worker
                    .join()
                    .map_err(|_| BusinessRuntimeError::WorkerPanicked)
            }
            Err(RecvTimeoutError::Timeout) => {
                log::error!(
                    "业务运行时在 {WORKER_SHUTDOWN_TIMEOUT:?} 内没有退出（步骤：等待业务线程收敛），本次放弃 join 并脱管该线程；停机流程继续，残留的玩法租约由下次启动自检收拾"
                );
                drop(worker);
                Err(BusinessRuntimeError::WorkerShutdownTimedOut)
            }
        };
        self.stop_external_workers();
        worker_result?;
        snapshot.ok_or(BusinessRuntimeError::RuntimeStopped)
    }

    /// 在 `deadline` 之前把停机指令投进队列。
    ///
    /// 队列满说明消费者没在取消息（正是我们要兜的卡死场景），
    /// 所以这里不能用会阻塞的 `send`。
    fn send_shutdown_bounded(&self, message: RuntimeMessage, deadline: Instant) -> bool {
        let mut pending = message;
        loop {
            match self.handle.channel.sender.try_send(pending) {
                Ok(()) => return true,
                Err(mpsc::TrySendError::Disconnected(_)) => return false,
                Err(mpsc::TrySendError::Full(returned)) => {
                    pending = returned;
                    if Instant::now() >= deadline {
                        log::error!(
                            "业务运行时队列在 {WORKER_SHUTDOWN_TIMEOUT:?} 内一直满（步骤：投递关停指令），消费者未在取消息；本次放弃投递并脱管该线程"
                        );
                        return false;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }
}

#[cfg(test)]
fn default_undercover_service() -> UndercoverRuntimeService {
    UndercoverRuntimeService::new(
        UndercoverConfig::default(),
        crate::test_support::test_state_store(),
    )
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
            // 同一 Token 只是改期：用 `reschedule` 原地更新。
            //
            // 不能走「`active.take()` + 异步 `Cancel`」：`Cancel` 只是把命令压进有界队列就
            // 返回，从 take 到 `CommandCompleted(Cancel)` 回来之间 `active` 是 `None`，
            // 并发消息看到 `None` 会立刻 `schedule` 同一个 Token —— 要么撞上
            // `TokenAlreadyScheduled`（这次同步报错），要么在取消真正落地后连日程一起被删，
            // 与 `pending_cancellations` 里的那份记录彻底错位。`reschedule` 换成
            // 「先查 token 是否存在、存在就整条替换」，没有这个窗口。
            let operation_id = operation_ids
                .allocate()
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            timer
                .reschedule(DeadlineSchedule::new(
                    previous.token.clone(),
                    operation_id,
                    previous.session_generation,
                    deadline,
                ))
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            *active = Some(ActiveCardGameDeadline {
                token: previous.token.clone(),
                operation_id,
                session_generation: previous.session_generation,
                deadline,
            });
            return Ok(());
        }
        let Some(previous) = active.take() else {
            return Err(BusinessRuntimeError::TimerOperationFailed(
                "card deadline state inconsistent".to_owned(),
            ));
        };
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
    let Some(service) = turtle_soup else {
        return Err(BusinessRuntimeError::TimerOperationFailed(
            "turtle soup service state inconsistent".to_owned(),
        ));
    };
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
            // 同一 Token 只是改期：`reschedule` 原地替换，不留「active 为空」的飞行窗口。
            // 细节见 `sync_card_game_deadline` 中同构分支的注释。
            let operation_id = operation_ids
                .allocate()
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            timer
                .reschedule(DeadlineSchedule::new(
                    previous.token.clone(),
                    operation_id,
                    previous.session_generation,
                    deadline,
                ))
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            *active = Some(ActiveTurtleSoupDeadline {
                token: previous.token.clone(),
                operation_id,
                session_generation: previous.session_generation,
                deadline,
            });
            return Ok(());
        }
        let Some(previous) = active.take() else {
            return Err(BusinessRuntimeError::TimerOperationFailed(
                "turtle soup deadline state inconsistent".to_owned(),
            ));
        };
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
            // 同一 Token 只是改期：`reschedule` 原地替换，不留「active 为空」的飞行窗口。
            // 细节见 `sync_card_game_deadline` 中同构分支的注释。
            let operation_id = operation_ids
                .allocate()
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            timer
                .reschedule(DeadlineSchedule::new(
                    previous.token.clone(),
                    operation_id,
                    previous.session_generation,
                    deadline,
                ))
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            *active = Some(ActiveUndercoverDeadline {
                token: previous.token.clone(),
                operation_id,
                session_generation: previous.session_generation,
                deadline,
            });
            return Ok(());
        }
        let Some(previous) = active.take() else {
            return Err(BusinessRuntimeError::TimerOperationFailed(
                "undercover deadline state inconsistent".to_owned(),
            ));
        };
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

// 截止时间桥接会显式传递各模块的状态和关联通道，避免编排器误将一个模块的待处理效果共享给另一个模块。
#[allow(clippy::too_many_arguments)]
fn handle_business_timer(
    event: BusinessDeadlineEvent,
    entertainment: &mut EntertainmentState,
    idiom_chain: &mut IdiomChainService,
    card_games: &mut CardGameService,
    undercover: &mut UndercoverRuntimeService,
    mut turtle_soup: Option<&mut TurtleSoupService>,
    task_port: &mut BusinessTaskPort,
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
                        task_port,
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
        log::info!("成语接龙已到期结束，占用已释放，可以开始下一局玩法");
    }
    // 到期结算已经落地，同步失败不该再把整条计时事件判为失败。
    if let Err(error) = sync_idiom_deadline(
        idiom_chain,
        timer,
        active_idiom_deadline,
        operation_ids,
        generation,
    ) {
        report_deadline_sync_failure("成语接龙", &error);
    }
    Ok(())
}

fn run_business_runtime(receiver: Receiver<RuntimeMessage>, worker_config: BusinessRuntimeWorker) {
    let BusinessRuntimeWorker {
        idiom_chain,
        card_games,
        undercover,
        number_bomb,
        turtle_soup,
        hall,
        playback,
        invite,
        mut task_port,
        timer,
        state_sink,
        clock,
        #[cfg(test)]
        abort_all_probe,
    } = worker_config;
    let mut snapshot = BusinessRuntimeSnapshot::default();
    let mut entertainment = EntertainmentComponent::new(
        idiom_chain,
        card_games,
        undercover,
        number_bomb,
        turtle_soup,
        invite,
        state_sink.clone(),
        clock.as_ref(),
    );
    let mut chat_listener = ChatListenerRuntimeState::new(state_sink.clone());
    let mut decision = DecisionRuntimeState::new(state_sink.clone());
    let mut task_state = TaskRuntimeState::new(state_sink.clone());
    let mut hall = HallComponent::new(hall, state_sink.clone());
    let mut playback = PlaybackComponent::new(playback, state_sink.clone());
    chat_listener.publish();
    decision.publish();
    task_state.publish(clock.now());
    let mut should_stop = false;
    while !should_stop {
        let Ok(message) = receiver.recv() else {
            break;
        };
        // 单条消息的 panic 不能让整个业务运行时永久停摆（点歌/决策/队列/值守都依赖它）。
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match message {
            RuntimeMessage::OperationalSnapshot { now, response } => {
                task_state.handle_snapshot(now, response);
            }
            RuntimeMessage::SetCommandsEnabled { enabled, response } => {
                task_state.handle_set_commands_enabled(enabled, clock.now(), response);
            }
            RuntimeMessage::ConfigureIdleExit {
                timeout,
                now,
                response,
            } => task_state.handle_configure_idle_exit(timeout, now, response),
            RuntimeMessage::RecordCommandActivity { now, response } => {
                task_state.handle_record_command_activity(now, clock.now(), response);
            }
            RuntimeMessage::ClaimIdleExit { now, response } => {
                task_state.handle_claim_idle_exit(now, task_port.is_idle(), response);
            }
            RuntimeMessage::ClearIdleExit(response) => {
                task_state.handle_clear_idle_exit(clock.now(), response);
            }
            RuntimeMessage::ChatListener(message) => chat_listener.handle(message),
            RuntimeMessage::Decision(message) => decision.handle(message),
            RuntimeMessage::Event(event) => match event {
                BusinessEvent::Timer(timer_event) => {
                    snapshot.apply(BusinessEvent::Timer(timer_event.clone()));
                    if let Err(error) = entertainment.handle_timer(
                        timer_event,
                        &mut task_port,
                        timer.as_ref(),
                        clock.as_ref(),
                    ) {
                        log::error!(
                            "业务运行时处理计时事件失败（步骤：分发计时到期事件），该次定时动作未执行，玩法状态可能停在等待中：{error}"
                        );
                    }
                }
                BusinessEvent::TurtleSoupAiCompleted(completion) => {
                    if let Err(error) = entertainment.apply_turtle_soup_ai_completion(
                        completion,
                        &mut task_port,
                        timer.as_ref(),
                        clock.as_ref(),
                    ) {
                        log::error!(
                            "海龟汤 AI 裁决已应用，但同步期限失败（步骤：写回阶段截止时间），本局可能提前判为超时：{error}"
                        );
                    }
                }
                other => snapshot.apply(other),
            },
            RuntimeMessage::HandleIdiomChain {
                player,
                command,
                observed_at,
                response,
            } => {
                let result = entertainment.handle_idiom(
                    &player,
                    &command,
                    observed_at.unwrap_or_else(|| clock.now()),
                    timer.as_ref(),
                );
                let _ = response.send(result);
            }
            RuntimeMessage::ExplainIdiomChain {
                player,
                command,
                observed_at,
                response,
            } => {
                let result = entertainment.explain_idiom(
                    &player,
                    &command,
                    observed_at.unwrap_or_else(|| clock.now()),
                    timer.as_ref(),
                );
                let _ = response.send(result);
            }
            RuntimeMessage::AbortIdiomChain(response) => {
                let _ = response.send(entertainment.abort_idiom(timer.as_ref()));
            }
            RuntimeMessage::ExpireIdiomChain(response) => {
                let _ = response.send(entertainment.expire_idiom(clock.now(), timer.as_ref()));
            }
            RuntimeMessage::CardGame(message) => {
                if let Err(error) =
                    entertainment.handle_card_game(message, timer.as_ref(), clock.as_ref())
                {
                    log::error!(
                        "业务运行时处理牌局消息失败（步骤：分发牌局消息），该条消息对应的牌局动作未执行：{error}"
                    );
                }
            }
            RuntimeMessage::Undercover(message) => {
                if let Err(error) =
                    entertainment.handle_undercover(message, timer.as_ref(), clock.as_ref())
                {
                    log::error!(
                        "业务运行时处理谁是卧底消息失败（步骤：分发卧底消息），该条消息对应的动作未执行：{error}"
                    );
                }
            }
            RuntimeMessage::UndercoverSnapshot(response) => {
                let _ = response.send(Ok(entertainment.undercover_snapshot(clock.now())));
            }
            RuntimeMessage::NumberBomb(message) => {
                if let Err(error) = entertainment.handle_number_bomb(message) {
                    log::error!(
                        "业务运行时处理数字炸弹消息失败（步骤：分发数字炸弹命令或超时），该条消息对应的动作未执行：{error}"
                    );
                }
            }
            RuntimeMessage::TurtleSoup(message) => {
                if let Err(error) = entertainment.handle_turtle_soup(
                    message,
                    &mut task_port,
                    timer.as_ref(),
                    clock.as_ref(),
                ) {
                    log::error!(
                        "业务运行时处理海龟汤消息失败（步骤：分发海龟汤消息），该条消息对应的动作未执行：{error}"
                    );
                }
            }
            RuntimeMessage::RefreshTurtleSoup {
                now,
                clock_active,
                response,
            } => {
                let _ = response.send(entertainment.refresh_turtle_soup(
                    timer.as_ref(),
                    now,
                    clock_active,
                ));
            }
            RuntimeMessage::TurtleSoupSnapshot(response) => {
                let _ = response.send(entertainment.turtle_soup_snapshot());
            }
            RuntimeMessage::InviteShouldAccept { sequence, response } => {
                let _ = response.send(entertainment.invite_should_accept(sequence));
            }
            RuntimeMessage::BeginInvite { request, response } => {
                let _ = response.send(entertainment.begin_invite(request));
            }
            RuntimeMessage::AcquireModerationWorkflow {
                key,
                direct,
                response,
            } => {
                task_state.handle_acquire_moderation_workflow(key, direct, response);
            }
            RuntimeMessage::ReleaseModerationWorkflow {
                key,
                token,
                response,
            } => {
                task_state.handle_release_moderation_workflow(key, token, response);
            }
            #[cfg(test)]
            RuntimeMessage::ContainsModerationWorkflow { key, response } => {
                task_state.handle_contains_moderation_workflow(key, response);
            }
            RuntimeMessage::Hall(message) => hall.handle(message),
            RuntimeMessage::Playback(message) => playback.handle(message),
            RuntimeMessage::ActiveEntertainment(response) => {
                let _ = response.send(entertainment.active());
            }
            RuntimeMessage::Snapshot(response) => {
                let _ = response.send(snapshot);
            }
            RuntimeMessage::PrepareShutdown(response) => {
                snapshot.quiescing = true;
                abort_all_safely(&mut entertainment, timer.as_ref(), clock.as_ref());
                let _ = response.send(snapshot);
            }
            RuntimeMessage::Shutdown(response) => {
                // 停机决定必须**先**置位，再做任何可能 panic 的清理：
                // `abort_all` 的 panic 由 `catch_unwind` 吞掉后流程会继续往下走，
                // 若置位晚于它（或晚于 `response.send`），worker 会重新回到
                // `recv()` 继续收消息，`stop_worker` 只能超时脱管一个仍然活着的线程。
                should_stop = true;
                // 注入点在 `abort_all_safely` **之外**：它模拟的是「清理 panic
                // 逃出了本地 catch_unwind」这一形状（原始缺陷里正是外层
                // `catch_unwind` 吞掉 panic、连带跳过 `should_stop` 与回包）。
                // 放在 `abort_all_safely` 内部会被它的 catch 吞掉，测试就没法
                // 区分置位顺序了。
                #[cfg(test)]
                if let Some(probe) = abort_all_probe.as_deref() {
                    probe.take_for_abort_all();
                }
                abort_all_safely(&mut entertainment, timer.as_ref(), clock.as_ref());
                let _ = response.send(snapshot);
            }
        }));
        if let Err(payload) = result {
            log::error!(
                "业务运行时处理消息时崩溃（步骤：分发一条聊天消息），该条消息已跳过且不会重试，后续消息继续处理：{}",
                panic_detail(payload.as_ref())
            );
        }
        publish_safely(&entertainment, clock.as_ref());
    }
}

/// 停机清理：`abort_all` 的 panic 不能把停机流程一起带走。
fn abort_all_safely(
    entertainment: &mut EntertainmentComponent,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    clock: &dyn Clock,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        entertainment.abort_all(timer, clock);
    }));
    if let Err(payload) = result {
        log::error!(
            "业务运行时停机清理时崩溃（步骤：中止各玩法并注销定时器），停机流程继续，残留的玩法租约由下次启动的自检收拾：{}",
            panic_detail(payload.as_ref())
        );
    }
}

/// 广播停机后的业务状态：sink 实现里的 panic 同样不能带走 worker。
fn publish_safely(entertainment: &EntertainmentComponent, clock: &dyn Clock) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        entertainment.publish(clock)
    }));
    if let Err(payload) = result {
        log::error!(
            "业务运行时广播状态时崩溃（步骤：推送玩法快照），本拍广播已跳过：{}",
            panic_detail(payload.as_ref())
        );
    }
}

/// 只在回归测试里用的停机故障注入点。
///
/// 每个运行时各持一份，**不共用全局标记**：测试并行执行，用全局/线程局部标记
/// 都会被别的用例的关停流程提前消费掉，注入变成假绿。探针挂在会被 worker
/// 搬走的那份配置里，天然与「这一条运行时」一一对应。
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct AbortAllPanicProbe {
    /// 关停时在 `abort_all` 清理路径上抛一次 panic（模拟清理崩溃逃出本地捕获）。
    armed: std::sync::atomic::AtomicBool,
    /// 注入是否真的触发过。测试必须先断言它，否则「停机顺序正确」可能只是因为
    /// 注入根本没生效——那样测试是真空的。
    triggered: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl AbortAllPanicProbe {
    pub(crate) fn armed() -> Arc<Self> {
        let probe = Self::default();
        probe.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        Arc::new(probe)
    }

    fn take_for_abort_all(&self) {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.triggered
                .store(true, std::sync::atomic::Ordering::SeqCst);
            // `resume_unwind` 不触发 panic hook，测试输出不会被这层注入噪音淹没。
            std::panic::resume_unwind(Box::new("注入的 abort_all panic（回归测试）"));
        }
    }

    pub(crate) fn triggered(&self) -> bool {
        self.triggered.load(std::sync::atomic::Ordering::SeqCst)
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
    number_bomb: &mut NumberBombService,
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
        log::error!(
            "业务运行时关闭时无法中止牌局（步骤：结束牌局），牌局租约可能残留到下次启动：{error:#}"
        );
    }
    if let Err(error) = idiom_chain.abort(entertainment) {
        log::error!(
            "业务运行时关闭时无法中止成语接龙（步骤：结束接龙），接龙租约可能残留到下次启动：{error:#}"
        );
    }
    let _ = undercover.abort(entertainment);
    let _ = number_bomb.abort(entertainment);
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
        log::error!(
            "业务运行时关闭时无法撤销成语接龙期限（步骤：注销定时器），该定时器可能在新一轮启动时误触发：{error}"
        );
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
        log::error!(
            "业务运行时关闭时无法撤销牌局期限（步骤：注销定时器），该定时器可能在新一轮启动时误触发：{error}"
        );
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
        log::error!(
            "业务运行时关闭时无法撤销谁是卧底期限（步骤：注销定时器），该定时器可能在新一轮启动时误触发：{error}"
        );
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
        log::error!(
            "业务运行时关闭时无法撤销海龟汤期限（步骤：注销定时器），该定时器可能在新一轮启动时误触发：{error}"
        );
    }
    pending_card_game_outcomes.clear();
    pending_card_game_cancellations.clear();
    pending_undercover_outcomes.clear();
    pending_undercover_cancellations.clear();
    pending_turtle_soup_cancellations.clear();
}

/// 领域状态已提交后，定时器同步失败**不得**回滚、也不得丢弃领域结果。
///
/// 玩法状态机（牌局/卧底/海龟汤）一旦推进，`EntertainmentState` 的互斥槽就已经易主，
/// 而 `Suspended(CardGameEffectRequest)` 这类效果句柄也已经在 `result` 里产生了。
/// 如果这时把 `sync_*_deadline` 的 `Err` 用 `?`/`.and_then` 往上抛，整个 `result`
/// 会跟着被丢掉 —— 调用方永远等不到那一步效果，玩法自己也不会再释放娱乐槽，
/// 结果是玩法状态机与 `EntertainmentState` **双向永久死锁**。
///
/// 正确语义是降级：这一局可能没有可用的超时定时器（不会自动超时/自动推进），
/// 但领域结果必须原样返回给调用方。失败分支对 `active_*_deadline` 的处理分两种：
/// `cancel` 失败时把原记录回填（旧定时器仍按旧 Token 在跑）；而 Token 变化路径上
/// `cancel` 已成功、随后的 `schedule` 失败时，`active` 会留在 `None`（旧定时器已撤销、
/// 新定时器没排上，本局此刻没有超时定时器）。
///
/// 两种退化都不需要补偿队列：下一次 `sync_*` 调用（下一条命令、`Tick`/`Poll`、
/// 计时器完成回调、退出时的 `abort_all`）会按同一套对比逻辑重新对齐——`active`
/// 为空时会重新 `schedule`，非空且 Token 一致时走 `reschedule`。
fn report_deadline_sync_failure(module: &str, error: &BusinessRuntimeError) {
    log::error!(
        "{module}的领域状态已提交，但同步超时期限失败（步骤：写回阶段截止时间）；本次没有安排超时定时器，下一次同步会重新对齐：{error}"
    );
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
            player_display,
            command,
            now,
            response,
        } => {
            let result = card_games
                .begin_command_with_display(entertainment, &player, &player_display, &command, now)
                .map_err(card_game_operation_failed);
            // 领域状态已提交就不能再回滚：同步失败只降级为「本局没有超时定时器」。
            let result = result.map(|result| {
                if let Err(error) = sync_card_game_deadline(
                    card_games,
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    now,
                    *clock_active,
                ) {
                    report_deadline_sync_failure("牌局", &error);
                }
                result
            });
            let _ = response.send(result);
        }
        CardGameRuntimeMessage::Claim { key, response } => {
            let result = card_games.claim(key).map_err(card_game_operation_failed);
            let result = result.map(|result| {
                if let Err(error) = sync_card_game_deadline(
                    card_games,
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    *clock_active,
                ) {
                    report_deadline_sync_failure("牌局", &error);
                }
                result
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
                .map_err(card_game_operation_failed);
            // `Resume` 的返回值里就带着下一个待执行的效果句柄（例如 `Suspended` 大厅投递）。
            // 同步失败绝不能把句柄吞掉，否则调用方与娱乐槽一起停在此处。
            let response_result = response_result.map(|result| {
                if let Err(error) = sync_card_game_deadline(
                    card_games,
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    *clock_active,
                ) {
                    report_deadline_sync_failure("牌局", &error);
                }
                result
            });
            let _ = response.send(response_result);
        }
        CardGameRuntimeMessage::Cancel { key, response } => {
            let result = card_games
                .cancel(entertainment, key)
                .map_err(card_game_operation_failed);
            let result = result.map(|result| {
                if let Err(error) = sync_card_game_deadline(
                    card_games,
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    *clock_active,
                ) {
                    report_deadline_sync_failure("牌局", &error);
                }
                result
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
                    // 轮询本身**不提交新的领域状态**，所以同步失败可以照实上行；
                    // 但仍然只记一次错误并继续把已到期结果交出去，避免超时的
                    // 玩家在这一次轮询里什么都拿不到。
                    let timed_out = pending_outcomes.pop_front();
                    match sync_card_game_deadline(
                        card_games,
                        Some(timer),
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        now,
                        requested_clock_active,
                    ) {
                        Ok(()) => Ok(timed_out),
                        Err(error) => {
                            report_deadline_sync_failure("牌局", &error);
                            Ok(timed_out)
                        }
                    }
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
                .map_err(card_game_operation_failed);
            let result = result.map(|result| {
                if let Err(error) = sync_card_game_deadline(
                    card_games,
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    *clock_active,
                ) {
                    report_deadline_sync_failure("牌局", &error);
                }
                result
            });
            let _ = response.send(result);
        }
    }
    Ok(())
}

struct TurtleSoupHandlerContext<'a> {
    turtle_soup: Option<&'a mut TurtleSoupService>,
    entertainment: &'a mut EntertainmentState,
    task_port: &'a mut BusinessTaskPort,
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
        task_port,
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
                    immediate_reply: Some(crate::features::turtle_soup::TurtleSoupReplyText::raw(
                        "海龟汤运行时不可用",
                    )),
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
            player_display,
            command,
            response,
        } => {
            let outcome =
                service.handle_hall_command(entertainment, &player_display, &command, task_port);
            if let Err(error) = sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            ) {
                log::error!(
                    "海龟汤大厅命令已处理，但同步期限失败（步骤：写回阶段截止时间），本局可能提前判为超时：{error}"
                );
            }
            let _ = response.send(outcome);
        }
        TurtleSoupRuntimeMessage::FriendCommand {
            player_display,
            command,
            response,
        } => {
            let outcome =
                service.handle_friend_command(entertainment, &player_display, &command, task_port);
            if let Err(error) = sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            ) {
                log::error!(
                    "海龟汤好友命令已处理，但同步期限失败（步骤：写回阶段截止时间），本局可能提前判为超时：{error}"
                );
            }
            let _ = response.send(outcome);
        }
        TurtleSoupRuntimeMessage::StartRandom { response } => {
            let result = service
                .start_random_from_web(entertainment, task_port)
                .map_err(turtle_soup_operation_failed);
            // 开局已经提交（题目已占用娱乐槽）。同步失败只降级为「本局没有超时定时器」。
            let result = result.map(|()| {
                if let Err(error) = sync_turtle_soup_deadline(
                    Some(&*service),
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    clock_active,
                ) {
                    report_deadline_sync_failure("海龟汤", &error);
                }
            });
            let _ = response.send(result);
        }
        TurtleSoupRuntimeMessage::StartById { id, response } => {
            let result = service
                .start_by_id_from_web(entertainment, &id, task_port)
                .map_err(turtle_soup_operation_failed);
            let result = result.map(|()| {
                if let Err(error) = sync_turtle_soup_deadline(
                    Some(&*service),
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    clock_active,
                ) {
                    report_deadline_sync_failure("海龟汤", &error);
                }
            });
            let _ = response.send(result);
        }
        TurtleSoupRuntimeMessage::End { response } => {
            let result = service
                .end_from_web(entertainment, task_port)
                .map_err(turtle_soup_operation_failed);
            let result = result.map(|ended| {
                if let Err(error) = sync_turtle_soup_deadline(
                    Some(&*service),
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    clock_active,
                ) {
                    report_deadline_sync_failure("海龟汤", &error);
                }
                ended
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
        TurtleSoupRuntimeMessage::Submit {
            question,
            observed_at,
            response,
        } => {
            let result = service
                .submit_question_at(question, observed_at)
                .map_err(turtle_soup_operation_failed);
            let result = result.map(|outcome| {
                if let Err(error) = sync_turtle_soup_deadline(
                    Some(&*service),
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    clock_active,
                ) {
                    report_deadline_sync_failure("海龟汤", &error);
                }
                outcome
            });
            let _ = response.send(result);
        }
        TurtleSoupRuntimeMessage::Abort { reason } => {
            // 这条路径上没有调用方在等返回值，但仍不能把同步失败往上抛：`Err` 会冒泡到
            // `run_business_runtime`，让整条消息分发记一次「处理失败」，而中止本身已经生效。
            service.abort_for_context_loss(entertainment, &reason);
            if let Err(error) = sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            ) {
                report_deadline_sync_failure("海龟汤", &error);
            }
        }
        TurtleSoupRuntimeMessage::DeliveryCurrent { delivery, response } => {
            let _ = response.send(service.delivery_is_current(delivery));
        }
        TurtleSoupRuntimeMessage::DeliverySuccess { delivery } => {
            service.handle_delivery_success(entertainment, delivery);
            // 投递结果已经记进领域状态，同步失败不能再上行成「消息处理失败」。
            if let Err(error) = sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            ) {
                report_deadline_sync_failure("海龟汤", &error);
            }
        }
        TurtleSoupRuntimeMessage::DeliveryFailure { delivery, error } => {
            let error = anyhow::anyhow!(error);
            service.handle_delivery_failure(entertainment, delivery, &error);
            if let Err(error) = sync_turtle_soup_deadline(
                Some(&*service),
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                clock_active,
            ) {
                report_deadline_sync_failure("海龟汤", &error);
            }
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
            player_display,
            source,
            command,
            now,
            response,
        } => {
            let operation_id = operation_ids.allocate().map_err(|error| {
                BusinessRuntimeError::UndercoverOperationFailed(error.to_string())
            })?;
            let result = undercover
                .begin_command(
                    entertainment,
                    &player,
                    &player_display,
                    source,
                    &command,
                    now,
                    operation_id,
                )
                .map_err(undercover_operation_failed);
            // 领域状态已提交就不能再回滚：同步失败只降级为「本局没有超时定时器」。
            let result = result.map(|result| {
                if let Err(error) = sync_undercover_deadline(
                    undercover,
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    now,
                    *clock_active,
                ) {
                    report_deadline_sync_failure("谁是卧底", &error);
                }
                result
            });
            let _ = response.send(result);
        }
        UndercoverRuntimeMessage::Claim { key, response } => {
            let result = undercover.claim(key).map_err(undercover_operation_failed);
            let result = result.map(|result| {
                if let Err(error) = sync_undercover_deadline(
                    undercover,
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    *clock_active,
                ) {
                    report_deadline_sync_failure("谁是卧底", &error);
                }
                result
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
                .map_err(undercover_operation_failed);
            let result = result.map(|result| {
                if let Err(error) = sync_undercover_deadline(
                    undercover,
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    *clock_active,
                ) {
                    report_deadline_sync_failure("谁是卧底", &error);
                }
                result
            });
            let _ = response.send(result);
        }
        UndercoverRuntimeMessage::Cancel { key, response } => {
            let result = undercover
                .cancel(entertainment, key)
                .map_err(undercover_operation_failed);
            let result = result.map(|result| {
                if let Err(error) = sync_undercover_deadline(
                    undercover,
                    timer,
                    active_deadline,
                    pending_cancellations,
                    operation_ids,
                    clock.now(),
                    *clock_active,
                ) {
                    report_deadline_sync_failure("谁是卧底", &error);
                }
                result
            });
            let _ = response.send(result);
        }
        UndercoverRuntimeMessage::Poll {
            now,
            clock_active: requested_clock_active,
            response,
        } => {
            *clock_active = requested_clock_active;
            let timed_out = pending_outcomes.pop_front();
            if let Err(error) = sync_undercover_deadline(
                undercover,
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                now,
                requested_clock_active,
            ) {
                report_deadline_sync_failure("谁是卧底", &error);
            }
            let _ = response.send(Ok(timed_out));
        }
        UndercoverRuntimeMessage::Abort(response) => {
            let aborted = undercover.abort(entertainment);
            if let Err(error) = sync_undercover_deadline(
                undercover,
                timer,
                active_deadline,
                pending_cancellations,
                operation_ids,
                clock.now(),
                *clock_active,
            ) {
                report_deadline_sync_failure("谁是卧底", &error);
            }
            let _ = response.send(Ok(aborted));
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

/// 封装操作控制状态与审核工作流占用表。
struct TaskRuntimeState {
    operational: OperationalState,
    moderation_workflows: ModerationWorkflowRegistry,
    state_sink: Option<Arc<dyn BusinessStateSink>>,
}

impl TaskRuntimeState {
    fn new(state_sink: Option<Arc<dyn BusinessStateSink>>) -> Self {
        Self {
            operational: OperationalState::new(),
            moderation_workflows: ModerationWorkflowRegistry::default(),
            state_sink,
        }
    }

    fn publish(&self, now: Instant) {
        if let Some(state_sink) = &self.state_sink {
            state_sink.publish_operational(self.operational.snapshot(now));
        }
    }

    fn handle_snapshot(
        &self,
        now: Instant,
        response: SyncSender<Result<BusinessOperationalSnapshot, BusinessRuntimeError>>,
    ) {
        let _ = response.send(Ok(self.operational.snapshot(now)));
    }

    fn handle_set_commands_enabled(
        &mut self,
        enabled: bool,
        now: Instant,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    ) {
        self.operational.commands_enabled = enabled;
        self.publish(now);
        let _ = response.send(Ok(()));
    }

    fn handle_configure_idle_exit(
        &mut self,
        timeout: Duration,
        now: Instant,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    ) {
        self.operational.configure_idle_exit(timeout, now);
        self.publish(now);
        let _ = response.send(Ok(()));
    }

    fn handle_record_command_activity(
        &mut self,
        observed_at: Instant,
        published_at: Instant,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    ) {
        self.operational.record_command_activity(observed_at);
        self.publish(published_at);
        let _ = response.send(Ok(()));
    }

    fn handle_claim_idle_exit(
        &mut self,
        now: Instant,
        scheduler_idle: Result<bool, TaskEngineError>,
        response: SyncSender<Result<Option<Duration>, BusinessRuntimeError>>,
    ) {
        let claimed = scheduler_idle
            .map(|scheduler_idle| self.operational.claim_idle_exit(now, scheduler_idle))
            .map_err(|error| BusinessRuntimeError::TaskEngineOperationFailed(error.to_string()));
        self.publish(now);
        let _ = response.send(claimed);
    }

    fn handle_clear_idle_exit(
        &mut self,
        now: Instant,
        response: SyncSender<Result<(), BusinessRuntimeError>>,
    ) {
        self.operational.idle_exit = None;
        self.publish(now);
        let _ = response.send(Ok(()));
    }

    fn handle_acquire_moderation_workflow(
        &mut self,
        key: ModerationWorkflowKey,
        direct: bool,
        response: SyncSender<Option<Arc<ModerationWorkflowToken>>>,
    ) {
        let acquired = self.moderation_workflows.acquire(key.clone(), direct);
        if let Err(error) = response.send(acquired)
            && let Some(token) = error.0
        {
            self.moderation_workflows.release(&key, &token);
        }
    }

    fn handle_release_moderation_workflow(
        &mut self,
        key: ModerationWorkflowKey,
        token: Arc<ModerationWorkflowToken>,
        response: SyncSender<bool>,
    ) {
        let _ = response.send(self.moderation_workflows.release(&key, &token));
    }

    #[cfg(test)]
    fn handle_contains_moderation_workflow(
        &self,
        key: ModerationWorkflowKey,
        response: SyncSender<bool>,
    ) {
        let _ = response.send(self.moderation_workflows.contains(&key));
    }
}

struct ChatListenerRuntimeState {
    state: ChatListenerState,
    state_sink: Option<Arc<dyn BusinessStateSink>>,
}

impl ChatListenerRuntimeState {
    fn new(state_sink: Option<Arc<dyn BusinessStateSink>>) -> Self {
        Self {
            state: ChatListenerState::new(),
            state_sink,
        }
    }

    fn publish(&self) {
        if let Some(state_sink) = &self.state_sink {
            state_sink.publish_chat_listener(self.state.snapshot());
        }
    }

    fn handle(&mut self, message: ChatListenerRuntimeMessage) {
        let mutated = match message {
            ChatListenerRuntimeMessage::Snapshot(response) => {
                let _ = response.send(Ok(self.state.snapshot()));
                false
            }
            ChatListenerRuntimeMessage::RequestMode { target, response } => {
                let changed = self.state.request_mode(target);
                let snapshot = self.state.snapshot();
                let _ = response.send(Ok((changed, snapshot)));
                changed
            }
            ChatListenerRuntimeMessage::CompleteMode { mode, response } => {
                self.state.complete_mode_switch(mode);
                let _ = response.send(Ok(()));
                true
            }
            ChatListenerRuntimeMessage::CancelModeRequest { target, response } => {
                self.state.cancel_mode_request(target);
                let _ = response.send(Ok(()));
                true
            }
            ChatListenerRuntimeMessage::FailModeSwitchToPrimary(response) => {
                self.state.fail_mode_switch_to_primary();
                let _ = response.send(Ok(()));
                true
            }
            ChatListenerRuntimeMessage::BeginTemporaryPrimary(response) => {
                self.state.begin_temporary_primary();
                let _ = response.send(Ok(()));
                true
            }
            ChatListenerRuntimeMessage::EndTemporaryPrimary(response) => {
                self.state.end_temporary_primary();
                let _ = response.send(Ok(()));
                true
            }
            ChatListenerRuntimeMessage::ClaimUnreadTask(response) => {
                let claimed = self.state.claim_unread_task();
                let _ = response.send(Ok(claimed));
                claimed
            }
            ChatListenerRuntimeMessage::FinishUnreadTask {
                processed_message,
                response,
            } => {
                self.state.finish_unread_task(processed_message);
                let _ = response.send(Ok(()));
                true
            }
            ChatListenerRuntimeMessage::ReleaseUnreadTask(response) => {
                self.state.release_unread_task();
                let _ = response.send(Ok(()));
                true
            }
            ChatListenerRuntimeMessage::FinishInitialUnreadClear(response) => {
                self.state.finish_initial_unread_clear();
                let _ = response.send(Ok(()));
                true
            }
            ChatListenerRuntimeMessage::FinishHallRound(response) => {
                self.state.finish_hall_round();
                let _ = response.send(Ok(()));
                true
            }
        };
        if mutated {
            self.publish();
        }
    }
}

struct DecisionRuntimeState {
    state: DecisionState,
    state_sink: Option<Arc<dyn BusinessStateSink>>,
    /// 上一次真正广播出去的快照。
    ///
    /// `handle` 每条消息末尾都会尝试发布，但**只有待裁决状态真的变了才值得广播**：
    /// 提交一次决策、被已提交规则拒绝一次、`Snapshot` 查询……都不改变「有没有待裁决项」，
    /// 无条件下发会把同一个 `decision.pending` 反复推给事件流与 /monitor，
    /// 外部控制器要为此重复决策。这里保存最后广播值做等值比较。
    last_published: Option<DecisionSnapshot>,
}

impl DecisionRuntimeState {
    fn new(state_sink: Option<Arc<dyn BusinessStateSink>>) -> Self {
        Self {
            state: DecisionState::new(),
            state_sink,
            last_published: None,
        }
    }

    /// 只在待裁决状态真的变化时广播。
    ///
    /// 首次调用也走这里：`last_published` 与「当前无待裁决项」等价，因此启动时不会凭空
    /// 发一条 `decision.cleared`。`state_sink` 缺席时仍然推进 `last_published`，
    /// 语义保持「最近一次广播值」不变，便于日后接入 sink 时行为一致。
    fn publish(&mut self) {
        let snapshot = self.state.snapshot();
        if snapshot == self.last_published {
            return;
        }
        if let Some(state_sink) = &self.state_sink {
            state_sink.publish_decision(snapshot.clone());
        }
        self.last_published = snapshot;
    }

    fn handle(&mut self, message: DecisionRuntimeMessage) {
        match message {
            DecisionRuntimeMessage::Begin {
                label,
                allow_switch_source,
                allow_ai,
                timeout,
                delivery,
                response,
            } => {
                let result = self
                    .state
                    .begin(label, allow_switch_source, allow_ai, timeout, delivery)
                    .map_err(BusinessRuntimeError::DecisionOperationFailed);
                let _ = response.send(result);
            }
            #[cfg(test)]
            DecisionRuntimeMessage::Snapshot(response) => {
                let _ = response.send(Ok(self.state.snapshot()));
            }
            DecisionRuntimeMessage::Submit {
                id,
                action,
                response,
            } => {
                let result = self
                    .state
                    .submit(id, action)
                    .map_err(BusinessRuntimeError::DecisionOperationFailed);
                let _ = response.send(result);
            }
            DecisionRuntimeMessage::Finish { id, response } => {
                self.state.finish(id);
                let _ = response.send(Ok(()));
            }
        }
        self.publish();
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;
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
    use crate::runtime::deadline::{BusinessDeadlineEvent, BusinessDeadlineToken};
    use crate::runtime::scheduler::{
        FormalTaskCancelOutcome, FormalTaskCancellationToken, FormalTaskDedupKey,
        FormalTaskEnqueueOutcome, FormalTaskExecutionOutcome, FormalTaskSubmission, FormalTaskWork,
    };
    use miliastra_kernel::clock::ManualClock;
    use miliastra_kernel::identity::{BusinessOperationId, SessionGeneration};
    use miliastra_kernel::timer::{DeadlineSchedule, TimerCore, TimerRuntime, TimerRuntimeEvent};

    struct TestFormalWork;

    impl FormalTaskWork for TestFormalWork {
        fn execute(
            self: Box<Self>,
            _cancellation: FormalTaskCancellationToken,
        ) -> FormalTaskExecutionOutcome {
            FormalTaskExecutionOutcome::Completed(Ok("done".to_string()))
        }

        fn cancel(self: Box<Self>) {}
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

    fn runtime_with_task_engine(queue_capacity: usize) -> (BusinessRuntime, TaskEngineHandle) {
        let task_engine = TaskEngineHandle::new(None);
        let runtime = BusinessRuntime::start_with_task_engine(
            queue_capacity,
            idiom_service(Some(Duration::from_secs(300))),
            CardGameService::new(LandlordConfig::default()),
            task_engine.clone(),
        )
        .unwrap();
        (runtime, task_engine)
    }

    #[test]
    fn business_runtime_owns_command_availability_and_idle_exit_claiming() {
        let (runtime, task_engine) = runtime_with_task_engine(8);
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
        handle
            .record_command_activity(started_at + Duration::from_secs(20))
            .unwrap();
        assert_eq!(
            handle
                .operational_snapshot(started_at + Duration::from_secs(40))
                .unwrap()
                .idle_exit_remaining_seconds(),
            Some(110)
        );

        let queued = task_engine
            .enqueue_formal(formal_submission("busy", Some("busy")))
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
            task_engine.cancel_formal(receipt.task_id).unwrap(),
            FormalTaskCancelOutcome::CanceledBeforeStart
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
    fn business_runtime_owns_chat_listener_mode_holds_and_unread_claims() {
        let runtime = runtime(16);
        let handle = runtime.handle();

        assert!(
            handle
                .request_chat_listener_mode(ChatListenerMode::Secondary)
                .unwrap()
                .0
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
            crate::test_support::test_state_store(),
        )
        .unwrap();
        crate::features::playback::PlaybackService::new(
            queue,
            playback_state,
            history,
            std::sync::Arc::new(std::sync::RwLock::new(
                crate::features::playback::SongDedupConfig::default(),
            )),
            0,
        )
    }

    fn hall_service(path: std::path::PathBuf) -> HallStateService {
        let clock = Arc::new(SystemClock);
        HallStateService::new_with_time(
            crate::features::hall::PersistentHallState::load(
                path,
                crate::test_support::test_state_store(),
            )
            .expect("hall state"),
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
            CardGameEffect::FriendVerify { ref player_display, .. } if player_display == "甲"
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

    /// 构造一份只有默认模块的 worker 配置，供停机回归用例共享。
    fn plain_worker() -> BusinessRuntimeWorker {
        BusinessRuntimeWorker {
            idiom_chain: idiom_service(Some(Duration::from_secs(300))),
            card_games: CardGameService::new(LandlordConfig::default()),
            undercover: default_undercover_service(),
            number_bomb: NumberBombService::new(NumberBombConfig::default()),
            turtle_soup: None,
            hall: None,
            playback: None,
            invite: InviteService::new(),
            task_port: TaskEngineHandle::new(None).business_port(),
            timer: None,
            state_sink: None,
            clock: Arc::new(SystemClock),
            abort_all_probe: None,
        }
    }

    /// B13-P1-1（回归）：`should_stop` 必须在 `abort_all` **之前**置位。
    ///
    /// 旧实现是 `abort_all(...)` → `response.send(...)` → `should_stop = true`，
    /// 而整段在 `catch_unwind` 里：`abort_all` panic 被吞掉后 `should_stop` 永远
    /// 不会被置位，worker 回到 `recv()` 继续收消息，`stop_worker` 只能超时脱管
    /// 一个仍然活着的线程（退出流程与配置重载一起挂住）。
    ///
    /// 注入点在 `abort_all` 之前抛 panic，所以「停机正常收敛 + 注入确实触发」
    /// 同时成立，才说明置位发生在清理之前。`triggered` 断言防真空：没有它，
    /// 注入没生效时这条测试会假绿。
    #[test]
    fn shutdown_keeps_stopping_even_when_abort_all_panics() {
        let worker = plain_worker().with_abort_all_panic_injection();
        let probe = worker
            .abort_all_probe()
            .expect("注入探针必须随 worker 一起创建");
        let runtime =
            BusinessRuntime::start_with_timer_and_modules_and_state_sink(4, worker).unwrap();

        let started_at = Instant::now();
        let result = runtime.shutdown();
        let elapsed = started_at.elapsed();

        assert!(
            probe.triggered(),
            "注入必须真的触发，否则这条测试覆盖不到停机顺序"
        );
        // panic 发生在回包之前，所以拿不到快照（`RuntimeStopped`）是预期结果；
        // 真正的判据是 **worker 自己退出了**：旧的置位顺序下 `should_stop` 漏置，
        // worker 回到 `recv()` 继续等消息，关停只能落在超时分支。
        assert_eq!(
            result,
            Err(BusinessRuntimeError::RuntimeStopped),
            "清理 panic 之后必须按正常路径收敛，不能落到超时脱管"
        );
        assert!(
            elapsed < WORKER_SHUTDOWN_TIMEOUT,
            "停机必须走正常收敛路径，不能靠超时脱管兜底：{elapsed:?}"
        );
    }

    /// B13-P1-1（回归）：业务线程卡住时，关停必须在有界时间内返回。
    ///
    /// 用真的卡住的 worker，而不是 mock：`start_internal` 起线程后，worker 先跑
    /// 启动广播（`chat_listener.publish()` → 我们的 sink 在这里永久阻塞），
    /// 于是它一条消息都不取，关停指令只能在队列里等到超时——正是
    /// `concurrency.md` P2-12 的场景（演员阻塞在端口调用里）。
    ///
    /// 断言：在约 3 秒内返回 `WorkerShutdownTimedOut`，而不是挂死到测试超时。
    #[test]
    fn shutdown_gives_up_on_a_wedged_worker_within_the_deadline() {
        struct WedgingSink {
            entered: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }

        impl BusinessStateSink for WedgingSink {
            fn publish_turtle_soup(&self, _snapshot: TurtleSoupSnapshot) {}
            fn publish_undercover(&self, _snapshot: UndercoverSnapshot) {}
            fn publish_chat_listener(&self, _snapshot: ChatListenerSnapshot) {
                // 只在第一次广播时卡住；关停路径自己也会广播，卡那里会把
                // 「超时脱管」伪装成「正常慢」。
                let _ = self.entered.try_send(());
                let _ = self
                    .release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .recv_timeout(Duration::from_secs(30));
            }
        }

        let (entered_sender, entered) = mpsc::sync_channel(1);
        let (release_sender, release) = mpsc::sync_channel(1);
        let sink = Arc::new(WedgingSink {
            entered: entered_sender,
            release: Mutex::new(release),
        });

        let mut worker = plain_worker();
        worker.state_sink = Some(sink);
        let runtime =
            BusinessRuntime::start_with_timer_and_modules_and_state_sink(4, worker).unwrap();
        // 等到 worker 真的进到广播里（此后它不会再取任何消息）。
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("worker 必须在启动广播处卡住");

        let started_at = Instant::now();
        let result = runtime.shutdown();
        let elapsed = started_at.elapsed();

        // 先放行卡住的线程，避免它在测试结束后仍占着运行时资源。
        let _ = release_sender.send(());
        assert_eq!(
            result,
            Err(BusinessRuntimeError::WorkerShutdownTimedOut),
            "卡死时必须以超时收场（而不是 panic 或无限阻塞）"
        );
        assert!(
            elapsed < WORKER_SHUTDOWN_TIMEOUT + Duration::from_secs(2),
            "关停必须在期限附近返回，实际 {elapsed:?}"
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
    fn queued_idiom_input_uses_observation_time_instead_of_execution_time() {
        let observed_at = Instant::now();
        let clock = Arc::new(ManualClock::new(observed_at));
        clock.advance(Duration::from_secs(31)).unwrap();
        let runtime = BusinessRuntime::start_with_clock(
            2,
            idiom_service(Some(Duration::from_secs(30))),
            CardGameService::new(LandlordConfig::default()),
            clock,
        )
        .unwrap();
        let handle = runtime.handle();

        handle
            .handle_idiom_chain_at(
                "Alice",
                &IdiomChainCommand::Start {
                    idiom: "画蛇添足".to_string(),
                    mode: IdiomChainMode::Exact,
                },
                observed_at,
            )
            .unwrap();

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
        let mut task_port = TaskEngineHandle::new(None).business_port();

        handle_business_timer(
            event,
            &mut entertainment,
            &mut idiom_chain,
            &mut card_games,
            &mut undercover,
            None,
            &mut task_port,
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

    /// 在可用定时器上跑一次「开局 → 验证通过 → 大厅成立」，返回建好的大厅服务。
    ///
    /// 抽出来是因为 `next_deadline` 只有在 `GameState::Lobby`/`Playing` 时才返回
    /// `Some`，而这两条用例都必须先到这个状态，否则 `sync_card_game_deadline`
    /// 会走「没有待调度期限」的早退分支，压根不碰定时器。
    fn card_game_in_lobby(
        now: Instant,
        extra_players: &[&str],
    ) -> (
        TimerRuntime<BusinessDeadlineToken>,
        CardGameService,
        EntertainmentState,
        Option<ActiveCardGameDeadline>,
    ) {
        let (event_sender, _event_receiver) = mpsc::sync_channel(4);
        let timer_runtime = TimerRuntime::start(4, event_sender).unwrap();
        let timer = timer_runtime.handle();
        let mut card_games = CardGameService::new(LandlordConfig::default());
        let mut entertainment = EntertainmentState::new();
        let mut active = None;
        let mut pending = Vec::new();
        let mut pending_outcomes = std::collections::VecDeque::new();
        let operation_ids = BusinessOperationIdAllocator::new();

        let (response, answer) = mpsc::sync_channel(1);
        handle_card_game_message(
            &mut card_games,
            &mut entertainment,
            CardGameRuntimeMessage::Begin {
                player: "甲".to_string(),
                player_display: "甲".to_string(),
                command: LandlordCommand::Start,
                now,
                response,
            },
            Some(&timer),
            &mut active,
            &mut pending,
            &operation_ids,
            &mut pending_outcomes,
            &mut true,
            &SystemClock,
        )
        .unwrap();
        let verification = match answer.recv().unwrap().unwrap() {
            CardGameCommandStart::Suspended(request) => request,
            CardGameCommandStart::Completed(_) => panic!("start should require verification"),
        };
        assert_eq!(
            card_games.claim(verification.key).unwrap(),
            CardGameEffectClaim::Claimed
        );
        let (response, answer) = mpsc::sync_channel(1);
        handle_card_game_message(
            &mut card_games,
            &mut entertainment,
            CardGameRuntimeMessage::Resume {
                key: verification.key,
                result: CardGameEffectResult::FriendVerify(Ok(true)),
                response,
            },
            Some(&timer),
            &mut active,
            &mut pending,
            &operation_ids,
            &mut pending_outcomes,
            &mut true,
            &SystemClock,
        )
        .unwrap();
        // 大厅成立后会挂一个「大厅到期」效果，领掉它才算走完这一步。
        if let Ok(CardGameResume::Suspended(hall)) = answer.recv().unwrap() {
            assert_eq!(
                card_games.claim(hall.key).unwrap(),
                CardGameEffectClaim::Claimed
            );
            let (response, answer) = mpsc::sync_channel(1);
            handle_card_game_message(
                &mut card_games,
                &mut entertainment,
                CardGameRuntimeMessage::Resume {
                    key: hall.key,
                    result: CardGameEffectResult::HallDelivery(Ok(())),
                    response,
                },
                Some(&timer),
                &mut active,
                &mut pending,
                &operation_ids,
                &mut pending_outcomes,
                &mut true,
                &SystemClock,
            )
            .unwrap();
            answer.recv().unwrap().unwrap();
        }

        for index in 0..extra_players.len() {
            let player = extra_players[index];
            let (response, answer) = mpsc::sync_channel(1);
            handle_card_game_message(
                &mut card_games,
                &mut entertainment,
                CardGameRuntimeMessage::Begin {
                    player: player.to_string(),
                    player_display: player.to_string(),
                    command: LandlordCommand::Join,
                    now: now + Duration::from_secs(index as u64 + 1),
                    response,
                },
                Some(&timer),
                &mut active,
                &mut pending,
                &operation_ids,
                &mut pending_outcomes,
                &mut true,
                &SystemClock,
            )
            .unwrap();
            let joined = match answer.recv().unwrap().unwrap() {
                CardGameCommandStart::Suspended(request) => request,
                CardGameCommandStart::Completed(_) => {
                    panic!("join should suspend its friend verification")
                }
            };
            assert_eq!(
                card_games.claim(joined.key).unwrap(),
                CardGameEffectClaim::Claimed
            );
            // 报名挂起的是好友验证（`VerifyJoin`），不是大厅投递。
            let (response, answer) = mpsc::sync_channel(1);
            handle_card_game_message(
                &mut card_games,
                &mut entertainment,
                CardGameRuntimeMessage::Resume {
                    key: joined.key,
                    result: CardGameEffectResult::FriendVerify(Ok(true)),
                    response,
                },
                Some(&timer),
                &mut active,
                &mut pending,
                &operation_ids,
                &mut pending_outcomes,
                &mut true,
                &SystemClock,
            )
            .unwrap();
            answer.recv().unwrap().unwrap();
        }

        assert!(
            card_games.next_deadline(now, true).is_some(),
            "前置条件：牌局必须真的进入大厅，否则 next_deadline 仍是 None"
        );
        (timer_runtime, card_games, entertainment, active)
    }

    /// B13-P0-2(v1)：定时器同步失败**不得**吞掉已经提交的领域结果。
    ///
    /// 用已经关停的 `TimerRuntime` 当注入式失败：命令通道关闭后
    /// `sync_card_game_deadline` 必定返回 `Err`。修复前 `Begin` 走
    /// `.and_then(|result| { sync(...)?; Ok(result) })`，这个 `Err` 会把
    /// `Suspended(CardGameEffectRequest)` 一起丢掉，调用方只收到
    /// `TimerOperationFailed` —— 而玩法状态与娱乐互斥槽此时**已经提交**，
    /// 那张效果句柄再也拿不回来，整局游戏与娱乐槽双死锁。
    ///
    /// # 为什么要先建大厅、并且要「第三个人」报名（旧版用例的真空之处）
    ///
    /// `sync_card_game_deadline` 只有在 `next_deadline` 返回 `Some(..)` **且**
    /// 与 `active` 里的记录不一致时才会真的碰定时器：
    ///
    /// - 单发一条 `LandlordCommand::Start` **不创建任何牌局状态**，它只挂起
    ///   「好友验证」效果并占住娱乐槽，`GameState` 仍是 `Idle` →
    ///   `next_deadline == None` → `sync` 走「没有待调度期限 → `return Ok(())`」
    ///   早退分支，`Err` 根本不会发生。旧用例就死在这里，回退成 `.and_then`
    ///   照样全绿。
    /// - 到了 `Lobby` 之后，第 2 个人报名走的是同一种「大厅到期」期限
    ///   （`LobbyExpiry` + 同一个 `deadline_after(now, elapsed, timeout)`），
    ///   与 `active` 完全相等 → `sync` 走「同一 Token、同代次、同期限 → 直接
    ///   `return Ok(())`」的等值早退分支，仍然碰不到定时器。
    /// - 只有第 3 个人报名会**发牌**把状态推到 `Playing`：期限种类从
    ///   `LobbyExpiry` 换成出价/出牌期限，绝对时刻也跟着变，于是 `sync` 真的
    ///   要 `reschedule`/`schedule` —— 这才是唯一能拿到 `Err` 的形状。
    #[test]
    fn card_game_begin_returns_the_suspended_handle_even_when_deadline_sync_fails() {
        let now = Instant::now();
        // 甲开局 + 乙报名 → 大厅（2/3），此时期限仍是 LobbyExpiry。
        let (timer_runtime, mut card_games, mut entertainment, active) =
            card_game_in_lobby(now, &["乙"]);
        let timer = timer_runtime.handle();
        let lobby_deadline = active.expect("大厅必须已经排上到期定时器");
        let mut active = Some(lobby_deadline.clone());
        let mut pending = Vec::new();
        let mut pending_outcomes = std::collections::VecDeque::new();
        let operation_ids = BusinessOperationIdAllocator::new();

        // 关停计时器：此后所有 schedule/reschedule/cancel 都返回 RuntimeStopped。
        timer_runtime.shutdown().unwrap();

        // 丙报名：第 3 个人触发发牌，状态推进到 `Playing`，期限必然变化 →
        // `sync_card_game_deadline` 一定会去改定时器，必然失败。
        let (response, answer) = mpsc::sync_channel(1);
        handle_card_game_message(
            &mut card_games,
            &mut entertainment,
            CardGameRuntimeMessage::Begin {
                player: "丙".to_string(),
                player_display: "丙".to_string(),
                command: LandlordCommand::Join,
                now: now + Duration::from_secs(10),
                response,
            },
            Some(&timer),
            &mut active,
            &mut pending,
            &operation_ids,
            &mut pending_outcomes,
            &mut true,
            &SystemClock,
        )
        .unwrap();

        let result = answer
            .recv()
            .unwrap()
            .expect("已提交的领域结果不能因定时器同步失败而丢失");
        let request = match result {
            CardGameCommandStart::Suspended(request) => request,
            CardGameCommandStart::Completed(_) => panic!("join should suspend its reply"),
        };

        // 报名被领域状态接受：拿到的是「丙」的报名验证句柄，而不是
        // 「已有牌局进行中」那类大厅回执——后者说明 `Begin` 根本没提交状态。
        assert!(
            matches!(
                &request.effect,
                CardGameEffect::FriendVerify { player_display, .. } if player_display == "丙"
            ),
            "报名必须已经被领域状态接受，实际效果：{:?}",
            request.effect
        );
        // 领域状态与娱乐槽都已提交，句柄有效。
        assert_eq!(entertainment.active(), Some(EntertainmentKind::Landlord));
        assert_eq!(
            card_games.claim(request.key).unwrap(),
            CardGameEffectClaim::Claimed
        );
        // 期限同步失败后降级：旧的 `active` 记录原地保留，也没有挂上待取消项
        // （下一次 sync 会按同一套对比逻辑重新对齐）。
        assert_eq!(active, Some(lobby_deadline));
        assert!(pending.is_empty());
    }

    /// B13-P1-5：只有待裁决状态**真的变化**时才广播，避免重复 `decision.pending`。
    #[test]
    fn decision_runtime_publishes_only_on_real_state_changes() {
        use std::sync::Mutex;

        struct RecordingSink {
            decisions: Mutex<Vec<Option<DecisionSnapshot>>>,
        }

        impl BusinessStateSink for RecordingSink {
            fn publish_turtle_soup(&self, _snapshot: TurtleSoupSnapshot) {}
            fn publish_undercover(&self, _snapshot: UndercoverSnapshot) {}
            fn publish_decision(&self, snapshot: Option<DecisionSnapshot>) {
                self.decisions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(snapshot);
            }
        }

        let sink = Arc::new(RecordingSink {
            decisions: Mutex::new(Vec::new()),
        });
        let mut runtime = DecisionRuntimeState::new(Some(sink.clone()));

        // 启动时的首次发布：当前没有待裁决项，不该凭空发一条 cleared。
        runtime.publish();
        assert!(
            sink.decisions.lock().unwrap().is_empty(),
            "启动时没有待裁决项，不该广播"
        );

        let (delivery, receiver) = mpsc::sync_channel(1);
        let (response, answer) = mpsc::sync_channel(1);
        runtime.handle(DecisionRuntimeMessage::Begin {
            label: "候选确认".to_string(),
            allow_switch_source: true,
            allow_ai: true,
            timeout: Duration::from_secs(30),
            delivery,
            response,
        });
        answer.recv().unwrap().unwrap();
        let pending = {
            let published = sink.decisions.lock().unwrap();
            assert_eq!(published.len(), 1, "出现待裁决项必须广播一次");
            published[0].clone().expect("这条应当是 pending 快照")
        };

        // 同样的状态再发布一次：不得重复广播。
        runtime.publish();
        assert_eq!(
            sink.decisions.lock().unwrap().len(),
            1,
            "状态未变化时不得重复广播"
        );

        let (response, answer) = mpsc::sync_channel(1);
        runtime.handle(DecisionRuntimeMessage::Submit {
            id: pending.id,
            action: DecisionAction::Confirm,
            response,
        });
        answer.recv().unwrap().unwrap();
        assert_eq!(
            receiver.try_recv().unwrap(),
            DecisionAction::Confirm,
            "提交必须送到等待方"
        );

        // 提交之后 `snapshot()` 过滤已提交记录 → 一条 cleared，且仅此一条。
        assert!(
            runtime.state.snapshot().is_none(),
            "已提交的记录不再算待裁决项"
        );
        runtime.publish();
        runtime.publish();
        let published = sink.decisions.lock().unwrap().clone();
        assert_eq!(
            published,
            vec![Some(pending), None],
            "应当只有 pending → cleared 两次广播"
        );
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
        let hall_state_path = std::env::temp_dir().join(format!("mwm-business-hall-{suffix}.json"));
        let queue = crate::features::playback::PersistentQueue::new_for_test(4).unwrap();
        let playback_state =
            crate::features::playback::PersistentPlaybackState::new_for_test().unwrap();
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
                track: Some(crate::features::playback::test_track(
                    "miliastra://track/qqmusic/1",
                    "晴天 - 周杰伦",
                )),
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let suffix = now.as_nanos();
        let updated_at = now.as_secs();
        let directory = std::env::temp_dir().join(format!("mwm-business-state-{suffix}"));
        let queue = crate::features::playback::PersistentQueue::new_for_test(4).unwrap();
        let playback_state =
            crate::features::playback::PersistentPlaybackState::new_for_test().unwrap();
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
                remaining_updated_at: Some(Some(updated_at)),
                expiring_warning_sent: Some(true),
            })
            .unwrap();
        let snapshot = handle.hall_state_snapshot().unwrap();

        assert_eq!(snapshot.remaining_minutes, Some(42));
        assert_eq!(snapshot.remaining_updated_at, Some(updated_at));
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
                        .begin_invite(InviteRequest::new(username, username, Some(9), None))
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
