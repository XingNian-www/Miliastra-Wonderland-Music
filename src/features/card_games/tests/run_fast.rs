//! 跑得快：独有规则（黑桃三先出）、强制动作与结算文案。

use super::support::*;
use super::*;

#[test]
fn hunan_run_fast_deals_sixteen_cards_and_spade_three_leads() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 41);
    assert_eq!(game.create_run_fast("甲", now).action, "created");
    assert_eq!(game.join("乙", now).action, "joined");

    let outcome = game.join("丙", now);

    assert_eq!(outcome.action, "run-fast-started");
    assert_eq!(outcome.private_deliveries.len(), 3);
    assert!(
        outcome
            .private_deliveries
            .iter()
            .all(|delivery| delivery.message.contains("初始手牌(16张)"))
    );
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.variant, CardGameVariant::HunanRunFast);
    assert!(playing.players.iter().all(|player| player.hand.len() == 16));
    let dealt_ranks = playing
        .players
        .iter()
        .flat_map(|player| player.hand.iter().map(|card| card.rank))
        .collect::<Vec<_>>();
    let dealt_counts = rank_counts(&dealt_ranks);
    assert_eq!(dealt_counts.get(&Rank::Ace), Some(&3));
    assert_eq!(dealt_counts.get(&Rank::Two), Some(&1));
    assert!(!dealt_counts.contains_key(&Rank::SmallJoker));
    assert!(!dealt_counts.contains_key(&Rank::BigJoker));
    assert_eq!(
        playing
            .players
            .iter()
            .flat_map(|player| &player.hand)
            .filter(|card| card.spade_three)
            .count(),
        1
    );
    assert!(
        playing.players[playing.current]
            .hand
            .iter()
            .any(|card| card.spade_three)
    );
    assert!(playing.opening_spade_three_required);
}
#[test]
fn hunan_run_fast_first_play_uses_spade_three() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 43);
    game.create_run_fast("甲", now);
    game.join("乙", now);
    game.join("丙", now);
    let (player, other_rank) = {
        let GameState::Playing(playing) = &game.state else {
            panic!("expected playing")
        };
        let current = &playing.players[playing.current];
        let other_rank = current
            .hand
            .iter()
            .find(|card| card.rank != Rank::Three)
            .expect("opening hand has a non-three")
            .rank;
        (current.name.clone(), other_rank)
    };

    assert_eq!(
        game.play(&player, other_rank.label(), now).action,
        "spade-three-required"
    );
    assert_eq!(game.play(&player, "3", now).action, "played");
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert!(!playing.opening_spade_three_required);
    assert!(
        playing
            .players
            .iter()
            .flat_map(|player| &player.hand)
            .all(|card| !card.spade_three)
    );
}
#[test]
fn hunan_run_fast_uses_confirmed_triple_pair_straight_and_four_card_rules() {
    assert_eq!(
        classify_for_variant(&ranks("AAA"), CardGameVariant::HunanRunFast),
        Some(PlayPattern::Triple(Rank::Ace))
    );
    assert_eq!(
        classify_for_variant(&ranks("AAA4"), CardGameVariant::HunanRunFast),
        Some(PlayPattern::TripleSingle(Rank::Ace))
    );
    assert_eq!(
        classify_for_variant(&ranks("3344"), CardGameVariant::HunanRunFast),
        Some(PlayPattern::PairStraight {
            high: Rank::Four,
            length: 2,
        })
    );
    assert_eq!(
        classify_for_variant(&ranks("444456"), CardGameVariant::HunanRunFast),
        Some(PlayPattern::FourTwoSingles(Rank::Four))
    );
    assert_eq!(
        classify_for_variant(&ranks("44445566"), CardGameVariant::HunanRunFast),
        Some(PlayPattern::FourTwoPairs(Rank::Four))
    );
    assert_eq!(
        classify_for_variant(&ranks("4444356"), CardGameVariant::HunanRunFast),
        None
    );
}
#[test]
fn hunan_run_fast_cannot_pass_when_a_beating_play_exists() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::Five)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Seven)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
        landlord: 2,
        current: 0,
        last_play: Some(LastPlay {
            player: 2,
            cards: vec![Card::new(Rank::Four)],
            pattern: PlayPattern::Single(Rank::Four),
        }),
        consecutive_passes: 0,
        timer: ActiveTimer::new(now),
        warning_sent: false,
        turns: 1,
        bombs: 0,
        opening_spade_three_required: false,
    });

    assert_eq!(game.pass("甲", now).action, "must-play");
}
#[test]
fn hunan_run_fast_can_pass_when_no_beating_play_exists() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::Three)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Seven)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
        landlord: 2,
        current: 0,
        last_play: Some(LastPlay {
            player: 2,
            cards: vec![Card::new(Rank::Four)],
            pattern: PlayPattern::Single(Rank::Four),
        }),
        consecutive_passes: 0,
        timer: ActiveTimer::new(now),
        warning_sent: false,
        turns: 1,
        bombs: 0,
        opening_spade_three_required: false,
    });

    let outcome = game.pass("甲", now);
    assert_eq!(outcome.action, "passed");
    assert_eq!(outcome.public_reply.as_deref(), Some("甲过，轮到乙"));
}
#[test]
fn hunan_run_fast_immediately_passes_with_no_legal_play() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::Four)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six), Card::new(Rank::Seven)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Eight), Card::new(Rank::Nine)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
        landlord: 2,
        current: 0,
        last_play: Some(LastPlay {
            player: 2,
            cards: vec![Card::new(Rank::Five)],
            pattern: PlayPattern::Single(Rank::Five),
        }),
        consecutive_passes: 0,
        timer: ActiveTimer::new(now),
        warning_sent: false,
        turns: 1,
        bombs: 0,
        opening_spade_three_required: false,
    });

    let outcome = game.tick(now, true).expect("forced pass");

    assert_eq!(outcome.action, "forced-pass");
    assert_eq!(outcome.public_reply.as_deref(), Some("甲无牌可压，轮到乙"));
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.current, 1);
    assert_eq!(playing.players[0].timeouts, 0);
    assert!(!playing.players[0].trustee);
}
#[test]
fn hunan_run_fast_combines_two_forced_passes_into_one_message() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
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
        ],
        variant: CardGameVariant::HunanRunFast,
        landlord: 0,
        current: 1,
        last_play: Some(LastPlay {
            player: 0,
            cards: vec![Card::new(Rank::King)],
            pattern: PlayPattern::Single(Rank::King),
        }),
        consecutive_passes: 0,
        timer: ActiveTimer::new(now),
        warning_sent: false,
        turns: 1,
        bombs: 0,
        opening_spade_three_required: false,
    });

    let first = game.tick(now, true).expect("first forced pass");
    assert_eq!(first.action, "forced-pass");
    assert!(first.public_reply.is_none());

    let second = game.tick(now, true).expect("second forced pass");
    assert_eq!(second.action, "forced-pass");
    assert_eq!(second.public_reply.as_deref(), Some("双方无牌可压，甲领出"));
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.current, 0);
    assert!(playing.last_play.is_none());
}
#[test]
fn hunan_run_fast_immediately_plays_the_only_legal_option() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![
                    Card::new(Rank::Four),
                    Card::new(Rank::Four),
                    Card::new(Rank::Five),
                ],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six), Card::new(Rank::Seven)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Eight)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
        landlord: 2,
        current: 0,
        last_play: Some(LastPlay {
            player: 2,
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

    let outcome = game.tick(now, true).expect("forced play");

    assert_eq!(outcome.action, "forced-play");
    assert_eq!(
        outcome.public_reply.as_deref(),
        Some("甲自动出:44[对子] 余1，轮到乙")
    );
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.players[0].hand, vec![Card::new(Rank::Five)]);
    assert_eq!(playing.players[0].timeouts, 0);
    assert!(!playing.players[0].trustee);
}
#[test]
fn hunan_run_fast_play_message_fits_forty_fullwidth_characters() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![
                    Card::new(Rank::Four),
                    Card::new(Rank::Four),
                    Card::new(Rank::Five),
                ],
                ..Player::new("甲甲甲甲甲甲甲甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six), Card::new(Rank::Seven)],
                ..Player::new("乙乙乙乙乙乙乙乙")
            },
            Player {
                hand: vec![Card::new(Rank::Eight)],
                ..Player::new("丙丙丙丙丙丙丙丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
        landlord: 2,
        current: 0,
        last_play: Some(LastPlay {
            player: 2,
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

    let reply = game
        .tick(now, true)
        .and_then(|outcome| outcome.public_reply)
        .expect("forced play reply");

    assert!(display_width(&reply) <= MAX_CHAT_WIDTH, "{reply}");
}
#[test]
fn hunan_run_fast_waits_when_multiple_legal_options_exist() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![
                    Card::new(Rank::Four),
                    Card::new(Rank::Four),
                    Card::new(Rank::Five),
                    Card::new(Rank::Five),
                ],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six), Card::new(Rank::Seven)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Eight)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
        landlord: 2,
        current: 0,
        last_play: Some(LastPlay {
            player: 2,
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

    assert!(game.tick(now, true).is_none());
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.current, 0);
    assert_eq!(playing.players[0].hand.len(), 4);
}
#[test]
fn hunan_run_fast_timeout_play_uses_compact_message() {
    let now = Instant::now();
    let config = LandlordConfig {
        turn_timeout_seconds: 1,
        ..LandlordConfig::default()
    };
    let mut game = LandlordGame::with_seed(config, 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![
                    Card::new(Rank::Four),
                    Card::new(Rank::Four),
                    Card::new(Rank::Five),
                    Card::new(Rank::Five),
                ],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six), Card::new(Rank::Seven)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Eight)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
        landlord: 2,
        current: 0,
        last_play: Some(LastPlay {
            player: 2,
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

    let outcome = game
        .tick(now + Duration::from_secs(1), true)
        .expect("timeout play");

    assert_eq!(outcome.action, "auto-play");
    assert_eq!(
        outcome.public_reply.as_deref(),
        Some("甲超时出:44[对子] 余2，轮到乙")
    );
}
#[test]
fn hunan_run_fast_requires_largest_single_when_next_player_has_one_card() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::Five), Card::new(Rank::King)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Seven), Card::new(Rank::Eight)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
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

    assert_eq!(game.play("甲", "5", now).action, "must-play-largest-single");
    let outcome = game.play("甲", "K", now);
    assert_eq!(outcome.action, "played");
    assert_eq!(
        outcome.public_reply.as_deref(),
        Some("甲出:K[单张] 余1，轮到乙")
    );
}
#[test]
fn hunan_run_fast_report_one_constraint_can_leave_one_forced_option() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::Five), Card::new(Rank::King)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Seven), Card::new(Rank::Eight)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
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

    assert_eq!(
        game.tick(now, true).expect("report-one forced play").action,
        "forced-play"
    );
    let GameState::Playing(playing) = &game.state else {
        panic!("expected playing")
    };
    assert_eq!(playing.players[0].hand, vec![Card::new(Rank::Five)]);
}
#[test]
fn hunan_run_fast_first_empty_hand_wins() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::King)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six), Card::new(Rank::Seven)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Eight), Card::new(Rank::Nine)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
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

    let outcome = game.play("甲", "K", now);

    assert!(outcome.ended);
    assert!(
        outcome
            .public_reply
            .is_some_and(|reply| reply.contains("获胜"))
    );
    assert!(!game.is_active());
}
#[test]
fn hunan_run_fast_leader_with_one_option_immediately_wins() {
    let now = Instant::now();
    let mut game = LandlordGame::with_seed(LandlordConfig::default(), 1);
    game.state = GameState::Playing(Playing {
        players: vec![
            Player {
                hand: vec![Card::new(Rank::King)],
                ..Player::new("甲")
            },
            Player {
                hand: vec![Card::new(Rank::Six), Card::new(Rank::Seven)],
                ..Player::new("乙")
            },
            Player {
                hand: vec![Card::new(Rank::Eight), Card::new(Rank::Nine)],
                ..Player::new("丙")
            },
        ],
        variant: CardGameVariant::HunanRunFast,
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

    let outcome = game.tick(now, true).expect("forced winning play");

    assert_eq!(outcome.action, "won");
    assert!(outcome.ended);
    assert_eq!(
        outcome.public_reply.as_deref(),
        Some("甲自动出:K[单张]，获胜；其余:2张/2张")
    );
    assert!(!game.is_active());
}
