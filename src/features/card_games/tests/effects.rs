//! 效果链：挂起/认领/恢复/取消/重试与投递端口行为。

use super::support::*;
use super::*;

#[test]
fn effect_state_machine_releases_a_start_after_friend_verification_is_rejected() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let now = Instant::now();

    let CardGameCommandStart::Suspended(verification) = service
        .begin_command("甲", &LandlordCommand::Start, now)
        .unwrap()
    else {
        panic!("start should wait for friend verification")
    };
    assert_eq!(verification.lane, CardGameEffectLane::Formal);
    assert!(matches!(
        verification.effect,
        CardGameEffect::FriendVerify { ref player_display, .. } if player_display == "甲"
    ));

    let CardGameResume::Suspended(failure_reply) = service
        .resume_claimed_for_test(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(false)),
        )
        .unwrap()
    else {
        panic!("rejected verification should announce the failure")
    };
    assert_ne!(
        verification.key.operation_id,
        failure_reply.key.operation_id
    );
    assert_eq!(
        verification.key.session_generation,
        failure_reply.key.session_generation
    );
    assert!(matches!(
        failure_reply.effect,
        CardGameEffect::HallDelivery { ref message, .. } if message.contains("报名失败")
    ));

    assert!(matches!(
        service
            .resume_claimed_for_test(
                failure_reply.key,
                CardGameEffectResult::HallDelivery(Ok(())),
            )
            .unwrap(),
        CardGameResume::Completed(_)
    ));
    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());
}
#[test]
fn effect_state_machine_cancels_a_suspended_start_and_ignores_its_late_result() {
    let mut service = TestCardGameService::new(LandlordConfig::default());

    let CardGameCommandStart::Suspended(verification) = service
        .begin_command("甲", &LandlordCommand::Start, Instant::now())
        .unwrap()
    else {
        panic!("start should wait for friend verification")
    };

    assert!(matches!(
        service.cancel(verification.key).unwrap(),
        CardGameCancel::Cancelled(_)
    ));
    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());
    assert!(matches!(
        service
            .resume_claimed_for_test(
                verification.key,
                CardGameEffectResult::FriendVerify(Ok(true)),
            )
            .unwrap(),
        CardGameResume::Late(_)
    ));
    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());
}
#[test]
fn effect_state_machine_rejects_a_result_before_the_effect_is_claimed() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let verification = suspended(
        service
            .begin_command("甲", &LandlordCommand::Start, Instant::now())
            .unwrap(),
    );

    let error = service
        .resume(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(true)),
        )
        .unwrap_err();

    assert!(error.to_string().contains("must be claimed"));
    assert!(matches!(
        service.cancel(verification.key).unwrap(),
        CardGameCancel::Cancelled(_)
    ));
}
#[test]
fn formal_effect_driver_rejects_a_cancelled_queued_effect() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let request = suspended(
        service
            .begin_command("甲", &LandlordCommand::Start, Instant::now())
            .unwrap(),
    );
    assert!(matches!(
        service.cancel(request.key).unwrap(),
        CardGameCancel::Cancelled(_)
    ));

    let error = service
        .drive_formal_for_test(
            CardGameCommandStart::Suspended(request),
            &NeverCalledCardGamePort,
        )
        .unwrap_err();

    assert!(error.to_string().contains("cancelled"));
}
#[test]
fn queued_cancel_does_not_cancel_a_claimed_effect() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let now = Instant::now();
    let verification = suspended(
        service
            .begin_command("甲", &LandlordCommand::Start, now)
            .unwrap(),
    );
    assert_eq!(
        service.claim(verification.key).unwrap(),
        CardGameEffectClaim::Claimed
    );

    assert!(matches!(
        service.cancel(verification.key).unwrap(),
        CardGameCancel::Late(_)
    ));
    assert_eq!(service.pending_effect_count_for_test(), 1);
    let hall = match service
        .resume(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(true)),
        )
        .unwrap()
    {
        CardGameResume::Suspended(request) => request,
        other => panic!("verified start should continue: {other:?}"),
    };
    service
        .resume_claimed_for_test(hall.key, CardGameEffectResult::HallDelivery(Ok(())))
        .unwrap();
    assert!(service.is_active().unwrap());
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
}

