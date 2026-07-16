use std::collections::VecDeque;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};

use super::repository::UndercoverBankStore;
use super::{
    UndercoverCommand, UndercoverConfig, UndercoverDelivery, UndercoverGame, UndercoverMode,
    UndercoverSnapshot, random_seed,
};
use crate::features::entertainment::{AcquireOutcome, EntertainmentKind, EntertainmentState};
use crate::features::friend_delivery::{
    FriendBatchFailure, FriendBatchFailureKind, FriendBatchOutcome, FriendMessage,
};
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
    FriendBatch { deliveries: Vec<FriendMessage> },
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
    FriendBatch(Result<FriendBatchOutcome>),
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
    fn send_friend_batch(&self, deliveries: &[FriendMessage]) -> Result<FriendBatchOutcome>;
    fn send_hall(&self, message: &str) -> Result<()>;
    fn send_hall_batch(&self, messages: &[String]) -> Result<()>;
}

/// Runtime-owned undercover application service.
pub struct UndercoverRuntimeService {
    game: UndercoverGame,
    bank: UndercoverBankStore,
    config: UndercoverConfig,
    generation: SessionGeneration,
    pending: std::collections::HashMap<BusinessOperationId, PendingUndercoverEffect>,
    pending_retry: Option<PendingUndercoverRetry>,
}

struct PendingUndercoverRetry {
    session_generation: SessionGeneration,
    deliveries: Vec<FriendMessage>,
    now: Instant,
    failure: FriendBatchFailure,
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
    pub fn new(config: UndercoverConfig) -> Self {
        let bank = UndercoverBankStore::new(
            config.word_bank_path.clone(),
            config.used_state_path.clone(),
        );
        Self {
            game: UndercoverGame::new(config.clone()),
            bank,
            config,
            generation: SessionGeneration::INITIAL,
            pending: std::collections::HashMap::new(),
            pending_retry: None,
        }
    }

    pub fn begin_command(
        &mut self,
        entertainment: &mut EntertainmentState,
        player: &str,
        source: UndercoverCommandSource,
        command: &UndercoverCommand,
        now: Instant,
        operation_id: BusinessOperationId,
    ) -> Result<UndercoverCommandStart> {
        let key = UndercoverEffectKey::new(operation_id, self.generation);
        if matches!(command, UndercoverCommand::Retry) {
            return Ok(self.begin_retry_delivery(key));
        }
        if self.pending_retry.is_some() {
            if matches!(command, UndercoverCommand::End) {
                self.pending_retry = None;
            } else {
                return Ok(self.begin_deliveries(
                    key,
                    UndercoverEffectLane::Formal,
                    vec![UndercoverDelivery::Hall(
                        "谁是卧底私聊投递尚未完成，请使用#重试或#结束".to_string(),
                    )],
                    "delivery-waiting",
                    false,
                ));
            }
        }
        let result = match command {
            UndercoverCommand::CreateSingle | UndercoverCommand::CreateDouble => {
                self.begin_create(entertainment, player, source, command, now, operation_id)
            }
            UndercoverCommand::Join => {
                self.begin_join(entertainment, player, source, now, operation_id)
            }
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
            UndercoverCommand::Retry => unreachable!("retry handled before command dispatch"),
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
        };
        self.reconcile_entertainment(entertainment);
        result
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
        entertainment: &mut EntertainmentState,
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
        let result = self.resume_pending(key, pending.continuation, result);
        self.reconcile_entertainment(entertainment);
        result
    }

