use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::{LandlordCommand, LandlordConfig, LandlordGame, LandlordOutcome};
use crate::features::entertainment::{AcquireOutcome, EntertainmentCoordinator, EntertainmentKind};

#[derive(Clone, Debug, PartialEq, Eq)]
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
}

pub trait CardGameDeliveryPort {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_hall(&self, message: &str) -> Result<()>;
}

#[derive(Clone)]
pub struct CardGameDeliveryTask {
    service: CardGameService,
    outcome: LandlordOutcome,
}

impl CardGameDeliveryTask {
    pub fn label(&self) -> String {
        format!("发送牌局计时结果({})", self.outcome.action)
    }

    pub fn execute(self, port: &dyn CardGameDeliveryPort) -> Result<()> {
        self.service.deliver(self.outcome, port)
    }

    pub fn cancel(&self) -> Result<bool> {
        self.service.cancel_delivery(&self.outcome)
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
            })),
            entertainment,
            enabled,
        }
    }

    pub fn execute(
        &self,
        player: &str,
        command: &LandlordCommand,
        port: &dyn CardGameDeliveryPort,
        now: Instant,
    ) -> Result<()> {
        match command {
            LandlordCommand::Start | LandlordCommand::RunFastStart => {
                let reservation = match self.prepare_start(command)? {
                    CardGameStartGate::Ready { reservation } => reservation,
                    CardGameStartGate::Reply(reply) => return port.send_hall(&reply),
                };
                let label = reservation.kind().label();
                let verified = match port
                    .verify_friend(player, &format!("{}报名成功，请回到大厅等待组局", label))
                {
                    Ok(verified) => verified,
                    Err(error) => {
                        if let Err(cancel_error) = self.cancel_start(reservation) {
                            log::error!("好友验证失败后无法取消牌局预留: {cancel_error:#}");
                        }
                        return Err(error);
                    }
                };
                if !verified {
                    self.cancel_start(reservation)?;
                    return port.send_hall(&format!("{}报名失败：好友列表未找到唯一昵称", label));
                }
                let outcome = self.complete_start(player, command, reservation, now)?;
                self.deliver(outcome, port)
            }
            LandlordCommand::Join => {
                let Some(kind) = self.lobby_kind_for_join(player)? else {
                    let outcome = self.handle(player, command, now)?;
                    return self.deliver(outcome, port);
                };
                let label = kind.label();
                if !port.verify_friend(player, &format!("{}报名成功，请回到大厅等待开局", label))?
                {
                    return port.send_hall(&format!("{}报名失败：好友列表未找到唯一昵称", label));
                }
                let outcome = self.handle(player, command, now)?;
                self.deliver(outcome, port)
            }
            LandlordCommand::Hand => {
                let outcome = self.handle(player, command, now)?;
                if let Some(message) = outcome.private_reply {
                    match port.send_friend(player, &message) {
                        Ok(true) => {}
                        Ok(false) => {
                            self.retry_hand_delivery(player)?;
                            bail!("牌局手牌发送失败：好友列表未找到 {}", player);
                        }
                        Err(error) => {
                            self.retry_hand_delivery(player)?;
                            return Err(error);
                        }
                    }
                }
                Ok(())
            }
            _ => {
                let outcome = self.handle(player, command, now)?;
                self.deliver(outcome, port)
            }
        }
    }

    pub fn deliver(&self, outcome: LandlordOutcome, port: &dyn CardGameDeliveryPort) -> Result<()> {
        self.begin_delivery(&outcome);
        for delivery in outcome.private_deliveries {
            match port.send_friend(&delivery.player, &delivery.message) {
                Ok(true) => {}
                Ok(false) => {
                    self.abort()?;
                    bail!("牌局发牌失败：好友列表未找到 {}", delivery.player);
                }
                Err(error) => {
                    self.abort()?;
                    return Err(error);
                }
            }
        }
        if let Some(reply) = outcome.public_reply {
            port.send_hall(&reply)?;
        }
        Ok(())
    }

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
        match self.entertainment.try_acquire(kind)? {
            AcquireOutcome::Acquired => {
                let reservation = CardGameStartReservation {
                    token: state.next_reservation_token,
                    kind,
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
        self.finish_outcome(&outcome);
        Ok(outcome)
    }

    pub fn handle(
        &self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<LandlordOutcome> {
        if command.reports_entertainment_conflict()
            && let Some(active) = self.entertainment.active()
            && !is_card_game_kind(active)
        {
            return Ok(LandlordOutcome::public(
                "occupied",
                format!("{}正在进行，请结束后再开始牌局", active.label()),
            ));
        }
        let outcome = self.state()?.game.handle(player, command, now);
        self.finish_outcome(&outcome);
        Ok(outcome)
    }

    pub fn tick(&self, now: Instant, clock_active: bool) -> Result<Option<LandlordOutcome>> {
        Ok(self.state()?.game.tick(now, clock_active))
    }

    pub fn delivery_task(&self, outcome: LandlordOutcome) -> CardGameDeliveryTask {
        CardGameDeliveryTask {
            service: self.clone(),
            outcome,
        }
    }

    fn cancel_delivery(&self, outcome: &LandlordOutcome) -> Result<bool> {
        if outcome.ended || !outcome.private_deliveries.is_empty() {
            self.abort()
        } else {
            Ok(false)
        }
    }

    pub fn abort(&self) -> Result<bool> {
        let aborted = {
            let mut state = self.state()?;
            let pending = state.pending_start.take().is_some();
            state.game.abort() || pending
        };
        let reserved = self.entertainment.active().is_some_and(is_card_game_kind);
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

    pub fn lobby_kind_for_join(&self, player: &str) -> Result<Option<EntertainmentKind>> {
        let active = self
            .entertainment
            .active()
            .filter(|kind| is_card_game_kind(*kind));
        let state = self.state()?;
        Ok(active.filter(|_| state.game.is_lobby() && !state.game.lobby_contains(player)))
    }

    pub fn retry_hand_delivery(&self, player: &str) -> Result<()> {
        self.state()?.game.retry_hand_delivery(player);
        Ok(())
    }

    pub fn begin_delivery(&self, outcome: &LandlordOutcome) {
        self.finish_outcome(outcome);
    }

    fn finish_outcome(&self, outcome: &LandlordOutcome) {
        if outcome.ended {
            self.release_active_card_game();
        }
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
