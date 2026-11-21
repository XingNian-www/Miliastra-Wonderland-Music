//! 斗地主：叫地主/抢地主、出牌、超时托管、截止时间与大厅退出。

use super::support::*;
use super::*;

#[test]
fn uid_key_never_leaks_into_card_game_copy() {
    let mut game = LandlordGame::new(LandlordConfig::default());
    let uuid = "00000000-0000-4000-8000-000000000001";
    let outcome = game.create_with_display(uuid, "显示名甲", Instant::now());
    let text = outcome
        .public_reply_template
        .as_ref()
        .expect("创建牌局应当返回模板")
        .render_default();
    assert!(!text.contains(uuid), "文案不得含身份 UUID: {text}");
    assert!(text.contains("显示名甲"), "文案应使用展示名: {text}");
}

/// 把一条已挂起的效果链走完（好友验证 / 批量投递 / 大厅回复都给成功结果）。
#[test]
fn identity_key_still_deduplicates_when_display_differs() {
    let mut game = LandlordGame::new(LandlordConfig::default());
    let uuid = "00000000-0000-4000-8000-000000000001";
    game.create_with_display(uuid, "显示名甲", Instant::now());
    // 同一 UUID、展示名换成另一个（例如期间改了备注）：仍应判为重复。
    let outcome = game.join_with_display(uuid, "显示名乙", Instant::now());
    assert_eq!(outcome.action, "duplicate-player");
}
#[test]
fn three_players_receive_hands_then_random_player_starts_bidding() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 7);
    assert_eq!(game.create("甲", now).action, "created");
    assert_eq!(game.join("乙", now).action, "joined");
    let outcome = game.join("丙", now);
    assert_eq!(outcome.action, "bidding-started");
    assert_eq!(outcome.private_deliveries.len(), 3);
    let GameState::Bidding(bidding) = &game.state else {
        panic!("expected bidding")
    };
    assert!(bidding.current < 3);
    assert!(bidding.players.iter().all(|player| player.hand.len() == 17));
    assert_eq!(bidding.bottom.len(), 3);

    finish_bidding_with_all_players_robbing(&mut game, now);
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.current, playing.landlord);
    assert_eq!(playing.players[playing.landlord].hand.len(), 20);
}
#[test]
fn caller_becomes_landlord_when_nobody_robs() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 29);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);

    let caller = current_bidder(&game);
    game.handle(&caller, &LandlordCommand::Call, now);
    for _ in 0..2 {
        let player = current_bidder(&game);
        game.handle(&player, &LandlordCommand::Decline, now);
    }

    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.players[playing.landlord].name, caller);
    assert_eq!(playing.players[playing.landlord].hand.len(), 20);
}
#[test]
fn robber_becomes_landlord_when_caller_declines_counter_rob() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 29);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);

    let caller = current_bidder(&game);
    game.handle(&caller, &LandlordCommand::Call, now);
    let robber = current_bidder(&game);
    game.handle(&robber, &LandlordCommand::Rob, now);
    let other = current_bidder(&game);
    game.handle(&other, &LandlordCommand::Decline, now);
    // 有人抢，原叫地主者获得一次反抢机会。
    assert_eq!(current_bidder(&game), caller);
    game.handle(&caller, &LandlordCommand::Decline, now);

    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.players[playing.landlord].name, robber);
    assert_eq!(playing.current, playing.landlord);
    assert_eq!(playing.players[playing.landlord].hand.len(), 20);
}
#[test]
fn caller_counter_robs_and_becomes_landlord() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 29);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);

    let caller = current_bidder(&game);
    game.handle(&caller, &LandlordCommand::Call, now);
    for _ in 0..2 {
        let player = current_bidder(&game);
        game.handle(&player, &LandlordCommand::Rob, now);
    }
    assert_eq!(current_bidder(&game), caller);
    // 反抢阶段直接用 #抢，不需要单独的反抢命令。
    let outcome = game.handle(&caller, &LandlordCommand::Rob, now);
    assert_eq!(outcome.action, "counter-robbed");

    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.players[playing.landlord].name, caller);
    assert_eq!(playing.players[playing.landlord].hand.len(), 20);
}
#[test]
fn robbing_before_calling_is_rejected() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 29);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);

    let player = current_bidder(&game);
    let outcome = game.handle(&player, &LandlordCommand::Rob, now);
    assert_eq!(outcome.action, "wrong-phase");
    assert!(matches!(game.state, GameState::Bidding(_)));
}
#[test]
fn all_players_declining_redeals_and_sends_new_hands() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 31);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);

    let mut final_outcome = None;
    for _ in 0..3 {
        let GameState::Bidding(bidding) = &game.state else {
            panic!("expected bidding")
        };
        let player = bidding.players[bidding.current].name.clone();
        final_outcome = Some(game.handle(&player, &LandlordCommand::Decline, now));
    }
    let outcome = final_outcome.expect("third bidding outcome");
    assert_eq!(outcome.action, "redealt");
    assert_eq!(outcome.private_deliveries.len(), 3);
    let GameState::Bidding(bidding) = &game.state else {
        panic!("expected bidding")
    };
    assert_eq!(bidding.decisions, 0);
    assert!(bidding.players.iter().all(|player| player.hand.len() == 17));
    assert_eq!(bidding.bottom.len(), 3);
}
#[test]
fn bidding_timeout_uses_the_turn_index_not_the_display_name() {
    let now = Instant::now();
    // 90 秒：warning 在 60 秒，避免 1 秒配置下"重置后立刻又发警告"干扰断言。
    let config = LandlordConfig {
        turn_timeout_seconds: 90,
        ..LandlordConfig::default()
    };
    let mut game = LandlordGame::with_seed(config, 41);
    // 已登记玩家：name 是展示名、key 是身份键，两者不同。
    // 旧实现拿 name 回灌 bid()，find_player 必然落空。
    game.state = GameState::Bidding(Bidding {
        players: vec![
            Player {
                key: player_key("00000000-0000-4000-8000-000000000001"),
                name: "展示甲".to_string(),
                ..Player::new("甲")
            },
            Player {
                key: player_key("00000000-0000-4000-8000-000000000002"),
                name: "展示乙".to_string(),
                ..Player::new("乙")
            },
            Player {
                key: player_key("00000000-0000-4000-8000-000000000003"),
                name: "展示丙".to_string(),
                ..Player::new("丙")
            },
        ],
        bottom: Vec::new(),
        phase: BiddingPhase::Calling,
        current: 0,
        decisions: 0,
        caller: None,
        candidate: None,
        timer: ActiveTimer::new(now),
        warning_sent: false,
    });

    let deadline = now + Duration::from_secs(90);
    let outcome = game
        .tick(deadline, true)
        .expect("bidding timeout must produce an outcome");

    assert_eq!(outcome.action, "call-declined", "超时不能判成 not-player");
    let GameState::Bidding(bidding) = &game.state else {
        panic!("expected bidding")
    };
    assert_eq!(bidding.current, 1);
    assert_eq!(bidding.decisions, 1);
    // 计时器必须已重置，否则每次 tick 都会重复报同一次超时。
    assert!(game.tick(deadline, true).is_none(), "计时器必须已重置");
}
#[test]
fn bidding_timeout_counts_as_decline() {
    let now = Instant::now();
    let config = LandlordConfig {
        turn_timeout_seconds: 1,
        ..LandlordConfig::default()
    };
    let mut game = LandlordGame::with_seed(config, 37);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);
    let GameState::Bidding(bidding) = &game.state else {
        panic!("expected bidding")
    };
    let first = bidding.current;

    let outcome = game
        .tick(now + Duration::from_secs(1), true)
        .expect("bidding timeout");
    assert_eq!(outcome.action, "call-declined");
    let GameState::Bidding(bidding) = &game.state else {
        panic!("expected bidding")
    };
    assert_eq!(bidding.decisions, 1);
    assert_ne!(bidding.current, first);
}
#[test]
fn hand_is_private_and_rate_limited() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 11);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);
    let first = game.hand("甲", now);
    assert!(
        first
            .private_reply
            .as_deref()
            .is_some_and(|reply| reply.contains("当前手牌"))
    );
    assert_eq!(
        game.hand("甲", now + Duration::from_secs(9)).action,
        "hand-cooldown"
    );
    assert_eq!(
        game.hand("甲", now + Duration::from_secs(10)).action,
        "hand"
    );
}
#[test]
fn successful_manual_action_clears_consecutive_timeout_count() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 17);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);
    finish_bidding_with_all_players_robbing(&mut game, now);
    let GameState::Playing(playing) = &mut game.state else {
        panic!("expected playing")
    };
    let current = playing.current;
    playing.players[current].timeouts = 1;
    let name = playing.players[current].name.clone();
    let card = playing.players[current].hand[0].rank.label().to_string();

    assert_eq!(game.play(&name, &card, now).action, "played");
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.players[current].timeouts, 0);
}
#[test]
fn clock_pauses_while_ui_is_busy_and_times_out_when_active() {
    let clock = ManualClock::new(Instant::now());
    let now = clock.now();
    let config = LandlordConfig {
        turn_timeout_seconds: 90,
        ..LandlordConfig::default()
    };
    let mut game = LandlordGame::with_seed(config, 13);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);
    finish_bidding_with_all_players_robbing(&mut game, now);
    clock.advance(Duration::from_secs(100)).unwrap();
    assert!(game.tick(clock.now(), false).is_none());
    clock.advance(Duration::from_secs(89)).unwrap();
    assert!(
        game.tick(clock.now(), true)
            .is_some_and(|item| item.action == "turn-warning")
    );
    clock.advance(Duration::from_secs(1)).unwrap();
    assert!(game.tick(clock.now(), true).is_some());
}
#[test]
fn deadline_projection_uses_active_time_and_stops_while_paused() {
    let clock = ManualClock::new(Instant::now());
    let now = clock.now();
    let config = LandlordConfig {
        lobby_timeout_seconds: 10,
        ..LandlordConfig::default()
    };
    let mut game = LandlordGame::with_seed(config, 19);
    game.create("甲", now);

    clock.advance(Duration::from_secs(3)).unwrap();
    game.sync_clock(clock.now(), true);
    assert_eq!(
        game.next_deadline(clock.now(), true),
        Some((
            CardGameDeadlineKind::LobbyExpiry,
            now + Duration::from_secs(10)
        ))
    );

    clock.advance(Duration::from_secs(97)).unwrap();
    game.sync_clock(clock.now(), false);
    assert_eq!(game.next_deadline(clock.now(), false), None);

    clock.advance(Duration::from_secs(5)).unwrap();
    game.sync_clock(clock.now(), true);
    assert_eq!(
        game.next_deadline(clock.now(), true),
        Some((
            CardGameDeadlineKind::LobbyExpiry,
            now + Duration::from_secs(107)
        ))
    );
}
#[test]
fn first_timeout_passes_instead_of_spending_a_beating_card() {
    let now = Instant::now();
    let config = LandlordConfig {
        turn_timeout_seconds: 1,
        trustee_after_timeouts: 2,
        ..LandlordConfig::default()
    };
    let players = vec![
        Player {
            hand: vec![Card::new(Rank::Three), Card::new(Rank::Five)],
            ..Player::new("甲")
        },
        Player {
            hand: vec![Card::new(Rank::Four), Card::new(Rank::Six)],
            ..Player::new("乙")
        },
        Player {
            hand: vec![Card::new(Rank::Seven)],
            ..Player::new("丙")
        },
    ];
    let mut game = LandlordGame::with_seed(config, 1);
    game.state = GameState::Playing(Playing {
        players,
        variant: CardGameVariant::Landlord,
        landlord: 0,
        current: 0,
        last_play: None,
        consecutive_passes: 0,
        timer: ActiveTimer::new(now),
        warning_sent: false,
        turns: 0,
        bombs: 0,
        opening_spade_three_required: false,
    });
    assert_eq!(game.play("甲", "3", now).action, "played");

    let outcome = game
        .tick(now + Duration::from_secs(1), true)
        .expect("timeout outcome");
    assert_eq!(outcome.action, "auto-pass");
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.players[1].hand.len(), 2);
    assert!(!playing.players[1].trustee);
}
#[test]
fn two_passes_return_the_lead_to_last_player() {
    let now = Instant::now();
    let players = vec![
        Player {
            hand: vec![Card::new(Rank::Three), Card::new(Rank::Four)],
            ..Player::new("甲")
        },
        Player {
            hand: vec![Card::new(Rank::Five)],
            ..Player::new("乙")
        },
        Player {
            hand: vec![Card::new(Rank::Six)],
            ..Player::new("丙")
        },
    ];
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players,
        variant: CardGameVariant::Landlord,
        landlord: 0,
        current: 0,
        last_play: None,
        consecutive_passes: 0,
        timer: ActiveTimer::new(now),
        warning_sent: false,
        turns: 0,
        bombs: 0,
        opening_spade_three_required: false,
    });
    assert_eq!(game.play("甲", "3", now).action, "played");
    assert_eq!(game.pass("乙", now).action, "passed");
    assert_eq!(game.pass("丙", now).action, "passed");
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.current, 0);
    assert!(playing.last_play.is_none());
}
#[test]
fn playing_the_last_card_ends_and_clears_the_game() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 19);
    game.create("甲", now);
    game.join("乙", now);
    game.join("丙", now);
    finish_bidding_with_all_players_robbing(&mut game, now);
    let (name, card) = {
        let GameState::Playing(playing) = &mut game.state else {
            panic!("expected playing")
        };
        let current = playing.current;
        let card = playing.players[current].hand[0];
        playing.players[current].hand = vec![card];
        (playing.players[current].name.clone(), card.rank.label())
    };

    let outcome = game.play(&name, card, now);
    assert!(outcome.ended);
    assert_eq!(outcome.action, "won");
    assert!(!game.is_active());
}

