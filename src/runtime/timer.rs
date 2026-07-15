use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        DeadlineIdentity, DeadlineKind, DeadlineModule, DeadlineSchedule, DeadlineToken, TimerCore,
        TimerCoreError,
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
}