/// 回归：已登记身份的 UUID 不得进玩家可见文案。
///
/// `player` 是身份键（UUID），`player_display` 是展示名。若两者混用，
/// 「{玩家}创建了牌局」里会出现一长串十六进制。
#[test]
fn service_effects_use_display_name_never_the_identity_key() {
    let uuid = "00000000-0000-4000-8000-000000000001";
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let now = Instant::now();

    let verification = suspended(
        service
            .begin_command_with_display(uuid, "显示名甲", &LandlordCommand::Start, now)
            .unwrap(),
    );
    match &verification.effect {
        CardGameEffect::FriendVerify { player_display, .. } => {
            assert_eq!(player_display, "显示名甲");
            assert!(!player_display.contains(uuid), "好友验证收件人不得是内部键");
        }
        other => panic!("期望好友验证效果，实际 {other:?}"),
    }
    drain_card_game_effects(&mut service, verification);

    for name in ["玩家乙", "玩家丙"] {
        let joined = suspended(
            service
                .begin_command(name, &LandlordCommand::Join, now)
                .unwrap(),
        );
        drain_card_game_effects(&mut service, joined);
    }

    let hand = suspended(
        service
            .begin_command_with_display(uuid, "显示名甲", &LandlordCommand::Hand, now)
            .unwrap(),
    );
    match &hand.effect {
        CardGameEffect::PrivateDelivery { player_display, .. } => {
            assert_eq!(player_display, "显示名甲");
            assert!(!player_display.contains(uuid), "私聊投递收件人不得是内部键");
        }
        other => panic!("期望私聊投递效果，实际 {other:?}"),
    }
}

