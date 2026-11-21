//! 牌局计时模块与截止类型。
//!
//! 从 `card_games.rs` 拆出：只描述「牌局有哪些截止事件」，不含倒计时推进逻辑
//! （那部分在 `landlord.rs` 的状态机里）。

use miliastra_kernel::timer::{DeadlineKind, DeadlineModule, DeadlineToken};

#[derive(Debug)]
pub struct CardGameDeadlineModule;

impl DeadlineModule for CardGameDeadlineModule {
    const NAME: &'static str = "card-game";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CardGameDeadlineKind {
    LobbyExpiry,
    TurnWarning,
    TurnExpiry,
}

impl DeadlineKind for CardGameDeadlineKind {
    type Module = CardGameDeadlineModule;
}

pub type CardGameDeadlineToken = DeadlineToken<CardGameDeadlineKind>;
