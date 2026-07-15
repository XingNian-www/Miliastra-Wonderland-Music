use std::collections::VecDeque;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::repository::UndercoverBankStore;
use super::{
    UndercoverCommand, UndercoverConfig, UndercoverDelivery, UndercoverGame, UndercoverMode,
    UndercoverSnapshot, random_seed,
};
use crate::features::entertainment::{AcquireOutcome, EntertainmentCoordinator, EntertainmentKind};
use crate::runtime::identity::{BusinessOperationId, SessionGeneration};

/// Identifies one UI effect chain owned by the business runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UndercoverEffectKey {
    pub operation_id: BusinessOperationId,
    pub session_generation: SessionGeneration,
}

impl UndercoverEffectKey {
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
pub enum UndercoverEffectLane {
    Formal,
    Deferred,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndercoverEffect {
    FriendVerify { player: String, message: String },
    Friend { player: String, message: String },
    SecretFriend { player: String, message: String },
    Hall { message: String },
    HallBatch { messages: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndercoverEffectRequest {
    pub key: UndercoverEffectKey,
    pub lane: UndercoverEffectLane,
    pub effect: UndercoverEffect,
}

#[derive(Debug)]
pub enum UndercoverEffectResult {
    FriendVerify(Result<bool>),
    Friend(Result<bool>),
    SecretFriend(Result<bool>),
    Hall(Result<()>),
    HallBatch(Result<()>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndercoverCompletion {
    pub action: &'static str,
    pub ended: bool,
}

#[derive(Clone, Debug)]
pub enum UndercoverCommandStart {
    Completed(UndercoverCompletion),
    Suspended(UndercoverEffectRequest),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndercoverLateResult {
    pub key: UndercoverEffectKey,
}

#[derive(Debug)]
pub enum UndercoverResume {
    Completed(UndercoverCompletion),
    Suspended(UndercoverEffectRequest),
    Late(UndercoverLateResult),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndercoverEffectClaim {
    Claimed,
    Late(UndercoverLateResult),
}

#[derive(Clone, Debug)]
pub struct UndercoverTimedOutcome {
    action: &'static str,
    request: UndercoverEffectRequest,
}

impl UndercoverTimedOutcome {
    pub fn action(&self) -> &'static str {
        self.action
    }

    pub fn into_request(self) -> UndercoverEffectRequest {
        self.request
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndercoverCommandSource {
    Hall,
    Friend,
    Console,
}

pub trait UndercoverDeliveryPort {
    fn verify_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_secret_friend(&self, player: &str, message: &str) -> Result<bool>;
    fn send_hall(&self, message: &str) -> Result<()>;
    fn send_hall_batch(&self, messages: &[String]) -> Result<()>;
}

/// Runtime-owned undercover application service.
pub struct UndercoverRuntimeService {
    game: UndercoverGame,
    bank: UndercoverBankStore,
    entertainment: EntertainmentCoordinator,
    config: UndercoverConfig,
    generation: SessionGeneration,
    pending: std::collections::HashMap<BusinessOperationId, PendingUndercoverEffect>,
}

struct PendingUndercoverEffect {
    key: UndercoverEffectKey,
    effect: UndercoverEffect,
    continuation: UndercoverContinuation,
    claimed: bool,
}

enum UndercoverContinuation {
    VerifyCreate {
        player: String,
        mode: UndercoverMode,
        source: UndercoverCommandSource,
        now: Instant,
    },
    VerifyJoin {
        player: String,
        source: UndercoverCommandSource,
        now: Instant,
    },
    DeliverSecrets {
        remaining: VecDeque<(String, String)>,
        now: Instant,
    },
    Deliveries {
        remaining: VecDeque<UndercoverDelivery>,
        lane: UndercoverEffectLane,
        action: &'static str,
        ended: bool,
    },
}

impl UndercoverRuntimeService {
    pub fn new(config: UndercoverConfig, entertainment: EntertainmentCoordinator) -> Self {
        let bank = UndercoverBankStore::new(
            config.word_bank_path.clone(),
            config.used_state_path.clone(),
        );
        Self {
            game: UndercoverGame::new(config.clone()),
            bank,
            entertainment,
            config,
            generation: SessionGeneration::INITIAL,
            pending: std::collections::HashMap::new(),
        }
    }

    pub fn begin_command(
        &mut self,
        player: &str,
        source: UndercoverCommandSource,
        command: &UndercoverCommand,
        now: Instant,
        operation_id: BusinessOperationId,
    ) -> Result<UndercoverCommandStart> {
        let key = UndercoverEffectKey::new(operation_id, self.generation);
        match command {
            UndercoverCommand::CreateSingle | UndercoverCommand::CreateDouble => {
                self.begin_create(player, source, command, now, operation_id)
            }
            UndercoverCommand::Join => self.begin_join(player, source, now, operation_id),
            UndercoverCommand::Start => self.begin_start(source, player, now, operation_id),
            UndercoverCommand::Status => {
                let status = self.game.status(player, now);
                Ok(self.begin_deliveries(
                    key,
                    UndercoverEffectLane::Formal,
                    vec![UndercoverDelivery::Hall(status)],
                    "status",
                    false,
                ))
            }
            UndercoverCommand::Exit => {
                let result = self.game.exit(player, now);
                match result {
                    Ok(deliveries) => {
                        let ended = !self.game.is_active();
                        Ok(self.begin_deliveries(
                            key,
                            UndercoverEffectLane::Formal,
                            deliveries,
                            "exit",
                            ended,
                        ))
                    }
                    Err(error) => self.begin_error(key, source, player, error.to_string()),
                }
            }
            UndercoverCommand::End => {
                let requester = (source != UndercoverCommandSource::Console).then_some(player);
                match self.game.end(requester) {
                    Ok(deliveries) => Ok(self.begin_deliveries(
                        key,
                        UndercoverEffectLane::Formal,
                        deliveries,
                        "ended",
                        true,
                    )),
                    Err(error) => self.begin_error(key, source, player, error.to_string()),
                }
            }
            UndercoverCommand::Describe(description) => {
                match self.game.describe(player, description, now) {
                    Ok(deliveries) => Ok(self.begin_deliveries(
                        key,
                        UndercoverEffectLane::Formal,
                        deliveries,
                        "described",
                        false,
                    )),
                    Err(error) => self.begin_error(key, source, player, error.to_string()),
                }
            }
            UndercoverCommand::Vote(position) => match self.game.vote(player, *position, now) {
                Ok(deliveries) => {
                    let ended = !self.game.is_active();
                    Ok(self.begin_deliveries(
                        key,
                        UndercoverEffectLane::Formal,
                        deliveries,
                        "voted",
                        ended,
                    ))
                }
                Err(error) => self.begin_error(key, source, player, error.to_string()),
            },
        }
    }

    pub fn claim(&mut self, key: UndercoverEffectKey) -> Result<UndercoverEffectClaim> {
        let Some(pending) = self.pending.get_mut(&key.operation_id) else {
            return Ok(UndercoverEffectClaim::Late(UndercoverLateResult { key }));
        };
        if pending.key != key || pending.claimed {
            return Ok(UndercoverEffectClaim::Late(UndercoverLateResult { key }));
        }
        pending.claimed = true;
        Ok(UndercoverEffectClaim::Claimed)
    }

    pub fn resume(
        &mut self,
        key: UndercoverEffectKey,
        result: UndercoverEffectResult,
    ) -> Result<UndercoverResume> {
        let Some(pending) = self.pending.get(&key.operation_id) else {
            return Ok(UndercoverResume::Late(UndercoverLateResult { key }));
        };
        if pending.key != key || !pending.claimed || key.session_generation != self.generation {
            return Ok(UndercoverResume::Late(UndercoverLateResult { key }));
        }
        if !effect_accepts(&pending.effect, &result) {
            bail!("谁是卧底效果结果与挂起效果不匹配");
        }
        let pending = self
            .pending
            .remove(&key.operation_id)
            .expect("validated undercover effect");
        self.resume_pending(key, pending.continuation, result)
    }

    pub fn cancel(&mut self, key: UndercoverEffectKey) -> Result<()> {
        let Some(pending) = self.pending.get(&key.operation_id) else {
            return Ok(());
        };
        if pending.key != key || pending.claimed {
            return Ok(());
        }
        let pending = self
            .pending
            .remove(&key.operation_id)
            .expect("validated undercover effect");
        match pending.continuation {
            UndercoverContinuation::VerifyCreate { .. } => {
                self.entertainment.release(EntertainmentKind::Undercover);
            }
            UndercoverContinuation::DeliverSecrets { .. } => {
                self.game.cancel_delivery();
                self.entertainment.release(EntertainmentKind::Undercover);
            }
            UndercoverContinuation::Deliveries { ended: true, .. } => {
                self.finish_session();
            }
            UndercoverContinuation::VerifyJoin { .. }
            | UndercoverContinuation::Deliveries { ended: false, .. } => {}
        }
        Ok(())
    }

    pub fn abort(&mut self) -> bool {
        self.pending.clear();
        let aborted = self.game.abort();
        if aborted {
            self.finish_session();
        }
        aborted
    }

    pub fn snapshot(&self, now: Instant) -> UndercoverSnapshot {
        self.game.snapshot(now)
    }

    pub fn session_generation(&self) -> SessionGeneration {
        self.generation
    }

    pub fn next_deadline(
        &self,
        now: Instant,
        clock_active: bool,
    ) -> Option<(super::UndercoverDeadlineKind, Instant)> {
        self.game.next_deadline(now, clock_active)
    }

    pub fn handle_deadline(
        &mut self,
        kind: super::UndercoverDeadlineKind,
        now: Instant,
        operation_id: BusinessOperationId,
    ) -> Result<Option<UndercoverTimedOutcome>> {
        let Some((expected, deadline)) = self.next_deadline(now, true) else {
            return Ok(None);
        };
        if expected != kind || deadline > now {
            return Ok(None);
        }
        let deliveries = self.game.tick(now);
        let ended = !self.game.is_active();
        if deliveries.is_empty() {
            return Ok(None);
        }
        let key = UndercoverEffectKey::new(operation_id, self.generation);
        let action = if ended {
            "timed-settlement"
        } else {
            "timed-reminder"
        };
        let mut queue = deliveries.into_iter().collect::<VecDeque<_>>();
        let first = queue.pop_front().expect("non-empty timed delivery list");
        let request = self.insert_effect(
            key,
            UndercoverEffectLane::Deferred,
            delivery_effect(&first),
            UndercoverContinuation::Deliveries {
                remaining: queue,
                lane: UndercoverEffectLane::Deferred,
                action,
                ended,
            },
        );
        Ok(Some(UndercoverTimedOutcome { action, request }))
    }

    fn begin_create(
        &mut self,
        player: &str,
        source: UndercoverCommandSource,
        command: &UndercoverCommand,
        now: Instant,
        operation_id: BusinessOperationId,
    ) -> Result<UndercoverCommandStart> {
        if !self.config.enabled {
            return self.begin_error(
                UndercoverEffectKey::new(operation_id, self.generation),
                source,
                player,
                "谁是卧底未启用".to_string(),
            );
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
                return self.begin_error(
                    UndercoverEffectKey::new(operation_id, self.generation),
                    source,
                    player,
                    "已有谁是卧底房间或牌局进行中".to_string(),
                );
            }
            AcquireOutcome::Occupied(kind) => {
                return self.begin_error(
                    UndercoverEffectKey::new(operation_id, self.generation),
                    source,
                    player,
                    format!("{}正在进行，请结束后再开始谁是卧底", kind.label()),
                );
            }
        }
        let Some(next_generation) = self.generation.checked_next() else {
            self.entertainment.release(EntertainmentKind::Undercover);
            return Err(anyhow!("谁是卧底会话代数已耗尽"));
        };
        self.generation = next_generation;
        let key = UndercoverEffectKey::new(operation_id, self.generation);
        let request = self.insert_effect(
            key,
            UndercoverEffectLane::Formal,
            UndercoverEffect::FriendVerify {
                player: player.trim().to_string(),
                message: "谁是卧底报名成功，请回到大厅等待组局".to_string(),
            },
            UndercoverContinuation::VerifyCreate {
                player: player.trim().to_string(),
                mode,
                source,
                now,
            },
        );
        Ok(UndercoverCommandStart::Suspended(request))
    }

    fn begin_join(
        &mut self,
        player: &str,
        source: UndercoverCommandSource,
        now: Instant,
        operation_id: BusinessOperationId,
    ) -> Result<UndercoverCommandStart> {
        let key = UndercoverEffectKey::new(operation_id, self.generation);
        if self.entertainment.active() != Some(EntertainmentKind::Undercover) {
            return self.begin_error(key, source, player, "当前没有谁是卧底报名房间".to_string());
        }
        if !self.game.is_lobby() {
            return self.begin_error(key, source, player, "谁是卧底已经开局".to_string());
        }
        if self.game.lobby_contains(player) {
            return self.begin_error(key, source, player, "你已经加入本局谁是卧底".to_string());
        }
        let request = self.insert_effect(
            key,
            UndercoverEffectLane::Formal,
            UndercoverEffect::FriendVerify {
                player: player.trim().to_string(),
                message: "谁是卧底报名成功，请回到大厅等待开局".to_string(),
            },
            UndercoverContinuation::VerifyJoin {
                player: player.trim().to_string(),
                source,
                now,
            },
        );
        Ok(UndercoverCommandStart::Suspended(request))
    }

    fn begin_start(
        &mut self,
        source: UndercoverCommandSource,
        player: &str,
        now: Instant,
        operation_id: BusinessOperationId,
    ) -> Result<UndercoverCommandStart> {
        let key = UndercoverEffectKey::new(operation_id, self.generation);
        if let Err(error) = self
            .game
            .authorize_start((source != UndercoverCommandSource::Console).then_some(player))
        {
            return self.begin_error(key, source, player, error.to_string());
        }
        let words = match self.bank.consume_random(random_seed()) {
            Ok(words) => words,
            Err(error) => return self.begin_error(key, source, player, error.to_string()),
        };
        let deliveries = match self.game.start(words, now) {
            Ok(deliveries) => deliveries,
            Err(error) => return self.begin_error(key, source, player, error.to_string()),
        };
        let secrets = deliveries
            .into_iter()
            .filter_map(|delivery| match delivery {
                UndercoverDelivery::Friend { player, message } => Some((player, message)),
                _ => None,
            })
            .collect::<VecDeque<_>>();
        let request = self.begin_effect(
            key,
            UndercoverEffectLane::Formal,
            Vec::new(),
            UndercoverContinuation::DeliverSecrets {
                remaining: secrets,
                now,
            },
        )?;
        Ok(UndercoverCommandStart::Suspended(request))
    }

    fn resume_pending(
        &mut self,
        key: UndercoverEffectKey,
        continuation: UndercoverContinuation,
        result: UndercoverEffectResult,
    ) -> Result<UndercoverResume> {
        match continuation {
            UndercoverContinuation::VerifyCreate {
                player,
                mode,
                source,
                now,
            } => match result {
                UndercoverEffectResult::FriendVerify(Ok(true)) => {
                    if let Err(error) = self.game.create(&player, mode, now) {
                        self.entertainment.release(EntertainmentKind::Undercover);
                        return self
                            .begin_error(key, source, &player, error.to_string())
                            .map(|start| match start {
                                UndercoverCommandStart::Completed(completion) => {
                                    UndercoverResume::Completed(completion)
                                }
                                UndercoverCommandStart::Suspended(request) => {
                                    UndercoverResume::Suspended(request)
                                }
                            });
                    }
                    let status = self.game.status(&player, now);
                    Ok(self.resume_with_deliveries(
                        key,
                        vec![UndercoverDelivery::Hall(status)],
                        UndercoverEffectLane::Formal,
                        "created",
                        false,
                    ))
                }
                UndercoverEffectResult::FriendVerify(Ok(false)) => {
                    self.entertainment.release(EntertainmentKind::Undercover);
                    Ok(self.resume_with_deliveries(
                        key,
                        vec![UndercoverDelivery::Hall(
                            "谁是卧底报名失败：好友列表未找到唯一昵称".to_string(),
                        )],
                        UndercoverEffectLane::Formal,
                        "verification-rejected",
                        false,
                    ))
                }
                UndercoverEffectResult::FriendVerify(Err(error)) => {
                    self.entertainment.release(EntertainmentKind::Undercover);
                    Err(error)
                }
                _ => unreachable!("effect result checked before resume"),
            },
            UndercoverContinuation::VerifyJoin {
                player,
                source,
                now,
            } => match result {
                UndercoverEffectResult::FriendVerify(Ok(true)) => {
                    let outcome = self.game.join(&player, now);
                    match outcome {
                        Ok(()) if self.game.lobby_is_full() => self
                            .begin_start(source, &player, now, key.operation_id)
                            .map(|start| match start {
                                UndercoverCommandStart::Completed(completion) => {
                                    UndercoverResume::Completed(completion)
                                }
                                UndercoverCommandStart::Suspended(request) => {
                                    UndercoverResume::Suspended(request)
                                }
                            }),
                        Ok(()) => {
                            let status = self.game.status(&player, now);
                            Ok(self.resume_with_deliveries(
                                key,
                                vec![UndercoverDelivery::Hall(status)],
                                UndercoverEffectLane::Formal,
                                "joined",
                                false,
                            ))
                        }
                        Err(error) => self
                            .begin_error(key, source, &player, error.to_string())
                            .map(|start| match start {
                                UndercoverCommandStart::Completed(completion) => {
                                    UndercoverResume::Completed(completion)
                                }
                                UndercoverCommandStart::Suspended(request) => {
                                    UndercoverResume::Suspended(request)
                                }
                            }),
                    }
                }
                UndercoverEffectResult::FriendVerify(Ok(false)) => Ok(self.resume_with_deliveries(
                    key,
                    vec![UndercoverDelivery::Hall(
                        "谁是卧底报名失败：好友列表未找到唯一昵称".to_string(),
                    )],
                    UndercoverEffectLane::Formal,
                    "verification-rejected",
                    false,
                )),
                UndercoverEffectResult::FriendVerify(Err(error)) => Err(error),
                _ => unreachable!("effect result checked before resume"),
            },
            UndercoverContinuation::DeliverSecrets { mut remaining, now } => match result {
                UndercoverEffectResult::SecretFriend(Ok(true)) => {
                    if remaining.is_empty() {
                        let opening = self.game.complete_delivery(now)?;
                        Ok(self.resume_with_deliveries(
                            key,
                            opening,
                            UndercoverEffectLane::Formal,
                            "started",
                            false,
                        ))
                    } else {
                        let (player, message) =
                            remaining.pop_front().expect("remaining secret delivery");
                        let request = self.begin_effect(
                            key,
                            UndercoverEffectLane::Formal,
                            vec![UndercoverDelivery::Friend { player, message }],
                            UndercoverContinuation::DeliverSecrets { remaining, now },
                        )?;
                        Ok(UndercoverResume::Suspended(request))
                    }
                }
                UndercoverEffectResult::SecretFriend(Ok(false)) => {
                    let canceled = self.game.cancel_delivery();
                    self.entertainment.release(EntertainmentKind::Undercover);
                    Ok(self.resume_with_deliveries(
                        key,
                        canceled,
                        UndercoverEffectLane::Formal,
                        "start-canceled",
                        true,
                    ))
                }
                UndercoverEffectResult::SecretFriend(Err(error)) => {
                    self.game.cancel_delivery();
                    self.entertainment.release(EntertainmentKind::Undercover);
                    Err(error)
                }
                _ => unreachable!("effect result checked before resume"),
            },
            UndercoverContinuation::Deliveries {
                mut remaining,
                lane,
                action,
                ended,
            } => match result {
                UndercoverEffectResult::Friend(Ok(true))
                | UndercoverEffectResult::SecretFriend(Ok(true))
                | UndercoverEffectResult::Hall(Ok(()))
                | UndercoverEffectResult::HallBatch(Ok(())) => {
                    if let Some(next) = remaining.pop_front() {
                        let request = self.begin_effect(
                            key,
                            lane,
                            vec![next],
                            UndercoverContinuation::Deliveries {
                                remaining,
                                lane,
                                action,
                                ended,
                            },
                        )?;
                        Ok(UndercoverResume::Suspended(request))
                    } else {
                        if ended {
                            self.finish_session();
                        }
                        Ok(UndercoverResume::Completed(UndercoverCompletion {
                            action,
                            ended,
                        }))
                    }
                }
                UndercoverEffectResult::Friend(Ok(false))
                | UndercoverEffectResult::SecretFriend(Ok(false)) => {
                    if ended {
                        self.finish_session();
                    }
                    bail!("谁是卧底好友消息发送失败")
                }
                UndercoverEffectResult::Friend(Err(error))
                | UndercoverEffectResult::SecretFriend(Err(error))
                | UndercoverEffectResult::Hall(Err(error))
                | UndercoverEffectResult::HallBatch(Err(error)) => {
                    if ended {
                        self.finish_session();
                    }
                    Err(error)
                }
                UndercoverEffectResult::FriendVerify(_) => {
                    unreachable!("effect result checked before resume")
                }
            },
        }
    }

    fn resume_with_deliveries(
        &mut self,
        key: UndercoverEffectKey,
        deliveries: Vec<UndercoverDelivery>,
        lane: UndercoverEffectLane,
        action: &'static str,
        ended: bool,
    ) -> UndercoverResume {
        let mut queue = deliveries.into_iter().collect::<VecDeque<_>>();
        let Some(first) = queue.pop_front() else {
            if ended {
                self.finish_session();
            }
            return UndercoverResume::Completed(UndercoverCompletion { action, ended });
        };
        let effect = delivery_effect(&first);
        let request = self.insert_effect(
            key,
            lane,
            effect,
            UndercoverContinuation::Deliveries {
                remaining: queue,
                lane,
                action,
                ended,
            },
        );
        UndercoverResume::Suspended(request)
    }

    fn begin_deliveries(
        &mut self,
        key: UndercoverEffectKey,
        lane: UndercoverEffectLane,
        deliveries: Vec<UndercoverDelivery>,
        action: &'static str,
        ended: bool,
    ) -> UndercoverCommandStart {
        let mut queue = deliveries.into_iter().collect::<VecDeque<_>>();
        let Some(first) = queue.pop_front() else {
            if ended {
                self.finish_session();
            }
            return UndercoverCommandStart::Completed(UndercoverCompletion { action, ended });
        };
        let request = self.insert_effect(
            key,
            lane,
            delivery_effect(&first),
            UndercoverContinuation::Deliveries {
                remaining: queue,
                lane,
                action,
                ended,
            },
        );
        UndercoverCommandStart::Suspended(request)
    }

    fn begin_effect(
        &mut self,
        key: UndercoverEffectKey,
        lane: UndercoverEffectLane,
        deliveries: Vec<UndercoverDelivery>,
        continuation: UndercoverContinuation,
    ) -> Result<UndercoverEffectRequest> {
        let Some(first) = deliveries.into_iter().next() else {
            if let UndercoverContinuation::DeliverSecrets { mut remaining, now } = continuation {
                let Some((player, message)) = remaining.pop_front() else {
                    let opening = self.game.complete_delivery(now)?;
                    return self.begin_effect(
                        key,
                        lane,
                        opening,
                        UndercoverContinuation::Deliveries {
                            remaining: VecDeque::new(),
                            lane,
                            action: "started",
                            ended: false,
                        },
                    );
                };
                return Ok(self.insert_effect(
                    key,
                    lane,
                    UndercoverEffect::SecretFriend { player, message },
                    UndercoverContinuation::DeliverSecrets { remaining, now },
                ));
            }
            bail!("谁是卧底效果链不能为空");
        };
        Ok(self.insert_effect(key, lane, delivery_effect(&first), continuation))
    }

    fn insert_effect(
        &mut self,
        key: UndercoverEffectKey,
        lane: UndercoverEffectLane,
        effect: UndercoverEffect,
        continuation: UndercoverContinuation,
    ) -> UndercoverEffectRequest {
        let request = UndercoverEffectRequest {
            key,
            lane,
            effect: effect.clone(),
        };
        self.pending.insert(
            key.operation_id,
            PendingUndercoverEffect {
                key,
                effect,
                continuation,
                claimed: false,
            },
        );
        request
    }

    fn begin_error(
        &mut self,
        key: UndercoverEffectKey,
        source: UndercoverCommandSource,
        player: &str,
        message: String,
    ) -> Result<UndercoverCommandStart> {
        let effect = if source == UndercoverCommandSource::Friend {
            UndercoverEffect::Friend {
                player: player.trim().to_string(),
                message,
            }
        } else {
            UndercoverEffect::Hall { message }
        };
        Ok(UndercoverCommandStart::Suspended(self.insert_effect(
            key,
            UndercoverEffectLane::Formal,
            effect,
            UndercoverContinuation::Deliveries {
                remaining: VecDeque::new(),
                lane: UndercoverEffectLane::Formal,
                action: "error",
                ended: false,
            },
        )))
    }

    fn finish_session(&mut self) {
        self.entertainment.release(EntertainmentKind::Undercover);
        self.generation = self.generation.checked_next().unwrap_or(self.generation);
    }
}

fn delivery_effect(delivery: &UndercoverDelivery) -> UndercoverEffect {
    match delivery {
        UndercoverDelivery::Hall(message) => UndercoverEffect::Hall {
            message: message.clone(),
        },
        UndercoverDelivery::HallBatch(messages) => UndercoverEffect::HallBatch {
            messages: messages.clone(),
        },
        UndercoverDelivery::Friend { player, message } => UndercoverEffect::SecretFriend {
            player: player.clone(),
            message: message.clone(),
        },
    }
}

fn effect_accepts(effect: &UndercoverEffect, result: &UndercoverEffectResult) -> bool {
    matches!(
        (effect, result),
        (
            UndercoverEffect::FriendVerify { .. },
            UndercoverEffectResult::FriendVerify(_)
        ) | (
            UndercoverEffect::Friend { .. },
            UndercoverEffectResult::Friend(_)
        ) | (
            UndercoverEffect::SecretFriend { .. },
            UndercoverEffectResult::SecretFriend(_)
        ) | (
            UndercoverEffect::Hall { .. },
            UndercoverEffectResult::Hall(_)
        ) | (
            UndercoverEffect::HallBatch { .. },
            UndercoverEffectResult::HallBatch(_)
        )
    )
}

#[cfg(test)]
mod runtime_tests {
    use std::fs;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::features::entertainment::EntertainmentCoordinator;
    use crate::runtime::identity::BusinessOperationId;

    fn service(timeout: u64) -> (UndercoverRuntimeService, EntertainmentCoordinator) {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("mwm-undercover-runtime-{suffix}"));
        fs::create_dir_all(&directory).expect("runtime test directory");
        let bank_path = directory.join("undercover.yaml");
        fs::write(
            &bank_path,
            "词组:\n  - 平民词: 苹果\n    卧底词: 梨\n    启用: true\n",
        )
        .expect("word bank");
        let entertainment = EntertainmentCoordinator::new();
        let service = UndercoverRuntimeService::new(
            UndercoverConfig {
                enabled: true,
                word_bank_path: bank_path,
                used_state_path: directory.join("used.yaml"),
                lobby_timeout_seconds: timeout,
                ..UndercoverConfig::default()
            },
            entertainment.clone(),
        );
        (service, entertainment)
    }

    #[test]
    fn runtime_effect_chain_keeps_friend_verification_outside_business_state() {
        let (mut service, entertainment) = service(60);
        let now = Instant::now();
        let start = service
            .begin_command(
                "甲",
                UndercoverCommandSource::Friend,
                &UndercoverCommand::CreateSingle,
                now,
                BusinessOperationId::new(1),
            )
            .expect("begin create");
        let UndercoverCommandStart::Suspended(verification) = start else {
            panic!("create should wait for friend verification");
        };
        assert!(matches!(
            verification.effect,
            UndercoverEffect::FriendVerify { .. }
        ));
        assert_eq!(
            service.claim(verification.key).expect("claim verification"),
            UndercoverEffectClaim::Claimed
        );
        let hall = match service
            .resume(
                verification.key,
                UndercoverEffectResult::FriendVerify(Ok(true)),
            )
            .expect("resume verification")
        {
            UndercoverResume::Suspended(request) => request,
            other => panic!("expected status delivery, got {other:?}"),
        };
        assert!(matches!(hall.effect, UndercoverEffect::Hall { .. }));
        service.claim(hall.key).expect("claim status");
        let completed = service
            .resume(hall.key, UndercoverEffectResult::Hall(Ok(())))
            .expect("resume status");
        assert!(matches!(completed, UndercoverResume::Completed(_)));
        assert_eq!(
            service.snapshot(now).phase,
            "lobby",
            "the runtime owns the lobby after UI work completes"
        );
        assert_eq!(entertainment.active(), Some(EntertainmentKind::Undercover));
    }

    #[test]
    fn timer_deadline_turns_lobby_timeout_into_a_deferred_effect() {
        let (mut service, _) = service(1);
        let now = Instant::now();
        let start = service
            .begin_command(
                "甲",
                UndercoverCommandSource::Friend,
                &UndercoverCommand::CreateSingle,
                now,
                BusinessOperationId::new(1),
            )
            .expect("begin create");
        let UndercoverCommandStart::Suspended(verification) = start else {
            panic!("create should suspend");
        };
        service.claim(verification.key).expect("claim verification");
        let hall = match service
            .resume(
                verification.key,
                UndercoverEffectResult::FriendVerify(Ok(true)),
            )
            .expect("resume verification")
        {
            UndercoverResume::Suspended(request) => request,
            _ => panic!("status should suspend"),
        };
        service.claim(hall.key).expect("claim status");
        service
            .resume(hall.key, UndercoverEffectResult::Hall(Ok(())))
            .expect("resume status");

        let (kind, deadline) = service.next_deadline(now, true).expect("lobby deadline");
        assert_eq!(kind, super::super::UndercoverDeadlineKind::LobbyIdle);
        assert!(deadline <= now + Duration::from_secs(1));
        let outcome = service
            .handle_deadline(
                kind,
                now + Duration::from_secs(2),
                BusinessOperationId::new(2),
            )
            .expect("handle timeout")
            .expect("timeout delivery");
        assert_eq!(outcome.action(), "timed-settlement");
        assert!(matches!(
            outcome.into_request().effect,
            UndercoverEffect::Hall { .. }
        ));
    }

    #[test]
    fn cancelling_a_timed_settlement_releases_the_entertainment_session() {
        let (mut service, entertainment) = service(1);
        let now = Instant::now();
        let start = service
            .begin_command(
                "甲",
                UndercoverCommandSource::Friend,
                &UndercoverCommand::CreateSingle,
                now,
                BusinessOperationId::new(1),
            )
            .expect("begin create");
        let UndercoverCommandStart::Suspended(verification) = start else {
            panic!("create should suspend");
        };
        service.claim(verification.key).expect("claim verification");
        let hall = match service
            .resume(
                verification.key,
                UndercoverEffectResult::FriendVerify(Ok(true)),
            )
            .expect("resume verification")
        {
            UndercoverResume::Suspended(request) => request,
            _ => panic!("status should suspend"),
        };
        service.claim(hall.key).expect("claim status");
        service
            .resume(hall.key, UndercoverEffectResult::Hall(Ok(())))
            .expect("resume status");

        let (kind, _) = service.next_deadline(now, true).expect("lobby deadline");
        let outcome = service
            .handle_deadline(
                kind,
                now + Duration::from_secs(2),
                BusinessOperationId::new(2),
            )
            .expect("handle timeout")
            .expect("timeout delivery");
        let request = outcome.into_request();
        service.cancel(request.key).expect("cancel timed delivery");

        assert_eq!(entertainment.active(), None);
    }
}