/// 同一人的 `key` 与 `name` 分离后，加入牌局仍能按身份键去重。
#[test]
fn aborting_while_a_formal_effect_is_in_flight_fails_its_command_chain() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let now = Instant::now();
    let verification = suspended(
        service
            .begin_command("甲", &LandlordCommand::Start, now)
            .unwrap(),
    );
    assert_eq!(
        service.claim(verification.key).unwrap(),
        CardGameEffectClaim::Claimed
    );

    let aborted = service.abort();

    assert!(aborted.unwrap());
    assert!(matches!(
        service
            .resume(
                verification.key,
                CardGameEffectResult::FriendVerify(Ok(true)),
            )
            .unwrap(),
        CardGameResume::Late(_)
    ));
    assert_eq!(service.pending_effect_count_for_test(), 0);
    assert!(!service.is_active().unwrap());
    assert_eq!(service.entertainment.active(), None);
}
#[test]
fn effect_state_machine_releases_a_start_when_friend_verification_errors() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let CardGameCommandStart::Suspended(verification) = service
        .begin_command("甲", &LandlordCommand::Start, Instant::now())
        .unwrap()
    else {
        panic!("start should wait for friend verification")
    };

    let error = service
        .resume_claimed_for_test(
            verification.key,
            CardGameEffectResult::FriendVerify(Err(anyhow::anyhow!("verification failed"))),
        )
        .unwrap_err();

    assert!(error.to_string().contains("verification failed"));
    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());
}
#[test]
fn effect_state_machine_does_not_roll_back_a_started_room_when_hall_delivery_fails() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let CardGameCommandStart::Suspended(verification) = service
        .begin_command("甲", &LandlordCommand::Start, Instant::now())
        .unwrap()
    else {
        panic!("start should wait for friend verification")
    };
    let CardGameResume::Suspended(hall_delivery) = service
        .resume_claimed_for_test(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(true)),
        )
        .unwrap()
    else {
        panic!("verified start should announce the room")
    };

    let error = service
        .resume_claimed_for_test(
            hall_delivery.key,
            CardGameEffectResult::HallDelivery(Err(anyhow::anyhow!("hall unavailable"))),
        )
        .unwrap_err();

    assert!(error.to_string().contains("hall unavailable"));
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());
}
#[test]
fn failed_initial_hand_batch_pauses_and_retries_only_safe_remaining_hands() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &port, now)
        .unwrap();

    let CardGameCommandStart::Suspended(verification) = service
        .begin_command("丙", &LandlordCommand::Join, now)
        .unwrap()
    else {
        panic!("third player should wait for friend verification")
    };
    let CardGameResume::Suspended(hand_batch) = service
        .resume_claimed_for_test(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(true)),
        )
        .unwrap()
    else {
        panic!("verified third player should start one hand batch")
    };
    let CardGameEffect::PrivateBatch { deliveries } = &hand_batch.effect else {
        panic!("initial hands must be one private batch")
    };
    assert_eq!(
        deliveries
            .iter()
            .map(|delivery| delivery.player.as_str())
            .collect::<Vec<_>>(),
        ["甲", "乙", "丙"]
    );
    let retryable = deliveries[1..].to_vec();
    let expected_retryable = retryable.clone();
    let CardGameResume::Suspended(failure_notice) = service
        .resume_claimed_for_test(
            hand_batch.key,
            CardGameEffectResult::PrivateBatchTemplates(Ok(CardGameFriendBatchOutcome::Failed {
                retryable,
                failure: FriendBatchFailure::new(
                    FriendBatchFailureKind::ConfirmedUnsent,
                    "second hand was not sent",
                ),
            })),
        )
        .unwrap()
    else {
        panic!("failed hand batch should publish a retry notice")
    };
    assert!(matches!(
        failure_notice.effect,
        CardGameEffect::HallDelivery { ref message, .. } if message.contains("#重试")
    ));
    assert!(matches!(
        service
            .resume_claimed_for_test(
                failure_notice.key,
                CardGameEffectResult::HallDelivery(Ok(())),
            )
            .unwrap(),
        CardGameResume::Completed(_)
    ));
    let retry = suspended(
        service
            .begin_command("甲", &LandlordCommand::Retry, now)
            .unwrap(),
    );
    let CardGameEffect::PrivateBatch { deliveries } = retry.effect else {
        panic!("retry should submit one safe remaining batch")
    };
    assert_eq!(deliveries, expected_retryable);
    assert_eq!(
        deliveries
            .iter()
            .map(|delivery| delivery.player.as_str())
            .collect::<Vec<_>>(),
        ["乙", "丙"]
    );
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());
}
#[test]
fn effect_state_machine_makes_a_failed_hand_delivery_immediately_retryable() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &port, now)
        .unwrap();
    service
        .execute("丙", &LandlordCommand::Join, &port, now)
        .unwrap();

    let CardGameCommandStart::Suspended(first_hand) = service
        .begin_command("甲", &LandlordCommand::Hand, now)
        .unwrap()
    else {
        panic!("first hand request should be delivered privately")
    };
    let error = service
        .resume_claimed_for_test(
            first_hand.key,
            CardGameEffectResult::PrivateDeliveryTemplate(Ok(false)),
        )
        .unwrap_err();
    assert!(error.to_string().contains("牌局手牌发送失败"));
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());

    let CardGameCommandStart::Suspended(retry) = service
        .begin_command("甲", &LandlordCommand::Hand, now)
        .unwrap()
    else {
        panic!("failed hand delivery should clear the cooldown")
    };
    assert_ne!(first_hand.key.operation_id, retry.key.operation_id);
    assert!(matches!(
        retry.effect,
        CardGameEffect::PrivateDelivery { ref player_display, ref template, .. }
            if player_display == "甲" && matches!(template, Some(CardGameReplyTemplate::Template { spec, .. }) if spec.key == "card_games.hand.current")
    ));
}
#[test]
fn hand_delivery_errors_and_cancellation_both_clear_the_cooldown() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &port, now)
        .unwrap();
    service
        .execute("丙", &LandlordCommand::Join, &port, now)
        .unwrap();

    let failed = suspended(
        service
            .begin_command("甲", &LandlordCommand::Hand, now)
            .unwrap(),
    );
    let error = service
        .resume_claimed_for_test(
            failed.key,
            CardGameEffectResult::PrivateDeliveryTemplate(Err(anyhow::anyhow!("send failed"))),
        )
        .unwrap_err();
    assert!(error.to_string().contains("send failed"));

    let cancelled = suspended(
        service
            .begin_command("甲", &LandlordCommand::Hand, now)
            .unwrap(),
    );
    assert!(matches!(
        service.cancel(cancelled.key).unwrap(),
        CardGameCancel::Cancelled(_)
    ));

    let retry = suspended(
        service
            .begin_command("甲", &LandlordCommand::Hand, now)
            .unwrap(),
    );
    assert!(matches!(
        retry.effect,
        CardGameEffect::PrivateDelivery { ref player_display, ref template, .. }
            if player_display == "甲" && matches!(template, Some(CardGameReplyTemplate::Template { spec, .. }) if spec.key == "card_games.hand.current")
    ));
}
#[test]
fn command_transaction_completes_without_registering_an_effect_when_no_reply_is_needed() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &port, now)
        .unwrap();
    service
        .execute("丙", &LandlordCommand::Join, &port, now)
        .unwrap();

    let first = suspended(
        service
            .begin_command("甲", &LandlordCommand::Hand, now)
            .unwrap(),
    );
    assert!(matches!(
        service
            .resume_claimed_for_test(
                first.key,
                CardGameEffectResult::PrivateDeliveryTemplate(Ok(true)),
            )
            .unwrap(),
        CardGameResume::Completed(_)
    ));

    assert!(matches!(
        service
            .begin_command("甲", &LandlordCommand::Hand, now)
            .unwrap(),
        CardGameCommandStart::Completed(CardGameCompletion {
            action: "hand-cooldown"
        })
    ));
}
#[test]
fn effect_state_machine_ignores_wrong_keys_without_consuming_the_pending_effect() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let CardGameCommandStart::Suspended(verification) = service
        .begin_command("甲", &LandlordCommand::Start, Instant::now())
        .unwrap()
    else {
        panic!("start should wait for friend verification")
    };
    let wrong_operation = CardGameEffectKey::new(
        verification.key.operation_id.checked_next().unwrap(),
        verification.key.session_generation,
    );
    let wrong_generation = CardGameEffectKey::new(
        verification.key.operation_id,
        verification.key.session_generation.checked_next().unwrap(),
    );

    for wrong_key in [wrong_operation, wrong_generation] {
        let CardGameResume::Late(CardGameLateResult { key }) = service
            .resume_claimed_for_test(wrong_key, CardGameEffectResult::FriendVerify(Ok(false)))
            .unwrap()
        else {
            panic!("wrong effect key should be late")
        };
        assert_eq!(key, wrong_key);
    }
    assert!(matches!(
        service
            .resume_claimed_for_test(
                verification.key,
                CardGameEffectResult::FriendVerify(Ok(false)),
            )
            .unwrap(),
        CardGameResume::Suspended(_)
    ));
}
#[test]
fn effect_state_machine_does_not_accept_a_repeated_private_batch_result() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &port, now)
        .unwrap();
    let CardGameCommandStart::Suspended(verification) = service
        .begin_command("丙", &LandlordCommand::Join, now)
        .unwrap()
    else {
        panic!("third player should wait for verification")
    };
    let CardGameResume::Suspended(hand_batch) = service
        .resume_claimed_for_test(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(true)),
        )
        .unwrap()
    else {
        panic!("verified join should deliver one hand batch")
    };
    let CardGameResume::Suspended(hall) = service
        .resume_claimed_for_test(
            hand_batch.key,
            CardGameEffectResult::PrivateBatchTemplates(Ok(CardGameFriendBatchOutcome::Complete)),
        )
        .unwrap()
    else {
        panic!("completed hand batch should advance to the hall announcement")
    };

    assert!(matches!(
        service
            .resume_claimed_for_test(
                hand_batch.key,
                CardGameEffectResult::PrivateBatchTemplates(Ok(
                    CardGameFriendBatchOutcome::Complete
                )),
            )
            .unwrap(),
        CardGameResume::Late(_)
    ));
    assert!(matches!(hall.effect, CardGameEffect::HallDelivery { .. }));
}
/// 回归：`accepts`/`continues_after` 同时接受通用 `PrivateBatch` 结果，但
/// `resume` 只实现了模板版。旧实现先把挂起项 `remove` 掉再落进 `_ => Err`，
/// 于是这批私聊投递既不会进 `pending_retry`、也不会释放娱乐锁。
#[test]
fn generic_private_batch_result_still_resumes_the_outcome_delivery() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &port, now)
        .unwrap();
    let verification = suspended(
        service
            .begin_command("丙", &LandlordCommand::Join, now)
            .unwrap(),
    );
    let hand_batch = match service
        .resume_claimed_for_test(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(true)),
        )
        .unwrap()
    {
        CardGameResume::Suspended(request) => request,
        other => panic!("verified join should deliver one hand batch: {other:?}"),
    };

    let hall = match service
        .resume_claimed_for_test(
            hand_batch.key,
            CardGameEffectResult::PrivateBatch(Ok(FriendBatchOutcome::Complete)),
        )
        .unwrap()
    {
        CardGameResume::Suspended(request) => request,
        other => panic!("generic complete batch should advance to the hall: {other:?}"),
    };
    assert!(matches!(hall.effect, CardGameEffect::HallDelivery { .. }));
}

