use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::features::card_games::{
    CardGameCancel, CardGameCommandStart, CardGameDeadlineKind, CardGameDeadlineToken,
    CardGameEffectClaim, CardGameEffectKey, CardGameEffectResult, CardGameResume, CardGameService,
    CardGameTimedOutcome, LandlordCommand,
};
use crate::features::idiom_chain::{
    IdiomChainCommand, IdiomChainDeadlineKind, IdiomChainDeadlineToken, IdiomChainOutcome,
    IdiomChainService,
};
use crate::observation::chat::{
    CompletionAdvance, ObservationCompletionEvent, ObservationWatermark,
};
use crate::observation::shared::ObservationGap;
use crate::runtime::deadline::{BusinessDeadlineEvent, BusinessDeadlineToken};
use crate::runtime::identity::{
    BusinessOperationId, BusinessOperationIdAllocator, SessionGeneration,
};
use crate::runtime::timer::{
    DeadlineCancellation, DeadlineSchedule, TimerCommandKind, TimerRuntimeEvent, TimerRuntimeHandle,
};

const IDIOM_DEADLINE_TOKEN_ID: u64 = 1;
const CARD_GAME_DEADLINE_TOKEN_ID: u64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusinessEvent {
    CompletionAdvance(CompletionAdvance),
    CompletionGap(ObservationGap),
    Timer(BusinessDeadlineEvent),
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
        }
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
    TimerOperationFailed(String),
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
            Self::TimerOperationFailed(message) => {
                write!(formatter, "business timer operation failed: {message}")
            }
        }
    }
}

impl Error for BusinessRuntimeError {}

enum RuntimeMessage {
    Event(BusinessEvent),
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
    Snapshot(SyncSender<BusinessRuntimeSnapshot>),
    PrepareShutdown(SyncSender<BusinessRuntimeSnapshot>),
    Shutdown(SyncSender<BusinessRuntimeSnapshot>),
}

#[derive(Clone, Debug)]
struct ActiveIdiomDeadline {
    token: BusinessDeadlineToken,
    operation_id: BusinessOperationId,
    session_generation: SessionGeneration,
    deadline: Instant,
}

