use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::repository::UndercoverBankStore;
use super::{
    UndercoverCommand, UndercoverConfig, UndercoverDelivery, UndercoverGame, UndercoverMode,
    UndercoverSnapshot, random_seed,
};
use crate::features::entertainment::{AcquireOutcome, EntertainmentCoordinator, EntertainmentKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndercoverCommandSource {
    Hall,
    Friend,
    Console,
}

pub struct UndercoverCommandContext<'a> {
    pub player: &'a str,
    pub source: UndercoverCommandSource,
}

pub trait UndercoverDeliveryPort {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_secret_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_hall(&self, message: &str) -> Result<()>;
    fn send_hall_batch(&self, messages: &[String]) -> Result<()>;
}

pub struct UndercoverDeliveryTask {
    service: UndercoverService,
    deliveries: Vec<UndercoverDelivery>,
}

impl UndercoverDeliveryTask {
    pub fn label(&self) -> &'static str {
        "发送谁是卧底阶段消息"
    }

    pub fn execute(self, port: &dyn UndercoverDeliveryPort) -> Result<()> {
        self.service.deliver(self.deliveries, port)
    }
}

#[derive(Clone)]
pub struct UndercoverService {
    game: Arc<Mutex<UndercoverGame>>,
    bank: UndercoverBankStore,
    entertainment: EntertainmentCoordinator,
    enabled: bool,
}

impl UndercoverService {
    pub fn new(config: UndercoverConfig, entertainment: EntertainmentCoordinator) -> Self {
        let bank = UndercoverBankStore::new(
            config.word_bank_path.clone(),
            config.used_state_path.clone(),
        );
        let enabled = config.enabled;
        Self {
            game: Arc::new(Mutex::new(UndercoverGame::new(config))),
            bank,
            entertainment,
            enabled,
        }
    }

    pub fn execute(
        &self,
        context: UndercoverCommandContext<'_>,
        command: &UndercoverCommand,
        port: &dyn UndercoverDeliveryPort,
        now: Instant,
    ) -> Result<()> {
        match command {
            UndercoverCommand::CreateSingle | UndercoverCommand::CreateDouble => {
                self.create(context, command, port, now)
            }
            UndercoverCommand::Join => self.join(context, port, now),
            UndercoverCommand::Start => {
                let requester =
                    (context.source != UndercoverCommandSource::Console).then_some(context.player);
                self.start(requester, port, now)
            }
            UndercoverCommand::Status => {
                let message = self.game()?.status(context.player, now);
                port.send_hall(&message)
            }
            UndercoverCommand::Exit => {
                let outcome = {
                    let mut game = self.game()?;
                    game.exit(context.player, now)
                        .map(|deliveries| (deliveries, !game.is_active()))
                };
                match outcome {
                    Ok((deliveries, ended)) => {
                        if ended {
                            self.entertainment.release(EntertainmentKind::Undercover);
                        }
                        self.deliver(deliveries, port)
                    }
                    Err(error) => self.send_error(context, &error.to_string(), port),
                }
            }
            UndercoverCommand::End => {
                let requester =
                    (context.source != UndercoverCommandSource::Console).then_some(context.player);
                let outcome = self.game()?.end(requester);
                match outcome {
                    Ok(deliveries) => {
                        self.entertainment.release(EntertainmentKind::Undercover);
                        self.deliver(deliveries, port)
                    }
                    Err(error) => self.send_error(context, &error.to_string(), port),
                }
            }
            UndercoverCommand::Describe(description) => {
                let outcome = self.game()?.describe(context.player, description, now);
                match outcome {
                    Ok(deliveries) => self.deliver(deliveries, port),
                    Err(error) => self.send_error(context, &error.to_string(), port),
                }
            }
            UndercoverCommand::Vote(position) => {
                let outcome = {
                    let mut game = self.game()?;
                    game.vote(context.player, *position, now)
                        .map(|deliveries| (deliveries, !game.is_active()))
                };
                match outcome {
                    Ok((deliveries, ended)) => {
                        if ended {
                            self.entertainment.release(EntertainmentKind::Undercover);
                        }
                        self.deliver(deliveries, port)
                    }
                    Err(error) => self.send_error(context, &error.to_string(), port),
                }
            }
        }
    }

    pub fn tick(&self, now: Instant, clock_active: bool) -> Result<Vec<UndercoverDelivery>> {
        if !clock_active {
            return Ok(Vec::new());
        }
        let (deliveries, ended) = {
            let mut game = self.game()?;
            let deliveries = game.tick(now);
            (deliveries, !game.is_active())
        };
        if ended {
            self.entertainment.release(EntertainmentKind::Undercover);
        }
        Ok(deliveries)
    }

    pub fn delivery_task(&self, deliveries: Vec<UndercoverDelivery>) -> UndercoverDeliveryTask {
        UndercoverDeliveryTask {
            service: self.clone(),
            deliveries,
        }
    }

    pub fn abort(&self) -> Result<bool> {
        let aborted = self.game()?.abort();
        if aborted {
            self.entertainment.release(EntertainmentKind::Undercover);
        }
        Ok(aborted)
    }

    pub fn snapshot(&self, now: Instant) -> Result<UndercoverSnapshot> {
        Ok(self.game()?.snapshot(now))
    }

