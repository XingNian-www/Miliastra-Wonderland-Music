//! 牌面基础类型与玩家身份键。
//!
//! 从 `card_games.rs` 拆出：`Rank`/`Card` 只描述牌本身，`find_player`/`player_key`
//! 把「展示名」和「身份键」分开，避免把内部 UUID 渲染给玩家。

use super::landlord::Player;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Rank {
    Three = 3,
    Four,
    Five,
    Six,
    Seven,
    Eight,
    Nine,
    Ten,
    Jack,
    Queen,
    King,
    Ace,
    Two,
    SmallJoker,
    BigJoker,
}

impl Rank {
    pub(super) const NORMAL: [Rank; 13] = [
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
        Rank::Ace,
        Rank::Two,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Three => "3",
            Self::Four => "4",
            Self::Five => "5",
            Self::Six => "6",
            Self::Seven => "7",
            Self::Eight => "8",
            Self::Nine => "9",
            Self::Ten => "10",
            Self::Jack => "J",
            Self::Queen => "Q",
            Self::King => "K",
            Self::Ace => "A",
            Self::Two => "2",
            Self::SmallJoker => "小王",
            Self::BigJoker => "大王",
        }
    }

    pub(super) fn sequence_value(self) -> Option<u8> {
        (self <= Self::Ace).then_some(self as u8)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Card {
    pub(super) rank: Rank,
    pub(super) spade_three: bool,
}

impl Card {
    pub(super) fn new(rank: Rank) -> Self {
        Self {
            rank,
            spade_three: false,
        }
    }
}

pub(super) fn rank_from_value(value: u8) -> Option<Rank> {
    Rank::NORMAL
        .iter()
        .copied()
        .find(|rank| *rank as u8 == value)
}

pub(super) fn find_player(players: &[Player], name: &str) -> Option<usize> {
    let key = player_key(name);
    players.iter().position(|player| player.key == key)
}

/// 展示名缺失时的占位。不能用内部键顶替，那会把 UUID 打到玩家面前。
pub(super) const UNKNOWN_PLAYER_DISPLAY: &str = "未知玩家";

pub(super) fn player_key(name: &str) -> String {
    crate::text::player_match_key(name)
}