    pub fn cancel(
        &mut self,
        entertainment: &mut EntertainmentState,
        key: UndercoverEffectKey,
    ) -> Result<()> {
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
            UndercoverContinuation::VerifyCreate { .. } => {}
            UndercoverContinuation::DeliverSecrets { .. } => {
                self.game.cancel_delivery();
            }
            UndercoverContinuation::Deliveries { ended: true, .. } => {
                self.finish_session();
            }
            UndercoverContinuation::VerifyJoin { .. }
            | UndercoverContinuation::Deliveries { ended: false, .. } => {}
        }
        self.reconcile_entertainment(entertainment);
        Ok(())
    }

    pub fn abort(&mut self, entertainment: &mut EntertainmentState) -> bool {
        self.pending.clear();
        self.pending_retry = None;
        let aborted = self.game.abort();
        if aborted {
            self.finish_session();
        }
        entertainment.release(EntertainmentKind::Undercover);
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
        entertainment: &mut EntertainmentState,
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
            self.reconcile_entertainment(entertainment);
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
        entertainment: &mut EntertainmentState,
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
        match entertainment.try_acquire(EntertainmentKind::Undercover)? {
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
            entertainment.release(EntertainmentKind::Undercover);
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
        entertainment: &EntertainmentState,
        player: &str,
        source: UndercoverCommandSource,
        now: Instant,
        operation_id: BusinessOperationId,
    ) -> Result<UndercoverCommandStart> {
        let key = UndercoverEffectKey::new(operation_id, self.generation);
        if entertainment.active() != Some(EntertainmentKind::Undercover) {
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
                UndercoverDelivery::Friend { player, message } => {
                    Some(FriendMessage::new(player, message))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let request = self.insert_effect(
            key,
            UndercoverEffectLane::Formal,
            UndercoverEffect::FriendBatch {
                deliveries: secrets,
            },
            UndercoverContinuation::DeliverSecrets { now },
        );
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
            UndercoverContinuation::DeliverSecrets { now } => match result {
                UndercoverEffectResult::FriendBatch(Ok(FriendBatchOutcome::Complete)) => {
                    self.pending_retry = None;
                    let opening = self.game.complete_delivery(now)?;
                    Ok(self.resume_with_deliveries(
                        key,
                        opening,
                        UndercoverEffectLane::Formal,
                        "started",
                        false,
                    ))
                }
                UndercoverEffectResult::FriendBatch(Ok(FriendBatchOutcome::Failed {
                    retryable,
                    failure,
                })) => Ok(self.pause_secret_delivery(key, retryable, now, failure)),
                UndercoverEffectResult::FriendBatch(Err(error)) => Ok(self.pause_secret_delivery(
                    key,
                    Vec::new(),
                    now,
                    FriendBatchFailure::new(
                        FriendBatchFailureKind::ResultUnknown,
                        format!("谁是卧底私聊投递执行异常: {error:#}"),
                    ),
                )),
                _ => unreachable!("effect result checked before resume"),
            },
            UndercoverContinuation::Deliveries {
                mut remaining,
                lane,
                action,
                ended,
            } => match result {
                UndercoverEffectResult::FriendBatch(Ok(FriendBatchOutcome::Complete))
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
                UndercoverEffectResult::FriendBatch(Ok(FriendBatchOutcome::Failed {
                    failure,
                    ..
                })) => {
                    if ended {
                        self.finish_session();
                    }
                    bail!("谁是卧底好友消息发送失败：{}", failure.reason())
                }
                UndercoverEffectResult::FriendBatch(Err(error))
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
            UndercoverEffect::FriendBatch {
                deliveries: vec![FriendMessage::new(player.trim(), message)],
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

    fn begin_retry_delivery(&mut self, key: UndercoverEffectKey) -> UndercoverCommandStart {
        let Some(retry) = self.pending_retry.take() else {
            return self.begin_deliveries(
                key,
                UndercoverEffectLane::Formal,
                vec![UndercoverDelivery::Hall(
                    "当前没有待重试的谁是卧底私聊投递".to_string(),
                )],
                "retry-unavailable",
                false,
            );
        };
        if retry.session_generation != self.generation {
            return self.begin_deliveries(
                key,
                UndercoverEffectLane::Formal,
                vec![UndercoverDelivery::Hall("待重试投递已失效".to_string())],
                "retry-expired",
                false,
            );
        }
        if retry.deliveries.is_empty() {
            let reason = retry.failure.reason().to_string();
            self.pending_retry = Some(retry);
            return self.begin_deliveries(
                key,
                UndercoverEffectLane::Formal,
                vec![UndercoverDelivery::Hall(format!(
                    "谁是卧底私聊投递结果未知，不能重试，请#结束：{reason}"
                ))],
                "retry-unsafe",
                false,
            );
        }
        let effect = UndercoverEffect::FriendBatch {
            deliveries: retry.deliveries.clone(),
        };
        UndercoverCommandStart::Suspended(self.insert_effect(
            key,
            UndercoverEffectLane::Formal,
            effect,
            UndercoverContinuation::DeliverSecrets { now: retry.now },
        ))
    }

    fn pause_secret_delivery(
        &mut self,
        key: UndercoverEffectKey,
        retryable: Vec<FriendMessage>,
        now: Instant,
        failure: FriendBatchFailure,
    ) -> UndercoverResume {
        log::error!(
            "谁是卧底私聊投递暂停: retryable={} kind={:?} reason={}",
            retryable.len(),
            failure.kind(),
            failure.reason()
        );
        let message = if retryable.is_empty() {
            match failure.kind() {
                FriendBatchFailureKind::ResultUnknown => {
                    "谁是卧底私聊投递结果未知，不能重试，请#结束"
                }
                FriendBatchFailureKind::ConfirmedUnsent => {
                    "谁是卧底私聊投递失败且没有安全剩余项，请#结束"
                }
            }
        } else {
            "谁是卧底私聊投递未完成，请#重试或#结束"
        };
        self.pending_retry = Some(PendingUndercoverRetry {
            session_generation: key.session_generation,
            deliveries: retryable,
            now,
            failure,
        });
        self.resume_with_deliveries(
            key,
            vec![UndercoverDelivery::Hall(message.to_string())],
            UndercoverEffectLane::Formal,
            "secret-delivery-paused",
            false,
        )
    }

    fn finish_session(&mut self) {
        self.pending_retry = None;
        self.generation = self.generation.checked_next().unwrap_or(self.generation);
    }

    fn reconcile_entertainment(&self, entertainment: &mut EntertainmentState) {
        if entertainment.active() != Some(EntertainmentKind::Undercover) {
            return;
        }
        let pending_session = self.pending.values().any(|pending| {
            matches!(
                pending.continuation,
                UndercoverContinuation::VerifyCreate { .. }
                    | UndercoverContinuation::DeliverSecrets { .. }
                    | UndercoverContinuation::Deliveries { ended: true, .. }
            )
        });
        if !self.game.is_active() && !pending_session && self.pending_retry.is_none() {
            entertainment.release(EntertainmentKind::Undercover);
        }
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
        UndercoverDelivery::Friend { player, message } => UndercoverEffect::FriendBatch {
            deliveries: vec![FriendMessage::new(player, message)],
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
            UndercoverEffect::FriendBatch { .. },
            UndercoverEffectResult::FriendBatch(_)
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
    use crate::features::entertainment::EntertainmentState;
    use crate::runtime::identity::BusinessOperationId;

    fn service(timeout: u64) -> (UndercoverRuntimeService, EntertainmentState) {
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
        let entertainment = EntertainmentState::new();
        let service = UndercoverRuntimeService::new(UndercoverConfig {
            enabled: true,
            word_bank_path: bank_path,
            used_state_path: directory.join("used.yaml"),
            lobby_timeout_seconds: timeout,
            ..UndercoverConfig::default()
        });
        (service, entertainment)
    }

    fn complete_membership(
        service: &mut UndercoverRuntimeService,
        entertainment: &mut EntertainmentState,
        player: &str,
        command: UndercoverCommand,
        now: Instant,
        operation_id: u64,
    ) {
        let start = service
            .begin_command(
                entertainment,
                player,
                UndercoverCommandSource::Friend,
                &command,
                now,
                BusinessOperationId::new(operation_id),
            )
            .expect("begin membership");
        let UndercoverCommandStart::Suspended(verification) = start else {
            panic!("membership should wait for friend verification");
        };
        service.claim(verification.key).expect("claim verification");
        let hall = match service
            .resume(
                entertainment,
                verification.key,
                UndercoverEffectResult::FriendVerify(Ok(true)),
            )
            .expect("resume verification")
        {
            UndercoverResume::Suspended(request) => request,
            other => panic!("membership should publish lobby status, got {other:?}"),
        };
        service.claim(hall.key).expect("claim lobby status");
        assert!(matches!(
            service
                .resume(
                    entertainment,
                    hall.key,
                    UndercoverEffectResult::Hall(Ok(())),
                )
                .expect("resume lobby status"),
            UndercoverResume::Completed(_)
        ));
    }

    #[test]
    fn starting_a_game_delivers_every_secret_in_one_ordered_friend_batch() {
        let (mut service, mut entertainment) = service(60);
        let now = Instant::now();
        complete_membership(
            &mut service,
            &mut entertainment,
            "甲",
            UndercoverCommand::CreateSingle,
            now,
            1,
        );
        for (operation_id, player) in [(2, "乙"), (3, "丙"), (4, "丁")] {
            complete_membership(
                &mut service,
                &mut entertainment,
                player,
                UndercoverCommand::Join,
                now,
                operation_id,
            );
        }

        let start = service
            .begin_command(
                &mut entertainment,
                "甲",
                UndercoverCommandSource::Hall,
                &UndercoverCommand::Start,
                now,
                BusinessOperationId::new(5),
            )
            .expect("begin game");
        let UndercoverCommandStart::Suspended(request) = start else {
            panic!("start should wait for secret delivery");
        };
        let UndercoverEffect::FriendBatch { deliveries } = request.effect else {
            panic!("all secrets should share one friend batch");
        };

        let mut recipients = deliveries
            .iter()
            .map(|delivery| delivery.recipient())
            .collect::<Vec<_>>();
        recipients.sort_unstable();
        assert_eq!(recipients, ["丁", "丙", "乙", "甲"]);
        for (index, delivery) in deliveries.iter().enumerate() {
            let position = (b'A' + index as u8) as char;
            assert!(
                delivery
                    .message()
                    .contains(&format!("你的位置：{position}"))
            );
        }
    }

    #[test]
    fn failed_secret_batch_pauses_the_game_and_retry_sends_only_safe_remainder() {
        use crate::features::friend_delivery::{FriendBatchFailure, FriendBatchFailureKind};

        let (mut service, mut entertainment) = service(60);
        let now = Instant::now();
        complete_membership(
            &mut service,
            &mut entertainment,
            "甲",
            UndercoverCommand::CreateSingle,
            now,
            1,
        );
        for (operation_id, player) in [(2, "乙"), (3, "丙"), (4, "丁")] {
            complete_membership(
                &mut service,
                &mut entertainment,
                player,
                UndercoverCommand::Join,
                now,
                operation_id,
            );
        }
        let start = service
            .begin_command(
                &mut entertainment,
                "甲",
                UndercoverCommandSource::Hall,
                &UndercoverCommand::Start,
                now,
                BusinessOperationId::new(5),
            )
            .expect("begin game");
        let UndercoverCommandStart::Suspended(batch) = start else {
            panic!("start should wait for secret delivery");
        };
        let UndercoverEffect::FriendBatch { deliveries } = &batch.effect else {
            panic!("secrets should use one friend batch");
        };
        let safe_remainder = deliveries.last().expect("four deliveries").clone();
        service.claim(batch.key).expect("claim secret batch");
        let notice = match service
            .resume(
                &mut entertainment,
                batch.key,
                UndercoverEffectResult::FriendBatch(Ok(FriendBatchOutcome::Failed {
                    retryable: vec![safe_remainder.clone()],
                    failure: FriendBatchFailure::new(
                        FriendBatchFailureKind::ConfirmedUnsent,
                        "friend row was not found",
                    ),
                })),
            )
            .expect("pause failed batch")
        {
            UndercoverResume::Suspended(request) => request,
            other => panic!("failed batch should publish a retry notice, got {other:?}"),
        };
        assert!(matches!(
            &notice.effect,
            UndercoverEffect::Hall { message }
                if message.contains("#重试") && message.contains("#结束")
        ));
        service.claim(notice.key).expect("claim retry notice");
        assert!(matches!(
            service
                .resume(
                    &mut entertainment,
                    notice.key,
                    UndercoverEffectResult::Hall(Ok(())),
                )
                .expect("resume retry notice"),
            UndercoverResume::Completed(_)
        ));

        let retry = service
            .begin_command(
                &mut entertainment,
                "甲",
                UndercoverCommandSource::Hall,
                &UndercoverCommand::Retry,
                now,
                BusinessOperationId::new(6),
            )
            .expect("retry safe remainder");
        let UndercoverCommandStart::Suspended(retry) = retry else {
            panic!("retry should submit the safe remainder");
        };
        assert_eq!(
            retry.effect,
            UndercoverEffect::FriendBatch {
                deliveries: vec![safe_remainder]
            }
        );
    }

    #[test]
    fn friend_error_replies_use_the_shared_friend_batch_contract() {
        let (mut service, mut entertainment) = service(60);
        let start = service
            .begin_command(
                &mut entertainment,
                "甲",
                UndercoverCommandSource::Friend,
                &UndercoverCommand::Join,
                Instant::now(),
                BusinessOperationId::new(1),
            )
            .expect("begin rejected join");
        let UndercoverCommandStart::Suspended(request) = start else {
            panic!("friend error reply should suspend for delivery");
        };

        assert!(matches!(
            request.effect,
            UndercoverEffect::FriendBatch { ref deliveries }
                if deliveries.len() == 1
                    && deliveries[0].recipient() == "甲"
                    && deliveries[0].message().contains("当前没有谁是卧底报名房间")
        ));
    }

    #[test]
    fn runtime_effect_chain_keeps_friend_verification_outside_business_state() {
        let (mut service, mut entertainment) = service(60);
        let now = Instant::now();
        let start = service
            .begin_command(
                &mut entertainment,
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
                &mut entertainment,
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
            .resume(
                &mut entertainment,
                hall.key,
                UndercoverEffectResult::Hall(Ok(())),
            )
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
        let (mut service, mut entertainment) = service(1);
        let now = Instant::now();
        let start = service
            .begin_command(
                &mut entertainment,
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
                &mut entertainment,
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
            .resume(
                &mut entertainment,
                hall.key,
                UndercoverEffectResult::Hall(Ok(())),
            )
            .expect("resume status");

        let (kind, deadline) = service.next_deadline(now, true).expect("lobby deadline");
        assert_eq!(kind, super::super::UndercoverDeadlineKind::LobbyIdle);
        assert!(deadline <= now + Duration::from_secs(1));
        let outcome = service
            .handle_deadline(
                &mut entertainment,
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
        let (mut service, mut entertainment) = service(1);
        let now = Instant::now();
        let start = service
            .begin_command(
                &mut entertainment,
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
                &mut entertainment,
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
            .resume(
                &mut entertainment,
                hall.key,
                UndercoverEffectResult::Hall(Ok(())),
            )
            .expect("resume status");

        let (kind, _) = service.next_deadline(now, true).expect("lobby deadline");
        let outcome = service
            .handle_deadline(
                &mut entertainment,
                kind,
                now + Duration::from_secs(2),
                BusinessOperationId::new(2),
            )
            .expect("handle timeout")
            .expect("timeout delivery");
        let request = outcome.into_request();
        service
            .cancel(&mut entertainment, request.key)
            .expect("cancel timed delivery");

        assert_eq!(entertainment.active(), None);
    }
}