    pub fn deliver(
        &self,
        deliveries: Vec<UndercoverDelivery>,
        port: &dyn UndercoverDeliveryPort,
    ) -> Result<()> {
        for delivery in deliveries {
            match delivery {
                UndercoverDelivery::Hall(message) => port.send_hall(&message)?,
                UndercoverDelivery::HallBatch(messages) => port.send_hall_batch(&messages)?,
                UndercoverDelivery::Friend { player, message } => {
                    if !port.send_secret_friend(&player, &message)? {
                        bail!("谁是卧底好友消息发送失败: {}", player);
                    }
                }
            }
        }
        Ok(())
    }

    fn create(
        &self,
        context: UndercoverCommandContext<'_>,
        command: &UndercoverCommand,
        port: &dyn UndercoverDeliveryPort,
        now: Instant,
    ) -> Result<()> {
        if !self.enabled {
            return port.send_hall("谁是卧底未启用");
        }
        let mode = if matches!(command, UndercoverCommand::CreateDouble) {
            UndercoverMode::Double
        } else {
            UndercoverMode::Single
        };
        match self
            .entertainment
            .try_acquire(EntertainmentKind::Undercover)?
        {
            AcquireOutcome::Acquired => {}
            AcquireOutcome::AlreadyOwned => {
                return self.send_error(context, "已有谁是卧底房间或牌局进行中", port);
            }
            AcquireOutcome::Occupied(kind) => {
                return self.send_error(
                    context,
                    &format!("{}正在进行，请结束后再开始谁是卧底", kind.label()),
                    port,
                );
            }
        }
        let verified = match port
            .verify_friend(context.player, "谁是卧底报名成功，请回到大厅等待组局")
        {
            Ok(verified) => verified,
            Err(error) => {
                self.entertainment.release(EntertainmentKind::Undercover);
                return Err(error);
            }
        };
        if !verified {
            self.entertainment.release(EntertainmentKind::Undercover);
            return port.send_hall("谁是卧底报名失败：好友列表未找到唯一昵称");
        }
        if let Err(error) = self.game()?.create(context.player, mode, now) {
            self.entertainment.release(EntertainmentKind::Undercover);
            return self.send_error(context, &error.to_string(), port);
        }
        let status = self.game()?.status(context.player, now);
        port.send_hall(&status)
    }

    fn join(
        &self,
        context: UndercoverCommandContext<'_>,
        port: &dyn UndercoverDeliveryPort,
        now: Instant,
    ) -> Result<()> {
        if self.entertainment.active() != Some(EntertainmentKind::Undercover) {
            return self.send_error(context, "当前没有谁是卧底报名房间", port);
        }
        let gate = {
            let game = self.game()?;
            if !game.is_lobby() {
                Err(anyhow!("谁是卧底已经开局"))
            } else if game.lobby_contains(context.player) {
                Err(anyhow!("你已经加入本局谁是卧底"))
            } else {
                Ok(())
            }
        };
        if let Err(error) = gate {
            return self.send_error(context, &error.to_string(), port);
        }
        if !port.verify_friend(context.player, "谁是卧底报名成功，请回到大厅等待开局")?
        {
            return port.send_hall("谁是卧底报名失败：好友列表未找到唯一昵称");
        }
        if let Err(error) = self.game()?.join(context.player, now) {
            return self.send_error(context, &error.to_string(), port);
        }
        if self.game()?.lobby_is_full() {
            self.start(None, port, now)
        } else {
            let status = self.game()?.status(context.player, now);
            port.send_hall(&status)
        }
    }

    fn start(
        &self,
        requester: Option<&str>,
        port: &dyn UndercoverDeliveryPort,
        now: Instant,
    ) -> Result<()> {
        let deliveries = {
            let mut game = self.game()?;
            if let Err(error) = game.authorize_start(requester) {
                return port.send_hall(&error.to_string());
            }
            let words = match self.bank.consume_random(random_seed()) {
                Ok(words) => words,
                Err(error) => return port.send_hall(&error.to_string()),
            };
            game.start(words, now)?
        };
        for delivery in deliveries {
            let UndercoverDelivery::Friend { player, message } = delivery else {
                continue;
            };
            let mut sent = false;
            for attempt in 1..=2 {
                match port.send_secret_friend(&player, &message) {
                    Ok(true) => {
                        sent = true;
                        break;
                    }
                    Ok(false) => log::warn!(
                        "谁是卧底发词确认失败: player={} attempt={}",
                        player,
                        attempt
                    ),
                    Err(error) => log::warn!(
                        "谁是卧底发词异常: player={} attempt={} error={:#}",
                        player,
                        attempt,
                        error
                    ),
                }
            }
            if !sent {
                let canceled = self.game()?.cancel_delivery();
                self.entertainment.release(EntertainmentKind::Undercover);
                return self.deliver(canceled, port);
            }
        }
        let opening = self.game()?.complete_delivery(Instant::now())?;
        self.deliver(opening, port)
    }

    fn send_error(
        &self,
        context: UndercoverCommandContext<'_>,
        message: &str,
        port: &dyn UndercoverDeliveryPort,
    ) -> Result<()> {
        if context.source == UndercoverCommandSource::Friend {
            if !port.send_friend(context.player, message)? {
                log::warn!("谁是卧底私聊错误回复失败: player={}", context.player);
            }
            Ok(())
        } else {
            port.send_hall(message)
        }
    }

    fn game(&self) -> Result<MutexGuard<'_, UndercoverGame>> {
        self.game
            .lock()
            .map_err(|_| anyhow!("undercover mutex poisoned"))
    }
}