/// 回归：通用 `PrivateBatch` 的失败项是 `FriendMessage`，不能当成卡牌模板重发。
/// 折算后 `retryable` 必须为空，提示玩家 `#结束`，并且娱乐锁继续由本局持有。
#[test]
fn generic_private_batch_failure_ends_instead_of_retrying() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &port, now)
        .unwrap();
    let verification = suspended(
        service
            .begin_command("丙", &LandlordCommand::Join, now)
            .unwrap(),
    );
    let hand_batch = match service
        .resume_claimed_for_test(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(true)),
        )
        .unwrap()
    {
        CardGameResume::Suspended(request) => request,
        other => panic!("verified join should deliver one hand batch: {other:?}"),
    };

    let hall = match service
        .resume_claimed_for_test(
            hand_batch.key,
            CardGameEffectResult::PrivateBatch(Ok(FriendBatchOutcome::Failed {
                retryable: vec![FriendMessage::new("丙", "手牌")],
                failure: FriendBatchFailure::new(
                    FriendBatchFailureKind::ResultUnknown,
                    "test generic batch failure",
                ),
            })),
        )
        .unwrap()
    {
        CardGameResume::Suspended(request) => request,
        other => panic!("generic failed batch should pause delivery: {other:?}"),
    };
    match &hall.effect {
        CardGameEffect::HallDelivery { message, .. } => {
            assert!(message.contains("#结束"), "应提示#结束：{message}");
            assert!(!message.contains("#重试"), "通用结果不能重试：{message}");
        }
        other => panic!("期望大厅公告，实际 {other:?}"),
    }
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
}

