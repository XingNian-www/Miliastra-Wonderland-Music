use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use super::identity::{BusinessOperationId, SessionGeneration};

/// Names the vertical module that owns a family of deadlines.
pub trait DeadlineModule: Send + Sync + 'static {
    const NAME: &'static str;
}

/// Describes one module-specific deadline kind.
///
/// Associating the module here prevents a deadline kind from being accidentally constructed as a
/// token owned by another vertical module.
pub trait DeadlineKind: Clone + Debug + Eq + Hash + Send + 'static {
    type Module: DeadlineModule;
}

/// A typed identity for a deadline owned by one vertical module.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeadlineToken<K: DeadlineKind> {
    id: u64,
    kind: K,
}

impl<K: DeadlineKind> DeadlineToken<K> {
    pub const fn new(id: u64, kind: K) -> Self {
        Self { id, kind }
    }

    pub const fn id(&self) -> u64 {
        self.id
    }

    pub const fn kind(&self) -> &K {
        &self.kind
    }
}

/// Common routing contract for typed tokens and a future top-level token enum.
pub trait DeadlineIdentity: Clone + Debug + Eq + Hash + Send + 'static {
    fn module_name(&self) -> &'static str;
}

impl<K: DeadlineKind> DeadlineIdentity for DeadlineToken<K> {
    fn module_name(&self) -> &'static str {
        K::Module::NAME
    }
}

/// One requested deadline, correlated to the operation and session that created it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeadlineSchedule<T> {
    token: T,
    operation_id: BusinessOperationId,
    session_generation: SessionGeneration,
    deadline: Instant,
}

impl<T> DeadlineSchedule<T> {
    pub const fn new(
        token: T,
        operation_id: BusinessOperationId,
        session_generation: SessionGeneration,
        deadline: Instant,
    ) -> Self {
        Self {
            token,
            operation_id,
            session_generation,
            deadline,
        }
    }

    pub const fn token(&self) -> &T {
        &self.token
    }

    pub const fn operation_id(&self) -> BusinessOperationId {
        self.operation_id
    }

    pub const fn session_generation(&self) -> SessionGeneration {
        self.session_generation
    }

    pub const fn deadline(&self) -> Instant {
        self.deadline
    }

    fn map_token_with<U>(self, map: &mut impl FnMut(T) -> U) -> DeadlineSchedule<U> {
        DeadlineSchedule {
            token: map(self.token),
            operation_id: self.operation_id,
            session_generation: self.session_generation,
            deadline: self.deadline,
        }
    }
}

/// A cancellation request has its own correlation identity, separate from the schedule it targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeadlineCancellation<T> {
    token: T,
    operation_id: BusinessOperationId,
    session_generation: SessionGeneration,
}

impl<T> DeadlineCancellation<T> {
    pub const fn new(
        token: T,
        operation_id: BusinessOperationId,
        session_generation: SessionGeneration,
    ) -> Self {
        Self {
            token,
            operation_id,
            session_generation,
        }
    }

    pub const fn token(&self) -> &T {
        &self.token
    }

    pub const fn operation_id(&self) -> BusinessOperationId {
        self.operation_id
    }

    pub const fn session_generation(&self) -> SessionGeneration {
        self.session_generation
    }
}

/// A timer event. The timer reports timing facts and leaves all business decisions to the owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeadlineExpired<T> {
    schedule: DeadlineSchedule<T>,
    emitted_at: Instant,
}

impl<T> DeadlineExpired<T> {
    pub const fn schedule(&self) -> &DeadlineSchedule<T> {
        &self.schedule
    }

    pub const fn token(&self) -> &T {
        self.schedule.token()
    }

    pub const fn operation_id(&self) -> BusinessOperationId {
        self.schedule.operation_id()
    }

    pub const fn session_generation(&self) -> SessionGeneration {
        self.schedule.session_generation()
    }

    pub const fn deadline(&self) -> Instant {
        self.schedule.deadline()
    }

    pub const fn emitted_at(&self) -> Instant {
        self.emitted_at
    }

    pub fn into_schedule(self) -> DeadlineSchedule<T> {
        self.schedule
    }

