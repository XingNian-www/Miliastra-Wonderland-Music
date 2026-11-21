//! 测试替身与共用驱动。
//!
//! 投递端口用假实现记录「真的发了什么」，效果链驱动把挂起请求一路走完，
//! 这样状态机测试不需要真实好友通道或大厅。

use std::sync::Mutex as StdMutex;

use super::*;

pub(super) struct TestCardGameService {
    pub(super) inner: CardGameService,
    pub(super) entertainment: EntertainmentState,
}
impl TestCardGameService {
    pub(super) fn new(config: LandlordConfig) -> Self {
        Self {
            inner: CardGameService::new(config),
            entertainment: EntertainmentState::new(),
        }
    }

    pub(super) fn begin_command(
        &mut self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<CardGameCommandStart> {
        self.inner
            .begin_command(&mut self.entertainment, player, command, now)
    }

    pub(super) fn begin_command_with_display(
        &mut self,
        player: &str,
        player_display: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<CardGameCommandStart> {
        self.inner.begin_command_with_display(
            &mut self.entertainment,
            player,
            player_display,
            command,
            now,
        )
    }

    pub(super) fn resume(
        &mut self,
        key: CardGameEffectKey,
        result: CardGameEffectResult,
    ) -> Result<CardGameResume> {
        self.inner.resume(&mut self.entertainment, key, result)
    }

    pub(super) fn cancel(&mut self, key: CardGameEffectKey) -> Result<CardGameCancel> {
        self.inner.cancel(&mut self.entertainment, key)
    }

    pub(super) fn abort(&mut self) -> Result<bool> {
        self.inner.abort(&mut self.entertainment)
    }

    pub(super) fn prepare_start(&mut self, command: &LandlordCommand) -> Result<CardGameStartGate> {
        self.inner.prepare_start(&mut self.entertainment, command)
    }

    pub(super) fn cancel_start(&mut self, reservation: CardGameStartReservation) -> Result<bool> {
        self.inner
            .cancel_start(&mut self.entertainment, reservation)
    }

    pub(super) fn complete_start(
        &mut self,
        player: &str,
        command: &LandlordCommand,
        reservation: CardGameStartReservation,
        now: Instant,
    ) -> Result<LandlordOutcome> {
        self.inner
            .complete_start(&mut self.entertainment, player, command, reservation, now)
    }

    pub(super) fn handle(
        &mut self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> Result<LandlordOutcome> {
        self.inner
            .handle(&mut self.entertainment, player, command, now)
    }

    pub(super) fn tick(
        &mut self,
        now: Instant,
        clock_active: bool,
    ) -> Result<Option<CardGameTimedOutcome>> {
        self.inner.tick(&mut self.entertainment, now, clock_active)
    }

    pub(super) fn begin_delivery_outcome(
        &mut self,
        outcome: LandlordOutcome,
    ) -> Result<CardGameCommandStart> {
        self.inner
            .begin_delivery_outcome(&mut self.entertainment, outcome)
    }
}
impl std::ops::Deref for TestCardGameService {
    type Target = CardGameService;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for TestCardGameService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
pub(super) struct FakeCardGamePort {
    pub(super) verified: bool,
    pub(super) friend_sent: bool,
    pub(super) hall_messages: StdMutex<Vec<String>>,
    pub(super) private_templates: StdMutex<Vec<(String, String, String)>>,
}
impl FakeCardGamePort {
    pub(super) fn new(verified: bool, friend_sent: bool) -> Self {
        Self {
            verified,
            friend_sent,
            hall_messages: StdMutex::new(Vec::new()),
            private_templates: StdMutex::new(Vec::new()),
        }
    }

    pub(super) fn hall_messages(&self) -> Vec<String> {
        self.hall_messages.lock().unwrap().clone()
    }
}
impl CardGameDeliveryPort for FakeCardGamePort {
    fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Ok(self.verified)
    }

    fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        Ok(self.friend_sent)
    }

    fn send_friend_template(
        &self,
        player: &str,
        message: &str,
        template: &CardGameReplyTemplate,
    ) -> Result<bool> {
        let CardGameReplyTemplate::Template { spec, .. } = template else {
            panic!("hand delivery should carry template metadata");
        };
        self.private_templates.lock().unwrap().push((
            player.to_string(),
            message.to_string(),
            spec.key.to_string(),
        ));
        Ok(self.friend_sent)
    }

    fn send_friend_batch(
        &self,
        deliveries: &[LandlordPrivateDelivery],
    ) -> Result<FriendBatchOutcome> {
        if self.friend_sent {
            Ok(FriendBatchOutcome::Complete)
        } else {
            Ok(FriendBatchOutcome::Failed {
                retryable: deliveries
                    .iter()
                    .map(|delivery| FriendMessage::new(&delivery.player, &delivery.message))
                    .collect(),
                failure: FriendBatchFailure::new(
                    FriendBatchFailureKind::ConfirmedUnsent,
                    "test friend batch failure",
                ),
            })
        }
    }

    fn send_hall(&self, message: &str) -> Result<()> {
        self.hall_messages.lock().unwrap().push(message.to_string());
        Ok(())
    }
}
pub(super) struct NeverCalledCardGamePort;
impl CardGameDeliveryPort for NeverCalledCardGamePort {
    fn verify_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        panic!("cancelled effect must not be executed")
    }

    fn send_friend(&self, _player: &str, _message: &str) -> Result<bool> {
        panic!("cancelled effect must not be executed")
    }

    fn send_friend_batch(
        &self,
        _deliveries: &[LandlordPrivateDelivery],
    ) -> Result<FriendBatchOutcome> {
        panic!("cancelled effect must not be executed")
    }

    fn send_hall(&self, _message: &str) -> Result<()> {
        panic!("cancelled effect must not be executed")
    }
}
pub(super) fn suspended(start: CardGameCommandStart) -> CardGameEffectRequest {
    match start {
        CardGameCommandStart::Suspended(request) => request,
        CardGameCommandStart::Completed(CardGameCompletion { action }) => {
            panic!("expected a suspended effect, completed as {action}")
        }
    }
}
pub(super) trait CardGameServiceTestDriver {
    fn resume_claimed_for_test(
        &mut self,
        key: CardGameEffectKey,
        result: CardGameEffectResult,
    ) -> Result<CardGameResume>;
    fn execute(
        &mut self,
        player: &str,
        command: &LandlordCommand,
        port: &dyn CardGameDeliveryPort,
        now: Instant,
    ) -> Result<()>;
    fn drive_formal_for_test(
        &mut self,
        progress: CardGameCommandStart,
        port: &dyn CardGameDeliveryPort,
    ) -> Result<()>;
    fn drive_timed_for_test(
        &mut self,
        outcome: CardGameTimedOutcome,
        port: &dyn CardGameDeliveryPort,
    ) -> Result<()>;
}
impl CardGameServiceTestDriver for TestCardGameService {
    fn resume_claimed_for_test(
        &mut self,
        key: CardGameEffectKey,
        result: CardGameEffectResult,
    ) -> Result<CardGameResume> {
        match self.claim(key)? {
            CardGameEffectClaim::Claimed => self.resume(key, result),
            CardGameEffectClaim::Late(late) => Ok(CardGameResume::Late(late)),
        }
    }

