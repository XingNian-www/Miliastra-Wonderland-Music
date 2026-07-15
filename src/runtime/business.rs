use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::features::card_games::{
    CardGameCancel, CardGameCommandStart, CardGameEffectClaim, CardGameEffectKey,
    CardGameEffectResult, CardGameResume, CardGameService, CardGameTimedOutcome, LandlordCommand,
};
use crate::features::idiom_chain::{IdiomChainCommand, IdiomChainOutcome, IdiomChainService};
use crate::observation::chat::{
    CompletionAdvance, ObservationCompletionEvent, ObservationWatermark,
};
use crate::observation::shared::ObservationGap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusinessEvent {
    CompletionAdvance(CompletionAdvance),
    CompletionGap(ObservationGap),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusinessRuntimeSnapshot {
    latest_watermark: Option<ObservationWatermark>,
    terminal_failure_count: u64,
    completion_gap_count: u64,
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
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusinessRuntimeError {
    ZeroQueueCapacity,
    RuntimeStopped,
    WorkerPanicked,
    IdiomChainOperationFailed(String),
    CardGameOperationFailed(String),
}

impl Display for BusinessRuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroQueueCapacity => {
                formatter.write_str("business runtime queue capacity must be greater than zero")
            }
            Self::RuntimeStopped => formatter.write_str("business runtime is stopped"),
            Self::WorkerPanicked => formatter.write_str("business runtime worker panicked"),
            Self::IdiomChainOperationFailed(message) => {
                write!(formatter, "idiom chain operation failed: {message}")
            }
            Self::CardGameOperationFailed(message) => {
                write!(formatter, "card game operation failed: {message}")
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
    Shutdown(SyncSender<BusinessRuntimeSnapshot>),
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
    accepting: Mutex<bool>,
}

#[derive(Clone)]
pub struct BusinessRuntimeHandle {
    channel: Arc<RuntimeChannel>,
}

impl BusinessRuntimeHandle {
    pub fn submit(&self, event: BusinessEvent) -> Result<(), BusinessRuntimeError> {
        self.send(RuntimeMessage::Event(event))
    }

    pub fn snapshot(&self) -> Result<BusinessRuntimeSnapshot, BusinessRuntimeError> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.send(RuntimeMessage::Snapshot(response))?;
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

    pub fn tick_card_game(
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
        self.send(message(response))?;
        receiver
            .recv()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?
    }

    fn send(&self, message: RuntimeMessage) -> Result<(), BusinessRuntimeError> {
        let accepting = self
            .channel
            .accepting
            .lock()
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
        if !*accepting {
            return Err(BusinessRuntimeError::RuntimeStopped);
        }
        self.channel
            .sender
            .send(message)
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)
    }
}

pub struct BusinessRuntime {
    handle: BusinessRuntimeHandle,
    worker: Option<JoinHandle<()>>,
}

impl BusinessRuntime {
    pub(crate) fn start(
        queue_capacity: usize,
        idiom_chain: IdiomChainService,
        card_games: CardGameService,
    ) -> Result<Self, BusinessRuntimeError> {
        if queue_capacity == 0 {
            return Err(BusinessRuntimeError::ZeroQueueCapacity);
        }
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let channel = Arc::new(RuntimeChannel {
            sender,
            accepting: Mutex::new(true),
        });
        let worker = thread::Builder::new()
            .name("business-runtime".to_string())
            .spawn(move || run_business_runtime(receiver, idiom_chain, card_games))
            .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
        Ok(Self {
            handle: BusinessRuntimeHandle { channel },
            worker: Some(worker),
        })
    }

    pub fn handle(&self) -> BusinessRuntimeHandle {
        self.handle.clone()
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
            let mut accepting = self
                .handle
                .channel
                .accepting
                .lock()
                .map_err(|_| BusinessRuntimeError::RuntimeStopped)?;
            *accepting = false;
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

fn run_business_runtime(
    receiver: Receiver<RuntimeMessage>,
    mut idiom_chain: IdiomChainService,
    mut card_games: CardGameService,
) {
    let mut snapshot = BusinessRuntimeSnapshot::default();
    while let Ok(message) = receiver.recv() {
        match message {
            RuntimeMessage::Event(event) => snapshot.apply(event),
            RuntimeMessage::HandleIdiomChain {
                player,
                command,
                response,
            } => {
                let _ = response.send(
                    idiom_chain
                        .handle(&player, &command)
                        .map_err(idiom_chain_operation_failed),
                );
            }
            RuntimeMessage::ExplainIdiomChain {
                player,
                command,
                response,
            } => {
                let _ = response.send(
                    idiom_chain
                        .explain(&player, &command)
                        .map_err(idiom_chain_operation_failed),
                );
            }
            RuntimeMessage::AbortIdiomChain(response) => {
                let _ = response.send(idiom_chain.abort().map_err(idiom_chain_operation_failed));
            }
            RuntimeMessage::ExpireIdiomChain(response) => {
                let _ = response.send(
                    idiom_chain
                        .expire_idle_now()
                        .map_err(idiom_chain_operation_failed),
                );
            }
            RuntimeMessage::CardGame(message) => handle_card_game_message(&mut card_games, message),
            RuntimeMessage::Snapshot(response) => {
                let _ = response.send(snapshot);
            }
            RuntimeMessage::Shutdown(response) => {
                if let Err(error) = card_games.abort() {
                    log::error!("业务运行时关闭时无法中止牌局: {error:#}");
                }
                if let Err(error) = idiom_chain.abort() {
                    log::error!("业务运行时关闭时无法中止成语接龙: {error:#}");
                }
                let _ = response.send(snapshot);
                break;
            }
        }
    }
}

fn handle_card_game_message(card_games: &mut CardGameService, message: CardGameRuntimeMessage) {
    match message {
        CardGameRuntimeMessage::Begin {
            player,
            command,
            now,
            response,
        } => {
            let _ = response.send(
                card_games
                    .begin_command(&player, &command, now)
                    .map_err(card_game_operation_failed),
            );
        }
        CardGameRuntimeMessage::Claim { key, response } => {
            let _ = response.send(card_games.claim(key).map_err(card_game_operation_failed));
        }
        CardGameRuntimeMessage::Resume {
            key,
            result,
            response,
        } => {
            let _ = response.send(
                card_games
                    .resume(key, result)
                    .map_err(card_game_operation_failed),
            );
        }
        CardGameRuntimeMessage::Cancel { key, response } => {
            let _ = response.send(card_games.cancel(key).map_err(card_game_operation_failed));
        }
        CardGameRuntimeMessage::Tick {
            now,
            clock_active,
            response,
        } => {
            let _ = response.send(
                card_games
                    .tick(now, clock_active)
                    .map_err(card_game_operation_failed),
            );
        }
        CardGameRuntimeMessage::Abort(response) => {
            let _ = response.send(card_games.abort().map_err(card_game_operation_failed));
        }
    }
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
        CardGameEffect, CardGameEffectLane, CardGameEffectRequest, CardGameLateResult,
        LandlordConfig,
    };
    use crate::features::entertainment::{EntertainmentCoordinator, EntertainmentKind};
    use crate::features::idiom_chain::IdiomChainMode;
    use crate::observation::chat::ChatObservationLedger;
    use crate::observation::shared::{ObservationGapKind, SharedObservationStream};

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