#[test]
fn landlord_parses_yaobuqi_as_pass() {
    assert_eq!(
        LandlordCommand::parse_hall("要不起"),
        Some(LandlordCommand::Pass)
    );
    assert_eq!(
        LandlordCommand::parse_hall("过"),
        Some(LandlordCommand::Pass)
    );
}
#[test]
fn landlord_auto_passes_when_the_current_player_cannot_beat() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 7);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::Nine)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Five), Card::new(Rank::Six)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Four), Card::new(Rank::Four)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::Landlord,
        landlord: 0,
        current: 1,
        last_play: Some(LastPlay {
            player: 0,
            cards: vec![Card::new(Rank::Three), Card::new(Rank::Three)],
            pattern: PlayPattern::Pair(Rank::Three),
        }),
        consecutive_passes: 0,
        timer: ActiveTimer::new(now),
        warning_sent: false,
        turns: 1,
        bombs: 0,
        opening_spade_three_required: false,
    });

    let outcome = game.tick(now, true).expect("auto pass");

    assert_eq!(outcome.action, "forced-pass");
    assert_eq!(
        outcome.public_reply.as_deref(),
        Some("乙要不起，自动跳过；轮到丙")
    );
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.current, 2);
    assert!(playing.last_play.is_some(), "丙还能压，本轮不清空");
}
#[test]
fn landlord_merges_consecutive_auto_passes_and_starts_a_new_trick() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 8);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::Nine)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Five), Card::new(Rank::Six)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Seven), Card::new(Rank::Eight)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::Landlord,
        landlord: 0,
        current: 1,
        last_play: Some(LastPlay {
            player: 0,
            cards: vec![Card::new(Rank::Three), Card::new(Rank::Three)],
            pattern: PlayPattern::Pair(Rank::Three),
        }),
        consecutive_passes: 0,
        timer: ActiveTimer::new(now),
        warning_sent: false,
        turns: 1,
        bombs: 0,
        opening_spade_three_required: false,
    });

    let outcome = game.tick(now, true).expect("auto pass chain");

    assert_eq!(outcome.action, "forced-pass");
    assert_eq!(
        outcome.public_reply.as_deref(),
        Some("乙、丙要不起，甲领出"),
        "连跳必须合并成一条"
    );
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.current, 0);
    assert!(playing.last_play.is_none(), "两家都要不起后重新领出");
    assert_eq!(playing.consecutive_passes, 0);
}
#[test]
fn landlord_does_not_auto_pass_when_a_beating_play_exists() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 9);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::Nine)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Four), Card::new(Rank::Four)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Five)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::Landlord,
        landlord: 0,
        current: 1,
        last_play: Some(LastPlay {
            player: 0,
            cards: vec![Card::new(Rank::Three), Card::new(Rank::Three)],
            pattern: PlayPattern::Pair(Rank::Three),
        }),
        consecutive_passes: 0,
        timer: ActiveTimer::new(now),
        warning_sent: false,
        turns: 1,
        bombs: 0,
        opening_spade_three_required: false,
    });

    assert!(game.tick(now, true).is_none(), "能压就必须等玩家自己出牌");
}
#[test]
fn lobby_creator_cancels_but_joiner_only_leaves() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 23);
    game.create("甲", now);
    game.join("乙", now);

    let left = game.exit("乙");
    assert_eq!(left.action, "left-lobby");
    assert!(game.is_lobby());
    let canceled = game.exit("甲");
    assert_eq!(canceled.action, "lobby-canceled");
    assert!(canceled.ended);
    assert!(!game.is_active());
}