    fn execute(
        &mut self,
        player: &str,
        command: &LandlordCommand,
        port: &dyn CardGameDeliveryPort,
        now: Instant,
    ) -> Result<()> {
        let start = self.begin_command(player, command, now)?;
        self.drive_formal_for_test(start, port)
    }

    fn drive_formal_for_test(
        &mut self,
        progress: CardGameCommandStart,
        port: &dyn CardGameDeliveryPort,
    ) -> Result<()> {
        drive_effects_for_test(self, progress, port, true)
    }

    fn drive_timed_for_test(
        &mut self,
        outcome: CardGameTimedOutcome,
        port: &dyn CardGameDeliveryPort,
    ) -> Result<()> {
        drive_effects_for_test(
            self,
            CardGameCommandStart::Suspended(outcome.into_request()),
            port,
            false,
        )
    }
}
pub(super) fn drive_effects_for_test(
    service: &mut TestCardGameService,
    mut progress: CardGameCommandStart,
    port: &dyn CardGameDeliveryPort,
    late_is_error: bool,
) -> Result<()> {
    loop {
        let request = match progress {
            CardGameCommandStart::Completed(_) => return Ok(()),
            CardGameCommandStart::Suspended(request) => request,
        };
        let key = request.key;
        if matches!(service.claim(key)?, CardGameEffectClaim::Late(_)) {
            if late_is_error {
                anyhow::bail!("card game command was cancelled before its effect chain completed");
            }
            return Ok(());
        }
        let result = match request.effect {
            CardGameEffect::FriendVerify {
                player_display,
                message,
            } => CardGameEffectResult::FriendVerify(port.verify_friend(&player_display, &message)),
            CardGameEffect::PrivateDelivery {
                player_display,
                message,
                template,
            } => match template {
                Some(template) => CardGameEffectResult::PrivateDeliveryTemplate(
                    port.send_friend_template(&player_display, &message, &template),
                ),
                None => CardGameEffectResult::PrivateDelivery(
                    port.send_friend(&player_display, &message),
                ),
            },
            CardGameEffect::PrivateBatch { deliveries } => {
                CardGameEffectResult::PrivateBatchTemplates(
                    port.send_friend_batch_templates(&deliveries),
                )
            }
            CardGameEffect::HallDelivery { message, .. } => {
                CardGameEffectResult::HallDelivery(port.send_hall(&message))
            }
        };
        progress = match service.resume(key, result)? {
            CardGameResume::Completed(_) => return Ok(()),
            CardGameResume::Late(_) => {
                if late_is_error {
                    anyhow::bail!(
                        "card game command was cancelled before its effect chain completed"
                    );
                }
                return Ok(());
            }
            CardGameResume::Suspended(request) => CardGameCommandStart::Suspended(request),
        };
    }
}
pub(super) fn drain_card_game_effects(
    service: &mut TestCardGameService,
    mut request: CardGameEffectRequest,
) {
    loop {
        let result = match &request.effect {
            CardGameEffect::FriendVerify { .. } => CardGameEffectResult::FriendVerify(Ok(true)),
            CardGameEffect::PrivateDelivery {
                template: Some(_), ..
            } => CardGameEffectResult::PrivateDeliveryTemplate(Ok(true)),
            CardGameEffect::PrivateDelivery { .. } => {
                CardGameEffectResult::PrivateDelivery(Ok(true))
            }
            CardGameEffect::PrivateBatch { .. } => CardGameEffectResult::PrivateBatchTemplates(Ok(
                CardGameFriendBatchOutcome::Complete,
            )),
            CardGameEffect::HallDelivery { .. } => CardGameEffectResult::HallDelivery(Ok(())),
        };
        match service
            .resume_claimed_for_test(request.key, result)
            .unwrap()
        {
            CardGameResume::Suspended(next) => request = next,
            CardGameResume::Completed(_) | CardGameResume::Late(_) => return,
        }
    }
}

/// 回归：服务层交给好友通道的收件人必须是展示名。
/// 曾经把内部键（UUID）塞进 FriendVerify / PrivateDelivery，
/// 结果「报名」和「发手牌」都拿 UUID 去好友列表里找人，必然找不到。
pub(super) fn ranks(text: &str) -> Vec<Rank> {
    parse_cards(text).unwrap()
}

/// 走完整套传统抢地主：叫地主 → 两家抢 → 原叫地主者反抢。
pub(super) fn finish_bidding_with_all_players_robbing(game: &mut LandlordGame, now: Instant) {
    fn current_name(game: &LandlordGame) -> String {
        let GameState::Bidding(bidding) = &game.state else {
            panic!("expected bidding")
        };
        bidding.players[bidding.current].name.clone()
    }
    let caller = current_name(game);
    game.handle(&caller, &LandlordCommand::Call, now);
    for _ in 0..2 {
        let player = current_name(game);
        game.handle(&player, &LandlordCommand::Rob, now);
    }
    let caller = current_name(game);
    game.handle(&caller, &LandlordCommand::Rob, now);
    assert!(matches!(game.state, GameState::Playing(_)));
}
pub(super) fn current_bidder(game: &LandlordGame) -> String {
    let GameState::Bidding(bidding) = &game.state else {
        panic!("expected bidding")
    };
    bidding.players[bidding.current].name.clone()
}