    pub fn map_token<U>(self, map: impl FnOnce(T) -> U) -> DeadlineExpired<U> {
        DeadlineExpired {
            schedule: DeadlineSchedule {
                token: map(self.schedule.token),
                operation_id: self.schedule.operation_id,
                session_generation: self.schedule.session_generation,
                deadline: self.schedule.deadline,
            },
            emitted_at: self.emitted_at,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerCommandKind {
    Schedule,
    Reschedule,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerCommandOutcome<T> {
    Scheduled,
    Rescheduled(DeadlineSchedule<T>),
    Cancelled(Option<DeadlineSchedule<T>>),
}

impl<T> TimerCommandOutcome<T> {
    fn map_token_with<U>(self, map: &mut impl FnMut(T) -> U) -> TimerCommandOutcome<U> {
        match self {
            Self::Scheduled => TimerCommandOutcome::Scheduled,
            Self::Rescheduled(previous) => {
                TimerCommandOutcome::Rescheduled(previous.map_token_with(map))
            }
            Self::Cancelled(previous) => {
                TimerCommandOutcome::Cancelled(previous.map(|item| item.map_token_with(map)))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimerCommandCompleted<T> {
    token: T,
    operation_id: BusinessOperationId,
    session_generation: SessionGeneration,
    command: TimerCommandKind,
    result: Result<TimerCommandOutcome<T>, TimerCoreError>,
}

impl<T> TimerCommandCompleted<T> {
    pub const fn token(&self) -> &T {
        &self.token
    }

    pub const fn operation_id(&self) -> BusinessOperationId {
        self.operation_id
    }

    pub const fn session_generation(&self) -> SessionGeneration {
        self.session_generation
    }

    pub const fn command(&self) -> TimerCommandKind {
        self.command
    }

    pub const fn result(&self) -> &Result<TimerCommandOutcome<T>, TimerCoreError> {
        &self.result
    }

    fn map_token<U>(self, mut map: impl FnMut(T) -> U) -> TimerCommandCompleted<U> {
        TimerCommandCompleted {
            token: map(self.token),
            operation_id: self.operation_id,
            session_generation: self.session_generation,
            command: self.command,
            result: self.result.map(|outcome| outcome.map_token_with(&mut map)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimerRuntimeEvent<T> {
    DeadlineExpired(DeadlineExpired<T>),
    CommandCompleted(TimerCommandCompleted<T>),
}

impl<T> TimerRuntimeEvent<T> {
    pub const fn token(&self) -> &T {
        match self {
            Self::DeadlineExpired(event) => event.token(),
            Self::CommandCompleted(event) => event.token(),
        }
    }

    pub fn map_token<U>(self, map: impl FnMut(T) -> U) -> TimerRuntimeEvent<U> {
        match self {
            Self::DeadlineExpired(event) => {
                let mut map = map;
                TimerRuntimeEvent::DeadlineExpired(event.map_token(&mut map))
            }
            Self::CommandCompleted(event) => {
                TimerRuntimeEvent::CommandCompleted(event.map_token(map))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerCoreError {
    Closed,
    TokenAlreadyScheduled,
    TokenNotScheduled,
    StableOrderExhausted,
}

impl Display for TimerCoreError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => formatter.write_str("timer core is closed"),
            Self::TokenAlreadyScheduled => {
                formatter.write_str("deadline token is already scheduled")
            }
            Self::TokenNotScheduled => formatter.write_str("deadline token is not scheduled"),
            Self::StableOrderExhausted => {
                formatter.write_str("timer stable-order sequence exhausted")
            }
        }
    }
}

impl Error for TimerCoreError {}

/// Pure deadline ordering and lifecycle state.
///
/// `TimerCore` neither reads a clock nor starts a worker. A runtime can drive it with a real clock,
/// while business tests can drive it with `ManualClock` without sleeping.
#[derive(Debug)]
pub struct TimerCore<T: DeadlineIdentity> {
    state: TimerCoreState,
    next_order: u64,
    by_due_order: BTreeMap<(Instant, u64), DeadlineSchedule<T>>,
    order_by_token: HashMap<T, (Instant, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimerCoreState {
    Open,
    Closed,
}

impl<T: DeadlineIdentity> Default for TimerCore<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: DeadlineIdentity> TimerCore<T> {
    pub fn new() -> Self {
        Self {
            state: TimerCoreState::Open,
            next_order: 0,
            by_due_order: BTreeMap::new(),
            order_by_token: HashMap::new(),
        }
    }

    pub fn schedule(&mut self, schedule: DeadlineSchedule<T>) -> Result<(), TimerCoreError> {
        self.ensure_open()?;
        if self.order_by_token.contains_key(schedule.token()) {
            return Err(TimerCoreError::TokenAlreadyScheduled);
        }
        let key = self.allocate_key(schedule.deadline())?;
        self.order_by_token.insert(schedule.token().clone(), key);
        self.by_due_order.insert(key, schedule);
        Ok(())
    }

    /// Replaces an existing token's complete correlation data and gives it a new stable order.
    pub fn reschedule(
        &mut self,
        schedule: DeadlineSchedule<T>,
    ) -> Result<DeadlineSchedule<T>, TimerCoreError> {
        self.ensure_open()?;
        let Some(previous_key) = self.order_by_token.get(schedule.token()).copied() else {
            return Err(TimerCoreError::TokenNotScheduled);
        };
        let new_key = self.allocate_key(schedule.deadline())?;
        let previous = self
            .by_due_order
            .remove(&previous_key)
            .expect("token and deadline indexes must stay consistent");
        self.order_by_token
            .insert(schedule.token().clone(), new_key);
        self.by_due_order.insert(new_key, schedule);
        Ok(previous)
    }

    pub fn cancel(&mut self, token: &T) -> Result<Option<DeadlineSchedule<T>>, TimerCoreError> {
        self.ensure_open()?;
        let Some(key) = self.order_by_token.remove(token) else {
            return Ok(None);
        };
        Ok(self.by_due_order.remove(&key))
    }

    pub fn drain_expired(
        &mut self,
        now: Instant,
    ) -> Result<Vec<DeadlineExpired<T>>, TimerCoreError> {
        self.ensure_open()?;
        let first_future = self
            .by_due_order
            .range((now, u64::MAX)..)
            .find_map(|(key, _)| (key.0 > now).then_some(*key));
        let due = match first_future {
            Some(first_future) => self.by_due_order.split_off(&first_future),
            None => BTreeMap::new(),
        };
        let expired = std::mem::replace(&mut self.by_due_order, due)
            .into_values()
            .map(|schedule| {
                self.order_by_token.remove(schedule.token());
                DeadlineExpired {
                    schedule,
                    emitted_at: now,
                }
            })
            .collect();
        Ok(expired)
    }

    /// Closes the core and returns every pending deadline in its deterministic due order.
    ///
    /// Closing is idempotent. Once closed, no deadline can be scheduled, changed, cancelled, or
    /// emitted.
    pub fn close(&mut self) -> Vec<DeadlineSchedule<T>> {
        if self.state == TimerCoreState::Closed {
            return Vec::new();
        }
        self.state = TimerCoreState::Closed;
        self.order_by_token.clear();
        std::mem::take(&mut self.by_due_order)
            .into_values()
            .collect()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.by_due_order.first_key_value().map(|(key, _)| key.0)
    }

    pub fn len(&self) -> usize {
        self.by_due_order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_due_order.is_empty()
    }

    pub fn is_closed(&self) -> bool {
        self.state == TimerCoreState::Closed
    }

    fn allocate_key(&mut self, deadline: Instant) -> Result<(Instant, u64), TimerCoreError> {
        let order = self.next_order;
        self.next_order = self
            .next_order
            .checked_add(1)
            .ok_or(TimerCoreError::StableOrderExhausted)?;
        Ok((deadline, order))
    }

    fn ensure_open(&self) -> Result<(), TimerCoreError> {
        if self.state == TimerCoreState::Closed {
            Err(TimerCoreError::Closed)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerSubmitError {
    QueueFull,
    RuntimeStopped,
}

impl Display for TimerSubmitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => formatter.write_str("timer runtime queue is full"),
            Self::RuntimeStopped => formatter.write_str("timer runtime is stopped"),
        }
    }
}

impl Error for TimerSubmitError {}

enum TimerCommand<T: DeadlineIdentity> {
    Schedule(DeadlineSchedule<T>),
    Reschedule(DeadlineSchedule<T>),
    Cancel(DeadlineCancellation<T>),
    Shutdown,
}

struct TimerChannel<T: DeadlineIdentity> {
    sender: SyncSender<TimerCommand<T>>,
    accepting: Mutex<bool>,
}

#[derive(Clone)]
pub struct TimerRuntimeHandle<T: DeadlineIdentity> {
    channel: Arc<TimerChannel<T>>,
}

impl<T: DeadlineIdentity> TimerRuntimeHandle<T> {
    pub fn schedule(&self, schedule: DeadlineSchedule<T>) -> Result<(), TimerSubmitError> {
        self.submit(TimerCommand::Schedule(schedule))
    }

    pub fn reschedule(&self, schedule: DeadlineSchedule<T>) -> Result<(), TimerSubmitError> {
        self.submit(TimerCommand::Reschedule(schedule))
    }

    pub fn cancel(&self, cancellation: DeadlineCancellation<T>) -> Result<(), TimerSubmitError> {
        self.submit(TimerCommand::Cancel(cancellation))
    }

    fn submit(&self, command: TimerCommand<T>) -> Result<(), TimerSubmitError> {
        let accepting = self
            .channel
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*accepting {
            return Err(TimerSubmitError::RuntimeStopped);
        }
        match self.channel.sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(TimerSubmitError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(TimerSubmitError::RuntimeStopped),
        }
    }
}

#[derive(Debug)]
pub enum TimerRuntimeStartError {
    ZeroQueueCapacity,
    Spawn(std::io::Error),
}

impl Display for TimerRuntimeStartError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroQueueCapacity => {
                formatter.write_str("timer runtime queue capacity must be greater than zero")
            }
            Self::Spawn(error) => write!(formatter, "failed to start timer runtime: {error}"),
        }
    }
}

impl Error for TimerRuntimeStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ZeroQueueCapacity => None,
            Self::Spawn(error) => Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimerShutdownError;

impl Display for TimerShutdownError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("timer runtime worker panicked")
    }
}

impl Error for TimerShutdownError {}

pub struct TimerRuntime<T: DeadlineIdentity> {
    handle: TimerRuntimeHandle<T>,
    worker: Option<JoinHandle<()>>,
}

impl<T: DeadlineIdentity> TimerRuntime<T> {
    pub(super) fn start(
        queue_capacity: usize,
        events: SyncSender<TimerRuntimeEvent<T>>,
    ) -> Result<Self, TimerRuntimeStartError> {
        if queue_capacity == 0 {
            return Err(TimerRuntimeStartError::ZeroQueueCapacity);
        }
        let (command_sender, command_receiver) = mpsc::sync_channel(queue_capacity);
        let channel = Arc::new(TimerChannel {
            sender: command_sender,
            accepting: Mutex::new(true),
        });
        let worker = thread::Builder::new()
            .name("timer-runtime".to_string())
            .spawn(move || run_timer_runtime(command_receiver, events))
            .map_err(TimerRuntimeStartError::Spawn)?;
        Ok(Self {
            handle: TimerRuntimeHandle { channel },
            worker: Some(worker),
        })
    }

    pub fn handle(&self) -> TimerRuntimeHandle<T> {
        self.handle.clone()
    }

    pub fn shutdown(mut self) -> Result<(), TimerShutdownError> {
        self.stop_worker()
    }

    fn stop_worker(&mut self) -> Result<(), TimerShutdownError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let mut accepting = self
            .handle
            .channel
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *accepting = false;
        let _ = self.handle.channel.sender.send(TimerCommand::Shutdown);
        drop(accepting);
        worker.join().map_err(|_| TimerShutdownError)
    }
}

impl<T: DeadlineIdentity> Drop for TimerRuntime<T> {
    fn drop(&mut self) {
        let _ = self.stop_worker();
    }
}

fn run_timer_runtime<T: DeadlineIdentity>(
    commands: Receiver<TimerCommand<T>>,
    events: SyncSender<TimerRuntimeEvent<T>>,
) {
    let mut core = TimerCore::new();
    loop {
        let now = Instant::now();
        let due = match core.drain_expired(now) {
            Ok(due) => due,
            Err(TimerCoreError::Closed) => break,
            Err(_) => unreachable!("draining an open timer core cannot fail"),
        };
        for event in due {
            if events
                .send(TimerRuntimeEvent::DeadlineExpired(event))
                .is_err()
            {
                core.close();
                return;
            }
        }

        let command = match core.next_deadline() {
            Some(deadline) => {
                match commands.recv_timeout(deadline.saturating_duration_since(now)) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match commands.recv() {
                Ok(command) => Some(command),
                Err(RecvError) => break,
            },
        };
        match command {
            Some(TimerCommand::Schedule(schedule)) => {
                let token = schedule.token().clone();
                let operation_id = schedule.operation_id();
                let session_generation = schedule.session_generation();
                let result = core
                    .schedule(schedule)
                    .map(|()| TimerCommandOutcome::Scheduled);
                let completed = TimerCommandCompleted {
                    token,
                    operation_id,
                    session_generation,
                    command: TimerCommandKind::Schedule,
                    result,
                };
                if events
                    .send(TimerRuntimeEvent::CommandCompleted(completed))
                    .is_err()
                {
                    core.close();
                    return;
                }
            }
            Some(TimerCommand::Reschedule(schedule)) => {
                let token = schedule.token().clone();
                let operation_id = schedule.operation_id();
                let session_generation = schedule.session_generation();
                let result = core
                    .reschedule(schedule)
                    .map(TimerCommandOutcome::Rescheduled);
                let completed = TimerCommandCompleted {
                    token,
                    operation_id,
                    session_generation,
                    command: TimerCommandKind::Reschedule,
                    result,
                };
                if events
                    .send(TimerRuntimeEvent::CommandCompleted(completed))
                    .is_err()
                {
                    core.close();
                    return;
                }
            }
            Some(TimerCommand::Cancel(cancellation)) => {
                let result = core
                    .cancel(cancellation.token())
                    .map(TimerCommandOutcome::Cancelled);
                let completed = TimerCommandCompleted {
                    token: cancellation.token,
                    operation_id: cancellation.operation_id,
                    session_generation: cancellation.session_generation,
                    command: TimerCommandKind::Cancel,
                    result,
                };
                if events
                    .send(TimerRuntimeEvent::CommandCompleted(completed))
                    .is_err()
                {
                    core.close();
                    return;
                }
            }
            Some(TimerCommand::Shutdown) => break,
            None => {}
        }
    }
    core.close();
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        DeadlineCancellation, DeadlineIdentity, DeadlineKind, DeadlineModule, DeadlineSchedule,
        DeadlineToken, TimerCommandKind, TimerCommandOutcome, TimerCore, TimerCoreError,
        TimerRuntime, TimerRuntimeEvent, TimerSubmitError,
    };
    use crate::runtime::clock::{Clock, ManualClock};
    use crate::runtime::identity::{BusinessOperationId, SessionGeneration};

    #[derive(Debug)]
    struct TestModule;

    impl DeadlineModule for TestModule {
        const NAME: &'static str = "test";
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum TestDeadline {
        Turn,
        Reminder,
    }

    impl DeadlineKind for TestDeadline {
        type Module = TestModule;
    }

    type Token = DeadlineToken<TestDeadline>;

    fn token(id: u64) -> Token {
        DeadlineToken::new(id, TestDeadline::Turn)
    }

    fn reminder(id: u64) -> Token {
        DeadlineToken::new(id, TestDeadline::Reminder)
    }

    fn schedule(
        token: Token,
        operation: u64,
        generation: u64,
        deadline: Instant,
    ) -> DeadlineSchedule<Token> {
        DeadlineSchedule::new(
            token,
            BusinessOperationId::new(operation),
            SessionGeneration::new(generation),
            deadline,
        )
    }

    fn runtime(
        queue_capacity: usize,
    ) -> (
        TimerRuntime<Token>,
        mpsc::Receiver<TimerRuntimeEvent<Token>>,
    ) {
        let (event_sender, events) = mpsc::sync_channel(queue_capacity);
        (
            TimerRuntime::start(queue_capacity, event_sender).unwrap(),
            events,
        )
    }

    #[test]
    fn typed_token_reports_its_owner_and_kind() {
        let token = reminder(7);

        assert_eq!(token.module_name(), "test");
        assert_eq!(token.id(), 7);
        assert_eq!(token.kind(), &TestDeadline::Reminder);
    }

    #[test]
    fn drains_only_due_deadlines_in_due_order() {
        let start = Instant::now();
        let mut core = TimerCore::new();
        core.schedule(schedule(token(1), 11, 2, start + Duration::from_secs(3)))
            .unwrap();
        core.schedule(schedule(token(2), 12, 2, start + Duration::from_secs(1)))
            .unwrap();
        core.schedule(schedule(token(3), 13, 2, start + Duration::from_secs(2)))
            .unwrap();

        assert!(core.drain_expired(start).unwrap().is_empty());
        let expired = core.drain_expired(start + Duration::from_secs(2)).unwrap();

        assert_eq!(
            expired
                .iter()
                .map(|event| event.token().id())
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(
            expired
                .iter()
                .all(|event| event.emitted_at() == start + Duration::from_secs(2))
        );
        assert_eq!(core.next_deadline(), Some(start + Duration::from_secs(3)));
    }

    #[test]
    fn manual_clock_drives_expiration_without_sleeping() {
        let start = Instant::now();
        let clock = ManualClock::new(start);
        let mut core = TimerCore::new();
        core.schedule(schedule(token(1), 11, 2, start + Duration::from_secs(180)))
            .unwrap();

        clock.advance(Duration::from_secs(179)).unwrap();
        assert!(core.drain_expired(clock.now()).unwrap().is_empty());
        clock.advance(Duration::from_secs(1)).unwrap();

        let expired = core.drain_expired(clock.now()).unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].deadline(), start + Duration::from_secs(180));
    }

    #[test]
    fn equal_deadlines_keep_schedule_order() {
        let deadline = Instant::now();
        let mut core = TimerCore::new();
        for id in [4, 1, 9] {
            core.schedule(schedule(token(id), id, 0, deadline)).unwrap();
        }

        let expired = core.drain_expired(deadline).unwrap();

        assert_eq!(
            expired
                .iter()
                .map(|event| event.token().id())
                .collect::<Vec<_>>(),
            vec![4, 1, 9]
        );
    }

    #[test]
    fn reschedule_replaces_correlation_and_moves_token_to_new_stable_order() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut core = TimerCore::new();
        core.schedule(schedule(token(1), 1, 1, deadline)).unwrap();
        core.schedule(schedule(token(2), 2, 1, deadline)).unwrap();

        let previous = core.reschedule(schedule(token(1), 3, 2, deadline)).unwrap();
        let expired = core.drain_expired(deadline).unwrap();

        assert_eq!(previous.operation_id(), BusinessOperationId::new(1));
        assert_eq!(
            expired
                .iter()
                .map(|event| event.token().id())
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(expired[1].operation_id(), BusinessOperationId::new(3));
        assert_eq!(expired[1].session_generation(), SessionGeneration::new(2));
    }

    #[test]
    fn cancel_is_idempotent_and_prevents_expiration() {
        let deadline = Instant::now();
        let mut core = TimerCore::new();
        core.schedule(schedule(token(1), 1, 0, deadline)).unwrap();

        assert_eq!(core.cancel(&token(1)).unwrap().unwrap().token(), &token(1));
        assert_eq!(core.cancel(&token(1)), Ok(None));
        assert!(core.drain_expired(deadline).unwrap().is_empty());
    }

    #[test]
    fn duplicate_and_unknown_reschedule_are_rejected_without_mutation() {
        let deadline = Instant::now();
        let mut core = TimerCore::new();
        core.schedule(schedule(token(1), 1, 0, deadline)).unwrap();

        assert_eq!(
            core.schedule(schedule(token(1), 2, 0, deadline)),
            Err(TimerCoreError::TokenAlreadyScheduled)
        );
        assert_eq!(
            core.reschedule(schedule(token(2), 2, 0, deadline)),
            Err(TimerCoreError::TokenNotScheduled)
        );
        assert_eq!(core.len(), 1);
    }

    #[test]
    fn stable_order_exhaustion_is_explicit_and_does_not_insert_the_schedule() {
        let mut core = TimerCore::new();
        core.next_order = u64::MAX;

        assert_eq!(
            core.schedule(schedule(token(1), 1, 0, Instant::now())),
            Err(TimerCoreError::StableOrderExhausted)
        );
        assert!(core.is_empty());
    }

    #[test]
    fn close_returns_pending_in_due_order_and_rejects_further_work() {
        let start = Instant::now();
        let mut core = TimerCore::new();
        core.schedule(schedule(token(1), 1, 0, start + Duration::from_secs(2)))
            .unwrap();
        core.schedule(schedule(token(2), 2, 0, start + Duration::from_secs(1)))
            .unwrap();

        let cancelled = core.close();

        assert_eq!(
            cancelled
                .iter()
                .map(|item| item.token().id())
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert!(core.is_closed());
        assert!(core.is_empty());
        assert!(core.close().is_empty());
        assert_eq!(
            core.schedule(schedule(token(3), 3, 0, start)),
            Err(TimerCoreError::Closed)
        );
        assert_eq!(core.cancel(&token(1)), Err(TimerCoreError::Closed));
        assert_eq!(core.drain_expired(start), Err(TimerCoreError::Closed));
    }

    #[test]
    fn same_numeric_id_is_distinct_for_different_deadline_kinds() {
        let deadline = Instant::now();
        let mut core = TimerCore::new();
        core.schedule(schedule(token(1), 1, 0, deadline)).unwrap();
        core.schedule(schedule(reminder(1), 2, 0, deadline))
            .unwrap();

        assert_eq!(core.drain_expired(deadline).unwrap().len(), 2);
    }

    #[test]
    fn runtime_emits_equal_deadlines_in_schedule_order_without_dropping_events() {
        let (runtime, event_receiver) = runtime(8);
        let handle = runtime.handle();
        let deadline = Instant::now() + Duration::from_millis(60);
        for id in [4, 1, 9] {
            handle
                .schedule(schedule(token(id), id, 0, deadline))
                .unwrap();
        }

        let events = (0..6)
            .map(|_| event_receiver.recv_timeout(Duration::from_secs(1)).unwrap())
            .collect::<Vec<_>>();
        let expired = events
            .iter()
            .filter_map(|event| match event {
                TimerRuntimeEvent::DeadlineExpired(event) => Some(event),
                TimerRuntimeEvent::CommandCompleted(_) => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            expired
                .iter()
                .map(|event| event.token().id())
                .collect::<Vec<_>>(),
            vec![4, 1, 9]
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn earlier_reschedule_wakes_a_worker_waiting_for_a_later_deadline() {
        let (runtime, event_receiver) = runtime(2);
        let handle = runtime.handle();
        let started = Instant::now();
        handle
            .schedule(schedule(
                token(1),
                1,
                0,
                started + Duration::from_millis(600),
            ))
            .unwrap();
        assert!(matches!(
            event_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            TimerRuntimeEvent::CommandCompleted(completed)
                if completed.result() == &Ok(TimerCommandOutcome::Scheduled)
        ));
        thread::sleep(Duration::from_millis(10));
        let earlier = Instant::now() + Duration::from_millis(60);

        handle
            .reschedule(schedule(token(1), 2, 1, earlier))
            .unwrap();
        assert!(matches!(
            event_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            TimerRuntimeEvent::CommandCompleted(completed)
                if completed.operation_id() == BusinessOperationId::new(2)
                    && completed.session_generation() == SessionGeneration::new(1)
                    && matches!(
                        completed.result(),
                        Ok(TimerCommandOutcome::Rescheduled(previous))
                            if previous.operation_id() == BusinessOperationId::new(1)
                                && previous.session_generation() == SessionGeneration::INITIAL
                    )
        ));

        let TimerRuntimeEvent::DeadlineExpired(expired) = event_receiver
            .recv_timeout(Duration::from_millis(300))
            .unwrap()
        else {
            panic!("rescheduled deadline should expire after its completion event")
        };
        assert_eq!(expired.token(), &token(1));
        assert_eq!(expired.operation_id(), BusinessOperationId::new(2));
        assert_eq!(expired.session_generation(), SessionGeneration::new(1));
        assert!(expired.emitted_at() < started + Duration::from_millis(400));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn runtime_cancel_prevents_the_deadline_event() {
        let (runtime, event_receiver) = runtime(2);
        let handle = runtime.handle();
        handle
            .schedule(schedule(
                token(1),
                1,
                0,
                Instant::now() + Duration::from_millis(60),
            ))
            .unwrap();
        handle
            .cancel(DeadlineCancellation::new(
                token(1),
                BusinessOperationId::new(2),
                SessionGeneration::INITIAL,
            ))
            .unwrap();

        let events = (0..2)
            .map(|_| event_receiver.recv_timeout(Duration::from_secs(1)).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            &events[1],
            TimerRuntimeEvent::CommandCompleted(completed)
                if completed.operation_id() == BusinessOperationId::new(2)
                    && matches!(
                        completed.result(),
                        Ok(TimerCommandOutcome::Cancelled(Some(previous)))
                            if previous.operation_id() == BusinessOperationId::new(1)
                                && previous.session_generation() == SessionGeneration::INITIAL
                    )
        ));
        assert!(
            event_receiver
                .recv_timeout(Duration::from_millis(120))
                .is_err()
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_closes_handles_without_panicking() {
        let (runtime, _events) = runtime(1);
        let handle = runtime.handle();

        runtime.shutdown().unwrap();

        assert!(matches!(
            handle.schedule(schedule(token(1), 1, 0, Instant::now())),
            Err(TimerSubmitError::RuntimeStopped)
        ));
    }

    #[test]
    fn full_command_queue_is_reported_while_expired_events_apply_backpressure() {
        let (runtime, event_receiver) = runtime(1);
        let handle = runtime.handle();
        handle
            .schedule(schedule(token(1), 1, 0, Instant::now()))
            .unwrap();
        assert!(matches!(
            event_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
            TimerRuntimeEvent::CommandCompleted(_)
        ));
        handle
            .schedule(schedule(
                token(2),
                2,
                0,
                Instant::now() + Duration::from_secs(1),
            ))
            .unwrap();
        let submit_by = Instant::now() + Duration::from_secs(1);
        loop {
            match handle.schedule(schedule(
                token(3),
                3,
                0,
                Instant::now() + Duration::from_secs(1),
            )) {
                Ok(()) => break,
                Err(TimerSubmitError::QueueFull) => {
                    assert!(
                        Instant::now() < submit_by,
                        "timer worker did not take token 2"
                    );
                    thread::yield_now();
                }
                Err(TimerSubmitError::RuntimeStopped) => panic!("timer stopped unexpectedly"),
            }
        }

        assert!(matches!(
            handle.schedule(schedule(
                token(4),
                4,
                0,
                Instant::now() + Duration::from_secs(1)
            )),
            Err(TimerSubmitError::QueueFull)
        ));
        assert!(matches!(
            event_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            TimerRuntimeEvent::DeadlineExpired(event) if event.token() == &token(1)
        ));
        for _ in 0..2 {
            assert!(matches!(
                event_receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
                TimerRuntimeEvent::CommandCompleted(_)
            ));
        }
        runtime.shutdown().unwrap();
    }

    #[test]
    fn runtime_core_errors_are_correlated_typed_events_instead_of_waitable_acks() {
        let (runtime, event_receiver) = runtime(8);
        let handle = runtime.handle();
        let deadline = Instant::now() + Duration::from_secs(60);
        handle
            .schedule(schedule(token(1), 11, 2, deadline))
            .unwrap();
        handle
            .schedule(schedule(token(1), 12, 3, deadline))
            .unwrap();
        handle
            .reschedule(schedule(token(2), 13, 4, deadline))
            .unwrap();
        handle
            .cancel(DeadlineCancellation::new(
                token(3),
                BusinessOperationId::new(14),
                SessionGeneration::new(5),
            ))
            .unwrap();

        let completions = (0..4)
            .map(|_| event_receiver.recv_timeout(Duration::from_secs(1)).unwrap())
            .collect::<Vec<_>>();

        assert!(matches!(
            &completions[0],
            TimerRuntimeEvent::CommandCompleted(completed)
                if completed.command() == TimerCommandKind::Schedule
                    && completed.operation_id() == BusinessOperationId::new(11)
                    && completed.session_generation() == SessionGeneration::new(2)
                    && completed.result() == &Ok(TimerCommandOutcome::Scheduled)
        ));
        assert!(matches!(
            &completions[1],
            TimerRuntimeEvent::CommandCompleted(completed)
                if completed.operation_id() == BusinessOperationId::new(12)
                    && completed.result() == &Err(TimerCoreError::TokenAlreadyScheduled)
        ));
        assert!(matches!(
            &completions[2],
            TimerRuntimeEvent::CommandCompleted(completed)
                if completed.command() == TimerCommandKind::Reschedule
                    && completed.operation_id() == BusinessOperationId::new(13)
                    && completed.result() == &Err(TimerCoreError::TokenNotScheduled)
        ));
        assert!(matches!(
            &completions[3],
            TimerRuntimeEvent::CommandCompleted(completed)
                if completed.command() == TimerCommandKind::Cancel
                    && completed.operation_id() == BusinessOperationId::new(14)
                    && completed.session_generation() == SessionGeneration::new(5)
                    && completed.result() == &Ok(TimerCommandOutcome::Cancelled(None))
        ));
        runtime.shutdown().unwrap();
    }
}