/// 回归：`Join` 回退分支（还没开局 / 不在大厅）也必须传展示名。
///
/// 旧实现走 `begin_game_outcome_in_state(entertainment, player, ...)`，把身份键
/// 同时当展示名，`{玩家}加入牌局` 会把 UUID 渲染到公屏。
#[test]
fn join_fallback_uses_display_name_never_the_identity_key() {
    let uuid = "00000000-0000-4000-8000-000000000002";
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let now = Instant::now();
    // 先直接建一个大厅（不经过 service 的开局效果，避免占用娱乐互斥位），
    // 让 Join 走「大厅存在但没有活动牌局」的回退分支。
    service.handle("甲", &LandlordCommand::Start, now).unwrap();
    let announcement = suspended(
        service
            .begin_command_with_display(uuid, "显示名乙", &LandlordCommand::Join, now)
            .unwrap(),
    );
    match &announcement.effect {
        CardGameEffect::HallDelivery { message, .. } => {
            assert!(
                message.contains("显示名乙"),
                "加入公告应使用展示名：{message}"
            );
            assert!(!message.contains(uuid), "加入公告不得泄露内部键：{message}");
        }
        other => panic!("期望大厅公告，实际 {other:?}"),
    }
}

#[test]
fn an_old_start_verification_cannot_cancel_a_new_start_reservation() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let now = Instant::now();
    let CardGameCommandStart::Suspended(old_verification) = service
        .begin_command("甲", &LandlordCommand::Start, now)
        .unwrap()
    else {
        panic!("first start should wait for verification")
    };
    assert!(matches!(
        service.cancel(old_verification.key).unwrap(),
        CardGameCancel::Cancelled(_)
    ));
    let CardGameCommandStart::Suspended(new_verification) = service
        .begin_command("乙", &LandlordCommand::Start, now)
        .unwrap()
    else {
        panic!("replacement start should wait for verification")
    };
    assert_ne!(
        old_verification.key.session_generation,
        new_verification.key.session_generation
    );

    assert!(matches!(
        service
            .resume_claimed_for_test(
                old_verification.key,
                CardGameEffectResult::FriendVerify(Ok(false)),
            )
            .unwrap(),
        CardGameResume::Late(_)
    ));
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());
    assert!(matches!(
        service
            .resume_claimed_for_test(
                new_verification.key,
                CardGameEffectResult::FriendVerify(Ok(true)),
            )
            .unwrap(),
        CardGameResume::Suspended(_)
    ));
}
#[test]
fn aborting_a_session_makes_its_suspended_effect_late() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let CardGameCommandStart::Suspended(verification) = service
        .begin_command("甲", &LandlordCommand::Start, Instant::now())
        .unwrap()
    else {
        panic!("start should wait for verification")
    };

    assert!(service.abort().unwrap());
    assert!(matches!(
        service
            .resume_claimed_for_test(
                verification.key,
                CardGameEffectResult::FriendVerify(Ok(true)),
            )
            .unwrap(),
        CardGameResume::Late(_)
    ));
    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());
}
#[test]
fn ending_a_game_invalidates_old_effects_but_keeps_the_terminal_announcement() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();

    let stale_status = suspended(
        service
            .begin_command("甲", &LandlordCommand::Status, now)
            .unwrap(),
    );
    let terminal = suspended(
        service
            .begin_command("甲", &LandlordCommand::Exit, now)
            .unwrap(),
    );

    assert_ne!(
        stale_status.key.session_generation,
        terminal.key.session_generation
    );
    assert!(matches!(
        service
            .resume_claimed_for_test(stale_status.key, CardGameEffectResult::HallDelivery(Ok(())),)
            .unwrap(),
        CardGameResume::Late(_)
    ));
    let blocked_start = suspended(
        service
            .begin_command("乙", &LandlordCommand::Start, now)
            .unwrap(),
    );
    assert_eq!(
        terminal.key.session_generation,
        blocked_start.key.session_generation
    );
    assert!(matches!(
        service
            .resume_claimed_for_test(terminal.key, CardGameEffectResult::HallDelivery(Ok(())),)
            .unwrap(),
        CardGameResume::Completed(_)
    ));
    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());

    let new_start = suspended(
        service
            .begin_command("乙", &LandlordCommand::Start, now)
            .unwrap(),
    );
    assert_ne!(
        terminal.key.session_generation,
        new_start.key.session_generation
    );
    assert!(matches!(
        service
            .resume_claimed_for_test(
                blocked_start.key,
                CardGameEffectResult::HallDelivery(Ok(())),
            )
            .unwrap(),
        CardGameResume::Late(_)
    ));
}
#[test]
fn redealt_and_landlord_bottom_delivery_failures_pause_the_game() {
    for (commands, expected_action) in [
        (
            vec![
                LandlordCommand::Decline,
                LandlordCommand::Decline,
                LandlordCommand::Decline,
            ],
            "redealt",
        ),
        (
            vec![
                LandlordCommand::Call,
                LandlordCommand::Rob,
                LandlordCommand::Decline,
                LandlordCommand::Decline,
            ],
            "landlord-selected",
        ),
    ] {
        let mut service = TestCardGameService::new(LandlordConfig::default());
        let port = FakeCardGamePort::new(true, true);
        let now = Instant::now();
        service
            .execute("甲", &LandlordCommand::Start, &port, now)
            .unwrap();
        service
            .execute("乙", &LandlordCommand::Join, &port, now)
            .unwrap();
        service
            .execute("丙", &LandlordCommand::Join, &port, now)
            .unwrap();

        let mut final_outcome = None;
        for command in &commands {
            let accepted = ["甲", "乙", "丙"]
                .into_iter()
                .map(|player| service.handle(player, command, now).unwrap())
                .find(|outcome| outcome.action != "wrong-bidder" && outcome.action != "wrong-phase")
                .expect("one player must own the bidding turn");
            final_outcome = Some(accepted);
        }
        let outcome = final_outcome.unwrap();
        assert_eq!(outcome.action, expected_action);
        assert!(!outcome.private_deliveries.is_empty());
        let CardGameCommandStart::Suspended(delivery) =
            service.begin_delivery_outcome(outcome).unwrap()
        else {
            panic!("private outcome should suspend for delivery")
        };

        let CardGameEffect::PrivateBatch { deliveries } = &delivery.effect else {
            panic!("private outcome should use a batch")
        };
        let result = if expected_action == "redealt" {
            Ok(CardGameFriendBatchOutcome::Failed {
                retryable: deliveries.clone(),
                failure: FriendBatchFailure::new(
                    FriendBatchFailureKind::ConfirmedUnsent,
                    "private delivery failed",
                ),
            })
        } else {
            Err(anyhow::anyhow!("private delivery failed"))
        };
        let CardGameResume::Suspended(notice) = service
            .resume_claimed_for_test(
                delivery.key,
                CardGameEffectResult::PrivateBatchTemplates(result),
            )
            .unwrap()
        else {
            panic!("failed private batch should publish a pause notice")
        };

        assert!(matches!(notice.effect, CardGameEffect::HallDelivery { .. }));
        assert_eq!(
            service.entertainment.active(),
            Some(EntertainmentKind::Landlord)
        );
        assert!(service.is_active().unwrap());
    }
}
#[test]
fn effect_requests_preserve_formal_and_deferred_delivery_lanes() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let now = Instant::now();

    let deferred = suspended(
        service
            .begin_command("甲", &LandlordCommand::Status, now)
            .unwrap(),
    );
    assert_eq!(deferred.lane, CardGameEffectLane::Deferred);
    service.cancel(deferred.key).unwrap();

    let formal = suspended(
        service
            .begin_command("甲", &LandlordCommand::Rob, now)
            .unwrap(),
    );
    assert_eq!(formal.lane, CardGameEffectLane::Formal);
    service.cancel(formal.key).unwrap();

    let timed_delivery = suspended(
        service
            .begin_delivery_outcome(LandlordOutcome::public("timer", "计时消息"))
            .unwrap(),
    );
    assert_eq!(timed_delivery.lane, CardGameEffectLane::Formal);
    service.cancel(timed_delivery.key).unwrap();
}
#[test]
fn cancelling_the_final_hall_effect_of_a_hand_deal_keeps_the_game_active() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &port, now)
        .unwrap();
    let verification = suspended(
        service
            .begin_command("丙", &LandlordCommand::Join, now)
            .unwrap(),
    );
    let CardGameResume::Suspended(delivery) = service
        .resume_claimed_for_test(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(true)),
        )
        .unwrap()
    else {
        panic!("verified join should start hand delivery")
    };
    let CardGameResume::Suspended(delivery) = service
        .resume_claimed_for_test(
            delivery.key,
            CardGameEffectResult::PrivateBatchTemplates(Ok(CardGameFriendBatchOutcome::Complete)),
        )
        .unwrap()
    else {
        panic!("the hand batch should be followed by a hall announcement")
    };
    assert!(matches!(
        delivery.effect,
        CardGameEffect::HallDelivery { .. }
    ));

    assert!(matches!(
        service.cancel(delivery.key).unwrap(),
        CardGameCancel::Cancelled(_)
    ));
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());
}
#[test]
fn failing_the_final_hall_effect_of_a_hand_deal_keeps_the_game_active() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(true, true);
    let now = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &port, now)
        .unwrap();
    let verification = suspended(
        service
            .begin_command("丙", &LandlordCommand::Join, now)
            .unwrap(),
    );
    let CardGameResume::Suspended(delivery) = service
        .resume_claimed_for_test(
            verification.key,
            CardGameEffectResult::FriendVerify(Ok(true)),
        )
        .unwrap()
    else {
        panic!("verified join should start hand delivery")
    };
    let CardGameResume::Suspended(delivery) = service
        .resume_claimed_for_test(
            delivery.key,
            CardGameEffectResult::PrivateBatchTemplates(Ok(CardGameFriendBatchOutcome::Complete)),
        )
        .unwrap()
    else {
        panic!("the hand batch should be followed by a hall announcement")
    };

    let error = service
        .resume_claimed_for_test(
            delivery.key,
            CardGameEffectResult::HallDelivery(Err(anyhow::anyhow!("hall unavailable"))),
        )
        .unwrap_err();

    assert!(error.to_string().contains("hall unavailable"));
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());
}
#[test]
fn application_service_releases_start_when_friend_verification_fails() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let port = FakeCardGamePort::new(false, true);

    service
        .execute("甲", &LandlordCommand::Start, &port, Instant::now())
        .unwrap();

    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());
    assert!(
        port.hall_messages()
            .iter()
            .any(|message| message.contains("报名失败"))
    );
}
#[test]
fn application_service_keeps_the_room_while_initial_hand_delivery_waits_for_retry() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    let successful_port = FakeCardGamePort::new(true, true);
    let now = Instant::now();

    service
        .execute("甲", &LandlordCommand::Start, &successful_port, now)
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Join, &successful_port, now)
        .unwrap();
    let failing_port = FakeCardGamePort::new(true, false);
    service
        .execute("丙", &LandlordCommand::Join, &failing_port, now)
        .unwrap();

    assert!(
        failing_port
            .hall_messages()
            .iter()
            .any(|message| message.contains("#重试"))
    );
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());
}
#[test]
fn application_service_releases_a_cancelled_start_reservation() {
    let mut service = TestCardGameService::new(LandlordConfig::default());

    let gate = service
        .prepare_start(&LandlordCommand::Start)
        .expect("start preparation should succeed");
    let CardGameStartGate::Ready { reservation } = gate else {
        panic!("enabled idle game should reserve a start");
    };
    assert_eq!(reservation.kind(), EntertainmentKind::Landlord);

    assert!(service.cancel_start(reservation).unwrap());

    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());
}
#[test]
fn application_service_start_reservations_cannot_cancel_each_other() {
    let mut service = TestCardGameService::new(LandlordConfig::default());

    let CardGameStartGate::Ready { reservation: first } =
        service.prepare_start(&LandlordCommand::Start).unwrap()
    else {
        panic!("first start should reserve the game");
    };
    assert!(matches!(
        service.prepare_start(&LandlordCommand::Start).unwrap(),
        CardGameStartGate::Reply(_)
    ));
    assert!(service.cancel_start(first).unwrap());

    let CardGameStartGate::Ready {
        reservation: second,
    } = service.prepare_start(&LandlordCommand::Start).unwrap()
    else {
        panic!("new start should reserve the released game");
    };
    assert!(!service.cancel_start(first).unwrap());
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());

    assert!(service.cancel_start(second).unwrap());
    assert_eq!(service.entertainment.active(), None);
}
#[test]
fn application_service_does_not_replace_another_entertainment_session() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    assert_eq!(
        service
            .entertainment
            .try_acquire(EntertainmentKind::Undercover)
            .unwrap(),
        AcquireOutcome::Acquired
    );

    let gate = service
        .prepare_start(&LandlordCommand::Start)
        .expect("occupied start preparation should return a reply");

    assert!(matches!(gate, CardGameStartGate::Reply(_)));
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Undercover)
    );
    assert!(!service.is_active().unwrap());
}
#[test]
fn application_service_preserves_executor_command_behavior_during_other_entertainment() {
    let mut service = TestCardGameService::new(LandlordConfig::default());
    service
        .entertainment
        .try_acquire(EntertainmentKind::Undercover)
        .unwrap();
    let now = Instant::now();

    let join = service.handle("甲", &LandlordCommand::Join, now).unwrap();
    let hand = service.handle("甲", &LandlordCommand::Hand, now).unwrap();
    let play = service
        .handle("甲", &LandlordCommand::Play("3".to_string()), now)
        .unwrap();

    assert_eq!(join.action, "no-lobby");
    assert_eq!(hand.action, "no-game");
    assert_eq!(play.action, "occupied");
}
#[test]
fn application_service_releases_a_timeout_when_delivery_begins() {
    let config = LandlordConfig {
        lobby_timeout_seconds: 1,
        ..LandlordConfig::default()
    };
    let mut service = TestCardGameService::new(config);
    let started_at = Instant::now();

    let CardGameStartGate::Ready { reservation } =
        service.prepare_start(&LandlordCommand::Start).unwrap()
    else {
        panic!("start should reserve the game");
    };
    let created = service
        .complete_start("甲", &LandlordCommand::Start, reservation, started_at)
        .unwrap();
    assert_eq!(created.action, "created");
    assert!(!service.cancel_start(reservation).unwrap());
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );

    let timeout = service
        .tick(started_at + Duration::from_secs(2), true)
        .unwrap()
        .expect("lobby should time out");
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );

    service
        .drive_timed_for_test(timeout, &FakeCardGamePort::new(true, true))
        .unwrap();

    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());
}
#[test]
fn application_service_releases_a_timeout_when_delivery_is_cancelled() {
    let config = LandlordConfig {
        lobby_timeout_seconds: 1,
        ..LandlordConfig::default()
    };
    let mut service = TestCardGameService::new(config);
    let started_at = Instant::now();

    let CardGameStartGate::Ready { reservation } =
        service.prepare_start(&LandlordCommand::Start).unwrap()
    else {
        panic!("start should reserve the game");
    };
    service
        .complete_start("甲", &LandlordCommand::Start, reservation, started_at)
        .unwrap();
    let timeout = service
        .tick(started_at + Duration::from_secs(2), true)
        .unwrap()
        .expect("lobby should time out");

    assert!(matches!(
        service.cancel(timeout.key()).unwrap(),
        CardGameCancel::Cancelled(_)
    ));

    assert_eq!(service.entertainment.active(), None);
    assert!(!service.is_active().unwrap());
}
#[test]
fn cancelling_an_old_timed_delivery_does_not_abort_a_new_game() {
    let config = LandlordConfig {
        lobby_timeout_seconds: 1,
        ..LandlordConfig::default()
    };
    let mut service = TestCardGameService::new(config);
    let port = FakeCardGamePort::new(true, true);
    let started_at = Instant::now();

    service
        .execute("甲", &LandlordCommand::Start, &port, started_at)
        .unwrap();
    let timeout = service
        .tick(started_at + Duration::from_secs(2), true)
        .unwrap()
        .expect("lobby should time out");
    let stale_key = timeout.key();
    service
        .drive_timed_for_test(timeout, &FakeCardGamePort::new(true, true))
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Start, &port, started_at)
        .unwrap();

    assert!(matches!(
        service.cancel(stale_key).unwrap(),
        CardGameCancel::Late(_)
    ));
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());
}
#[test]
fn executing_an_old_timed_delivery_does_not_send_into_a_new_game() {
    let config = LandlordConfig {
        lobby_timeout_seconds: 1,
        ..LandlordConfig::default()
    };
    let mut service = TestCardGameService::new(config);
    let setup_port = FakeCardGamePort::new(true, true);
    let started_at = Instant::now();

    service
        .execute("甲", &LandlordCommand::Start, &setup_port, started_at)
        .unwrap();
    let timeout = service
        .tick(started_at + Duration::from_secs(2), true)
        .unwrap()
        .expect("lobby should time out");
    let stale_delivery = timeout.clone();
    service
        .drive_timed_for_test(timeout, &FakeCardGamePort::new(true, true))
        .unwrap();
    service
        .execute("乙", &LandlordCommand::Start, &setup_port, started_at)
        .unwrap();
    let stale_port = FakeCardGamePort::new(true, true);

    service
        .drive_timed_for_test(stale_delivery, &stale_port)
        .unwrap();

    assert!(stale_port.hall_messages().is_empty());
    assert_eq!(
        service.entertainment.active(),
        Some(EntertainmentKind::Landlord)
    );
    assert!(service.is_active().unwrap());
}
#[test]
fn cloned_timed_delivery_only_sends_its_registered_effect_once() {
    let mut service = TestCardGameService::new(LandlordConfig {
        lobby_timeout_seconds: 1,
        ..LandlordConfig::default()
    });
    let setup_port = FakeCardGamePort::new(true, true);
    let started_at = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &setup_port, started_at)
        .unwrap();
    let timeout = service
        .tick(started_at + Duration::from_secs(2), true)
        .unwrap()
        .expect("lobby should time out");
    let first = timeout.clone();
    let duplicate = timeout;
    let delivery_port = FakeCardGamePort::new(true, true);

    service.drive_timed_for_test(first, &delivery_port).unwrap();
    service
        .drive_timed_for_test(duplicate, &delivery_port)
        .unwrap();

    assert_eq!(delivery_port.hall_messages().len(), 1);
}
#[test]
fn timed_delivery_that_becomes_late_in_flight_stays_idempotent() {
    let mut service = TestCardGameService::new(LandlordConfig {
        lobby_timeout_seconds: 1,
        ..LandlordConfig::default()
    });
    let setup_port = FakeCardGamePort::new(true, true);
    let started_at = Instant::now();
    service
        .execute("甲", &LandlordCommand::Start, &setup_port, started_at)
        .unwrap();
    let timeout = service
        .tick(started_at + Duration::from_secs(2), true)
        .unwrap()
        .expect("lobby should time out");
    let duplicate = timeout.clone();
    let request = timeout.into_request();
    assert_eq!(
        service.claim(request.key).unwrap(),
        CardGameEffectClaim::Claimed
    );
    let port = FakeCardGamePort::new(true, true);

    let aborted = service.abort();
    let CardGameEffect::HallDelivery { message, .. } = request.effect else {
        panic!("lobby timeout should be a hall delivery")
    };
    let result = CardGameEffectResult::HallDelivery(port.send_hall(&message));
    let first_result = service.resume(request.key, result).unwrap();
    service.drive_timed_for_test(duplicate, &port).unwrap();

    assert!(aborted.unwrap());
    assert!(matches!(first_result, CardGameResume::Late(_)));
    assert_eq!(port.hall_messages().len(), 1);
    assert_eq!(service.pending_effect_count_for_test(), 0);
    assert!(!service.is_active().unwrap());
    assert_eq!(service.entertainment.active(), None);
}
