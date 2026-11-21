//! 牌类玩法测试。
//!
//! 按子域分文件：规则/发牌、跑得快、斗地主、效果链；共用替身在 `support`。

mod support;

mod effects;
mod landlord;
mod rules;
mod run_fast;

use std::time::{Duration, Instant};

use anyhow::Result;
use miliastra_kernel::clock::{Clock, ManualClock};

use super::cards::{Card, Rank, find_player, player_key};
use super::dealing::{deal, format_play};
use super::landlord::{
    ActiveTimer, Bidding, BiddingPhase, CardGameVariant, GameState, LastPlay, Player, Playing,
};
use super::rng::SplitMix64;
use super::rules::{PlayPattern, classify_for_variant, parse_cards, rank_counts};
use super::service::CardGameStartGate;
use super::*;
use crate::features::chat_text::{MAX_CHAT_WIDTH, display_width};
use crate::features::entertainment::{AcquireOutcome, EntertainmentKind, EntertainmentState};
use crate::features::friend_delivery::{
    FriendBatchFailure, FriendBatchFailureKind, FriendBatchOutcome, FriendMessage,
};
