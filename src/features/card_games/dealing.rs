//! 发牌与展示格式。
//!
//! 从 `card_games.rs` 拆出：只关心「怎么发牌、怎么显示」，不涉及出牌规则与对局状态机。

use super::*;

pub(super) fn deal(rng: &mut SplitMix64) -> ([Vec<Card>; 3], Vec<Card>) {
    let mut deck = Vec::with_capacity(54);
    for rank in Rank::NORMAL {
        for _ in 0..4 {
            deck.push(Card::new(rank));
        }
    }
    deck.push(Card::new(Rank::SmallJoker));
    deck.push(Card::new(Rank::BigJoker));
    rng.shuffle(&mut deck);

    let mut hands: [Vec<Card>; 3] = std::array::from_fn(|_| Vec::with_capacity(17));
    for index in 0..51 {
        hands[index % 3].push(deck.pop().expect("deck has enough cards"));
    }
    let mut bottom = deck;
    for hand in &mut hands {
        sort_hand(hand);
    }
    sort_hand(&mut bottom);
    (hands, bottom)
}

pub(super) fn sort_hand(hand: &mut [Card]) {
    hand.sort_by_key(|card| (card.rank, card.spade_three));
}

pub(super) fn format_hand(hand: &[Card]) -> String {
    hand.iter()
        .map(|card| {
            if card.spade_three {
                "♠3".to_string()
            } else {
                card.rank.label().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn initial_hand_deliveries(
    players: &[Player],
    variant: CardGameVariant,
) -> Vec<LandlordPrivateDelivery> {
    players
        .iter()
        .map(|player| LandlordPrivateDelivery {
            player: player.name.clone(),
            message: reply(
                &templates::HAND_INITIAL,
                ReplyVars::new()
                    .with("玩法", variant.label())
                    .with("张数", player.hand.len().to_string())
                    .with("手牌", format_hand(&player.hand)),
            ),
        })
        .collect()
}

pub(super) fn deal_hunan_run_fast(rng: &mut SplitMix64) -> ([Vec<Card>; 3], usize) {
    let mut deck = Vec::with_capacity(48);
    for rank in [
        Rank::Three,
        Rank::Four,
        Rank::Five,
        Rank::Six,
        Rank::Seven,
        Rank::Eight,
        Rank::Nine,
        Rank::Ten,
        Rank::Jack,
        Rank::Queen,
        Rank::King,
    ] {
        for copy in 0..4 {
            deck.push(Card {
                rank,
                spade_three: rank == Rank::Three && copy == 0,
            });
        }
    }
    for _ in 0..3 {
        deck.push(Card::new(Rank::Ace));
    }
    deck.push(Card::new(Rank::Two));
    rng.shuffle(&mut deck);

    let mut hands: [Vec<Card>; 3] = std::array::from_fn(|_| Vec::with_capacity(16));
    for index in 0..48 {
        hands[index % 3].push(deck.pop().expect("run-fast deck has enough cards"));
    }
    for hand in &mut hands {
        sort_hand(hand);
    }
    let first = hands
        .iter()
        .position(|hand| hand.iter().any(|card| card.spade_three))
        .expect("run-fast deck contains spade three");
    (hands, first)
}

pub(super) fn format_play(cards: &[Card]) -> String {
    let mut ranks = cards.iter().map(|card| card.rank).collect::<Vec<_>>();
    ranks.sort_unstable();
    ranks
        .into_iter()
        .map(|rank| rank.label())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn format_play_compact(cards: &[Card]) -> String {
    let mut ranks = cards.iter().map(|card| card.rank).collect::<Vec<_>>();
    ranks.sort_unstable();
    ranks
        .into_iter()
        .map(|rank| rank.label())
        .collect::<Vec<_>>()
        .join("")
}
