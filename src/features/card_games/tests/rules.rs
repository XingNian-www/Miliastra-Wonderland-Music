//! 牌型识别、压制判据与发牌。

use super::support::*;
use super::*;

use crate::features::card_games::rules::{classify, lowest_beating_play, parse_cards};

#[test]
fn parses_compact_and_separated_cards() {
    assert_eq!(ranks("33441010jQKA2小王大王").len(), 13);
    assert_eq!(
        ranks("10 10，J、j"),
        vec![Rank::Ten, Rank::Ten, Rank::Jack, Rank::Jack]
    );
    assert!(parse_cards("♠3").is_err());
    assert!(parse_cards("王").is_err());
}
#[test]
fn classifies_complete_supported_pattern_set() {
    for (text, label) in [
        ("3", "单张"),
        ("33", "对子"),
        ("333", "三张"),
        ("3334", "三带一"),
        ("33344", "三带二"),
        ("34567", "顺子"),
        ("334455", "连对"),
        ("333444", "飞机"),
        ("33344455", "飞机带单"),
        ("3334445566", "飞机带对"),
        ("333345", "四带二单"),
        ("33334455", "四带二对"),
        ("3333", "炸弹"),
        ("小王大王", "王炸"),
    ] {
        assert_eq!(
            classify(&ranks(text)).map(|item| item.label()),
            Some(label),
            "{text}"
        );
    }
    assert!(classify(&ranks("JQKA2")).is_none());
    assert!(classify(&ranks("3334445555")).is_none());
}
#[test]
fn compares_only_matching_shapes_except_bombs() {
    assert!(
        classify(&ranks("45678"))
            .unwrap()
            .beats(&classify(&ranks("34567")).unwrap())
    );
    assert!(
        !classify(&ranks("456789"))
            .unwrap()
            .beats(&classify(&ranks("34567")).unwrap())
    );
    assert!(
        classify(&ranks("3333"))
            .unwrap()
            .beats(&classify(&ranks("AA")).unwrap())
    );
    assert!(
        classify(&ranks("小王大王"))
            .unwrap()
            .beats(&classify(&ranks("2222")).unwrap())
    );
}
#[test]
fn trustee_uses_the_lowest_legal_beating_play_before_passing() {
    let hand = ranks("445555小王大王")
        .into_iter()
        .map(Card::new)
        .collect::<Vec<_>>();
    let previous = classify(&ranks("33")).unwrap();
    let (cards, pattern) = lowest_beating_play(&hand, &previous).expect("beating pair");

    assert_eq!(pattern, PlayPattern::Pair(Rank::Four));
    assert_eq!(format_play(&cards), "4 4");
}
#[test]
fn classic_deal_gives_each_player_seventeen_cards_and_three_bottom_cards() {
    for seed in 0..100 {
        let mut rng = SplitMix64::new(seed);
        let (hands, bottom) = deal(&mut rng);
        assert!(hands.iter().all(|hand| hand.len() == 17));
        assert_eq!(bottom.len(), 3);
        assert_eq!(hands.iter().map(Vec::len).sum::<usize>() + bottom.len(), 54);
    }
}
