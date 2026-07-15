use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::clock::{Clock, SystemClock};
use super::identity::{BusinessOperationId, BusinessOperationIdAllocator};
use super::player::{PlayerObservation, PlayerObservationConfig, PlayerObserver, RawPlayerSample};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerRuntimeConfig {
    pub observation: PlayerObservationConfig,
    pub normal_observation_interval: Duration,
    pub fast_observation_interval: Duration,
    pub observation_command_capacity: usize,
    pub active_fast_demand_capacity: usize,
    pub control_queue_capacity: usize,
    pub search_queue_capacity: usize,
}

impl Default for PlayerRuntimeConfig {
    fn default() -> Self {
        Self {
            observation: PlayerObservationConfig::default(),
            normal_observation_interval: Duration::from_secs(1),
            fast_observation_interval: Duration::from_millis(300),
            observation_command_capacity: 16,
            active_fast_demand_capacity: 16,
            control_queue_capacity: 16,
            search_queue_capacity: 16,
        }
    }
}

impl PlayerRuntimeConfig {
    pub fn validate(&self) -> Result<(), PlayerRuntimeConfigError> {
        let fields = [
            (
                self.normal_observation_interval.is_zero(),
                "normal_observation_interval",
            ),
            (
                self.fast_observation_interval.is_zero(),
                "fast_observation_interval",
            ),
            (self.observation.stale_timeout.is_zero(), "stale_timeout"),
            (
                self.observation.restart_previous_progress.is_zero(),
                "restart_previous_progress",
            ),
            (
                self.observation.restart_near_start.is_zero(),
                "restart_near_start",
            ),
            (
                self.observation.restart_minimum_drop.is_zero(),
                "restart_minimum_drop",
            ),
            (
                self.observation_command_capacity == 0,
                "observation_command_capacity",
            ),
            (
                self.active_fast_demand_capacity == 0,
                "active_fast_demand_capacity",
            ),
            (self.control_queue_capacity == 0, "control_queue_capacity"),
            (self.search_queue_capacity == 0, "search_queue_capacity"),
        ];
        match fields.into_iter().find(|(invalid, _)| *invalid) {
            Some((_, field)) => Err(PlayerRuntimeConfigError::ZeroDurationOrCapacity(field)),
            None if self.fast_observation_interval >= self.normal_observation_interval => {
                Err(PlayerRuntimeConfigError::FastIntervalNotFaster)
            }
            None => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerRuntimeConfigError {
    ZeroDurationOrCapacity(&'static str),
    FastIntervalNotFaster,
}

impl Display for PlayerRuntimeConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDurationOrCapacity(field) => {
                write!(
                    formatter,
                    "player runtime {field} must be greater than zero"
                )
            }
            Self::FastIntervalNotFaster => formatter.write_str(
                "player fast observation interval must be shorter than the normal interval",
            ),
        }
    }
}

impl Error for PlayerRuntimeConfigError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerObservationReadError {
    message: String,
}

impl PlayerObservationReadError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Display for PlayerObservationReadError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for PlayerObservationReadError {}

pub trait PlayerObservationPort: Send + 'static {
    fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerControl {
    PlayUri(String),
    Pause,
    Resume,
    Next,
    Previous,
    SetVolume(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlDispatchOutcome {
    Acknowledged { response: String },
    Rejected { reason: String },
    NotSent { reason: String },
    OutcomeUnknown { reason: String },
}

impl ControlDispatchOutcome {
    pub fn acknowledged(response: impl Into<String>) -> Self {
        Self::Acknowledged {
            response: response.into(),
        }
    }

    pub fn rejected(reason: impl Into<String>) -> Self {
        Self::Rejected {
            reason: reason.into(),
        }
    }

    pub fn not_sent(reason: impl Into<String>) -> Self {
        Self::NotSent {
            reason: reason.into(),
        }
    }

    pub fn outcome_unknown(reason: impl Into<String>) -> Self {
        Self::OutcomeUnknown {
            reason: reason.into(),
        }
    }
}

pub trait PlayerControlPort: Send + 'static {
    fn dispatch(&mut self, control: &PlayerControl) -> ControlDispatchOutcome;
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SearchCandidate {
    pub text: String,
    pub uri: String,
}

impl SearchCandidate {
    pub fn new(text: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            uri: uri.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickedCandidate {
    pub candidate: SearchCandidate,
    pub formatted_candidates: String,
}

impl PickedCandidate {
    pub fn new(candidate: SearchCandidate, formatted_candidates: impl Into<String>) -> Self {
        Self {
            candidate,
            formatted_candidates: formatted_candidates.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerSearchError {
    message: String,
}

impl PlayerSearchError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl Display for PlayerSearchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for PlayerSearchError {}

pub trait PlayerSearchPort: Send + 'static {
    fn search_text(&mut self, keyword: &str, source: &str) -> Result<String, PlayerSearchError>;

    fn search_candidates(
        &mut self,
        keyword: &str,
        source: &str,
    ) -> Result<Vec<SearchCandidate>, PlayerSearchError>;

    fn search_and_pick(
        &mut self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> Result<Option<PickedCandidate>, PlayerSearchError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerSearch {
    Text {
        keyword: String,
        source: String,
    },
    Candidates {
        keyword: String,
        source: String,
    },
    Pick {
        keyword: String,
        source: String,
        prefer_accompaniment: bool,
    },
}

impl PlayerSearch {
    pub fn text(keyword: impl Into<String>, source: impl Into<String>) -> Self {
        Self::Text {
            keyword: keyword.into(),
            source: source.into(),
        }
    }

    pub fn candidates(keyword: impl Into<String>, source: impl Into<String>) -> Self {
        Self::Candidates {
            keyword: keyword.into(),
            source: source.into(),
        }
    }

    pub fn pick(
        keyword: impl Into<String>,
        source: impl Into<String>,
        prefer_accompaniment: bool,
    ) -> Self {
        Self::Pick {
            keyword: keyword.into(),
            source: source.into(),
            prefer_accompaniment,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerSearchOutcome {
    Text(String),
    Candidates(Vec<SearchCandidate>),
    Picked(Option<PickedCandidate>),
    Failed(PlayerSearchError),
    NotRun { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlOperationResult {
    pub operation_id: BusinessOperationId,
    pub control: PlayerControl,
    pub outcome: ControlDispatchOutcome,
    pub started_at: Option<Instant>,
    pub finished_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchOperationResult {
    pub operation_id: BusinessOperationId,
    pub search: PlayerSearch,
    pub outcome: PlayerSearchOutcome,
    pub started_at: Option<Instant>,
    pub finished_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerLane {
    Observation,
    Control,
    Search,
}

impl Display for PlayerLane {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Observation => formatter.write_str("observation"),
            Self::Control => formatter.write_str("control"),
            Self::Search => formatter.write_str("search"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerLaneError {
    QueueFull(PlayerLane),
    RuntimeStopped(PlayerLane),
}

impl Display for PlayerLaneError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull(lane) => write!(formatter, "player {lane} lane queue is full"),
            Self::RuntimeStopped(lane) => write!(formatter, "player {lane} lane is stopped"),
        }
    }
}

impl Error for PlayerLaneError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerOperationReceiveError {
    TimedOut(PlayerLane),
    RuntimeStopped(PlayerLane),
    AlreadyCompleted(PlayerLane),
}

impl Display for PlayerOperationReceiveError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut(lane) => write!(formatter, "timed out waiting for player {lane} result"),
            Self::RuntimeStopped(lane) => {
                write!(
                    formatter,
                    "player {lane} lane stopped before returning a result"
                )
            }
            Self::AlreadyCompleted(lane) => {
                write!(
                    formatter,
                    "player {lane} operation result was already received"
                )
            }
        }
    }
}

impl Error for PlayerOperationReceiveError {}

enum PlayerOperationState<T> {
    Pending(Receiver<T>),
    Completed,
    Disconnected,
}

pub struct PlayerOperation<T> {
    operation_id: BusinessOperationId,
    lane: PlayerLane,
    state: Mutex<PlayerOperationState<T>>,
}

impl<T> PlayerOperation<T> {
    fn new(operation_id: BusinessOperationId, lane: PlayerLane, response: Receiver<T>) -> Self {
        Self {
            operation_id,
            lane,
            state: Mutex::new(PlayerOperationState::Pending(response)),
        }
    }

    pub const fn operation_id(&self) -> BusinessOperationId {
        self.operation_id
    }

    pub fn wait(&self) -> Result<T, PlayerOperationReceiveError> {
        let mut state = self.lock_state();
        let result = match &*state {
            PlayerOperationState::Pending(response) => response.recv(),
            PlayerOperationState::Completed => {
                return Err(PlayerOperationReceiveError::AlreadyCompleted(self.lane));
            }
            PlayerOperationState::Disconnected => {
                return Err(PlayerOperationReceiveError::RuntimeStopped(self.lane));
            }
        };
        match result {
            Ok(result) => {
                *state = PlayerOperationState::Completed;
                Ok(result)
            }
            Err(_) => {
                *state = PlayerOperationState::Disconnected;
                Err(PlayerOperationReceiveError::RuntimeStopped(self.lane))
            }
        }
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Result<T, PlayerOperationReceiveError> {
        let mut state = self.lock_state();
        let result = match &*state {
            PlayerOperationState::Pending(response) => response.recv_timeout(timeout),
            PlayerOperationState::Completed => {
                return Err(PlayerOperationReceiveError::AlreadyCompleted(self.lane));
            }
            PlayerOperationState::Disconnected => {
                return Err(PlayerOperationReceiveError::RuntimeStopped(self.lane));
            }
        };
        match result {
            Ok(result) => {
                *state = PlayerOperationState::Completed;
                Ok(result)
            }
            Err(RecvTimeoutError::Timeout) => Err(PlayerOperationReceiveError::TimedOut(self.lane)),
            Err(RecvTimeoutError::Disconnected) => {
                *state = PlayerOperationState::Disconnected;
                Err(PlayerOperationReceiveError::RuntimeStopped(self.lane))
            }
        }
    }

    pub fn try_result(&self) -> Result<Option<T>, PlayerOperationReceiveError> {
        let mut state = self.lock_state();
        let result = match &*state {
            PlayerOperationState::Pending(response) => response.try_recv(),
            PlayerOperationState::Completed => {
                return Err(PlayerOperationReceiveError::AlreadyCompleted(self.lane));
            }
            PlayerOperationState::Disconnected => {
                return Err(PlayerOperationReceiveError::RuntimeStopped(self.lane));
            }
        };
        match result {
            Ok(result) => {
                *state = PlayerOperationState::Completed;
                Ok(Some(result))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                *state = PlayerOperationState::Disconnected;
                Err(PlayerOperationReceiveError::RuntimeStopped(self.lane))
            }
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, PlayerOperationState<T>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub type ControlOperation = PlayerOperation<ControlOperationResult>;
pub type SearchOperation = PlayerOperation<SearchOperationResult>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlayerObservationRevision(u64);

impl PlayerObservationRevision {
    pub const INITIAL: Self = Self(0);

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug)]
pub struct RevisedPlayerObservation {
    revision: PlayerObservationRevision,
    observation: Arc<PlayerObservation>,
    read_error: Option<PlayerObservationReadError>,
}

impl RevisedPlayerObservation {
    pub const fn revision(&self) -> PlayerObservationRevision {
        self.revision
    }

    pub fn observation(&self) -> &PlayerObservation {
        &self.observation
    }

    pub fn shared_observation(&self) -> Arc<PlayerObservation> {
        Arc::clone(&self.observation)
    }

    pub fn read_error(&self) -> Option<&PlayerObservationReadError> {
        self.read_error.as_ref()
    }

    fn reevaluated_at(&self, now: Instant, stale_timeout: Duration) -> Self {
        Self {
            revision: self.revision,
            observation: Arc::new(self.observation.reevaluated_at(now, stale_timeout)),
            read_error: self.read_error.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ObservationWaitOutcome {
    Advanced(RevisedPlayerObservation),
    TimedOut,
    RuntimeStopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FastObservationDemandStatus {
    Active,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FastObservationCancelStatus {
    Cancelled,
    NotActive,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastObservationDemandResult {
    pub operation_id: BusinessOperationId,
    pub status: Result<FastObservationDemandStatus, PlayerLaneError>,
}

pub type FastObservationOperation = PlayerOperation<FastObservationDemandResult>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastObservationCancelResult {
    pub operation_id: BusinessOperationId,
    pub status: Result<FastObservationCancelStatus, PlayerLaneError>,
}

pub type FastObservationCancelOperation = PlayerOperation<FastObservationCancelResult>;

#[derive(Default)]
struct LatestObservationState {
    latest: Option<RevisedPlayerObservation>,
    closed: bool,
}

struct LatestObservationStore {
    state: Mutex<LatestObservationState>,
    changed: Condvar,
    clock: Arc<dyn Clock>,
    stale_timeout: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObservationPublishOutcome {
    Published,
    StoreClosed,
    RevisionExhausted,
}

impl LatestObservationStore {
    fn new(stale_timeout: Duration, clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Mutex::new(LatestObservationState::default()),
            changed: Condvar::new(),
            clock,
            stale_timeout,
        }
    }

    fn publish(
        &self,
        observation: PlayerObservation,
        read_error: Option<PlayerObservationReadError>,
    ) -> ObservationPublishOutcome {
        let mut state = self.lock();
        if state.closed {
            return ObservationPublishOutcome::StoreClosed;
        }
        let next_revision = state
            .latest
            .as_ref()
            .map_or(Some(1), |latest| latest.revision.0.checked_add(1));
        let Some(next_revision) = next_revision else {
            state.closed = true;
            self.changed.notify_all();
            return ObservationPublishOutcome::RevisionExhausted;
        };
        state.latest = Some(RevisedPlayerObservation {
            revision: PlayerObservationRevision(next_revision),
            observation: Arc::new(observation),
            read_error,
        });
        self.changed.notify_all();
        ObservationPublishOutcome::Published
    }

    fn latest(&self) -> Option<RevisedPlayerObservation> {
        let now = self.clock.now();
        self.lock()
            .latest
            .as_ref()
            .map(|latest| latest.reevaluated_at(now, self.stale_timeout))
    }

    fn wait_after(
        &self,
        revision: PlayerObservationRevision,
        timeout: Duration,
    ) -> ObservationWaitOutcome {
        let started_at = Instant::now();
        let mut state = self.lock();
        loop {
            if let Some(latest) = state
                .latest
                .as_ref()
                .filter(|latest| latest.revision > revision)
            {
                return ObservationWaitOutcome::Advanced(
                    latest.reevaluated_at(self.clock.now(), self.stale_timeout),
                );
            }
            if state.closed {
                return ObservationWaitOutcome::RuntimeStopped;
            }
            let remaining = timeout.saturating_sub(started_at.elapsed());
            if remaining.is_zero() {
                return ObservationWaitOutcome::TimedOut;
            }
            let waited = self.changed.wait_timeout(state, remaining);
            let (next_state, timed_out) = match waited {
                Ok((next_state, result)) => (next_state, result.timed_out()),
                Err(poisoned) => {
                    let (next_state, result) = poisoned.into_inner();
                    (next_state, result.timed_out())
                }
            };
            state = next_state;
            if timed_out
                && state
                    .latest
                    .as_ref()
                    .is_none_or(|latest| latest.revision <= revision)
            {
                return if state.closed {
                    ObservationWaitOutcome::RuntimeStopped
                } else {
                    ObservationWaitOutcome::TimedOut
                };
            }
        }
    }

    fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        self.changed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, LatestObservationState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct RuntimeLane<M> {
    sender: SyncSender<M>,
    accepting: Mutex<bool>,
    accepting_changed: Condvar,
    stopping: AtomicBool,
}

impl<M> RuntimeLane<M> {
    fn new(sender: SyncSender<M>) -> Self {
        Self {
            sender,
            accepting: Mutex::new(true),
            accepting_changed: Condvar::new(),
            stopping: AtomicBool::new(false),
        }
    }

    fn try_submit(&self, message: M, lane: PlayerLane) -> Result<(), PlayerLaneError> {
        let accepting = self
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*accepting {
            return Err(PlayerLaneError::RuntimeStopped(lane));
        }
        match self.sender.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(PlayerLaneError::QueueFull(lane)),
            Err(TrySendError::Disconnected(_)) => Err(PlayerLaneError::RuntimeStopped(lane)),
        }
    }

    fn begin_shutdown(&self, wake: M) {
        let mut accepting = self
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *accepting = false;
        self.stopping.store(true, Ordering::Release);
        self.accepting_changed.notify_all();
        let _ = self.sender.try_send(wake);
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    #[cfg(test)]
    fn wait_until_stopped(&self, timeout: Duration) -> bool {
        let started_at = Instant::now();
        let mut accepting = self
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *accepting {
            let remaining = timeout.saturating_sub(started_at.elapsed());
            if remaining.is_zero() {
                return false;
            }
            accepting = match self.accepting_changed.wait_timeout(accepting, remaining) {
                Ok((accepting, _)) => accepting,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        true
    }
}

struct ControlRequest {
    operation_id: BusinessOperationId,
    control: PlayerControl,
    response: SyncSender<ControlOperationResult>,
}

enum ControlCommand {
    Dispatch(ControlRequest),
    Shutdown,
}

struct SearchRequest {
    operation_id: BusinessOperationId,
    search: PlayerSearch,
    response: SyncSender<SearchOperationResult>,
}

enum SearchCommand {
    Execute(SearchRequest),
    Shutdown,
}

enum ObservationCommand {
    RequestFast {
        operation_id: BusinessOperationId,
        expires_at: Instant,
        response: SyncSender<FastObservationDemandResult>,
    },
    CancelFast {
        operation_id: BusinessOperationId,
        response: SyncSender<FastObservationCancelResult>,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct PlayerRuntimeHandle {
    observation: Arc<RuntimeLane<ObservationCommand>>,
    control: Arc<RuntimeLane<ControlCommand>>,
    search: Arc<RuntimeLane<SearchCommand>>,
    latest_observation: Arc<LatestObservationStore>,
}

impl PlayerRuntimeHandle {
    pub fn submit_control(
        &self,
        operation_id: BusinessOperationId,
        control: PlayerControl,
    ) -> Result<ControlOperation, PlayerLaneError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.control.try_submit(
            ControlCommand::Dispatch(ControlRequest {
                operation_id,
                control,
                response,
            }),
            PlayerLane::Control,
        )?;
        Ok(PlayerOperation::new(
            operation_id,
            PlayerLane::Control,
            receiver,
        ))
    }

    pub fn submit_search(
        &self,
        operation_id: BusinessOperationId,
        search: PlayerSearch,
    ) -> Result<SearchOperation, PlayerLaneError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.search.try_submit(
            SearchCommand::Execute(SearchRequest {
                operation_id,
                search,
                response,
            }),
            PlayerLane::Search,
        )?;
        Ok(PlayerOperation::new(
            operation_id,
            PlayerLane::Search,
            receiver,
        ))
    }

    pub fn request_fast_observation(
        &self,
        operation_id: BusinessOperationId,
        expires_at: Instant,
    ) -> Result<FastObservationOperation, PlayerLaneError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.observation.try_submit(
            ObservationCommand::RequestFast {
                operation_id,
                expires_at,
                response,
            },
            PlayerLane::Observation,
        )?;
        Ok(PlayerOperation::new(
            operation_id,
            PlayerLane::Observation,
            receiver,
        ))
    }

    pub fn cancel_fast_observation(
        &self,
        operation_id: BusinessOperationId,
    ) -> Result<FastObservationCancelOperation, PlayerLaneError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.observation.try_submit(
            ObservationCommand::CancelFast {
                operation_id,
                response,
            },
            PlayerLane::Observation,
        )?;
        Ok(PlayerOperation::new(
            operation_id,
            PlayerLane::Observation,
            receiver,
        ))
    }

    pub fn latest_observation(&self) -> Option<RevisedPlayerObservation> {
        self.latest_observation.latest()
    }

    pub fn latest_observation_revision(&self) -> PlayerObservationRevision {
        self.latest_observation()
            .map_or(PlayerObservationRevision::INITIAL, |latest| {
                latest.revision()
            })
    }

    /// Waits for a publication newer than `revision`.
    ///
    /// Snapshot aging is evaluated when a value is returned, but aging alone does not wake this
    /// wait. Only a new publication, runtime closure, or the requested timeout does so.
    pub fn wait_for_observation_after(
        &self,
        revision: PlayerObservationRevision,
        timeout: Duration,
    ) -> ObservationWaitOutcome {
        self.latest_observation.wait_after(revision, timeout)
    }
}

#[derive(Clone)]
pub struct PlayerSearchClient {
    runtime: PlayerRuntimeHandle,
    operation_ids: BusinessOperationIdAllocator,
}

impl PlayerSearchClient {
    pub fn new(runtime: PlayerRuntimeHandle, operation_ids: BusinessOperationIdAllocator) -> Self {
        Self {
            runtime,
            operation_ids,
        }
    }

    pub fn search_text(
        &self,
        keyword: &str,
        source: &str,
    ) -> Result<String, PlayerSearchClientError> {
        match self.execute(PlayerSearch::text(keyword, source))? {
            PlayerSearchOutcome::Text(text) => Ok(text),
            _ => Err(PlayerSearchClientError::UnexpectedOutcome("text")),
        }
    }

    pub fn search_candidates(
        &self,
        keyword: &str,
        source: &str,
    ) -> Result<Vec<SearchCandidate>, PlayerSearchClientError> {
        match self.execute(PlayerSearch::candidates(keyword, source))? {
            PlayerSearchOutcome::Candidates(candidates) => Ok(candidates),
            _ => Err(PlayerSearchClientError::UnexpectedOutcome("candidates")),
        }
    }

    pub fn search_and_pick(
        &self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> Result<Option<PickedCandidate>, PlayerSearchClientError> {
        match self.execute(PlayerSearch::pick(keyword, source, prefer_accompaniment))? {
            PlayerSearchOutcome::Picked(candidate) => Ok(candidate),
            _ => Err(PlayerSearchClientError::UnexpectedOutcome(
                "picked candidate",
            )),
        }
    }

    fn execute(
        &self,
        search: PlayerSearch,
    ) -> Result<PlayerSearchOutcome, PlayerSearchClientError> {
        let operation_id = self
            .operation_ids
            .allocate()
            .map_err(|_| PlayerSearchClientError::OperationIdExhausted)?;
        let operation = self
            .runtime
            .submit_search(operation_id, search)
            .map_err(PlayerSearchClientError::from_lane_error)?;
        let result = operation
            .wait()
            .map_err(PlayerSearchClientError::from_receive_error)?;
        match result.outcome {
            PlayerSearchOutcome::Failed(error) => Err(PlayerSearchClientError::Failed(error)),
            PlayerSearchOutcome::NotRun { reason } => {
                Err(PlayerSearchClientError::NotRun { reason })
            }
            outcome => Ok(outcome),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlayerSearchClientError {
    OperationIdExhausted,
    QueueFull,
    RuntimeStopped,
    Failed(PlayerSearchError),
    NotRun { reason: String },
    UnexpectedOutcome(&'static str),
}

impl PlayerSearchClientError {
    fn from_lane_error(error: PlayerLaneError) -> Self {
        match error {
            PlayerLaneError::QueueFull(PlayerLane::Search) => Self::QueueFull,
            PlayerLaneError::RuntimeStopped(PlayerLane::Search) => Self::RuntimeStopped,
            PlayerLaneError::QueueFull(_) | PlayerLaneError::RuntimeStopped(_) => {
                Self::UnexpectedOutcome("search lane")
            }
        }
    }

    fn from_receive_error(error: PlayerOperationReceiveError) -> Self {
        match error {
            PlayerOperationReceiveError::RuntimeStopped(PlayerLane::Search) => Self::RuntimeStopped,
            PlayerOperationReceiveError::TimedOut(_)
            | PlayerOperationReceiveError::RuntimeStopped(_)
            | PlayerOperationReceiveError::AlreadyCompleted(_) => {
                Self::UnexpectedOutcome("pending search result")
            }
        }
    }
}

impl Display for PlayerSearchClientError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OperationIdExhausted => {
                formatter.write_str("player operation identifiers are exhausted")
            }
            Self::QueueFull => formatter.write_str("player search lane queue is full"),
            Self::RuntimeStopped => formatter.write_str("player search lane is stopped"),
            Self::Failed(error) => write!(formatter, "player search failed: {error}"),
            Self::NotRun { reason } => write!(formatter, "player search was not run: {reason}"),
            Self::UnexpectedOutcome(expected) => {
                write!(
                    formatter,
                    "player search returned an unexpected result; expected {expected}"
                )
            }
        }
    }
}

impl Error for PlayerSearchClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum PlayerRuntimeStartError {
    InvalidConfig(PlayerRuntimeConfigError),
    Spawn {
        lane: PlayerLane,
        source: std::io::Error,
    },
}

impl Display for PlayerRuntimeStartError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(error) => error.fmt(formatter),
            Self::Spawn { lane, source } => {
                write!(formatter, "failed to start player {lane} lane: {source}")
            }
        }
    }
}

impl Error for PlayerRuntimeStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(error) => Some(error),
            Self::Spawn { source, .. } => Some(source),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct PlayerShutdownError {
    panicked_lanes: Vec<PlayerLane>,
}

impl PlayerShutdownError {
    pub fn panicked_lanes(&self) -> &[PlayerLane] {
        &self.panicked_lanes
    }
}

impl Display for PlayerShutdownError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("player runtime worker panicked: ")?;
        for (index, lane) in self.panicked_lanes.iter().enumerate() {
            if index > 0 {
                formatter.write_str(", ")?;
            }
            lane.fmt(formatter)?;
        }
        Ok(())
    }
}

impl Error for PlayerShutdownError {}

pub struct PlayerRuntime {
    handle: PlayerRuntimeHandle,
    observation_worker: Option<JoinHandle<()>>,
    control_worker: Option<JoinHandle<()>>,
    search_worker: Option<JoinHandle<()>>,
}

impl PlayerRuntime {
    pub fn start(
        observation_port: impl PlayerObservationPort,
        control_port: impl PlayerControlPort,
        search_port: impl PlayerSearchPort,
        config: PlayerRuntimeConfig,
    ) -> Result<Self, PlayerRuntimeStartError> {
        Self::start_with_clock(
            observation_port,
            control_port,
            search_port,
            config,
            Arc::new(SystemClock),
        )
    }

    fn start_with_clock(
        observation_port: impl PlayerObservationPort,
        control_port: impl PlayerControlPort,
        search_port: impl PlayerSearchPort,
        config: PlayerRuntimeConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, PlayerRuntimeStartError> {
        config
            .validate()
            .map_err(PlayerRuntimeStartError::InvalidConfig)?;

        let (observation_sender, observation_receiver) =
            mpsc::sync_channel(config.observation_command_capacity);
        let (control_sender, control_receiver) = mpsc::sync_channel(config.control_queue_capacity);
        let (search_sender, search_receiver) = mpsc::sync_channel(config.search_queue_capacity);
        let observation = Arc::new(RuntimeLane::new(observation_sender));
        let control = Arc::new(RuntimeLane::new(control_sender));
        let search = Arc::new(RuntimeLane::new(search_sender));
        let latest_observation = Arc::new(LatestObservationStore::new(
            config.observation.stale_timeout,
            clock,
        ));

        let worker_observation = Arc::clone(&observation);
        let worker_latest = Arc::clone(&latest_observation);
        let observation_worker = thread::Builder::new()
            .name("player-observation-runtime".to_string())
            .spawn(move || {
                run_observation_lane(
                    observation_port,
                    observation_receiver,
                    worker_observation,
                    worker_latest,
                    config,
                );
            })
            .map_err(|source| PlayerRuntimeStartError::Spawn {
                lane: PlayerLane::Observation,
                source,
            })?;

        let worker_control = Arc::clone(&control);
        let control_worker = match thread::Builder::new()
            .name("player-control-runtime".to_string())
            .spawn(move || run_control_lane(control_port, control_receiver, worker_control))
        {
            Ok(worker) => worker,
            Err(source) => {
                latest_observation.close();
                observation.begin_shutdown(ObservationCommand::Shutdown);
                let _ = observation_worker.join();
                return Err(PlayerRuntimeStartError::Spawn {
                    lane: PlayerLane::Control,
                    source,
                });
            }
        };

        let worker_search = Arc::clone(&search);
        let search_worker = match thread::Builder::new()
            .name("player-search-runtime".to_string())
            .spawn(move || run_search_lane(search_port, search_receiver, worker_search))
        {
            Ok(worker) => worker,
            Err(source) => {
                latest_observation.close();
                observation.begin_shutdown(ObservationCommand::Shutdown);
                control.begin_shutdown(ControlCommand::Shutdown);
                let _ = observation_worker.join();
                let _ = control_worker.join();
                return Err(PlayerRuntimeStartError::Spawn {
                    lane: PlayerLane::Search,
                    source,
                });
            }
        };

        Ok(Self {
            handle: PlayerRuntimeHandle {
                observation,
                control,
                search,
                latest_observation,
            },
            observation_worker: Some(observation_worker),
            control_worker: Some(control_worker),
            search_worker: Some(search_worker),
        })
    }

    pub fn handle(&self) -> PlayerRuntimeHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) -> Result<(), PlayerShutdownError> {
        self.stop_workers()
    }

    fn stop_workers(&mut self) -> Result<(), PlayerShutdownError> {
        if self.observation_worker.is_none()
            && self.control_worker.is_none()
            && self.search_worker.is_none()
        {
            return Ok(());
        }

        self.handle.latest_observation.close();
        self.handle
            .observation
            .begin_shutdown(ObservationCommand::Shutdown);
        self.handle.control.begin_shutdown(ControlCommand::Shutdown);
        self.handle.search.begin_shutdown(SearchCommand::Shutdown);

        let mut panicked_lanes = Vec::new();
        join_lane(
            &mut self.observation_worker,
            PlayerLane::Observation,
            &mut panicked_lanes,
        );
        join_lane(
            &mut self.control_worker,
            PlayerLane::Control,
            &mut panicked_lanes,
        );
        join_lane(
            &mut self.search_worker,
            PlayerLane::Search,
            &mut panicked_lanes,
        );
        if panicked_lanes.is_empty() {
            Ok(())
        } else {
            Err(PlayerShutdownError { panicked_lanes })
        }
    }
}

impl Drop for PlayerRuntime {
    fn drop(&mut self) {
        let _ = self.stop_workers();
    }
}

fn join_lane(
    worker: &mut Option<JoinHandle<()>>,
    lane: PlayerLane,
    panicked_lanes: &mut Vec<PlayerLane>,
) {
    if worker.take().is_some_and(|worker| worker.join().is_err()) {
        panicked_lanes.push(lane);
    }
}

struct ObservationCloseGuard(Arc<LatestObservationStore>);

impl Drop for ObservationCloseGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn run_observation_lane(
    mut port: impl PlayerObservationPort,
    receiver: Receiver<ObservationCommand>,
    lane: Arc<RuntimeLane<ObservationCommand>>,
    latest: Arc<LatestObservationStore>,
    config: PlayerRuntimeConfig,
) {
    let _close_guard = ObservationCloseGuard(Arc::clone(&latest));
    let mut observer = PlayerObserver::new(SystemClock, config.observation);
    let mut active_fast_demands = HashMap::<BusinessOperationId, Instant>::new();
    let mut last_sample_completed_at = None;

    loop {
        if lane.is_stopping() {
            drain_observation_commands(&receiver);
            break;
        }

        let now = Instant::now();
        active_fast_demands.retain(|_, expires_at| *expires_at > now);
        let interval = if active_fast_demands.is_empty() {
            config.normal_observation_interval
        } else {
            config.fast_observation_interval
        };
        let sample_due = last_sample_completed_at
            .and_then(|completed_at: Instant| completed_at.checked_add(interval))
            .or(Some(now).filter(|_| last_sample_completed_at.is_none()));
        if sample_due.is_some_and(|due| due <= now) {
            let (observation, read_error) = match port.read_sample() {
                Ok(sample) => (observer.observe_sample(sample), None),
                Err(error) => (observer.observe_failure(), Some(error)),
            };
            match latest.publish(observation, read_error) {
                ObservationPublishOutcome::Published => {
                    last_sample_completed_at = Some(Instant::now());
                    continue;
                }
                ObservationPublishOutcome::StoreClosed
                | ObservationPublishOutcome::RevisionExhausted => {
                    lane.begin_shutdown(ObservationCommand::Shutdown);
                    drain_observation_commands(&receiver);
                    break;
                }
            }
        }

        let next_expiry = active_fast_demands.values().copied().min();
        let next_wake = match (sample_due, next_expiry) {
            (Some(sample), Some(expiry)) => Some(sample.min(expiry)),
            (Some(sample), None) => Some(sample),
            (None, Some(expiry)) => Some(expiry),
            (None, None) => None,
        };
        let command = match next_wake {
            Some(deadline) => {
                match receiver.recv_timeout(deadline.saturating_duration_since(now)) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match receiver.recv() {
                Ok(command) => Some(command),
                Err(_) => break,
            },
        };
        let Some(command) = command else {
            continue;
        };
        if lane.is_stopping() {
            complete_observation_stopped(command);
            drain_observation_commands(&receiver);
            break;
        }
        match command {
            ObservationCommand::RequestFast {
                operation_id,
                expires_at,
                response,
            } => {
                let now = Instant::now();
                active_fast_demands.retain(|_, deadline| *deadline > now);
                let result = if let Some(active_until) = active_fast_demands.get_mut(&operation_id)
                {
                    *active_until = (*active_until).max(expires_at);
                    Ok(FastObservationDemandStatus::Active)
                } else if expires_at <= now {
                    Ok(FastObservationDemandStatus::Expired)
                } else if active_fast_demands.len() < config.active_fast_demand_capacity {
                    active_fast_demands.insert(operation_id, expires_at);
                    Ok(FastObservationDemandStatus::Active)
                } else {
                    Err(PlayerLaneError::QueueFull(PlayerLane::Observation))
                };
                let _ = response.send(FastObservationDemandResult {
                    operation_id,
                    status: result,
                });
            }
            ObservationCommand::CancelFast {
                operation_id,
                response,
            } => {
                let status = if active_fast_demands.remove(&operation_id).is_some() {
                    FastObservationCancelStatus::Cancelled
                } else {
                    FastObservationCancelStatus::NotActive
                };
                let _ = response.send(FastObservationCancelResult {
                    operation_id,
                    status: Ok(status),
                });
            }
            ObservationCommand::Shutdown => {
                drain_observation_commands(&receiver);
                break;
            }
        }
    }
}

fn drain_observation_commands(receiver: &Receiver<ObservationCommand>) {
    while let Ok(command) = receiver.try_recv() {
        complete_observation_stopped(command);
    }
}

fn complete_observation_stopped(command: ObservationCommand) {
    match command {
        ObservationCommand::RequestFast {
            operation_id,
            response,
            ..
        } => {
            let _ = response.send(FastObservationDemandResult {
                operation_id,
                status: Err(PlayerLaneError::RuntimeStopped(PlayerLane::Observation)),
            });
        }
        ObservationCommand::CancelFast {
            operation_id,
            response,
        } => {
            let _ = response.send(FastObservationCancelResult {
                operation_id,
                status: Err(PlayerLaneError::RuntimeStopped(PlayerLane::Observation)),
            });
        }
        ObservationCommand::Shutdown => {}
    }
}

fn run_control_lane(
    mut port: impl PlayerControlPort,
    receiver: Receiver<ControlCommand>,
    lane: Arc<RuntimeLane<ControlCommand>>,
) {
    loop {
        let command = match receiver.recv() {
            Ok(command) => command,
            Err(_) => break,
        };
        if lane.is_stopping() {
            complete_control_not_sent(command);
            drain_control_commands(&receiver);
            break;
        }
        match command {
            ControlCommand::Dispatch(request) => {
                let started_at = Instant::now();
                let outcome = port.dispatch(&request.control);
                let _ = request.response.send(ControlOperationResult {
                    operation_id: request.operation_id,
                    control: request.control,
                    outcome,
                    started_at: Some(started_at),
                    finished_at: Instant::now(),
                });
            }
            ControlCommand::Shutdown => {
                drain_control_commands(&receiver);
                break;
            }
        }
    }
}

fn complete_control_not_sent(command: ControlCommand) {
    if let ControlCommand::Dispatch(request) = command {
        let _ = request.response.send(ControlOperationResult {
            operation_id: request.operation_id,
            control: request.control,
            outcome: ControlDispatchOutcome::not_sent("player runtime shut down before dispatch"),
            started_at: None,
            finished_at: Instant::now(),
        });
    }
}

fn drain_control_commands(receiver: &Receiver<ControlCommand>) {
    while let Ok(command) = receiver.try_recv() {
        complete_control_not_sent(command);
    }
}

fn run_search_lane(
    mut port: impl PlayerSearchPort,
    receiver: Receiver<SearchCommand>,
    lane: Arc<RuntimeLane<SearchCommand>>,
) {
    loop {
        let command = match receiver.recv() {
            Ok(command) => command,
            Err(_) => break,
        };
        if lane.is_stopping() {
            complete_search_not_run(command);
            drain_search_commands(&receiver);
            break;
        }
        match command {
            SearchCommand::Execute(request) => {
                let started_at = Instant::now();
                let outcome = execute_search(&mut port, &request.search);
                let _ = request.response.send(SearchOperationResult {
                    operation_id: request.operation_id,
                    search: request.search,
                    outcome,
                    started_at: Some(started_at),
                    finished_at: Instant::now(),
                });
            }
            SearchCommand::Shutdown => {
                drain_search_commands(&receiver);
                break;
            }
        }
    }
}

fn execute_search(port: &mut impl PlayerSearchPort, search: &PlayerSearch) -> PlayerSearchOutcome {
    match search {
        PlayerSearch::Text { keyword, source } => port
            .search_text(keyword, source)
            .map(PlayerSearchOutcome::Text)
            .unwrap_or_else(PlayerSearchOutcome::Failed),
        PlayerSearch::Candidates { keyword, source } => port
            .search_candidates(keyword, source)
            .map(PlayerSearchOutcome::Candidates)
            .unwrap_or_else(PlayerSearchOutcome::Failed),
        PlayerSearch::Pick {
            keyword,
            source,
            prefer_accompaniment,
        } => port
            .search_and_pick(keyword, source, *prefer_accompaniment)
            .map(PlayerSearchOutcome::Picked)
            .unwrap_or_else(PlayerSearchOutcome::Failed),
    }
}

fn complete_search_not_run(command: SearchCommand) {
    if let SearchCommand::Execute(request) = command {
        let _ = request.response.send(SearchOperationResult {
            operation_id: request.operation_id,
            search: request.search,
            outcome: PlayerSearchOutcome::NotRun {
                reason: "player runtime shut down before search".to_string(),
            },
            started_at: None,
            finished_at: Instant::now(),
        });
    }
}

fn drain_search_commands(receiver: &Receiver<SearchCommand>) {
    while let Ok(command) = receiver.try_recv() {
        complete_search_not_run(command);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::{
        ControlDispatchOutcome, FastObservationCancelStatus, FastObservationDemandStatus,
        ObservationWaitOutcome, PickedCandidate, PlayerControl, PlayerControlPort, PlayerLane,
        PlayerLaneError, PlayerObservationPort, PlayerObservationReadError,
        PlayerOperationReceiveError, PlayerRuntime, PlayerRuntimeConfig, PlayerRuntimeConfigError,
        PlayerSearch, PlayerSearchClient, PlayerSearchClientError, PlayerSearchError,
        PlayerSearchOutcome, PlayerSearchPort, SearchCandidate,
    };
    use crate::runtime::clock::{ManualClock, SystemClock};
    use crate::runtime::identity::{BusinessOperationId, BusinessOperationIdAllocator};
    use crate::runtime::player::PlayerObservationConfig;
    use crate::runtime::player::{
        ObservationFreshness, PlayerObserver, RawPlayerSample, TransportState,
    };

    const FAKE_PORT_BLOCK_TIMEOUT: Duration = Duration::from_secs(2);

    struct ReleaseOnDrop(Option<SyncSender<()>>);

    impl ReleaseOnDrop {
        fn new(sender: SyncSender<()>) -> Self {
            Self(Some(sender))
        }

        fn release(mut self) {
            if let Some(sender) = self.0.take() {
                sender.send(()).unwrap();
            }
        }
    }

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    struct ConstantObservationPort;

    impl PlayerObservationPort for ConstantObservationPort {
        fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
            Ok(RawPlayerSample::new(
                "fuo://song/observation",
                TransportState::Playing,
            ))
        }
    }

    struct RecordingControlPort {
        calls: Arc<Mutex<Vec<PlayerControl>>>,
        outcomes: VecDeque<ControlDispatchOutcome>,
    }

    impl PlayerControlPort for RecordingControlPort {
        fn dispatch(&mut self, control: &PlayerControl) -> ControlDispatchOutcome {
            self.calls.lock().unwrap().push(control.clone());
            self.outcomes
                .pop_front()
                .expect("one fake outcome per call")
        }
    }

    struct EmptySearchPort;

    impl PlayerSearchPort for EmptySearchPort {
        fn search_text(
            &mut self,
            _keyword: &str,
            _source: &str,
        ) -> Result<String, PlayerSearchError> {
            Ok(String::new())
        }

        fn search_candidates(
            &mut self,
            _keyword: &str,
            _source: &str,
        ) -> Result<Vec<SearchCandidate>, PlayerSearchError> {
            Ok(Vec::new())
        }

        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> Result<Option<PickedCandidate>, PlayerSearchError> {
            Ok(None)
        }
    }

    struct FailingSearchPort;

    impl PlayerSearchPort for FailingSearchPort {
        fn search_text(
            &mut self,
            _keyword: &str,
            _source: &str,
        ) -> Result<String, PlayerSearchError> {
            Err(PlayerSearchError::new("backend failed"))
        }

        fn search_candidates(
            &mut self,
            _keyword: &str,
            _source: &str,
        ) -> Result<Vec<SearchCandidate>, PlayerSearchError> {
            Err(PlayerSearchError::new("backend failed"))
        }

        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> Result<Option<PickedCandidate>, PlayerSearchError> {
            Err(PlayerSearchError::new("backend failed"))
        }
    }

    struct ScriptedObservationPort {
        samples: VecDeque<Result<RawPlayerSample, PlayerObservationReadError>>,
    }

    impl PlayerObservationPort for ScriptedObservationPort {
        fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
            self.samples.pop_front().unwrap_or_else(|| {
                Err(PlayerObservationReadError::new(
                    "scripted observation exhausted",
                ))
            })
        }
    }

    struct BlockingObservationPort {
        started: SyncSender<()>,
        release: Receiver<()>,
        block_next: bool,
    }

    struct BlockingAfterSamplesObservationPort {
        samples_before_block: usize,
        started: SyncSender<()>,
        release: Receiver<()>,
        blocked: bool,
    }

    impl PlayerObservationPort for BlockingAfterSamplesObservationPort {
        fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
            if self.samples_before_block > 0 {
                self.samples_before_block -= 1;
            } else if !self.blocked {
                self.blocked = true;
                self.started.send(()).unwrap();
                self.release
                    .recv_timeout(FAKE_PORT_BLOCK_TIMEOUT)
                    .expect("test observation port release timed out");
            }
            let mut sample =
                RawPlayerSample::new("fuo://song/observation", TransportState::Playing);
            sample.title = Some("cached title".to_string());
            sample.progress = Some(Duration::from_secs(20));
            sample.duration = Some(Duration::from_secs(180));
            Ok(sample)
        }
    }

    impl PlayerObservationPort for BlockingObservationPort {
        fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
            if self.block_next {
                self.block_next = false;
                self.started.send(()).unwrap();
                self.release
                    .recv_timeout(FAKE_PORT_BLOCK_TIMEOUT)
                    .expect("test observation port release timed out");
            }
            Ok(RawPlayerSample::new(
                "fuo://song/observation",
                TransportState::Playing,
            ))
        }
    }

    struct BlockingControlPort {
        started: SyncSender<()>,
        release: Receiver<()>,
        block_next: bool,
        calls: Arc<Mutex<Vec<PlayerControl>>>,
    }

    impl PlayerControlPort for BlockingControlPort {
        fn dispatch(&mut self, control: &PlayerControl) -> ControlDispatchOutcome {
            self.calls.lock().unwrap().push(control.clone());
            if self.block_next {
                self.block_next = false;
                self.started.send(()).unwrap();
                self.release
                    .recv_timeout(FAKE_PORT_BLOCK_TIMEOUT)
                    .expect("test control port release timed out");
            }
            ControlDispatchOutcome::acknowledged("ok")
        }
    }

    struct BlockingSearchPort {
        started: SyncSender<()>,
        release: Receiver<()>,
        block_next: bool,
        calls: Arc<Mutex<Vec<PlayerSearch>>>,
    }

    impl PlayerSearchPort for BlockingSearchPort {
        fn search_text(
            &mut self,
            keyword: &str,
            source: &str,
        ) -> Result<String, PlayerSearchError> {
            self.calls
                .lock()
                .unwrap()
                .push(PlayerSearch::text(keyword, source));
            self.block_if_requested();
            Ok(format!("raw search: {keyword}"))
        }

        fn search_candidates(
            &mut self,
            keyword: &str,
            source: &str,
        ) -> Result<Vec<SearchCandidate>, PlayerSearchError> {
            self.calls
                .lock()
                .unwrap()
                .push(PlayerSearch::candidates(keyword, source));
            self.block_if_requested();
            Ok(vec![SearchCandidate::new(
                format!("{keyword} result"),
                "fuo://song/result",
            )])
        }

        fn search_and_pick(
            &mut self,
            keyword: &str,
            source: &str,
            prefer_accompaniment: bool,
        ) -> Result<Option<PickedCandidate>, PlayerSearchError> {
            self.calls.lock().unwrap().push(PlayerSearch::pick(
                keyword,
                source,
                prefer_accompaniment,
            ));
            self.block_if_requested();
            Ok(Some(PickedCandidate::new(
                SearchCandidate::new(format!("{keyword} picked"), "fuo://song/picked"),
                "candidate listing",
            )))
        }
    }

    impl BlockingSearchPort {
        fn block_if_requested(&mut self) {
            if self.block_next {
                self.block_next = false;
                self.started.send(()).unwrap();
                self.release
                    .recv_timeout(FAKE_PORT_BLOCK_TIMEOUT)
                    .expect("test search port release timed out");
            }
        }
    }

    fn config() -> PlayerRuntimeConfig {
        PlayerRuntimeConfig {
            observation: PlayerObservationConfig::default(),
            normal_observation_interval: Duration::from_secs(1),
            fast_observation_interval: Duration::from_millis(300),
            observation_command_capacity: 4,
            active_fast_demand_capacity: 4,
            control_queue_capacity: 4,
            search_queue_capacity: 4,
        }
    }

    #[test]
    fn configuration_rejects_zero_durations_and_capacities() {
        let invalid = [
            PlayerRuntimeConfig {
                normal_observation_interval: Duration::ZERO,
                ..config()
            },
            PlayerRuntimeConfig {
                fast_observation_interval: Duration::ZERO,
                ..config()
            },
            PlayerRuntimeConfig {
                observation: PlayerObservationConfig {
                    stale_timeout: Duration::ZERO,
                    ..PlayerObservationConfig::default()
                },
                ..config()
            },
            PlayerRuntimeConfig {
                observation: PlayerObservationConfig {
                    restart_previous_progress: Duration::ZERO,
                    ..PlayerObservationConfig::default()
                },
                ..config()
            },
            PlayerRuntimeConfig {
                observation: PlayerObservationConfig {
                    restart_near_start: Duration::ZERO,
                    ..PlayerObservationConfig::default()
                },
                ..config()
            },
            PlayerRuntimeConfig {
                observation: PlayerObservationConfig {
                    restart_minimum_drop: Duration::ZERO,
                    ..PlayerObservationConfig::default()
                },
                ..config()
            },
            PlayerRuntimeConfig {
                observation_command_capacity: 0,
                ..config()
            },
            PlayerRuntimeConfig {
                active_fast_demand_capacity: 0,
                ..config()
            },
            PlayerRuntimeConfig {
                control_queue_capacity: 0,
                ..config()
            },
            PlayerRuntimeConfig {
                search_queue_capacity: 0,
                ..config()
            },
        ];

        for candidate in invalid {
            assert!(matches!(
                candidate.validate(),
                Err(PlayerRuntimeConfigError::ZeroDurationOrCapacity(_))
            ));
        }

        for fast_interval in [Duration::from_secs(1), Duration::from_secs(2)] {
            assert_eq!(
                PlayerRuntimeConfig {
                    fast_observation_interval: fast_interval,
                    ..config()
                }
                .validate(),
                Err(PlayerRuntimeConfigError::FastIntervalNotFaster)
            );
        }
    }

    #[test]
    fn control_lane_is_fifo_and_preserves_dispatch_classification_without_retrying() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let outcomes = VecDeque::from([
            ControlDispatchOutcome::acknowledged("paused"),
            ControlDispatchOutcome::rejected("not allowed"),
            ControlDispatchOutcome::outcome_unknown("connection lost after write"),
        ]);
        let runtime = PlayerRuntime::start(
            ConstantObservationPort,
            RecordingControlPort {
                calls: Arc::clone(&calls),
                outcomes,
            },
            EmptySearchPort,
            config(),
        )
        .unwrap();
        let handle = runtime.handle();

        let first = handle
            .submit_control(BusinessOperationId::new(11), PlayerControl::Pause)
            .unwrap();
        let second = handle
            .submit_control(BusinessOperationId::new(12), PlayerControl::Resume)
            .unwrap();
        let third = handle
            .submit_control(BusinessOperationId::new(13), PlayerControl::Next)
            .unwrap();

        assert_eq!(
            first.wait().unwrap().outcome,
            ControlDispatchOutcome::acknowledged("paused")
        );
        assert_eq!(
            second.wait().unwrap().outcome,
            ControlDispatchOutcome::rejected("not allowed")
        );
        let third = third.wait().unwrap();
        assert_eq!(
            third.outcome,
            ControlDispatchOutcome::outcome_unknown("connection lost after write")
        );
        assert!(third.started_at.is_some());
        assert!(third.finished_at >= third.started_at.unwrap());
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                PlayerControl::Pause,
                PlayerControl::Resume,
                PlayerControl::Next
            ]
        );

        runtime.shutdown().unwrap();
    }

    #[test]
    fn observation_publications_advance_revision_and_preserve_read_failures() {
        let mut runtime_config = config();
        runtime_config.normal_observation_interval = Duration::from_millis(20);
        runtime_config.fast_observation_interval = Duration::from_millis(5);
        let runtime = PlayerRuntime::start(
            ScriptedObservationPort {
                samples: VecDeque::from([
                    Ok(RawPlayerSample::new(
                        "fuo://song/one",
                        TransportState::Playing,
                    )),
                    Err(PlayerObservationReadError::new("status RPC failed")),
                ]),
            },
            RecordingControlPort {
                calls: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
            EmptySearchPort,
            runtime_config,
        )
        .unwrap();
        let handle = runtime.handle();

        let first = match handle.wait_for_observation_after(
            super::PlayerObservationRevision::INITIAL,
            Duration::from_secs(1),
        ) {
            ObservationWaitOutcome::Advanced(observation) => observation,
            _ => panic!("initial observation must be published"),
        };
        assert!(first.read_error().is_none());

        let second =
            match handle.wait_for_observation_after(first.revision(), Duration::from_secs(1)) {
                ObservationWaitOutcome::Advanced(observation) => observation,
                _ => panic!("failed observation attempt must still publish"),
            };
        assert_eq!(second.revision().get(), first.revision().get() + 1);
        assert_eq!(
            second.read_error().map(PlayerObservationReadError::message),
            Some("status RPC failed")
        );
        assert_eq!(
            handle.latest_observation().unwrap().revision(),
            second.revision()
        );

        runtime.shutdown().unwrap();
        assert!(matches!(
            handle.wait_for_observation_after(
                handle.latest_observation_revision(),
                Duration::from_secs(1)
            ),
            ObservationWaitOutcome::RuntimeStopped
        ));
    }

    #[test]
    fn blocked_rpc_and_shutdown_reevaluate_cached_observation_without_new_revision() {
        let clock = ManualClock::new(Instant::now());
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_sender = ReleaseOnDrop::new(release_sender);
        let mut runtime_config = config();
        runtime_config.normal_observation_interval = Duration::from_millis(10);
        runtime_config.fast_observation_interval = Duration::from_millis(2);
        runtime_config.observation.stale_timeout = Duration::from_millis(50);
        let runtime = PlayerRuntime::start_with_clock(
            BlockingAfterSamplesObservationPort {
                samples_before_block: 2,
                started: started_sender,
                release: release_receiver,
                blocked: false,
            },
            RecordingControlPort {
                calls: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
            EmptySearchPort,
            runtime_config,
            Arc::new(clock.clone()),
        )
        .unwrap();
        let handle = runtime.handle();
        let first = match handle.wait_for_observation_after(
            super::PlayerObservationRevision::INITIAL,
            Duration::from_secs(1),
        ) {
            ObservationWaitOutcome::Advanced(observation) => observation,
            _ => panic!("first observation must be published"),
        };
        let fresh =
            match handle.wait_for_observation_after(first.revision(), Duration::from_secs(1)) {
                ObservationWaitOutcome::Advanced(observation) => observation,
                _ => panic!("stable observation must be published"),
            };
        assert!(fresh.observation().uri_freshness.is_fresh());
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        clock
            .advance_to(
                fresh.observation().last_successful_observed_at.unwrap()
                    + Duration::from_millis(80),
            )
            .unwrap();

        let shutdown = thread::spawn(move || runtime.shutdown());
        assert!(
            handle
                .observation
                .wait_until_stopped(Duration::from_secs(1))
        );
        let aged = handle.latest_observation().unwrap();
        assert_eq!(aged.revision(), fresh.revision());
        assert_eq!(
            aged.observation().uri_freshness,
            ObservationFreshness::Unknown
        );
        assert_eq!(
            aged.observation().transport_freshness,
            ObservationFreshness::Unknown
        );
        assert_eq!(aged.observation().uri, None);
        assert_eq!(aged.observation().transport, None);
        assert_eq!(aged.observation().title, None);
        let waited = handle.wait_for_observation_after(first.revision(), Duration::ZERO);
        let ObservationWaitOutcome::Advanced(waited) = waited else {
            panic!("an already-published newer revision must remain readable");
        };
        assert_eq!(waited.revision(), fresh.revision());
        assert_eq!(
            waited.observation().uri_freshness,
            ObservationFreshness::Unknown
        );
        assert!(matches!(
            handle.wait_for_observation_after(fresh.revision(), Duration::from_millis(10)),
            ObservationWaitOutcome::RuntimeStopped
        ));

        release_sender.release();
        shutdown.join().unwrap().unwrap();
    }

    #[test]
    fn revision_exhaustion_stops_observation_lane_and_drains_accepted_demands() {
        let now = Instant::now();
        let clock = ManualClock::new(now);
        let mut observer = PlayerObserver::new(clock, PlayerObservationConfig::default());
        observer.observe_sample(RawPlayerSample::new(
            "fuo://song/seed",
            TransportState::Playing,
        ));
        let observation = observer.observe_sample(RawPlayerSample::new(
            "fuo://song/seed",
            TransportState::Playing,
        ));
        let latest = Arc::new(super::LatestObservationStore::new(
            Duration::from_secs(5),
            Arc::new(SystemClock),
        ));
        latest.lock().latest = Some(super::RevisedPlayerObservation {
            revision: super::PlayerObservationRevision(u64::MAX),
            observation: Arc::new(observation),
            read_error: None,
        });
        let (sender, receiver) = mpsc::sync_channel(2);
        let lane = Arc::new(super::RuntimeLane::new(sender));
        let (response, result) = mpsc::sync_channel(1);
        lane.try_submit(
            super::ObservationCommand::RequestFast {
                operation_id: BusinessOperationId::new(19),
                expires_at: now + Duration::from_secs(1),
                response,
            },
            PlayerLane::Observation,
        )
        .unwrap();
        let worker_lane = Arc::clone(&lane);
        let worker_latest = Arc::clone(&latest);
        let worker = thread::spawn(move || {
            super::run_observation_lane(
                ConstantObservationPort,
                receiver,
                worker_lane,
                worker_latest,
                config(),
            );
        });

        worker.join().unwrap();
        let result = result.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(result.operation_id, BusinessOperationId::new(19));
        assert_eq!(
            result.status,
            Err(PlayerLaneError::RuntimeStopped(PlayerLane::Observation))
        );
        assert!(matches!(
            lane.try_submit(super::ObservationCommand::Shutdown, PlayerLane::Observation),
            Err(PlayerLaneError::RuntimeStopped(PlayerLane::Observation))
        ));
    }

    #[test]
    fn fast_observation_demands_merge_cancel_and_expire_idempotently() {
        let mut runtime_config = config();
        runtime_config.normal_observation_interval = Duration::from_millis(500);
        runtime_config.fast_observation_interval = Duration::from_millis(25);
        runtime_config.observation_command_capacity = 4;
        runtime_config.active_fast_demand_capacity = 2;
        let runtime = PlayerRuntime::start(
            ConstantObservationPort,
            RecordingControlPort {
                calls: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
            EmptySearchPort,
            runtime_config,
        )
        .unwrap();
        let handle = runtime.handle();
        let initial = match handle.wait_for_observation_after(
            super::PlayerObservationRevision::INITIAL,
            Duration::from_secs(1),
        ) {
            ObservationWaitOutcome::Advanced(observation) => observation,
            _ => panic!("initial observation must be published"),
        };
        let expiry = Instant::now() + Duration::from_secs(2);

        assert_eq!(
            handle
                .request_fast_observation(BusinessOperationId::new(21), expiry)
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationDemandStatus::Active
        );
        assert_eq!(
            handle
                .request_fast_observation(BusinessOperationId::new(22), expiry)
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationDemandStatus::Active
        );
        assert_eq!(
            handle
                .request_fast_observation(BusinessOperationId::new(21), Instant::now())
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationDemandStatus::Active
        );
        assert_eq!(
            handle
                .request_fast_observation(BusinessOperationId::new(23), expiry)
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status,
            Err(PlayerLaneError::QueueFull(PlayerLane::Observation))
        );

        let first_fast = match handle
            .wait_for_observation_after(initial.revision(), Duration::from_millis(250))
        {
            ObservationWaitOutcome::Advanced(observation) => observation,
            _ => panic!("active fast demand must accelerate observations"),
        };
        assert_eq!(
            handle
                .cancel_fast_observation(BusinessOperationId::new(22))
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationCancelStatus::Cancelled
        );
        let second_fast = match handle
            .wait_for_observation_after(first_fast.revision(), Duration::from_millis(250))
        {
            ObservationWaitOutcome::Advanced(observation) => observation,
            _ => panic!("remaining fast demand must keep the shared fast cadence"),
        };
        assert_eq!(
            handle
                .cancel_fast_observation(BusinessOperationId::new(22))
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationCancelStatus::NotActive
        );
        assert_eq!(
            handle
                .cancel_fast_observation(BusinessOperationId::new(21))
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationCancelStatus::Cancelled
        );
        assert!(matches!(
            handle.wait_for_observation_after(second_fast.revision(), Duration::from_millis(100)),
            ObservationWaitOutcome::TimedOut
        ));

        let expired_id = BusinessOperationId::new(24);
        assert_eq!(
            handle
                .request_fast_observation(expired_id, Instant::now())
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationDemandStatus::Expired
        );
        assert_eq!(
            handle
                .cancel_fast_observation(expired_id)
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationCancelStatus::NotActive
        );
        let automatic_id = BusinessOperationId::new(25);
        assert_eq!(
            handle
                .request_fast_observation(automatic_id, Instant::now() + Duration::from_millis(70))
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationDemandStatus::Active
        );
        thread::sleep(Duration::from_millis(120));
        assert_eq!(
            handle
                .cancel_fast_observation(automatic_id)
                .unwrap()
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationCancelStatus::NotActive
        );

        runtime.shutdown().unwrap();
    }

    #[test]
    fn fast_observation_submission_does_not_wait_for_a_blocking_status_rpc() {
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_sender = ReleaseOnDrop::new(release_sender);
        let mut runtime_config = config();
        runtime_config.observation_command_capacity = 1;
        let runtime = PlayerRuntime::start(
            BlockingObservationPort {
                started: started_sender,
                release: release_receiver,
                block_next: true,
            },
            RecordingControlPort {
                calls: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
            EmptySearchPort,
            runtime_config,
        )
        .unwrap();
        let handle = runtime.handle();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let operation = handle
            .request_fast_observation(
                BusinessOperationId::new(26),
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
        assert!(operation.try_result().unwrap().is_none());
        assert!(matches!(
            handle.request_fast_observation(
                BusinessOperationId::new(27),
                Instant::now() + Duration::from_secs(1)
            ),
            Err(PlayerLaneError::QueueFull(PlayerLane::Observation))
        ));

        release_sender.release();
        assert_eq!(
            operation
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .status
                .unwrap(),
            FastObservationDemandStatus::Active
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_terminally_rejects_queued_fast_demand_and_cancel_operations() {
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_sender = ReleaseOnDrop::new(release_sender);
        let runtime = PlayerRuntime::start(
            BlockingObservationPort {
                started: started_sender,
                release: release_receiver,
                block_next: true,
            },
            RecordingControlPort {
                calls: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
            EmptySearchPort,
            config(),
        )
        .unwrap();
        let handle = runtime.handle();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let demand = handle
            .request_fast_observation(
                BusinessOperationId::new(28),
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
        let cancel = handle
            .cancel_fast_observation(BusinessOperationId::new(29))
            .unwrap();
        assert!(demand.try_result().unwrap().is_none());
        assert!(cancel.try_result().unwrap().is_none());

        let shutdown = thread::spawn(move || runtime.shutdown());
        assert!(
            handle
                .observation
                .wait_until_stopped(Duration::from_secs(1))
        );
        release_sender.release();
        shutdown.join().unwrap().unwrap();

        let demand_result = demand.wait().unwrap();
        assert_eq!(demand_result.operation_id, BusinessOperationId::new(28));
        assert_eq!(
            demand_result.status,
            Err(PlayerLaneError::RuntimeStopped(PlayerLane::Observation))
        );
        let cancel_result = cancel.wait().unwrap();
        assert_eq!(cancel_result.operation_id, BusinessOperationId::new(29));
        assert_eq!(
            cancel_result.status,
            Err(PlayerLaneError::RuntimeStopped(PlayerLane::Observation))
        );
    }

    #[test]
    fn full_control_queue_is_explicit_and_does_not_block_search_or_observation() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_sender = ReleaseOnDrop::new(release_sender);
        let mut runtime_config = config();
        runtime_config.normal_observation_interval = Duration::from_millis(20);
        runtime_config.fast_observation_interval = Duration::from_millis(5);
        runtime_config.control_queue_capacity = 1;
        let runtime = PlayerRuntime::start(
            ConstantObservationPort,
            BlockingControlPort {
                started: started_sender,
                release: release_receiver,
                block_next: true,
                calls: Arc::clone(&calls),
            },
            EmptySearchPort,
            runtime_config,
        )
        .unwrap();
        let handle = runtime.handle();
        let baseline = handle.latest_observation_revision();

        let first = handle
            .submit_control(BusinessOperationId::new(31), PlayerControl::Pause)
            .unwrap();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let second = handle
            .submit_control(BusinessOperationId::new(32), PlayerControl::Resume)
            .unwrap();
        assert!(matches!(
            handle.submit_control(BusinessOperationId::new(33), PlayerControl::Next),
            Err(PlayerLaneError::QueueFull(PlayerLane::Control))
        ));

        let search = handle
            .submit_search(
                BusinessOperationId::new(34),
                PlayerSearch::candidates("song", "source"),
            )
            .unwrap();
        assert!(matches!(
            search.wait_timeout(Duration::from_secs(1)).unwrap().outcome,
            PlayerSearchOutcome::Candidates(_)
        ));
        assert!(matches!(
            handle.wait_for_observation_after(baseline, Duration::from_secs(1)),
            ObservationWaitOutcome::Advanced(_)
        ));

        release_sender.release();
        first.wait_timeout(Duration::from_secs(1)).unwrap();
        second.wait_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(calls.lock().unwrap().len(), 2);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn operation_timeout_keeps_the_result_and_completed_polling_is_not_runtime_stopped() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_sender = ReleaseOnDrop::new(release_sender);
        let runtime = PlayerRuntime::start(
            ConstantObservationPort,
            BlockingControlPort {
                started: started_sender,
                release: release_receiver,
                block_next: true,
                calls,
            },
            EmptySearchPort,
            config(),
        )
        .unwrap();
        let handle = runtime.handle();
        let operation = handle
            .submit_control(BusinessOperationId::new(35), PlayerControl::Pause)
            .unwrap();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        assert_eq!(
            operation.wait_timeout(Duration::from_millis(10)),
            Err(PlayerOperationReceiveError::TimedOut(PlayerLane::Control))
        );
        release_sender.release();
        assert_eq!(
            operation.wait().unwrap().operation_id,
            BusinessOperationId::new(35)
        );
        assert_eq!(
            operation.try_result(),
            Err(PlayerOperationReceiveError::AlreadyCompleted(
                PlayerLane::Control
            ))
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn full_search_queue_is_explicit_and_does_not_block_control() {
        let search_calls = Arc::new(Mutex::new(Vec::new()));
        let control_calls = Arc::new(Mutex::new(Vec::new()));
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_sender = ReleaseOnDrop::new(release_sender);
        let mut runtime_config = config();
        runtime_config.search_queue_capacity = 1;
        let runtime = PlayerRuntime::start(
            ConstantObservationPort,
            RecordingControlPort {
                calls: Arc::clone(&control_calls),
                outcomes: VecDeque::from([ControlDispatchOutcome::acknowledged("paused")]),
            },
            BlockingSearchPort {
                started: started_sender,
                release: release_receiver,
                block_next: true,
                calls: Arc::clone(&search_calls),
            },
            runtime_config,
        )
        .unwrap();
        let handle = runtime.handle();

        let first = handle
            .submit_search(
                BusinessOperationId::new(41),
                PlayerSearch::text("one", "source"),
            )
            .unwrap();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let second = handle
            .submit_search(
                BusinessOperationId::new(42),
                PlayerSearch::pick("two", "source", true),
            )
            .unwrap();
        assert!(matches!(
            handle.submit_search(
                BusinessOperationId::new(43),
                PlayerSearch::candidates("three", "source")
            ),
            Err(PlayerLaneError::QueueFull(PlayerLane::Search))
        ));

        let control = handle
            .submit_control(BusinessOperationId::new(44), PlayerControl::Pause)
            .unwrap();
        assert_eq!(
            control
                .wait_timeout(Duration::from_secs(1))
                .unwrap()
                .outcome,
            ControlDispatchOutcome::acknowledged("paused")
        );

        release_sender.release();
        let first_result = first.wait_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(first_result.operation_id, BusinessOperationId::new(41));
        assert!(matches!(
            first_result.outcome,
            PlayerSearchOutcome::Text(text) if text == "raw search: one"
        ));
        let second_result = second.wait_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(second_result.operation_id, BusinessOperationId::new(42));
        assert!(matches!(
            second_result.outcome,
            PlayerSearchOutcome::Picked(Some(_))
        ));
        assert_eq!(search_calls.lock().unwrap().len(), 2);
        assert_eq!(
            control_calls.lock().unwrap().as_slice(),
            &[PlayerControl::Pause]
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn search_client_routes_every_search_shape_through_the_runtime_lane() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_sender, _started_receiver) = mpsc::sync_channel(1);
        let (_release_sender, release_receiver) = mpsc::sync_channel(1);
        let runtime = PlayerRuntime::start(
            ConstantObservationPort,
            RecordingControlPort {
                calls: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
            BlockingSearchPort {
                started: started_sender,
                release: release_receiver,
                block_next: false,
                calls: Arc::clone(&calls),
            },
            config(),
        )
        .unwrap();
        let client = PlayerSearchClient::new(runtime.handle(), BusinessOperationIdAllocator::new());

        assert_eq!(
            client.search_text("one", "source").unwrap(),
            "raw search: one"
        );
        assert_eq!(
            client.search_candidates("two", "source").unwrap(),
            vec![SearchCandidate::new("two result", "fuo://song/result")]
        );
        assert_eq!(
            client.search_and_pick("three", "source", true).unwrap(),
            Some(PickedCandidate::new(
                SearchCandidate::new("three picked", "fuo://song/picked"),
                "candidate listing"
            ))
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[
                PlayerSearch::text("one", "source"),
                PlayerSearch::candidates("two", "source"),
                PlayerSearch::pick("three", "source", true),
            ]
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn search_client_reports_backend_failure_and_runtime_shutdown_explicitly() {
        let runtime = PlayerRuntime::start(
            ConstantObservationPort,
            RecordingControlPort {
                calls: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
            FailingSearchPort,
            config(),
        )
        .unwrap();
        let client = PlayerSearchClient::new(runtime.handle(), BusinessOperationIdAllocator::new());

        assert_eq!(
            client.search_candidates("song", "source"),
            Err(PlayerSearchClientError::Failed(PlayerSearchError::new(
                "backend failed"
            )))
        );

        runtime.shutdown().unwrap();
        assert_eq!(
            client.search_text("song", "source"),
            Err(PlayerSearchClientError::RuntimeStopped)
        );
    }

    #[test]
    fn search_client_reports_a_full_runtime_queue_without_retrying() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let release_sender = ReleaseOnDrop::new(release_sender);
        let runtime = PlayerRuntime::start(
            ConstantObservationPort,
            RecordingControlPort {
                calls: Arc::new(Mutex::new(Vec::new())),
                outcomes: VecDeque::new(),
            },
            BlockingSearchPort {
                started: started_sender,
                release: release_receiver,
                block_next: true,
                calls,
            },
            PlayerRuntimeConfig {
                search_queue_capacity: 1,
                ..config()
            },
        )
        .unwrap();
        let handle = runtime.handle();
        let client = PlayerSearchClient::new(handle.clone(), BusinessOperationIdAllocator::new());
        let in_flight = handle
            .submit_search(
                BusinessOperationId::new(61),
                PlayerSearch::text("in-flight", "source"),
            )
            .unwrap();
        started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first search started");
        let queued = handle
            .submit_search(
                BusinessOperationId::new(62),
                PlayerSearch::text("queued", "source"),
            )
            .unwrap();

        assert_eq!(
            client.search_text("rejected", "source"),
            Err(PlayerSearchClientError::QueueFull)
        );

        release_sender.release();
        in_flight.wait_timeout(Duration::from_secs(1)).unwrap();
        queued.wait_timeout(Duration::from_secs(1)).unwrap();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_finishes_accepted_but_not_started_control_and_search_operations() {
        let control_calls = Arc::new(Mutex::new(Vec::new()));
        let search_calls = Arc::new(Mutex::new(Vec::new()));
        let (control_started_sender, control_started_receiver) = mpsc::sync_channel(1);
        let (control_release_sender, control_release_receiver) = mpsc::sync_channel(1);
        let (search_started_sender, search_started_receiver) = mpsc::sync_channel(1);
        let (search_release_sender, search_release_receiver) = mpsc::sync_channel(1);
        let control_release_sender = ReleaseOnDrop::new(control_release_sender);
        let search_release_sender = ReleaseOnDrop::new(search_release_sender);
        let runtime = PlayerRuntime::start(
            ConstantObservationPort,
            BlockingControlPort {
                started: control_started_sender,
                release: control_release_receiver,
                block_next: true,
                calls: Arc::clone(&control_calls),
            },
            BlockingSearchPort {
                started: search_started_sender,
                release: search_release_receiver,
                block_next: true,
                calls: Arc::clone(&search_calls),
            },
            config(),
        )
        .unwrap();
        let handle = runtime.handle();

        let in_flight_control = handle
            .submit_control(BusinessOperationId::new(51), PlayerControl::Pause)
            .unwrap();
        let in_flight_search = handle
            .submit_search(
                BusinessOperationId::new(52),
                PlayerSearch::candidates("one", "source"),
            )
            .unwrap();
        control_started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        search_started_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let queued_control = handle
            .submit_control(BusinessOperationId::new(53), PlayerControl::Resume)
            .unwrap();
        let queued_search = handle
            .submit_search(
                BusinessOperationId::new(54),
                PlayerSearch::pick("two", "source", false),
            )
            .unwrap();

        let shutdown = thread::spawn(move || runtime.shutdown());
        assert!(handle.control.wait_until_stopped(Duration::from_secs(1)));
        assert!(matches!(
            handle.submit_control(BusinessOperationId::new(55), PlayerControl::Next),
            Err(PlayerLaneError::RuntimeStopped(PlayerLane::Control))
        ));
        control_release_sender.release();
        search_release_sender.release();
        shutdown.join().unwrap().unwrap();

        assert_eq!(
            in_flight_control.wait().unwrap().outcome,
            ControlDispatchOutcome::acknowledged("ok")
        );
        assert!(matches!(
            in_flight_search.wait().unwrap().outcome,
            PlayerSearchOutcome::Candidates(_)
        ));
        assert!(matches!(
            queued_control.wait().unwrap().outcome,
            ControlDispatchOutcome::NotSent { .. }
        ));
        assert!(matches!(
            queued_search.wait().unwrap().outcome,
            PlayerSearchOutcome::NotRun { .. }
        ));
        assert_eq!(control_calls.lock().unwrap().len(), 1);
        assert_eq!(search_calls.lock().unwrap().len(), 1);
    }
}