#[derive(Clone, Debug)]
struct ActiveCardGameDeadline {
    token: BusinessDeadlineToken,
    operation_id: BusinessOperationId,
    session_generation: SessionGeneration,
    deadline: Instant,
}

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

    #[deprecated(
        note = "use poll_card_game_timed_outcome; retained for internal API compatibility"
    )]
    pub fn tick_card_game(
        &self,
        now: Instant,
        clock_active: bool,
    ) -> Result<Option<CardGameTimedOutcome>, BusinessRuntimeError> {
        self.poll_card_game_timed_outcome(now, clock_active)
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

pub struct BusinessRuntime {
    handle: BusinessRuntimeHandle,
    worker: Option<JoinHandle<()>>,
}

impl BusinessRuntime {
    #[cfg(test)]
    pub(crate) fn start(
        queue_capacity: usize,
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
    ) -> Result<Self, BusinessRuntimeError> {
        Self::start_internal(queue_capacity, idiom_chain, card_games, None)
    }

    pub(crate) fn start_with_timer(
        queue_capacity: usize,
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
        timer: TimerRuntimeHandle<BusinessDeadlineToken>,
    ) -> Result<Self, BusinessRuntimeError> {
        Self::start_internal(queue_capacity, idiom_chain, card_games, Some(timer))
    }

    fn start_internal(
        queue_capacity: usize,
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
        timer: Option<TimerRuntimeHandle<BusinessDeadlineToken>>,
    ) -> Result<Self, BusinessRuntimeError> {
        if queue_capacity == 0 {
            return Err(BusinessRuntimeError::ZeroQueueCapacity);
        }
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let channel = Arc::new(RuntimeChannel {
            sender,
            state: Mutex::new(RuntimeChannelState::Running),
        });
        let worker = thread::Builder::new()
            .name("business-runtime".to_string())
            .spawn(move || run_business_runtime(receiver, idiom_chain, card_games, timer))
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
        Ok(Self {
            handle: BusinessRuntimeHandle { channel },
            worker: Some(worker),
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

    fn stop_worker(&mut self) -> Result<BusinessRuntimeSnapshot, BusinessRuntimeError> {
        let Some(worker) = self.worker.take() else {
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
        worker
            .join()
            .map_err(|_| BusinessRuntimeError::WorkerPanicked)?;
        snapshot.ok_or(BusinessRuntimeError::RuntimeStopped)
    }
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
            let previous_token = previous.token.clone();
            let operation_id = operation_ids
                .allocate()
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            timer
                .reschedule(DeadlineSchedule::new(
                    previous_token,
                    operation_id,
                    session_generation,
                    deadline,
                ))
                .map_err(|error| BusinessRuntimeError::TimerOperationFailed(error.to_string()))?;
            let previous = active
                .as_mut()
                .expect("active card deadline remains while rescheduling");
            previous.operation_id = operation_id;
            previous.session_generation = session_generation;
            previous.deadline = deadline;
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

fn handle_business_timer(
    event: BusinessDeadlineEvent,
    idiom_chain: &mut IdiomChainService,
    card_games: &mut CardGameService,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active_idiom_deadline: &mut Option<ActiveIdiomDeadline>,
    active_card_game_deadline: &mut Option<ActiveCardGameDeadline>,
    pending_card_game_cancellations: &mut Vec<ActiveCardGameDeadline>,
    pending_card_game_outcomes: &mut std::collections::VecDeque<CardGameTimedOutcome>,
    operation_ids: &BusinessOperationIdAllocator,
    generation: &mut SessionGeneration,
    clock_active: bool,
) -> Result<(), BusinessRuntimeError> {
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
                let handled_at = Instant::now();
                if let Some(outcome) = card_games
                    .handle_deadline(*expired.token().kind(), handled_at)
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
                    pending_card_game_cancellations.remove(index);
                    if completed.result().is_err() {
                        return sync_card_game_deadline(
                            card_games,
                            timer,
                            active_card_game_deadline,
                            pending_card_game_cancellations,
                            operation_ids,
                            Instant::now(),
                            clock_active,
                        );
                    }
                    return Ok(());
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
                        Instant::now(),
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
        .expire_idle_at(expired.emitted_at())
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

fn run_business_runtime(
    receiver: Receiver<RuntimeMessage>,
    mut idiom_chain: IdiomChainService,
    mut card_games: CardGameService,
    timer: Option<TimerRuntimeHandle<BusinessDeadlineToken>>,
) {
    let mut snapshot = BusinessRuntimeSnapshot::default();
    let mut active_idiom_deadline = None;
    let mut active_card_game_deadline = None;
    let mut pending_card_game_cancellations = Vec::new();
    let mut pending_card_game_outcomes = std::collections::VecDeque::new();
    let mut card_game_clock_active = true;
    let operation_ids = BusinessOperationIdAllocator::new();
    let mut session_generation = SessionGeneration::INITIAL;
    while let Ok(message) = receiver.recv() {
        match message {
            RuntimeMessage::Event(event) => {
                if let BusinessEvent::Timer(timer_event) = &event {
                    snapshot.apply(BusinessEvent::Timer(timer_event.clone()));
                    if let Err(error) = handle_business_timer(
                        timer_event.clone(),
                        &mut idiom_chain,
                        &mut card_games,
                        timer.as_ref(),
                        &mut active_idiom_deadline,
                        &mut active_card_game_deadline,
                        &mut pending_card_game_cancellations,
                        &mut pending_card_game_outcomes,
                        &operation_ids,
                        &mut session_generation,
                        card_game_clock_active,
                    ) {
                        log::error!("业务运行时处理计时事件失败: {error}");
                    }
                } else {
                    snapshot.apply(event);
                }
            }
            RuntimeMessage::HandleIdiomChain {
                player,
                command,
                response,
            } => {
                let result = idiom_chain
                    .handle(&player, &command)
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
                    .explain(&player, &command)
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
                    .abort()
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
                    .expire_idle_at(Instant::now())
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
                    message,
                    timer.as_ref(),
                    &mut active_card_game_deadline,
                    &mut pending_card_game_cancellations,
                    &operation_ids,
                    &mut pending_card_game_outcomes,
                    &mut card_game_clock_active,
                ) {
                    log::error!("业务运行时处理牌局消息失败: {error}");
                }
            }
            RuntimeMessage::Snapshot(response) => {
                let _ = response.send(snapshot);
            }
            RuntimeMessage::PrepareShutdown(response) => {
                abort_business_modules(
                    &mut idiom_chain,
                    &mut card_games,
                    timer.as_ref(),
                    &mut active_idiom_deadline,
                    &mut active_card_game_deadline,
                    &mut pending_card_game_cancellations,
                    &operation_ids,
                    &mut session_generation,
                    &mut pending_card_game_outcomes,
                    card_game_clock_active,
                );
                snapshot.quiescing = true;
                let _ = response.send(snapshot);
            }
            RuntimeMessage::Shutdown(response) => {
                abort_business_modules(
                    &mut idiom_chain,
                    &mut card_games,
                    timer.as_ref(),
                    &mut active_idiom_deadline,
                    &mut active_card_game_deadline,
                    &mut pending_card_game_cancellations,
                    &operation_ids,
                    &mut session_generation,
                    &mut pending_card_game_outcomes,
                    card_game_clock_active,
                );
                let _ = response.send(snapshot);
                break;
            }
        }
    }
}

fn abort_business_modules(
    idiom_chain: &mut IdiomChainService,
    card_games: &mut CardGameService,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active_idiom_deadline: &mut Option<ActiveIdiomDeadline>,
    active_card_game_deadline: &mut Option<ActiveCardGameDeadline>,
    pending_card_game_cancellations: &mut Vec<ActiveCardGameDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    session_generation: &mut SessionGeneration,
    pending_card_game_outcomes: &mut std::collections::VecDeque<CardGameTimedOutcome>,
    clock_active: bool,
) {
    if let Err(error) = card_games.abort() {
        log::error!("业务运行时关闭时无法中止牌局: {error:#}");
    }
    if let Err(error) = idiom_chain.abort() {
        log::error!("业务运行时关闭时无法中止成语接龙: {error:#}");
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
        Instant::now(),
        clock_active,
    ) {
        log::error!("业务运行时关闭时无法撤销牌局期限: {error}");
    }
    pending_card_game_outcomes.clear();
    pending_card_game_cancellations.clear();
}

fn handle_card_game_message(
    card_games: &mut CardGameService,
    message: CardGameRuntimeMessage,
    timer: Option<&TimerRuntimeHandle<BusinessDeadlineToken>>,
    active_deadline: &mut Option<ActiveCardGameDeadline>,
    pending_cancellations: &mut Vec<ActiveCardGameDeadline>,
    operation_ids: &BusinessOperationIdAllocator,
    pending_outcomes: &mut std::collections::VecDeque<CardGameTimedOutcome>,
    clock_active: &mut bool,
) -> Result<(), BusinessRuntimeError> {
    match message {
        CardGameRuntimeMessage::Begin {
            player,
            command,
            now,
            response,
        } => {
            let result = card_games
                .begin_command(&player, &command, now)
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
                        Instant::now(),
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
                .resume(key, result)
                .map_err(card_game_operation_failed)
                .and_then(|result| {
                    sync_card_game_deadline(
                        card_games,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        Instant::now(),
                        *clock_active,
                    )?;
                    Ok(result)
                });
            let _ = response.send(response_result);
        }
        CardGameRuntimeMessage::Cancel { key, response } => {
            let result = card_games
                .cancel(key)
                .map_err(card_game_operation_failed)
                .and_then(|result| {
                    sync_card_game_deadline(
                        card_games,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        Instant::now(),
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
                            .tick(now, requested_clock_active)
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
                .abort()
                .map_err(card_game_operation_failed)
                .and_then(|result| {
                    sync_card_game_deadline(
                        card_games,
                        timer,
                        active_deadline,
                        pending_cancellations,
                        operation_ids,
                        Instant::now(),
                        *clock_active,
                    )?;
                    Ok(result)
                });
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::features::card_games::{
        CardGameDeadlineKind, CardGameDeadlineToken, CardGameEffect, CardGameEffectLane,
        CardGameEffectRequest, CardGameLateResult, LandlordConfig,
    };
    use crate::features::entertainment::{EntertainmentCoordinator, EntertainmentKind};
    use crate::features::idiom_chain::{
        IdiomChainDeadlineKind, IdiomChainDeadlineToken, IdiomChainMode,
    };
    use crate::features::turtle_soup::{TurtleSoupDeadlineKind, TurtleSoupDeadlineToken};
    use crate::features::undercover::{UndercoverDeadlineKind, UndercoverDeadlineToken};
    use crate::observation::chat::ChatObservationLedger;
    use crate::observation::shared::{ObservationGapKind, SharedObservationStream};
    use crate::runtime::deadline::{BusinessDeadlineEvent, BusinessDeadlineToken};
    use crate::runtime::identity::{BusinessOperationId, SessionGeneration};
    use crate::runtime::timer::{DeadlineSchedule, TimerCore, TimerRuntimeEvent};

    fn idiom_service(
        entertainment: EntertainmentCoordinator,
        idle_timeout: Option<Duration>,
    ) -> IdiomChainService {
        IdiomChainService::from_entries_for_test(
            &["画蛇添足", "足智多谋", "谋事在人", "人山人海"],
            entertainment,
            idle_timeout,
        )
    }

    fn runtime(queue_capacity: usize) -> BusinessRuntime {
        let entertainment = EntertainmentCoordinator::new();
        BusinessRuntime::start(
            queue_capacity,
            idiom_service(entertainment.clone(), Some(Duration::from_secs(300))),
            CardGameService::new(LandlordConfig::default(), entertainment),
        )
        .unwrap()
    }

    fn runtime_with_entertainment(
        queue_capacity: usize,
    ) -> (BusinessRuntime, EntertainmentCoordinator) {
        let entertainment = EntertainmentCoordinator::new();
        let runtime = BusinessRuntime::start(
            queue_capacity,
            idiom_service(entertainment.clone(), Some(Duration::from_secs(300))),
            CardGameService::new(LandlordConfig::default(), entertainment.clone()),
        )
        .unwrap();
        (runtime, entertainment)
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
        let (runtime, entertainment) = runtime_with_entertainment(8);
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
        assert_eq!(entertainment.active(), None);
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
        let (runtime, entertainment) = runtime_with_entertainment(8);
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
        assert_eq!(entertainment.active(), Some(EntertainmentKind::Landlord));
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
        let (runtime, entertainment) = runtime_with_entertainment(4);
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
        assert_eq!(entertainment.active(), None);
    }

    #[test]
    fn shutdown_aborts_active_idiom_chain_and_releases_entertainment() {
        let entertainment = EntertainmentCoordinator::new();
        let runtime = BusinessRuntime::start(
            4,
            idiom_service(entertainment.clone(), Some(Duration::from_secs(300))),
            CardGameService::new(LandlordConfig::default(), entertainment.clone()),
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

        runtime.shutdown().unwrap();

        assert_eq!(entertainment.active(), None);
    }

    #[test]
    fn idiom_chain_requests_share_worker_owned_state() {
        let entertainment = EntertainmentCoordinator::new();
        let runtime = BusinessRuntime::start(
            4,
            idiom_service(entertainment.clone(), Some(Duration::from_secs(300))),
            CardGameService::new(LandlordConfig::default(), entertainment.clone()),
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
        assert_eq!(entertainment.active(), None);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn idiom_chain_idle_expiration_runs_on_the_business_worker() {
        let entertainment = EntertainmentCoordinator::new();
        let runtime = BusinessRuntime::start(
            2,
            idiom_service(entertainment.clone(), Some(Duration::ZERO)),
            CardGameService::new(LandlordConfig::default(), entertainment.clone()),
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
        assert_eq!(entertainment.active(), None);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn matching_idiom_deadline_event_expires_the_owned_session() {
        let entertainment = EntertainmentCoordinator::new();
        let mut idiom_chain = idiom_service(entertainment.clone(), Some(Duration::ZERO));
        let started = idiom_chain
            .handle(
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
        let mut card_games =
            CardGameService::new(LandlordConfig::default(), EntertainmentCoordinator::new());
        let mut card_active = None;
        let mut pending_card_cancellations = Vec::new();
        let mut pending_card_outcomes = std::collections::VecDeque::new();

        handle_business_timer(
            event,
            &mut idiom_chain,
            &mut card_games,
            None,
            &mut active,
            &mut card_active,
            &mut pending_card_cancellations,
            &mut pending_card_outcomes,
            &operation_ids,
            &mut next_generation,
            true,
        )
        .unwrap();

        assert!(active.is_none());
        assert_eq!(entertainment.active(), None);
        assert_eq!(idiom_chain.idle_deadline(), None);
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert!(matches!(
            BusinessRuntime::start(
                0,
                idiom_service(EntertainmentCoordinator::new(), None),
                CardGameService::new(LandlordConfig::default(), EntertainmentCoordinator::new(),),
            ),
            Err(BusinessRuntimeError::ZeroQueueCapacity)
        ));
    }
}
