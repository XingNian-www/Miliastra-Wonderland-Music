use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::{
    LandlordCommand, LandlordConfig, LandlordGame, LandlordOutcome, LandlordPrivateDelivery,
};
use crate::features::entertainment::{AcquireOutcome, EntertainmentCoordinator, EntertainmentKind};
use crate::runtime::identity::{BusinessOperationId, SessionGeneration};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardGameEffectKey {
    pub operation_id: BusinessOperationId,
    pub session_generation: SessionGeneration,
}

impl CardGameEffectKey {
    pub const fn new(
        operation_id: BusinessOperationId,
        session_generation: SessionGeneration,
    ) -> Self {
        Self {
            operation_id,
            session_generation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardGameEffectLane {
    Formal,
    Deferred,
}

#[derive(Clone, Copy)]
enum CompatibilityLatePolicy {
    Error,
    Ignore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardGameEffect {
    FriendVerify { player: String, message: String },
    PrivateDelivery { player: String, message: String },
    HallDelivery { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardGameEffectRequest {
    pub key: CardGameEffectKey,
    pub lane: CardGameEffectLane,
    pub effect: CardGameEffect,
}

#[derive(Debug)]
pub enum CardGameEffectResult {
    FriendVerify(Result<bool>),
    PrivateDelivery(Result<bool>),
    HallDelivery(Result<()>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardGameCompletion {
    pub action: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardGameCommandStart {
    Completed(CardGameCompletion),
    Suspended(CardGameEffectRequest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardGameLateResult {
    pub key: CardGameEffectKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardGameResume {
    Completed(CardGameCompletion),
    Suspended(CardGameEffectRequest),
    Late(CardGameLateResult),
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "removed when CardGameService moves into BusinessRuntime"
)]
pub enum CardGameCancel {
    Cancelled(CardGameCompletion),
    Late(CardGameLateResult),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardGameTimedOutcome {
    pub session_generation: SessionGeneration,
    action: &'static str,
    request: CardGameEffectRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardGameDeliveryCancel {
    Cancelled {
        session_generation: SessionGeneration,
    },
    Late {
        session_generation: SessionGeneration,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg(test)]
pub enum CardGameStartGate {
    Ready {
        reservation: CardGameStartReservation,
    },
    Reply(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CardGameStartReservation {
    token: u64,
    kind: EntertainmentKind,
    session_generation: SessionGeneration,
}

pub trait CardGameDeliveryPort {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_hall(&self, message: &str) -> Result<()>;
}

#[derive(Clone)]
pub struct CardGameDeliveryTask {
    service: CardGameService,
    outcome: CardGameTimedOutcome,
}

impl CardGameDeliveryTask {
    pub fn label(&self) -> String {
        format!("发送牌局计时结果({})", self.outcome.action)
    }

    pub fn execute(self, port: &dyn CardGameDeliveryPort) -> Result<()> {
        self.service.drive_compatibility(
            CardGameCommandStart::Suspended(self.outcome.request),
            port,
            CompatibilityLatePolicy::Ignore,
        )
    }

    pub fn cancel(&self) -> Result<CardGameDeliveryCancel> {
        Ok(match self.service.cancel(self.outcome.request.key)? {
            CardGameCancel::Cancelled(_) => CardGameDeliveryCancel::Cancelled {
                session_generation: self.outcome.session_generation,
            },
            CardGameCancel::Late(_) => CardGameDeliveryCancel::Late {
                session_generation: self.outcome.session_generation,
            },
        })
    }
}

impl CardGameStartReservation {
    pub fn kind(self) -> EntertainmentKind {
        self.kind
    }
}

struct CardGameState {
    game: LandlordGame,
    pending_start: Option<CardGameStartReservation>,
    next_reservation_token: u64,
    next_operation_id: u64,
    session_generation: SessionGeneration,
    pending_effects: BTreeMap<BusinessOperationId, PendingEffectState>,
}

struct PendingEffectState {
    key: CardGameEffectKey,
    effect: PendingCardGameEffect,
    claimed: bool,
}

enum PendingCardGameEffect {
    VerifyStart {
        player: String,
        command: LandlordCommand,
        reservation: CardGameStartReservation,
        now: Instant,
    },
    VerifyJoin {
        player: String,
        kind: EntertainmentKind,
        now: Instant,
    },
    DeliverOutcomePrivate {
        player: String,
        remaining: VecDeque<LandlordPrivateDelivery>,
        public_reply: Option<String>,
        action: &'static str,
        lane: CardGameEffectLane,
        release_on_completion: bool,
    },
    DeliverHand {
        player: String,
        action: &'static str,
    },
    Hall {
        action: &'static str,
        release_on_completion: bool,
    },
}

impl PendingCardGameEffect {
    fn accepts(&self, result: &CardGameEffectResult) -> bool {
        matches!(
            (self, result),
            (
                Self::VerifyStart { .. },
                CardGameEffectResult::FriendVerify(_)
            ) | (
                Self::VerifyJoin { .. },
                CardGameEffectResult::FriendVerify(_)
            ) | (
                Self::DeliverOutcomePrivate { .. },
                CardGameEffectResult::PrivateDelivery(_)
            ) | (
                Self::DeliverHand { .. },
                CardGameEffectResult::PrivateDelivery(_)
            ) | (Self::Hall { .. }, CardGameEffectResult::HallDelivery(_))
        )
    }

    fn continues_after(&self, result: &CardGameEffectResult) -> bool {
        match (self, result) {
            (
                Self::VerifyStart { .. } | Self::VerifyJoin { .. },
                CardGameEffectResult::FriendVerify(Ok(_)),
            ) => true,
            (
                Self::DeliverOutcomePrivate {
                    remaining,
                    public_reply,
                    ..
                },
                CardGameEffectResult::PrivateDelivery(Ok(true)),
            ) => !remaining.is_empty() || public_reply.is_some(),
            _ => false,
        }
    }

    fn aborts_after(&self, result: &CardGameEffectResult) -> bool {
        matches!(
            (self, result),
            (
                Self::DeliverOutcomePrivate { .. },
                CardGameEffectResult::PrivateDelivery(Ok(false) | Err(_))
            )
        )
    }
}

#[derive(Clone)]
pub struct CardGameService {
    state: Arc<Mutex<CardGameState>>,
    entertainment: EntertainmentCoordinator,
    enabled: bool,
}

impl CardGameService {
    pub fn new(config: LandlordConfig, entertainment: EntertainmentCoordinator) -> Self {
        let enabled = config.enabled;
        Self {
            state: Arc::new(Mutex::new(CardGameState {
                game: LandlordGame::new(config),
                pending_start: None,
                next_reservation_token: 1,
                next_operation_id: 1,
                session_generation: SessionGeneration::INITIAL,
                pending_effects: BTreeMap::new(),
            })),
            entertainment,
            enabled,
        }
    }

    pub fn begin_command(
        &self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<CardGameCommandStart> {
        let (start, release_now) = {
            let mut state = self.state()?;
            ensure_operation_capacity(&state)?;
            match command {
                LandlordCommand::Start | LandlordCommand::RunFastStart => (
                    self.begin_start_in_state(&mut state, player, command, now)?,
                    false,
                ),
                LandlordCommand::Join => {
                    let kind = self
                        .entertainment
                        .active()
                        .filter(|kind| is_card_game_kind(*kind));
                    if let Some(kind) =
                        kind.filter(|_| state.game.is_lobby() && !state.game.lobby_contains(player))
                    {
                        let generation = state.session_generation;
                        let request = suspend_effect(
                            &mut state,
                            generation,
                            CardGameEffectLane::Formal,
                            CardGameEffect::FriendVerify {
                                player: player.to_string(),
                                message: format!("{}报名成功，请回到大厅等待开局", kind.label()),
                            },
                            PendingCardGameEffect::VerifyJoin {
                                player: player.to_string(),
                                kind,
                                now,
                            },
                        )?;
                        (CardGameCommandStart::Suspended(request), false)
                    } else {
                        self.begin_game_outcome_in_state(
                            &mut state,
                            player,
                            command,
                            now,
                            CardGameEffectLane::Formal,
                        )?
                    }
                }
                LandlordCommand::Hand => {
                    if let Err(error) = ensure_generation_capacity(&state) {
                        clear_session_without_generation(&mut state);
                        self.release_active_card_game();
                        return Err(error);
                    }
                    let outcome = state.game.handle(player, command, now);
                    let action = outcome.action;
                    let Some(message) = outcome.private_reply else {
                        return Ok(CardGameCommandStart::Completed(CardGameCompletion {
                            action,
                        }));
                    };
                    let generation = state.session_generation;
                    let request = suspend_effect(
                        &mut state,
                        generation,
                        CardGameEffectLane::Formal,
                        CardGameEffect::PrivateDelivery {
                            player: player.to_string(),
                            message,
                        },
                        PendingCardGameEffect::DeliverHand {
                            player: player.to_string(),
                            action,
                        },
                    )?;
                    (CardGameCommandStart::Suspended(request), false)
                }
                _ => {
                    let lane = if command.requires_executor() {
                        CardGameEffectLane::Formal
                    } else {
                        CardGameEffectLane::Deferred
                    };
                    self.begin_game_outcome_in_state(&mut state, player, command, now, lane)?
                }
            }
        };
        if release_now {
            self.release_active_card_game();
        }
        Ok(start)
    }

    fn begin_start_in_state(
        &self,
        state: &mut CardGameState,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<CardGameCommandStart> {
        let kind = card_game_kind(command)?;
        let label = kind.label();
        let reply = if !self.enabled {
            Some(format!("{}未启用", label))
        } else if state.game.is_active() || state.pending_start.is_some() {
            Some("已有牌局或房间进行中".to_string())
        } else {
            let next_generation = next_start_generation(state)?;
            match self.entertainment.try_acquire(kind)? {
                AcquireOutcome::Acquired => {
                    state.session_generation = next_generation;
                    state.pending_effects.clear();
                    let reservation = CardGameStartReservation {
                        token: state.next_reservation_token,
                        kind,
                        session_generation: state.session_generation,
                    };
                    state.next_reservation_token =
                        state.next_reservation_token.wrapping_add(1).max(1);
                    state.pending_start = Some(reservation);
                    let request = suspend_effect(
                        state,
                        reservation.session_generation,
                        CardGameEffectLane::Formal,
                        CardGameEffect::FriendVerify {
                            player: player.to_string(),
                            message: format!("{}报名成功，请回到大厅等待组局", label),
                        },
                        PendingCardGameEffect::VerifyStart {
                            player: player.to_string(),
                            command: command.clone(),
                            reservation,
                            now,
                        },
                    )?;
                    return Ok(CardGameCommandStart::Suspended(request));
                }
                AcquireOutcome::AlreadyOwned => Some("已有牌局或房间进行中".to_string()),
                AcquireOutcome::Occupied(active) => Some(format!(
                    "{}正在进行，请结束后再开始{}",
                    active.label(),
                    label
                )),
            }
        };
        let generation = state.session_generation;
        suspend_effect(
            state,
            generation,
            CardGameEffectLane::Formal,
            CardGameEffect::HallDelivery {
                message: reply.expect("non-started card game has a reply"),
            },
            PendingCardGameEffect::Hall {
                action: "start-rejected",
                release_on_completion: false,
            },
        )
        .map(CardGameCommandStart::Suspended)
    }

    fn begin_game_outcome_in_state(
        &self,
        state: &mut CardGameState,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
        lane: CardGameEffectLane,
    ) -> Result<(CardGameCommandStart, bool)> {
        let outcome = if command.reports_entertainment_conflict()
            && let Some(active) = self.entertainment.active()
            && !is_card_game_kind(active)
        {
            LandlordOutcome::public(
                "occupied",
                format!("{}正在进行，请结束后再开始牌局", active.label()),
            )
        } else {
            if let Err(error) = ensure_generation_capacity(state) {
                clear_session_without_generation(state);
                self.release_active_card_game();
                return Err(error);
            }
            state.game.handle(player, command, now)
        };
        if outcome.ended {
            advance_session_generation(state)?;
        }
        let release_on_completion = outcome.ended;
        let generation = state.session_generation;
        let resumed = Self::begin_outcome_resume_in_state(state, generation, outcome, lane)?;
        let release_now = release_on_completion && matches!(resumed, CardGameResume::Completed(_));
        Ok((command_start_from_resume(resumed), release_now))
    }

    pub fn resume(
        &self,
        key: CardGameEffectKey,
        result: CardGameEffectResult,
    ) -> Result<CardGameResume> {
        let mut state = self.state()?;
        let Some(pending_state) = state.pending_effects.get(&key.operation_id) else {
            return Ok(CardGameResume::Late(CardGameLateResult { key }));
        };
        if pending_state.key != key {
            return Ok(CardGameResume::Late(CardGameLateResult { key }));
        }
        if state.session_generation != key.session_generation {
            state.pending_effects.remove(&key.operation_id);
            return Ok(CardGameResume::Late(CardGameLateResult { key }));
        }
        if !pending_state.effect.accepts(&result) {
            bail!("card game effect result does not match the suspended effect");
        }
        let continues = pending_state.effect.continues_after(&result);
        let aborts = pending_state.effect.aborts_after(&result);
        if continues && let Err(error) = ensure_operation_capacity(&state) {
            clear_session_without_generation(&mut state);
            self.release_active_card_game();
            return Err(error);
        }
        if aborts && let Err(error) = ensure_generation_capacity(&state) {
            clear_session_without_generation(&mut state);
            self.release_active_card_game();
            return Err(error);
        }
        let pending = state
            .pending_effects
            .remove(&key.operation_id)
            .expect("validated pending card game effect")
            .effect;
        match (pending, result) {
            (
                PendingCardGameEffect::VerifyStart {
                    player,
                    command,
                    reservation,
                    now,
                },
                CardGameEffectResult::FriendVerify(result),
            ) => match result {
                Ok(true) => {
                    let kind = card_game_kind(&command)?;
                    if reservation.kind != kind {
                        bail!("card game start reservation does not match command");
                    }
                    if state.pending_start != Some(reservation) {
                        return Ok(CardGameResume::Late(CardGameLateResult { key }));
                    }
                    state.pending_start = None;
                    let outcome = state.game.handle(&player, &command, now);
                    if outcome.action != "created" {
                        self.entertainment.release(kind);
                    }
                    Self::begin_outcome_resume_in_state(
                        &mut state,
                        key.session_generation,
                        outcome,
                        CardGameEffectLane::Formal,
                    )
                }
                Ok(false) => {
                    if state.pending_start == Some(reservation) {
                        state.pending_start = None;
                        self.entertainment.release(reservation.kind);
                    }
                    suspend_effect(
                        &mut state,
                        key.session_generation,
                        CardGameEffectLane::Formal,
                        CardGameEffect::HallDelivery {
                            message: format!(
                                "{}报名失败：好友列表未找到唯一昵称",
                                reservation.kind().label()
                            ),
                        },
                        PendingCardGameEffect::Hall {
                            action: "verification-rejected",
                            release_on_completion: false,
                        },
                    )
                    .map(CardGameResume::Suspended)
                }
                Err(error) => {
                    if state.pending_start == Some(reservation) {
                        state.pending_start = None;
                        self.entertainment.release(reservation.kind);
                    }
                    Err(error)
                }
            },
            (
                PendingCardGameEffect::DeliverHand { player, action },
                CardGameEffectResult::PrivateDelivery(result),
            ) => match result {
                Ok(true) => Ok(CardGameResume::Completed(CardGameCompletion { action })),
                Ok(false) => {
                    state.game.retry_hand_delivery(&player);
                    bail!("牌局手牌发送失败：好友列表未找到 {}", player)
                }
                Err(error) => {
                    state.game.retry_hand_delivery(&player);
                    Err(error)
                }
            },
            (
                PendingCardGameEffect::VerifyJoin { player, kind, now },
                CardGameEffectResult::FriendVerify(result),
            ) => match result {
                Ok(true) => {
                    let outcome = state.game.handle(&player, &LandlordCommand::Join, now);
                    if outcome.ended {
                        advance_session_generation(&mut state)?;
                        self.entertainment.release(kind);
                    }
                    let generation = state.session_generation;
                    Self::begin_outcome_resume_in_state(
                        &mut state,
                        generation,
                        outcome,
                        CardGameEffectLane::Formal,
                    )
                }
                Ok(false) => suspend_effect(
                    &mut state,
                    key.session_generation,
                    CardGameEffectLane::Formal,
                    CardGameEffect::HallDelivery {
                        message: format!("{}报名失败：好友列表未找到唯一昵称", kind.label()),
                    },
                    PendingCardGameEffect::Hall {
                        action: "verification-rejected",
                        release_on_completion: false,
                    },
                )
                .map(CardGameResume::Suspended),
                Err(error) => Err(error),
            },
            (
                PendingCardGameEffect::DeliverOutcomePrivate {
                    player,
                    remaining,
                    public_reply,
                    action,
                    lane,
                    release_on_completion,
                },
                CardGameEffectResult::PrivateDelivery(result),
            ) => match result {
                Ok(true) => {
                    let resumed = Self::continue_outcome_delivery_in_state(
                        &mut state,
                        key.session_generation,
                        remaining,
                        public_reply,
                        action,
                        lane,
                        release_on_completion,
                    )?;
                    if release_on_completion && matches!(resumed, CardGameResume::Completed(_)) {
                        self.release_active_card_game();
                    }
                    Ok(resumed)
                }
                Ok(false) => {
                    abort_state(&mut state)?;
                    self.release_active_card_game();
                    bail!("牌局发牌失败：好友列表未找到 {}", player)
                }
                Err(error) => {
                    abort_state(&mut state)?;
                    self.release_active_card_game();
                    Err(error)
                }
            },
            (
                PendingCardGameEffect::Hall {
                    action,
                    release_on_completion,
                },
                CardGameEffectResult::HallDelivery(result),
            ) => {
                if release_on_completion {
                    self.release_active_card_game();
                }
                result?;
                Ok(CardGameResume::Completed(CardGameCompletion { action }))
            }
            _ => unreachable!("pending effect type was checked before removal"),
        }
    }

    #[allow(
        dead_code,
        reason = "removed when CardGameService moves into BusinessRuntime"
    )]
    pub fn cancel(&self, key: CardGameEffectKey) -> Result<CardGameCancel> {
        let mut state = self.state()?;
        let Some(pending_state) = state.pending_effects.get(&key.operation_id) else {
            return Ok(CardGameCancel::Late(CardGameLateResult { key }));
        };
        if pending_state.key != key {
            return Ok(CardGameCancel::Late(CardGameLateResult { key }));
        }
        if state.session_generation != key.session_generation {
            state.pending_effects.remove(&key.operation_id);
            return Ok(CardGameCancel::Late(CardGameLateResult { key }));
        }
        if pending_state.claimed {
            return Ok(CardGameCancel::Late(CardGameLateResult { key }));
        }
        if matches!(
            pending_state.effect,
            PendingCardGameEffect::DeliverOutcomePrivate { .. }
        ) && let Err(error) = ensure_generation_capacity(&state)
        {
            clear_session_without_generation(&mut state);
            self.release_active_card_game();
            return Err(error);
        }
        let pending = state
            .pending_effects
            .remove(&key.operation_id)
            .expect("validated pending card game effect")
            .effect;
        let action = match pending {
            PendingCardGameEffect::VerifyStart { reservation, .. } => {
                if state.pending_start == Some(reservation) {
                    state.pending_start = None;
                    self.entertainment.release(reservation.kind);
                }
                "start-verification"
            }
            PendingCardGameEffect::VerifyJoin { .. } => "join-verification",
            PendingCardGameEffect::DeliverOutcomePrivate { .. } => {
                abort_state(&mut state)?;
                self.release_active_card_game();
                "private-delivery"
            }
            PendingCardGameEffect::DeliverHand { player, .. } => {
                state.game.retry_hand_delivery(&player);
                "hand-delivery"
            }
            PendingCardGameEffect::Hall {
                action,
                release_on_completion,
            } => {
                if release_on_completion {
                    self.release_active_card_game();
                }
                action
            }
        };
        Ok(CardGameCancel::Cancelled(CardGameCompletion { action }))
    }

    #[cfg(test)]
    fn begin_outcome_command(
        &self,
        outcome: LandlordOutcome,
        lane: CardGameEffectLane,
        generation: SessionGeneration,
    ) -> Result<CardGameCommandStart> {
        match self.begin_outcome_resume(outcome, lane, generation)? {
            CardGameResume::Completed(completion) => {
                Ok(CardGameCommandStart::Completed(completion))
            }
            CardGameResume::Suspended(request) => Ok(CardGameCommandStart::Suspended(request)),
            CardGameResume::Late(_) => unreachable!("a new outcome cannot already be late"),
        }
    }

    #[cfg(test)]
    fn begin_outcome_resume(
        &self,
        outcome: LandlordOutcome,
        lane: CardGameEffectLane,
        generation: SessionGeneration,
    ) -> Result<CardGameResume> {
        let ended = outcome.ended;
        let progress = {
            let mut state = self.state()?;
            Self::begin_outcome_resume_in_state(&mut state, generation, outcome, lane)?
        };
        if ended && matches!(progress, CardGameResume::Completed(_)) {
            self.release_active_card_game();
        }
        Ok(progress)
    }

    fn begin_outcome_resume_in_state(
        state: &mut CardGameState,
        generation: SessionGeneration,
        mut outcome: LandlordOutcome,
        lane: CardGameEffectLane,
    ) -> Result<CardGameResume> {
        if state.session_generation != generation {
            bail!("card game session was replaced before delivering its outcome");
        }
        let action = outcome.action;
        let release_on_completion = outcome.ended;
        let mut deliveries = outcome
            .private_deliveries
            .drain(..)
            .collect::<VecDeque<_>>();
        if let Some(delivery) = deliveries.pop_front() {
            suspend_effect(
                state,
                generation,
                lane,
                CardGameEffect::PrivateDelivery {
                    player: delivery.player.clone(),
                    message: delivery.message,
                },
                PendingCardGameEffect::DeliverOutcomePrivate {
                    player: delivery.player,
                    remaining: deliveries,
                    public_reply: outcome.public_reply,
                    action,
                    lane,
                    release_on_completion,
                },
            )
            .map(CardGameResume::Suspended)
        } else if let Some(message) = outcome.public_reply {
            suspend_effect(
                state,
                generation,
                lane,
                CardGameEffect::HallDelivery { message },
                PendingCardGameEffect::Hall {
                    action,
                    release_on_completion,
                },
            )
            .map(CardGameResume::Suspended)
        } else {
            Ok(CardGameResume::Completed(CardGameCompletion { action }))
        }
    }

    fn continue_outcome_delivery_in_state(
        state: &mut CardGameState,
        generation: SessionGeneration,
        mut remaining: VecDeque<LandlordPrivateDelivery>,
        public_reply: Option<String>,
        action: &'static str,
        lane: CardGameEffectLane,
        release_on_completion: bool,
    ) -> Result<CardGameResume> {
        if let Some(delivery) = remaining.pop_front() {
            return suspend_effect(
                state,
                generation,
                lane,
                CardGameEffect::PrivateDelivery {
                    player: delivery.player.clone(),
                    message: delivery.message,
                },
                PendingCardGameEffect::DeliverOutcomePrivate {
                    player: delivery.player,
                    remaining,
                    public_reply,
                    action,
                    lane,
                    release_on_completion,
                },
            )
            .map(CardGameResume::Suspended);
        }
        if let Some(message) = public_reply {
            return suspend_effect(
                state,
                generation,
                lane,
                CardGameEffect::HallDelivery { message },
                PendingCardGameEffect::Hall {
                    action,
                    release_on_completion,
                },
            )
            .map(CardGameResume::Suspended);
        }
        Ok(CardGameResume::Completed(CardGameCompletion { action }))
    }

    pub fn execute(
        &self,
        player: &str,
        command: &LandlordCommand,
        port: &dyn CardGameDeliveryPort,
        now: Instant,
    ) -> Result<()> {
        let start = self.begin_command(player, command, now)?;
        self.drive_compatibility(start, port, CompatibilityLatePolicy::Error)
    }

    #[cfg(test)]
    pub fn begin_delivery_outcome(&self, outcome: LandlordOutcome) -> Result<CardGameCommandStart> {
        let generation = self.state()?.session_generation;
        self.begin_outcome_command(outcome, CardGameEffectLane::Formal, generation)
    }

    fn drive_compatibility(
        &self,
        mut progress: CardGameCommandStart,
        port: &dyn CardGameDeliveryPort,
        late_policy: CompatibilityLatePolicy,
    ) -> Result<()> {
        loop {
            let request = match progress {
                CardGameCommandStart::Completed(_) => return Ok(()),
                CardGameCommandStart::Suspended(request) => request,
            };
            let key = request.key;
            if !self.claim_effect(key)? {
                return compatibility_late(late_policy);
            }
            let result = match request.effect {
                CardGameEffect::FriendVerify { player, message } => {
                    CardGameEffectResult::FriendVerify(port.verify_friend(&player, &message))
                }
                CardGameEffect::PrivateDelivery { player, message } => {
                    CardGameEffectResult::PrivateDelivery(port.send_friend(&player, &message))
                }
                CardGameEffect::HallDelivery { message } => {
                    CardGameEffectResult::HallDelivery(port.send_hall(&message))
                }
            };
            progress = match self.resume(key, result)? {
                CardGameResume::Completed(_) => return Ok(()),
                CardGameResume::Late(_) => return compatibility_late(late_policy),
                CardGameResume::Suspended(request) => CardGameCommandStart::Suspended(request),
            };
        }
    }

    #[cfg(test)]
    pub(crate) fn drive_formal_for_test(
        &self,
        progress: CardGameCommandStart,
        port: &dyn CardGameDeliveryPort,
    ) -> Result<()> {
        self.drive_compatibility(progress, port, CompatibilityLatePolicy::Error)
    }

    fn claim_effect(&self, key: CardGameEffectKey) -> Result<bool> {
        let mut state = self.state()?;
        if state.session_generation != key.session_generation {
            state.pending_effects.remove(&key.operation_id);
            return Ok(false);
        }
        let Some(pending) = state.pending_effects.get_mut(&key.operation_id) else {
            return Ok(false);
        };
        if pending.key != key || pending.claimed {
            return Ok(false);
        }
        pending.claimed = true;
        Ok(true)
    }

    #[cfg(test)]
    pub fn prepare_start(&self, command: &LandlordCommand) -> Result<CardGameStartGate> {
        let kind = card_game_kind(command)?;
        let label = kind.label();
        if !self.enabled {
            return Ok(CardGameStartGate::Reply(format!("{}未启用", label)));
        }
        let mut state = self.state()?;
        if state.game.is_active() || state.pending_start.is_some() {
            return Ok(CardGameStartGate::Reply("已有牌局或房间进行中".to_string()));
        }
        let next_generation = next_start_generation(&state)?;
        match self.entertainment.try_acquire(kind)? {
            AcquireOutcome::Acquired => {
                state.session_generation = next_generation;
                state.pending_effects.clear();
                let reservation = CardGameStartReservation {
                    token: state.next_reservation_token,
                    kind,
                    session_generation: state.session_generation,
                };
                state.next_reservation_token = state.next_reservation_token.wrapping_add(1).max(1);
                state.pending_start = Some(reservation);
                Ok(CardGameStartGate::Ready { reservation })
            }
            AcquireOutcome::AlreadyOwned => {
                Ok(CardGameStartGate::Reply("已有牌局或房间进行中".to_string()))
            }
            AcquireOutcome::Occupied(active) => Ok(CardGameStartGate::Reply(format!(
                "{}正在进行，请结束后再开始{}",
                active.label(),
                label
            ))),
        }
    }

    #[cfg(test)]
    pub fn cancel_start(&self, reservation: CardGameStartReservation) -> Result<bool> {
        let cancelled = {
            let mut state = self.state()?;
            if state.pending_start == Some(reservation) {
                state.pending_start = None;
                true
            } else {
                false
            }
        };
        if cancelled {
            self.entertainment.release(reservation.kind);
        }
        Ok(cancelled)
    }

    #[cfg(test)]
    pub fn complete_start(
        &self,
        player: &str,
        command: &LandlordCommand,
        reservation: CardGameStartReservation,
        now: Instant,
    ) -> Result<LandlordOutcome> {
        let kind = card_game_kind(command)?;
        if reservation.kind != kind {
            bail!("card game start reservation does not match command");
        }
        let outcome = {
            let mut state = self.state()?;
            if state.pending_start != Some(reservation) {
                bail!("card game start reservation is no longer active");
            }
            state.pending_start = None;
            state.game.handle(player, command, now)
        };
        if outcome.action != "created" {
            self.entertainment.release(kind);
        }
        Ok(outcome)
    }

    #[cfg(test)]
    pub fn handle(
        &self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<LandlordOutcome> {
        self.handle_with_generation(player, command, now)
            .map(|(outcome, _)| outcome)
    }

    #[cfg(test)]
    fn handle_with_generation(
        &self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<(LandlordOutcome, SessionGeneration)> {
        if command.reports_entertainment_conflict()
            && let Some(active) = self.entertainment.active()
            && !is_card_game_kind(active)
        {
            let generation = self.state()?.session_generation;
            return Ok((
                LandlordOutcome::public(
                    "occupied",
                    format!("{}正在进行，请结束后再开始牌局", active.label()),
                ),
                generation,
            ));
        }
        let (outcome, generation) = {
            let mut state = self.state()?;
            ensure_generation_capacity(&state)?;
            let outcome = state.game.handle(player, command, now);
            if outcome.ended {
                advance_session_generation(&mut state)?;
            }
            let generation = state.session_generation;
            (outcome, generation)
        };
        Ok((outcome, generation))
    }

    pub fn tick(&self, now: Instant, clock_active: bool) -> Result<Option<CardGameTimedOutcome>> {
        let (timed, release_now) = {
            let mut state = self.state()?;
            if !state.game.is_active() {
                return Ok(None);
            }
            if let Err(error) = ensure_generation_capacity(&state) {
                clear_session_without_generation(&mut state);
                self.release_active_card_game();
                return Err(error);
            }
            if let Err(error) = ensure_operation_capacity(&state) {
                clear_session_without_generation(&mut state);
                self.release_active_card_game();
                return Err(error);
            }
            let Some(outcome) = state.game.tick(now, clock_active) else {
                return Ok(None);
            };
            let action = outcome.action;
            let release_on_completion = outcome.ended;
            if release_on_completion {
                advance_session_generation(&mut state)?;
            }
            let generation = state.session_generation;
            let resumed = Self::begin_outcome_resume_in_state(
                &mut state,
                generation,
                outcome,
                CardGameEffectLane::Formal,
            )?;
            match resumed {
                CardGameResume::Suspended(request) => (
                    Some(CardGameTimedOutcome {
                        session_generation: generation,
                        action,
                        request,
                    }),
                    false,
                ),
                CardGameResume::Completed(_) => (None, release_on_completion),
                CardGameResume::Late(_) => {
                    unreachable!("a newly registered timed effect cannot be late")
                }
            }
        };
        if release_now {
            self.release_active_card_game();
        }
        Ok(timed)
    }

    pub fn delivery_task(&self, outcome: CardGameTimedOutcome) -> CardGameDeliveryTask {
        CardGameDeliveryTask {
            service: self.clone(),
            outcome,
        }
    }

    pub fn abort(&self) -> Result<bool> {
        let reserved = self.entertainment.active().is_some_and(is_card_game_kind);
        let aborted = {
            let mut state = self.state()?;
            let had_pending_effects = !state.pending_effects.is_empty();
            let has_session = state.game.is_active()
                || state.pending_start.is_some()
                || had_pending_effects
                || reserved;
            if has_session {
                let Some(next_generation) = state.session_generation.checked_next() else {
                    clear_session_without_generation(&mut state);
                    self.release_active_card_game();
                    bail!("card game session generation exhausted");
                };
                state.session_generation = next_generation;
            }
            let pending = state.pending_start.take().is_some();
            state.pending_effects.clear();
            state.game.abort() || pending || had_pending_effects
        };
        if aborted || reserved {
            self.release_active_card_game();
        }
        Ok(aborted || reserved)
    }

    #[cfg(test)]
    pub(crate) fn is_active(&self) -> Result<bool> {
        let state = self.state()?;
        Ok(state.game.is_active() || state.pending_start.is_some())
    }

    #[cfg(test)]
    pub(crate) fn set_next_operation_id_for_test(&self, value: u64) {
        self.state.lock().unwrap().next_operation_id = value;
    }

    #[cfg(test)]
    pub(crate) fn set_session_generation_for_test(&self, value: u64) {
        self.state.lock().unwrap().session_generation = SessionGeneration::new(value);
    }

    #[cfg(test)]
    pub(crate) fn pending_effect_key_for_test(&self) -> Option<CardGameEffectKey> {
        self.state
            .lock()
            .unwrap()
            .pending_effects
            .values()
            .next()
            .map(|pending| pending.key)
    }

    #[cfg(test)]
    pub(crate) fn pending_effect_count_for_test(&self) -> usize {
        self.state.lock().unwrap().pending_effects.len()
    }

    fn release_active_card_game(&self) {
        if let Some(kind) = self
            .entertainment
            .active()
            .filter(|kind| is_card_game_kind(*kind))
        {
            self.entertainment.release(kind);
        }
    }

    fn state(&self) -> Result<MutexGuard<'_, CardGameState>> {
        self.state
            .lock()
            .map_err(|_| anyhow!("landlord mutex poisoned"))
    }
}

fn ensure_generation_capacity(state: &CardGameState) -> Result<()> {
    state
        .session_generation
        .checked_next()
        .ok_or_else(|| anyhow!("card game session generation exhausted"))?;
    Ok(())
}

fn next_start_generation(state: &CardGameState) -> Result<SessionGeneration> {
    let next = state
        .session_generation
        .checked_next()
        .ok_or_else(|| anyhow!("card game session generation exhausted"))?;
    next.checked_next()
        .ok_or_else(|| anyhow!("card game session generation exhausted"))?;
    Ok(next)
}

fn compatibility_late(policy: CompatibilityLatePolicy) -> Result<()> {
    match policy {
        CompatibilityLatePolicy::Ignore => Ok(()),
        CompatibilityLatePolicy::Error => {
            bail!("card game command was cancelled before its effect chain completed")
        }
    }
}

fn command_start_from_resume(resumed: CardGameResume) -> CardGameCommandStart {
    match resumed {
        CardGameResume::Completed(completion) => CardGameCommandStart::Completed(completion),
        CardGameResume::Suspended(request) => CardGameCommandStart::Suspended(request),
        CardGameResume::Late(_) => unreachable!("a newly registered effect cannot be late"),
    }
}

fn ensure_operation_capacity(state: &CardGameState) -> Result<()> {
    state
        .next_operation_id
        .checked_add(1)
        .ok_or_else(|| anyhow!("card game operation identifier exhausted"))?;
    let operation_id = BusinessOperationId::new(state.next_operation_id);
    if state.pending_effects.contains_key(&operation_id) {
        bail!("card game operation identifier is already pending");
    }
    Ok(())
}

fn suspend_effect(
    state: &mut CardGameState,
    session_generation: SessionGeneration,
    lane: CardGameEffectLane,
    effect: CardGameEffect,
    pending: PendingCardGameEffect,
) -> Result<CardGameEffectRequest> {
    if state.session_generation != session_generation {
        bail!("card game session was replaced before suspending its effect");
    }
    let next_operation_id = state
        .next_operation_id
        .checked_add(1)
        .ok_or_else(|| anyhow!("card game operation identifier exhausted"))?;
    let operation_id = BusinessOperationId::new(state.next_operation_id);
    if state.pending_effects.contains_key(&operation_id) {
        bail!("card game operation identifier is already pending");
    }
    let key = CardGameEffectKey::new(operation_id, session_generation);
    state.pending_effects.insert(
        operation_id,
        PendingEffectState {
            key,
            effect: pending,
            claimed: false,
        },
    );
    state.next_operation_id = next_operation_id;
    Ok(CardGameEffectRequest { key, lane, effect })
}

fn abort_state(state: &mut CardGameState) -> Result<bool> {
    let had_pending_effects = !state.pending_effects.is_empty();
    let has_session =
        state.game.is_active() || state.pending_start.is_some() || had_pending_effects;
    if has_session {
        advance_session_generation(state)?;
    }
    let pending = state.pending_start.take().is_some();
    Ok(state.game.abort() || pending || had_pending_effects)
}

fn clear_session_without_generation(state: &mut CardGameState) {
    state.pending_start = None;
    state.pending_effects.clear();
    state.game.abort();
}

fn advance_session_generation(state: &mut CardGameState) -> Result<SessionGeneration> {
    let next = state
        .session_generation
        .checked_next()
        .ok_or_else(|| anyhow!("card game session generation exhausted"))?;
    state.session_generation = next;
    state.pending_effects.clear();
    Ok(next)
}

fn card_game_kind(command: &LandlordCommand) -> Result<EntertainmentKind> {
    match command {
        LandlordCommand::Start => Ok(EntertainmentKind::Landlord),
        LandlordCommand::RunFastStart => Ok(EntertainmentKind::RunFast),
        _ => bail!("card game start gate requires a start command"),
    }
}

fn is_card_game_kind(kind: EntertainmentKind) -> bool {
    matches!(
        kind,
        EntertainmentKind::Landlord | EntertainmentKind::RunFast
    )
}
