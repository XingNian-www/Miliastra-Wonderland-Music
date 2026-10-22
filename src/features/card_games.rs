use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::features::chat_text::{command_identity, compact_command};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
#[cfg(test)]
use crate::features::friend_delivery::{
    FriendBatchFailure, FriendBatchFailureKind, FriendBatchOutcome, FriendMessage,
};
use crate::features::reply_template::{ReplyVars, card_games as templates};
use miliastra_kernel::timer::{DeadlineKind, DeadlineModule, DeadlineToken};

mod dealing;
mod rules;
mod service;

use dealing::{
    deal, deal_hunan_run_fast, format_hand, format_play, format_play_compact,
    initial_hand_deliveries, sort_hand,
};
use rules::{
    PlayPattern, RunFastPlayFilter, classify, classify_airplane, classify_for_variant,
    collect_beating_plays, collect_run_fast_legal_plays, consecutive, group_rank,
    lowest_beating_play, lowest_beating_play_for_variant, parse_cards, rank_counts, remove_cards,
    single_group, take_cards, take_cards_for_play,
};
#[cfg(test)]
use service::CardGameStartGate;
pub use service::{
    CardGameApplication, CardGameCancel, CardGameCommandStart, CardGameEffectClaim,
    CardGameEffectKey, CardGameEffectLane, CardGameEffectResult, CardGameEffectTask,
    CardGameResume, CardGameRuntimePort, CardGameTimedOutcome,
};
#[cfg(test)]
pub(crate) use service::{
    CardGameCompletion, CardGameEffect, CardGameEffectRequest, CardGameLateResult,
    CardGameStartReservation,
};
pub use service::{CardGameDeliveryPort, CardGameService};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LandlordConfig {
    pub enabled: bool,
    pub lobby_timeout_seconds: u64,
    pub turn_timeout_seconds: u64,
    pub trustee_after_timeouts: u32,
    pub hand_cooldown_seconds: u64,
}

impl Default for LandlordConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lobby_timeout_seconds: 120,
            turn_timeout_seconds: 90,
            trustee_after_timeouts: 2,
            hand_cooldown_seconds: 10,
        }
    }
}

impl LandlordConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.enabled
            && (self.lobby_timeout_seconds == 0
                || self.turn_timeout_seconds == 0
                || self.trustee_after_timeouts == 0)
        {
            bail!("牌局大厅、回合超时和托管触发次数必须大于 0");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum LandlordCommand {
    Start,
    RunFastStart,
    Join,
    Rob,
    Decline,
    Status,
    Play(String),
    Pass,
    Hand,
    Retry,
    Exit,
}

impl LandlordCommand {
    pub(crate) fn claims_start_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && envelope.authority() == CommandAuthority::HallMember
            && matches!(
                compact_command(envelope.command_text()).as_str(),
                "斗地主" | "跑得快"
            )
    }

    pub(crate) fn claims_active_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && match envelope.authority() {
                CommandAuthority::HallMember => true,
                CommandAuthority::Friend => compact_command(envelope.command_text()) == "手牌",
            }
    }

    pub(crate) fn parse_start_chat(
        envelope: &CommandEnvelope,
    ) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_start_chat(envelope) {
            return None;
        }
        Self::parse_start(envelope.command_text())
            .map(|command| FeatureCommandMatch::new("#", envelope.command_text(), command))
    }

    pub(crate) fn parse_active_chat(
        envelope: &CommandEnvelope,
    ) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_active_chat(envelope) {
            return None;
        }
        let command = match envelope.authority() {
            CommandAuthority::HallMember => Self::parse_hall(envelope.command_text())?,
            CommandAuthority::Friend => Self::parse_friend(envelope.command_text())?,
        };
        Some(FeatureCommandMatch::new(
            "#",
            envelope.command_text(),
            command,
        ))
    }

    pub(crate) fn parse_start(payload: &str) -> Option<Self> {
        match compact_command(payload).as_str() {
            "斗地主" => Some(Self::Start),
            "跑得快" => Some(Self::RunFastStart),
            _ => None,
        }
    }

    pub(crate) fn parse_hall(payload: &str) -> Option<Self> {
        let command = match compact_command(payload).as_str() {
            "加入" => Self::Join,
            "抢" => Self::Rob,
            "不抢" => Self::Decline,
            "过" => Self::Pass,
            "状态" => Self::Status,
            "重试" => Self::Retry,
            "结束" => Self::Exit,
            "手牌" => return None,
            _ => {
                let cards = payload
                    .strip_prefix('出')
                    .unwrap_or(payload)
                    .trim_start_matches(['：', ':', ' ', '\t'])
                    .trim();
                if cards.is_empty() {
                    return None;
                }
                Self::Play(cards.to_string())
            }
        };
        Some(command)
    }

    pub(crate) fn parse_friend(payload: &str) -> Option<Self> {
        (compact_command(payload) == "手牌").then_some(Self::Hand)
    }

    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Start => "landlord:start".to_string(),
            Self::RunFastStart => "run_fast:start".to_string(),
            Self::Join => "landlord:join".to_string(),
            Self::Rob => "landlord:rob".to_string(),
            Self::Decline => "landlord:decline".to_string(),
            Self::Status => "landlord:status".to_string(),
            Self::Play(cards) => format!("landlord:play:{}", command_identity(cards)),
            Self::Pass => "landlord:pass".to_string(),
            Self::Hand => "landlord:hand".to_string(),
            Self::Retry => "landlord:retry".to_string(),
            Self::Exit => "landlord:exit".to_string(),
        }
    }

    pub(crate) fn requires_executor(&self) -> bool {
        matches!(
            self,
            Self::Start
                | Self::RunFastStart
                | Self::Join
                | Self::Rob
                | Self::Decline
                | Self::Hand
                | Self::Retry
        )
    }

    fn reports_entertainment_conflict(&self) -> bool {
        matches!(self, Self::Play(_) | Self::Pass | Self::Exit)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandlordOutcome {
    pub action: &'static str,
    pub public_reply: Option<String>,
    pub public_reply_template: Option<CardGameReplyTemplate>,
    pub private_reply: Option<String>,
    pub private_reply_template: Option<CardGameReplyTemplate>,
    pub private_deliveries: Vec<LandlordPrivateDelivery>,
    pub ended: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardGameReplyTemplate {
    Template {
        spec: &'static crate::features::reply_template::ReplyTemplate,
        vars: ReplyVars,
    },
    Raw(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandlordPrivateDelivery {
    pub player: String,
    pub message: String,
    pub template: Option<CardGameReplyTemplate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardGameFriendBatchOutcome {
    Complete,
    Failed {
        retryable: Vec<LandlordPrivateDelivery>,
        failure: crate::features::friend_delivery::FriendBatchFailure,
    },
}

impl LandlordOutcome {
    fn public(action: &'static str, reply: impl Into<CardGameReplyTemplate>) -> Self {
        let reply = reply.into();
        Self {
            action,
            public_reply: Some(reply.render_default()),
            public_reply_template: match reply {
                CardGameReplyTemplate::Template { .. } => Some(reply),
                CardGameReplyTemplate::Raw(_) => None,
            },
            private_reply: None,
            private_reply_template: None,
            private_deliveries: Vec::new(),
            ended: false,
        }
    }

    fn private(action: &'static str, reply: impl Into<CardGameReplyTemplate>) -> Self {
        let reply = reply.into();
        Self {
            action,
            public_reply: None,
            public_reply_template: None,
            private_reply: Some(reply.render_default()),
            private_reply_template: match reply {
                CardGameReplyTemplate::Template { .. } => Some(reply),
                CardGameReplyTemplate::Raw(_) => None,
            },
            private_deliveries: Vec::new(),
            ended: false,
        }
    }

    fn ended(action: &'static str, reply: impl Into<CardGameReplyTemplate>) -> Self {
        let reply = reply.into();
        Self {
            action,
            public_reply: Some(reply.render_default()),
            public_reply_template: match reply {
                CardGameReplyTemplate::Template { .. } => Some(reply),
                CardGameReplyTemplate::Raw(_) => None,
            },
            private_reply: None,
            private_reply_template: None,
            private_deliveries: Vec::new(),
            ended: true,
        }
    }

    fn with_private_deliveries(mut self, deliveries: Vec<LandlordPrivateDelivery>) -> Self {
        self.private_deliveries = deliveries;
        self
    }
}

pub struct LandlordGame {
    config: LandlordConfig,
    state: GameState,
    rng: SplitMix64,
}

enum GameState {
    Idle,
    Lobby(Lobby),
    Bidding(Bidding),
    Playing(Playing),
}

struct Lobby {
    players: Vec<Player>,
    variant: CardGameVariant,
    timer: ActiveTimer,
}

struct Bidding {
    players: Vec<Player>,
    bottom: Vec<Card>,
    current: usize,
    decisions: u8,
    candidate: Option<usize>,
    timer: ActiveTimer,
    warning_sent: bool,
}

struct Playing {
    players: Vec<Player>,
    variant: CardGameVariant,
    landlord: usize,
    current: usize,
    last_play: Option<LastPlay>,
    consecutive_passes: u8,
    timer: ActiveTimer,
    warning_sent: bool,
    turns: u32,
    bombs: u32,
    opening_spade_three_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CardGameVariant {
    Landlord,
    HunanRunFast,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionOrigin {
    Manual,
    Timeout,
    Forced,
}

enum ForcedRunFastAction {
    Pass,
    Play {
        cards: Vec<Card>,
        pattern: PlayPattern,
    },
}

impl CardGameVariant {
    fn label(self) -> &'static str {
        match self {
            Self::Landlord => "斗地主",
            Self::HunanRunFast => "跑得快",
        }
    }
}

#[derive(Clone)]
struct Player {
    name: String,
    key: String,
    hand: Vec<Card>,
    timeouts: u32,
    trustee: bool,
    last_hand_reply: Option<Instant>,
}

impl Player {
    fn new(name: &str) -> Self {
        Self {
            name: name.trim().to_string(),
            key: player_key(name),
            hand: Vec::new(),
            timeouts: 0,
            trustee: false,
            last_hand_reply: None,
        }
    }
}

struct LastPlay {
    player: usize,
    cards: Vec<Card>,
    pattern: PlayPattern,
}

struct ActiveTimer {
    elapsed: Duration,
    last_tick: Instant,
}

impl ActiveTimer {
    fn new(now: Instant) -> Self {
        Self {
            elapsed: Duration::ZERO,
            last_tick: now,
        }
    }

    fn tick(&mut self, now: Instant, active: bool) {
        if active {
            self.elapsed = self
                .elapsed
                .saturating_add(now.saturating_duration_since(self.last_tick));
        }
        self.last_tick = now;
    }

    fn reset(&mut self, now: Instant) {
        self.elapsed = Duration::ZERO;
        self.last_tick = now;
    }

    fn elapsed_at(&self, now: Instant, active: bool) -> Duration {
        self.elapsed.saturating_add(if active {
            now.saturating_duration_since(self.last_tick)
        } else {
            Duration::ZERO
        })
    }
}

impl LandlordGame {
    pub fn new(config: LandlordConfig) -> Self {
        Self::with_seed(config, random_seed())
    }

    fn with_seed(config: LandlordConfig, seed: u64) -> Self {
        Self {
            config,
            state: GameState::Idle,
            rng: SplitMix64::new(seed),
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.state, GameState::Idle)
    }

    pub fn is_lobby(&self) -> bool {
        matches!(self.state, GameState::Lobby(_))
    }

    pub fn lobby_contains(&self, player: &str) -> bool {
        matches!(
            &self.state,
            GameState::Lobby(lobby) if find_player(&lobby.players, player).is_some()
        )
    }

    pub fn abort(&mut self) -> bool {
        if matches!(self.state, GameState::Idle) {
            false
        } else {
            self.state = GameState::Idle;
            true
        }
    }

    pub(crate) fn next_deadline(
        &self,
        now: Instant,
        clock_active: bool,
    ) -> Option<(CardGameDeadlineKind, Instant)> {
        if !clock_active {
            return None;
        }
        match &self.state {
            GameState::Idle => None,
            GameState::Lobby(lobby) => Some((
                CardGameDeadlineKind::LobbyExpiry,
                deadline_after(
                    now,
                    lobby.timer.elapsed_at(now, true),
                    Duration::from_secs(self.config.lobby_timeout_seconds.max(1)),
                ),
            )),
            GameState::Bidding(game) => {
                let elapsed = game.timer.elapsed_at(now, true);
                let timeout = Duration::from_secs(self.config.turn_timeout_seconds.max(1));
                let warning_at = timeout.saturating_sub(Duration::from_secs(30));
                let kind = if !game.warning_sent && elapsed < timeout {
                    CardGameDeadlineKind::TurnWarning
                } else {
                    CardGameDeadlineKind::TurnExpiry
                };
                let target = if kind == CardGameDeadlineKind::TurnWarning {
                    warning_at
                } else {
                    timeout
                };
                Some((kind, deadline_after(now, elapsed, target)))
            }
            GameState::Playing(game) => {
                if forced_run_fast_action(game).is_some() || game.players[game.current].trustee {
                    return Some((CardGameDeadlineKind::TurnExpiry, now));
                }
                let elapsed = game.timer.elapsed_at(now, true);
                let timeout = Duration::from_secs(self.config.turn_timeout_seconds.max(1));
                let warning_at = timeout.saturating_sub(Duration::from_secs(30));
                let kind = if !game.warning_sent && elapsed < timeout {
                    CardGameDeadlineKind::TurnWarning
                } else {
                    CardGameDeadlineKind::TurnExpiry
                };
                let target = if kind == CardGameDeadlineKind::TurnWarning {
                    warning_at
                } else {
                    timeout
                };
                Some((kind, deadline_after(now, elapsed, target)))
            }
        }
    }

    pub(crate) fn sync_clock(&mut self, now: Instant, clock_active: bool) {
        match &mut self.state {
            GameState::Idle => {}
            GameState::Lobby(lobby) => lobby.timer.tick(now, clock_active),
            GameState::Bidding(game) => game.timer.tick(now, clock_active),
            GameState::Playing(game) => game.timer.tick(now, clock_active),
        }
    }

    pub fn retry_hand_delivery(&mut self, player: &str) {
        match &mut self.state {
            GameState::Bidding(game) => {
                if let Some(index) = find_player(&game.players, player) {
                    game.players[index].last_hand_reply = None;
                }
            }
            GameState::Playing(game) => {
                if let Some(index) = find_player(&game.players, player) {
                    game.players[index].last_hand_reply = None;
                }
            }
            GameState::Idle | GameState::Lobby(_) => {}
        }
    }

    pub fn create(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        self.create_variant(player, CardGameVariant::Landlord, now)
    }

    pub fn create_run_fast(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        self.create_variant(player, CardGameVariant::HunanRunFast, now)
    }

    fn create_variant(
        &mut self,
        player: &str,
        variant: CardGameVariant,
        now: Instant,
    ) -> LandlordOutcome {
        if !self.config.enabled {
            return LandlordOutcome::public(
                "disabled",
                keyed_reply(
                    &templates::DISABLED,
                    ReplyVars::new().with("玩法", variant.label()),
                ),
            );
        }
        if self.is_active() {
            return LandlordOutcome::public(
                "already-active",
                keyed_reply(&templates::ALREADY_ACTIVE, ReplyVars::new()),
            );
        }
        self.state = GameState::Lobby(Lobby {
            players: vec![Player::new(player)],
            variant,
            timer: ActiveTimer::new(now),
        });
        LandlordOutcome::public(
            "created",
            keyed_reply(
                &templates::LOBBY_CREATED,
                ReplyVars::new()
                    .with("玩家", player.trim())
                    .with("玩法", variant.label()),
            ),
        )
    }

    pub fn join(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        let GameState::Lobby(lobby) = &mut self.state else {
            return LandlordOutcome::public(
                "no-lobby",
                keyed_reply(&templates::LOBBY_NONE, ReplyVars::new()),
            );
        };
        let key = player_key(player);
        if lobby.players.iter().any(|item| item.key == key) {
            return LandlordOutcome::public(
                "duplicate-player",
                keyed_reply(
                    &templates::LOBBY_ALREADY_JOINED,
                    ReplyVars::new().with("玩法", lobby.variant.label()),
                ),
            );
        }
        lobby.players.push(Player::new(player));
        lobby.timer.reset(now);
        if lobby.players.len() < 3 {
            return LandlordOutcome::public(
                "joined",
                keyed_reply(
                    &templates::LOBBY_JOINED,
                    ReplyVars::new()
                        .with("玩家", player.trim())
                        .with("玩法", lobby.variant.label()),
                ),
            );
        }

        let variant = lobby.variant;
        let mut players = std::mem::take(&mut lobby.players);
        if variant == CardGameVariant::HunanRunFast {
            let (hands, first) = deal_hunan_run_fast(&mut self.rng);
            for (player, hand) in players.iter_mut().zip(hands) {
                player.hand = hand;
            }
            let first_name = players[first].name.clone();
            let deliveries = initial_hand_deliveries(&players, variant);
            self.state = GameState::Playing(Playing {
                players,
                variant,
                landlord: first,
                current: first,
                last_play: None,
                consecutive_passes: 0,
                timer: ActiveTimer::new(now),
                warning_sent: false,
                turns: 0,
                bombs: 0,
                opening_spade_three_required: true,
            });
            return LandlordOutcome::public(
                "run-fast-started",
                keyed_reply(
                    &templates::RUN_FAST_STARTED,
                    ReplyVars::new().with("玩家", first_name),
                ),
            )
            .with_private_deliveries(deliveries);
        }

        let (hands, bottom) = deal(&mut self.rng);
        for (player, hand) in players.iter_mut().zip(hands) {
            player.hand = hand;
        }
        let first = self.rng.index(players.len());
        let first_name = players[first].name.clone();
        let deliveries = initial_hand_deliveries(&players, variant);
        self.state = GameState::Bidding(Bidding {
            players,
            bottom,
            current: first,
            decisions: 0,
            candidate: None,
            timer: ActiveTimer::new(now),
            warning_sent: false,
        });
        LandlordOutcome::public(
            "bidding-started",
            keyed_reply(
                &templates::BIDDING_STARTED,
                ReplyVars::new().with("玩家", first_name),
            ),
        )
        .with_private_deliveries(deliveries)
    }

    pub fn handle(
        &mut self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> LandlordOutcome {
        match command {
            LandlordCommand::Start => self.create(player, now),
            LandlordCommand::RunFastStart => self.create_run_fast(player, now),
            LandlordCommand::Join => self.join(player, now),
            LandlordCommand::Rob => self.bid(player, true, now),
            LandlordCommand::Decline => self.bid(player, false, now),
            LandlordCommand::Status => self.status(),
            LandlordCommand::Play(cards) => self.play(player, cards, now),
            LandlordCommand::Pass => self.pass(player, now),
            LandlordCommand::Hand => self.hand(player, now),
            LandlordCommand::Retry => LandlordOutcome::public(
                "retry-unavailable",
                keyed_reply(&templates::RETRY_NONE, ReplyVars::new()),
            ),
            LandlordCommand::Exit => self.exit(player),
        }
    }

    fn bid(&mut self, player: &str, rob: bool, now: Instant) -> LandlordOutcome {
        let GameState::Bidding(bidding) = &mut self.state else {
            return LandlordOutcome::public(
                "not-bidding",
                keyed_reply(&templates::BIDDING_NOT_BIDDING, ReplyVars::new()),
            );
        };
        let Some(index) = find_player(&bidding.players, player) else {
            return LandlordOutcome::public(
                "not-player",
                keyed_reply(&templates::NOT_IN_LANDLORD_GAME, ReplyVars::new()),
            );
        };
        if index != bidding.current {
            return LandlordOutcome::public(
                "wrong-bidder",
                keyed_reply(
                    &templates::BIDDING_TURN,
                    ReplyVars::new().with("玩家", bidding.players[bidding.current].name.clone()),
                ),
            );
        }

        if rob {
            bidding.candidate = Some(index);
        }
        bidding.decisions = bidding.decisions.saturating_add(1);
        let name = bidding.players[index].name.clone();
        if bidding.decisions < bidding.players.len() as u8 {
            bidding.current = (bidding.current + 1) % bidding.players.len();
            bidding.timer.reset(now);
            bidding.warning_sent = false;
            let next = bidding.players[bidding.current].name.clone();
            return LandlordOutcome::public(
                if rob { "robbed" } else { "declined" },
                keyed_reply(
                    &templates::BIDDING_DECIDED,
                    ReplyVars::new()
                        .with("玩家", name)
                        .with("决定", if rob { "抢地主" } else { "不抢" })
                        .with("下一位", next),
                ),
            );
        }

        let GameState::Bidding(mut bidding) = std::mem::replace(&mut self.state, GameState::Idle)
        else {
            unreachable!("bidding state was checked")
        };
        let Some(landlord) = bidding.candidate else {
            let (hands, bottom) = deal(&mut self.rng);
            for (player, hand) in bidding.players.iter_mut().zip(hands) {
                player.hand = hand;
                player.timeouts = 0;
                player.trustee = false;
                player.last_hand_reply = None;
            }
            let first = self.rng.index(bidding.players.len());
            let first_name = bidding.players[first].name.clone();
            let deliveries = initial_hand_deliveries(&bidding.players, CardGameVariant::Landlord);
            self.state = GameState::Bidding(Bidding {
                players: bidding.players,
                bottom,
                current: first,
                decisions: 0,
                candidate: None,
                timer: ActiveTimer::new(now),
                warning_sent: false,
            });
            return LandlordOutcome::public(
                "redealt",
                keyed_reply(
                    &templates::BIDDING_REDEALT,
                    ReplyVars::new().with("玩家", first_name),
                ),
            )
            .with_private_deliveries(deliveries);
        };

        let bottom_text = format_play(&bidding.bottom);
        bidding.players[landlord].hand.append(&mut bidding.bottom);
        sort_hand(&mut bidding.players[landlord].hand);
        let landlord_name = bidding.players[landlord].name.clone();
        let landlord_delivery = LandlordPrivateDelivery {
            player: landlord_name.clone(),
            message: reply(
                &templates::HAND_LANDLORD_INITIAL,
                ReplyVars::new()
                    .with("张数", bidding.players[landlord].hand.len().to_string())
                    .with("手牌", format_hand(&bidding.players[landlord].hand)),
            ),
            template: Some(keyed_reply(
                &templates::HAND_LANDLORD_INITIAL,
                ReplyVars::new()
                    .with("张数", bidding.players[landlord].hand.len().to_string())
                    .with("手牌", format_hand(&bidding.players[landlord].hand)),
            )),
        };
        self.state = GameState::Playing(Playing {
            players: bidding.players,
            variant: CardGameVariant::Landlord,
            landlord,
            current: landlord,
            last_play: None,
            consecutive_passes: 0,
            timer: ActiveTimer::new(now),
            warning_sent: false,
            turns: 0,
            bombs: 0,
            opening_spade_three_required: false,
        });
        LandlordOutcome::public(
            "landlord-selected",
            keyed_reply(
                &templates::LANDLORD_SELECTED,
                ReplyVars::new()
                    .with("玩家", landlord_name)
                    .with("底牌", bottom_text),
            ),
        )
        .with_private_deliveries(vec![landlord_delivery])
    }

    pub fn tick(&mut self, now: Instant, clock_active: bool) -> Option<LandlordOutcome> {
        match &mut self.state {
            GameState::Idle => None,
            GameState::Lobby(lobby) => {
                lobby.timer.tick(now, clock_active);
                if lobby.timer.elapsed
                    < Duration::from_secs(self.config.lobby_timeout_seconds.max(1))
                {
                    return None;
                }
                let label = lobby.variant.label();
                self.state = GameState::Idle;
                Some(LandlordOutcome::ended(
                    "lobby-timeout",
                    keyed_reply(
                        &templates::LOBBY_TIMEOUT,
                        ReplyVars::new().with("玩法", label),
                    ),
                ))
            }
            GameState::Bidding(game) => {
                game.timer.tick(now, clock_active);
                if !clock_active {
                    return None;
                }
                let timeout = Duration::from_secs(self.config.turn_timeout_seconds.max(1));
                let warning_at = timeout.saturating_sub(Duration::from_secs(30));
                if !game.warning_sent
                    && game.timer.elapsed >= warning_at
                    && game.timer.elapsed < timeout
                {
                    game.warning_sent = true;
                    return Some(LandlordOutcome::public(
                        "bid-warning",
                        keyed_reply(
                            &templates::BIDDING_WARNING,
                            ReplyVars::new().with("玩家", game.players[game.current].name.clone()),
                        ),
                    ));
                }
                if game.timer.elapsed < timeout {
                    return None;
                }
                let player = game.players[game.current].name.clone();
                let mut outcome = self.bid(&player, false, now);
                if let Some(continuation) = outcome.public_reply.take() {
                    outcome.public_reply = Some(reply(
                        &templates::BIDDING_TIMEOUT,
                        ReplyVars::new()
                            .with("玩家", player)
                            .with("后续", continuation),
                    ));
                }
                Some(outcome)
            }
            GameState::Playing(game) => {
                game.timer.tick(now, clock_active);
                if !clock_active {
                    return None;
                }
                let current = game.current;
                if let Some(action) = forced_run_fast_action(game) {
                    let outcome = match action {
                        ForcedRunFastAction::Pass => {
                            let mut outcome =
                                pass_playing(game, current, now, ActionOrigin::Forced);
                            let next = game.current;
                            if game.last_play.is_some()
                                && (forced_run_fast_action(game).is_some()
                                    || game.players[next].trustee)
                            {
                                outcome.public_reply = None;
                            }
                            outcome
                        }
                        ForcedRunFastAction::Play { cards, pattern } => {
                            play_cards(game, current, cards, pattern, now, ActionOrigin::Forced)
                        }
                    };
                    if outcome.ended {
                        self.state = GameState::Idle;
                    }
                    return Some(outcome);
                }
                if game.players[current].trustee {
                    return Some(self.timeout_action(now));
                }
                let timeout = Duration::from_secs(self.config.turn_timeout_seconds.max(1));
                let warning_at = timeout.saturating_sub(Duration::from_secs(30));
                if !game.warning_sent
                    && game.timer.elapsed >= warning_at
                    && game.timer.elapsed < timeout
                {
                    game.warning_sent = true;
                    return Some(LandlordOutcome::public(
                        "turn-warning",
                        keyed_reply(
                            &templates::TURN_WARNING,
                            ReplyVars::new().with("玩家", game.players[current].name.clone()),
                        ),
                    ));
                }
                if game.timer.elapsed >= timeout {
                    Some(self.timeout_action(now))
                } else {
                    None
                }
            }
        }
    }

    fn timeout_action(&mut self, now: Instant) -> LandlordOutcome {
        let GameState::Playing(game) = &mut self.state else {
            unreachable!("timeout action requires an active game");
        };
        let current = game.current;
        game.players[current].timeouts = game.players[current].timeouts.saturating_add(1);
        if game.players[current].timeouts >= self.config.trustee_after_timeouts.max(1) {
            game.players[current].trustee = true;
        }
        let name = game.players[current].name.clone();
        let can_pass = game
            .last_play
            .as_ref()
            .is_some_and(|last| last.player != current);
        if can_pass {
            if game.variant == CardGameVariant::HunanRunFast {
                let previous = &game
                    .last_play
                    .as_ref()
                    .expect("can pass has last play")
                    .pattern;
                if let Some((mut cards, mut pattern)) = lowest_beating_play_for_variant(
                    &game.players[current].hand,
                    previous,
                    game.variant,
                ) {
                    if matches!(pattern, PlayPattern::Single(_))
                        && let Some(card) = report_one_largest_single(game, current)
                    {
                        let largest_single = PlayPattern::Single(card.rank);
                        if largest_single.beats(previous) {
                            cards = vec![card];
                            pattern = largest_single;
                        }
                    }
                    let outcome =
                        play_cards(game, current, cards, pattern, now, ActionOrigin::Timeout);
                    if outcome.ended {
                        self.state = GameState::Idle;
                    }
                    return outcome;
                }
                pass_playing(game, current, now, ActionOrigin::Timeout);
                let next = game.players[game.current].name.clone();
                return LandlordOutcome::public(
                    "auto-pass",
                    keyed_reply(
                        &templates::PASS_TIMEOUT_NO_BEATING,
                        ReplyVars::new().with("玩家", name).with("下一位", next),
                    ),
                );
            }
            if !game.players[current].trustee {
                pass_playing(game, current, now, ActionOrigin::Timeout);
                let next = game.players[game.current].name.clone();
                return LandlordOutcome::public(
                    "auto-pass",
                    keyed_reply(
                        &templates::PASS_TIMEOUT,
                        ReplyVars::new().with("玩家", name).with("下一位", next),
                    ),
                );
            }
            let previous = &game
                .last_play
                .as_ref()
                .expect("can pass has last play")
                .pattern;
            if let Some((cards, pattern)) =
                lowest_beating_play(&game.players[current].hand, previous)
            {
                let outcome = play_cards(game, current, cards, pattern, now, ActionOrigin::Timeout);
                if outcome.ended {
                    self.state = GameState::Idle;
                }
                outcome
            } else {
                pass_playing(game, current, now, ActionOrigin::Timeout);
                let next = game.players[game.current].name.clone();
                LandlordOutcome::public(
                    "auto-pass",
                    keyed_reply(
                        &templates::PASS_TIMEOUT_WITH_SUFFIX,
                        ReplyVars::new()
                            .with("玩家", name)
                            .with("托管后缀", trustee_suffix(game, current))
                            .with("下一位", next),
                    ),
                )
            }
        } else {
            let card = if game.opening_spade_three_required {
                game.players[current]
                    .hand
                    .iter()
                    .find(|card| card.spade_three)
                    .copied()
                    .expect("run-fast opening player has spade three")
            } else if let Some(card) = report_one_largest_single(game, current) {
                card
            } else {
                game.players[current]
                    .hand
                    .first()
                    .copied()
                    .expect("active player has cards")
            };
            let outcome = play_cards(
                game,
                current,
                vec![card],
                PlayPattern::Single(card.rank),
                now,
                ActionOrigin::Timeout,
            );
            if outcome.ended {
                self.state = GameState::Idle;
            }
            outcome
        }
    }

    fn play(&mut self, player: &str, text: &str, now: Instant) -> LandlordOutcome {
        let GameState::Playing(game) = &mut self.state else {
            return LandlordOutcome::public(
                "no-game",
                keyed_reply(&templates::NO_ACTIVE_GAME, ReplyVars::new()),
            );
        };
        let Some(player_index) = find_player(&game.players, player) else {
            return LandlordOutcome::public(
                "not-player",
                keyed_reply(
                    &templates::NOT_IN_GAME,
                    ReplyVars::new().with("玩法", game.variant.label()),
                ),
            );
        };
        if player_index != game.current {
            return LandlordOutcome::public(
                "wrong-turn",
                keyed_reply(
                    &templates::WRONG_TURN,
                    ReplyVars::new().with("玩家", game.players[game.current].name.clone()),
                ),
            );
        }
        let ranks = match parse_cards(text) {
            Ok(ranks) if !ranks.is_empty() => ranks,
            Ok(_) => {
                return LandlordOutcome::public(
                    "empty-play",
                    keyed_reply(&templates::PLAY_EMPTY, ReplyVars::new()),
                );
            }
            Err(error) => return LandlordOutcome::public("invalid-cards", error),
        };
        let cards = match take_cards_for_play(
            &game.players[player_index].hand,
            &ranks,
            game.variant == CardGameVariant::HunanRunFast && game.opening_spade_three_required,
        ) {
            Ok(cards) => cards,
            Err(error) => return LandlordOutcome::public("missing-card", error),
        };
        if game.variant == CardGameVariant::HunanRunFast
            && game.opening_spade_three_required
            && !cards.iter().any(|card| card.spade_three)
        {
            return LandlordOutcome::public(
                "spade-three-required",
                keyed_reply(&templates::SPADE_THREE_REQUIRED, ReplyVars::new()),
            );
        }
        let pattern = match classify_for_variant(&ranks, game.variant) {
            Some(pattern) => pattern,
            None => {
                return LandlordOutcome::public(
                    "invalid-pattern",
                    keyed_reply(&templates::INVALID_PATTERN, ReplyVars::new()),
                );
            }
        };
        if game.variant == CardGameVariant::HunanRunFast
            && matches!(pattern, PlayPattern::Single(_))
            && let Some(largest) = report_one_largest_single(game, player_index)
            && !matches!(pattern, PlayPattern::Single(rank) if rank == largest.rank)
        {
            return LandlordOutcome::public(
                "must-play-largest-single",
                keyed_reply(&templates::MUST_PLAY_LARGEST_SINGLE, ReplyVars::new()),
            );
        }
        if let Some(last) = &game.last_play
            && last.player != player_index
            && !pattern.beats(&last.pattern)
        {
            return LandlordOutcome::public(
                "cannot-beat",
                keyed_reply(
                    &templates::CANNOT_BEAT,
                    ReplyVars::new()
                        .with("牌型", pattern.label())
                        .with("上一手牌型", last.pattern.label()),
                ),
            );
        }
        let outcome = play_cards(
            game,
            player_index,
            cards,
            pattern,
            now,
            ActionOrigin::Manual,
        );
        if outcome.ended {
            self.state = GameState::Idle;
        }
        outcome
    }

    fn pass(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        let GameState::Playing(game) = &mut self.state else {
            return LandlordOutcome::public(
                "no-game",
                keyed_reply(&templates::NO_ACTIVE_GAME, ReplyVars::new()),
            );
        };
        let Some(player_index) = find_player(&game.players, player) else {
            return LandlordOutcome::public(
                "not-player",
                keyed_reply(
                    &templates::NOT_IN_GAME,
                    ReplyVars::new().with("玩法", game.variant.label()),
                ),
            );
        };
        if player_index != game.current {
            return LandlordOutcome::public(
                "wrong-turn",
                keyed_reply(
                    &templates::WRONG_TURN,
                    ReplyVars::new().with("玩家", game.players[game.current].name.clone()),
                ),
            );
        }
        if game
            .last_play
            .as_ref()
            .is_none_or(|last| last.player == player_index)
        {
            return LandlordOutcome::public(
                "cannot-pass",
                keyed_reply(&templates::LEADER_CANNOT_PASS, ReplyVars::new()),
            );
        }
        if game.variant == CardGameVariant::HunanRunFast {
            let previous = &game
                .last_play
                .as_ref()
                .expect("pass requires previous play")
                .pattern;
            if lowest_beating_play_for_variant(
                &game.players[player_index].hand,
                previous,
                game.variant,
            )
            .is_some()
            {
                return LandlordOutcome::public(
                    "must-play",
                    keyed_reply(&templates::MUST_PLAY_RUN_FAST, ReplyVars::new()),
                );
            }
        }
        pass_playing(game, player_index, now, ActionOrigin::Manual)
    }

    fn hand(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        let (players, label) = match &mut self.state {
            GameState::Bidding(game) => (&mut game.players, CardGameVariant::Landlord.label()),
            GameState::Playing(game) => (&mut game.players, game.variant.label()),
            GameState::Idle | GameState::Lobby(_) => {
                return LandlordOutcome::private(
                    "no-game",
                    keyed_reply(&templates::HAND_NO_DEALT_GAME, ReplyVars::new()),
                );
            }
        };
        let Some(index) = find_player(players, player) else {
            return LandlordOutcome::private(
                "not-player",
                keyed_reply(
                    &templates::NOT_IN_GAME,
                    ReplyVars::new().with("玩法", label),
                ),
            );
        };
        let cooldown = Duration::from_secs(self.config.hand_cooldown_seconds);
        if players[index]
            .last_hand_reply
            .is_some_and(|last| now.saturating_duration_since(last) < cooldown)
        {
            return LandlordOutcome {
                action: "hand-cooldown",
                public_reply: None,
                public_reply_template: None,
                private_reply: None,
                private_reply_template: None,
                private_deliveries: Vec::new(),
                ended: false,
            };
        }
        players[index].last_hand_reply = Some(now);
        LandlordOutcome::private(
            "hand",
            keyed_reply(
                &templates::HAND_CURRENT,
                ReplyVars::new()
                    .with("张数", players[index].hand.len().to_string())
                    .with("手牌", format_hand(&players[index].hand)),
            ),
        )
    }

    fn status(&self) -> LandlordOutcome {
        match &self.state {
            GameState::Idle => LandlordOutcome::public(
                "idle",
                keyed_reply(&templates::STATUS_IDLE, ReplyVars::new()),
            ),
            GameState::Lobby(lobby) => LandlordOutcome::public(
                "lobby-status",
                keyed_reply(
                    &templates::STATUS_LOBBY,
                    ReplyVars::new()
                        .with("玩法", lobby.variant.label())
                        .with(
                            "名单",
                            lobby
                                .players
                                .iter()
                                .map(|player| player.name.as_str())
                                .collect::<Vec<_>>()
                                .join("、"),
                        )
                        .with(
                            "人数",
                            (3usize.saturating_sub(lobby.players.len())).to_string(),
                        ),
                ),
            ),
            GameState::Bidding(game) => LandlordOutcome::public(
                "bidding-status",
                keyed_reply(
                    &templates::STATUS_BIDDING,
                    ReplyVars::new()
                        .with("玩家", game.players[game.current].name.clone())
                        .with(
                            "当前抢地主",
                            game.candidate
                                .map(|index| game.players[index].name.as_str())
                                .unwrap_or("暂无"),
                        ),
                ),
            ),
            GameState::Playing(game) => {
                let last = game.last_play.as_ref().map_or_else(
                    || "无".to_string(),
                    |last| {
                        format!(
                            "{}:{}[{}]",
                            game.players[last.player].name,
                            format_play(&last.cards),
                            last.pattern.label()
                        )
                    },
                );
                let remaining = game
                    .players
                    .iter()
                    .map(|player| {
                        reply(
                            &templates::PLAYER_CARD_COUNT,
                            ReplyVars::new()
                                .with("玩家", player.name.as_str())
                                .with("张数", player.hand.len().to_string()),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("、");
                let message = if game.variant == CardGameVariant::HunanRunFast {
                    reply(
                        &templates::STATUS_PLAYING_RUN_FAST,
                        ReplyVars::new()
                            .with("玩家", game.players[game.current].name.clone())
                            .with("剩余", remaining)
                            .with("上一手", last),
                    )
                } else {
                    reply(
                        &templates::STATUS_PLAYING_LANDLORD,
                        ReplyVars::new()
                            .with("地主", game.players[game.landlord].name.clone())
                            .with("玩家", game.players[game.current].name.clone())
                            .with("剩余", remaining)
                            .with("上一手", last),
                    )
                };
                LandlordOutcome::public("playing-status", message)
            }
        }
    }

    fn exit(&mut self, player: &str) -> LandlordOutcome {
        match &mut self.state {
            GameState::Idle => LandlordOutcome::public(
                "idle",
                keyed_reply(&templates::STATUS_IDLE, ReplyVars::new()),
            ),
            GameState::Lobby(lobby) => {
                let Some(index) = find_player(&lobby.players, player) else {
                    return LandlordOutcome::public(
                        "not-player",
                        keyed_reply(
                            &templates::NOT_IN_LOBBY,
                            ReplyVars::new().with("玩法", lobby.variant.label()),
                        ),
                    );
                };
                let name = lobby.players[index].name.clone();
                let variant = lobby.variant;
                if index == 0 {
                    self.state = GameState::Idle;
                    LandlordOutcome::ended(
                        "lobby-canceled",
                        keyed_reply(
                            &templates::LOBBY_CANCELED,
                            ReplyVars::new()
                                .with("玩家", name)
                                .with("玩法", variant.label()),
                        ),
                    )
                } else {
                    lobby.players.remove(index);
                    LandlordOutcome::public(
                        "left-lobby",
                        keyed_reply(
                            &templates::LOBBY_LEFT,
                            ReplyVars::new()
                                .with("玩家", name)
                                .with("玩法", variant.label()),
                        ),
                    )
                }
            }
            GameState::Bidding(game) => {
                let Some(index) = find_player(&game.players, player) else {
                    return LandlordOutcome::public(
                        "not-player",
                        keyed_reply(&templates::NOT_IN_LANDLORD_GAME, ReplyVars::new()),
                    );
                };
                let name = game.players[index].name.clone();
                self.state = GameState::Idle;
                LandlordOutcome::ended(
                    "game-aborted",
                    keyed_reply(
                        &templates::EXIT_LANDLORD_ENDED,
                        ReplyVars::new().with("玩家", name),
                    ),
                )
            }
            GameState::Playing(game) => {
                let Some(index) = find_player(&game.players, player) else {
                    return LandlordOutcome::public(
                        "not-player",
                        keyed_reply(
                            &templates::NOT_IN_GAME,
                            ReplyVars::new().with("玩法", game.variant.label()),
                        ),
                    );
                };
                let name = game.players[index].name.clone();
                let label = game.variant.label();
                self.state = GameState::Idle;
                LandlordOutcome::ended(
                    "game-aborted",
                    keyed_reply(
                        &templates::EXIT_GAME_ENDED,
                        ReplyVars::new().with("玩家", name).with("玩法", label),
                    ),
                )
            }
        }
    }
}

/// Preserve the key and variables while retaining the built-in rendered text.
fn reply(spec: &crate::features::reply_template::ReplyTemplate, vars: ReplyVars) -> String {
    crate::features::reply_template::render(spec, None, &vars).text
}

fn keyed_reply(
    spec: &'static crate::features::reply_template::ReplyTemplate,
    vars: ReplyVars,
) -> CardGameReplyTemplate {
    CardGameReplyTemplate::Template { spec, vars }
}

impl From<String> for CardGameReplyTemplate {
    fn from(text: String) -> Self {
        Self::Raw(text)
    }
}

impl From<&str> for CardGameReplyTemplate {
    fn from(text: &str) -> Self {
        Self::Raw(text.to_string())
    }
}

impl CardGameReplyTemplate {
    pub fn render_default(&self) -> String {
        match self {
            Self::Template { spec, vars } => {
                crate::features::reply_template::render(spec, None, vars).text
            }
            Self::Raw(text) => text.clone(),
        }
    }
}

fn deadline_after(now: Instant, elapsed: Duration, target: Duration) -> Instant {
    now + target.saturating_sub(elapsed)
}

fn report_one_largest_single(game: &Playing, player: usize) -> Option<Card> {
    if game.variant != CardGameVariant::HunanRunFast {
        return None;
    }
    let next = (player + 1) % game.players.len();
    (game.players[next].hand.len() == 1)
        .then(|| {
            game.players[player]
                .hand
                .iter()
                .max_by_key(|card| card.rank)
                .copied()
        })
        .flatten()
}

fn forced_run_fast_action(game: &Playing) -> Option<ForcedRunFastAction> {
    if game.variant != CardGameVariant::HunanRunFast {
        return None;
    }
    let current = game.current;
    let previous = game
        .last_play
        .as_ref()
        .filter(|last| last.player != current)
        .map(|last| &last.pattern);
    let filter = RunFastPlayFilter {
        previous,
        requires_spade_three: game.opening_spade_three_required,
        required_single: report_one_largest_single(game, current).map(|card| card.rank),
    };
    let hand = &game.players[current].hand;
    let groups = rank_counts(&hand.iter().map(|card| card.rank).collect::<Vec<_>>())
        .into_iter()
        .collect::<Vec<_>>();
    let mut selected = Vec::new();
    let mut options = Vec::with_capacity(2);
    collect_run_fast_legal_plays(&groups, 0, &mut selected, &filter, &mut options);
    match options.as_slice() {
        [] if previous.is_some() => Some(ForcedRunFastAction::Pass),
        [(ranks, pattern)] => Some(ForcedRunFastAction::Play {
            cards: take_cards_for_play(hand, ranks, filter.requires_spade_three).ok()?,
            pattern: pattern.clone(),
        }),
        _ => None,
    }
}

fn play_cards(
    game: &mut Playing,
    player: usize,
    cards: Vec<Card>,
    pattern: PlayPattern,
    now: Instant,
    origin: ActionOrigin,
) -> LandlordOutcome {
    game.opening_spade_three_required = false;
    if origin == ActionOrigin::Manual {
        game.players[player].timeouts = 0;
    }
    remove_cards(&mut game.players[player].hand, &cards);
    game.turns = game.turns.saturating_add(1);
    if pattern.is_bomb() {
        game.bombs = game.bombs.saturating_add(1);
    }
    let name = game.players[player].name.clone();
    let role = if player == game.landlord {
        "地主"
    } else {
        "农民"
    };
    let played = format_play(&cards);
    let pattern_name = pattern.label();
    if game.players[player].hand.is_empty() {
        if game.variant == CardGameVariant::HunanRunFast {
            let remaining = game
                .players
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != player)
                .map(|(_, item)| item.hand.len())
                .collect::<Vec<_>>();
            let action_text = match origin {
                ActionOrigin::Manual => "出",
                ActionOrigin::Timeout if game.players[player].trustee => "托管出",
                ActionOrigin::Timeout => "超时出",
                ActionOrigin::Forced => "自动出",
            };
            return LandlordOutcome::ended(
                "won",
                keyed_reply(
                    &templates::WON_RUN_FAST,
                    ReplyVars::new()
                        .with("玩家", name)
                        .with("动作", action_text)
                        .with("牌面", format_play_compact(&cards))
                        .with("牌型", pattern_name)
                        .with("剩余", format!("{}张/{}张", remaining[0], remaining[1])),
                ),
            );
        }
        let winner = if player == game.landlord {
            "地主方"
        } else {
            "农民方"
        };
        let summary = game
            .players
            .iter()
            .map(|item| {
                reply(
                    &templates::PLAYER_CARD_COUNT,
                    ReplyVars::new()
                        .with("玩家", item.name.as_str())
                        .with("张数", item.hand.len().to_string()),
                )
            })
            .collect::<Vec<_>>()
            .join("、");
        return LandlordOutcome::ended(
            "won",
            keyed_reply(
                &templates::WON_LANDLORD,
                ReplyVars::new()
                    .with("玩家", name)
                    .with("角色", role)
                    .with("牌面", played)
                    .with("牌型", pattern_name)
                    .with("阵营", winner)
                    .with("剩余", summary)
                    .with("炸弹", game.bombs.to_string())
                    .with("手数", game.turns.to_string()),
            ),
        );
    }
    game.last_play = Some(LastPlay {
        player,
        cards,
        pattern,
    });
    game.consecutive_passes = 0;
    advance_turn(game, now);
    let message = if game.variant == CardGameVariant::HunanRunFast {
        let action_text = match origin {
            ActionOrigin::Manual => "出",
            ActionOrigin::Timeout if game.players[player].trustee => "托管出",
            ActionOrigin::Timeout => "超时出",
            ActionOrigin::Forced => "自动出",
        };
        reply(
            &templates::PLAY_RUN_FAST,
            ReplyVars::new()
                .with("玩家", name)
                .with("动作", action_text)
                .with(
                    "牌面",
                    format_play_compact(&game.last_play.as_ref().expect("play was recorded").cards),
                )
                .with("牌型", pattern_name)
                .with("剩余", game.players[player].hand.len().to_string())
                .with("下一位", game.players[game.current].name.clone()),
        )
    } else {
        let action_text = match origin {
            ActionOrigin::Manual => reply(&templates::LANDLORD_ACTION_PLAY, ReplyVars::new()),
            ActionOrigin::Timeout => reply(
                &templates::LANDLORD_ACTION_TIMEOUT,
                ReplyVars::new().with("托管后缀", trustee_suffix(game, player)),
            ),
            ActionOrigin::Forced => unreachable!("forced play only applies to run fast"),
        };
        reply(
            &templates::PLAY_LANDLORD,
            ReplyVars::new()
                .with("玩家", name)
                .with("角色", role)
                .with("动作", action_text)
                .with("牌面", played)
                .with("牌型", pattern_name)
                .with("剩余", game.players[player].hand.len().to_string())
                .with("下一位", game.players[game.current].name.clone()),
        )
    };
    LandlordOutcome::public(
        match origin {
            ActionOrigin::Manual => "played",
            ActionOrigin::Timeout => "auto-play",
            ActionOrigin::Forced => "forced-play",
        },
        message,
    )
}

fn pass_playing(
    game: &mut Playing,
    player: usize,
    now: Instant,
    origin: ActionOrigin,
) -> LandlordOutcome {
    if origin == ActionOrigin::Manual {
        game.players[player].timeouts = 0;
    }
    let name = game.players[player].name.clone();
    game.turns = game.turns.saturating_add(1);
    game.consecutive_passes = game.consecutive_passes.saturating_add(1);
    let new_trick = game.consecutive_passes >= 2;
    if new_trick {
        let leader = game
            .last_play
            .as_ref()
            .expect("pass requires last play")
            .player;
        game.current = leader;
        game.last_play = None;
        game.consecutive_passes = 0;
        game.timer.reset(now);
        game.warning_sent = false;
    } else {
        advance_turn(game, now);
    }
    let next = &game.players[game.current].name;
    let message = if game.variant == CardGameVariant::HunanRunFast {
        // "双方无牌可压，{下一位}领出" 不出现玩家名，其余分支都带玩家名。
        let spec = match (origin, new_trick) {
            (ActionOrigin::Manual, false) => &templates::PASS_MANUAL_NEXT,
            (ActionOrigin::Manual, true) => &templates::PASS_MANUAL_LEAD,
            (ActionOrigin::Timeout, false) => &templates::PASS_TIMEOUT_NEXT,
            (ActionOrigin::Timeout, true) => &templates::PASS_TIMEOUT_LEAD,
            (ActionOrigin::Forced, false) => &templates::PASS_FORCED_NEXT,
            (ActionOrigin::Forced, true) => &templates::PASS_FORCED_LEAD,
        };
        let vars = if origin == ActionOrigin::Forced && new_trick {
            ReplyVars::new().with("下一位", next.clone())
        } else {
            ReplyVars::new()
                .with("玩家", name)
                .with("下一位", next.clone())
        };
        reply(spec, vars)
    } else if new_trick {
        reply(
            &templates::PASS_NEW_TRICK,
            ReplyVars::new().with("玩家", name).with("下一位", next),
        )
    } else {
        reply(
            &templates::PASS_NEXT,
            ReplyVars::new().with("玩家", name).with("下一位", next),
        )
    };
    LandlordOutcome::public(
        match origin {
            ActionOrigin::Manual => "passed",
            ActionOrigin::Timeout => "auto-pass",
            ActionOrigin::Forced => "forced-pass",
        },
        message,
    )
}

fn advance_turn(game: &mut Playing, now: Instant) {
    game.current = (game.current + 1) % game.players.len();
    game.timer.reset(now);
    game.warning_sent = false;
}

fn trustee_suffix(game: &Playing, player: usize) -> &'static str {
    if game.players[player].trustee {
        "并进入托管"
    } else {
        ""
    }
}

fn find_player(players: &[Player], name: &str) -> Option<usize> {
    let key = player_key(name);
    players.iter().position(|player| player.key == key)
}

fn player_key(name: &str) -> String {
    // 只做去首尾空白 + 小写,保留标点:避免 "小明!" 与 "小明" 碰撞
    // (否则可代他人出牌/抢地主/收到私聊手牌)。
    name.trim().chars().flat_map(char::to_lowercase).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]

enum Rank {
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
    const NORMAL: [Rank; 13] = [
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

    fn label(self) -> &'static str {
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

    fn sequence_value(self) -> Option<u8> {
        (self <= Self::Ace).then_some(self as u8)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Card {
    rank: Rank,
    spade_three: bool,
}

impl Card {
    fn new(rank: Rank) -> Self {
        Self {
            rank,
            spade_three: false,
        }
    }
}

fn rank_from_value(value: u8) -> Option<Rank> {
    Rank::NORMAL
        .iter()
        .copied()
        .find(|rank| *rank as u8 == value)
}

fn random_seed() -> u64 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (time.as_nanos() as u64) ^ (std::process::id() as u64).rotate_left(17)
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
    }

    fn index(&mut self, len: usize) -> usize {
        (self.next() % len.max(1) as u64) as usize
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            values.swap(index, self.index(index + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::features::chat_text::{MAX_CHAT_WIDTH, display_width};
    use crate::features::entertainment::{AcquireOutcome, EntertainmentKind, EntertainmentState};
    use miliastra_kernel::clock::{Clock, ManualClock};

    struct TestCardGameService {
        inner: CardGameService,
        entertainment: EntertainmentState,
    }

    impl TestCardGameService {
        fn new(config: LandlordConfig) -> Self {
            Self {
                inner: CardGameService::new(config),
                entertainment: EntertainmentState::new(),
            }
        }

        fn begin_command(
            &mut self,
            player: &str,
            command: &LandlordCommand,
            now: Instant,
        ) -> Result<CardGameCommandStart> {
            self.inner
                .begin_command(&mut self.entertainment, player, command, now)
        }

        fn resume(
            &mut self,
            key: CardGameEffectKey,
            result: CardGameEffectResult,
        ) -> Result<CardGameResume> {
            self.inner.resume(&mut self.entertainment, key, result)
        }

        fn cancel(&mut self, key: CardGameEffectKey) -> Result<CardGameCancel> {
            self.inner.cancel(&mut self.entertainment, key)
        }

        fn abort(&mut self) -> Result<bool> {
            self.inner.abort(&mut self.entertainment)
        }

        fn prepare_start(&mut self, command: &LandlordCommand) -> Result<CardGameStartGate> {
            self.inner.prepare_start(&mut self.entertainment, command)
        }

        fn cancel_start(&mut self, reservation: CardGameStartReservation) -> Result<bool> {
            self.inner
                .cancel_start(&mut self.entertainment, reservation)
        }

        fn complete_start(
            &mut self,
            player: &str,
            command: &LandlordCommand,
            reservation: CardGameStartReservation,
            now: Instant,
        ) -> Result<LandlordOutcome> {
            self.inner
                .complete_start(&mut self.entertainment, player, command, reservation, now)
        }

        fn handle(
            &mut self,
            player: &str,
            command: &LandlordCommand,
            now: Instant,
        ) -> Result<LandlordOutcome> {
            self.inner
                .handle(&mut self.entertainment, player, command, now)
        }

        fn tick(
            &mut self,
            now: Instant,
            clock_active: bool,
        ) -> Result<Option<CardGameTimedOutcome>> {
            self.inner.tick(&mut self.entertainment, now, clock_active)
        }

        fn begin_delivery_outcome(
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

    struct FakeCardGamePort {
        verified: bool,
        friend_sent: bool,
        hall_messages: StdMutex<Vec<String>>,
        private_templates: StdMutex<Vec<(String, String, String)>>,
    }

    impl FakeCardGamePort {
        fn new(verified: bool, friend_sent: bool) -> Self {
            Self {
                verified,
                friend_sent,
                hall_messages: StdMutex::new(Vec::new()),
                private_templates: StdMutex::new(Vec::new()),
            }
        }

        fn hall_messages(&self) -> Vec<String> {
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

    struct NeverCalledCardGamePort;

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

    fn suspended(start: CardGameCommandStart) -> CardGameEffectRequest {
        match start {
            CardGameCommandStart::Suspended(request) => request,
            CardGameCommandStart::Completed(CardGameCompletion { action }) => {
                panic!("expected a suspended effect, completed as {action}")
            }
        }
    }

    trait CardGameServiceTestDriver {
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

    fn drive_effects_for_test(
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
                    anyhow::bail!(
                        "card game command was cancelled before its effect chain completed"
                    );
                }
                return Ok(());
            }
            let result = match request.effect {
                CardGameEffect::FriendVerify { player, message } => {
                    CardGameEffectResult::FriendVerify(port.verify_friend(&player, &message))
                }
                CardGameEffect::PrivateDelivery {
                    player,
                    message,
                    template,
                } => match template {
                    Some(template) => CardGameEffectResult::PrivateDeliveryTemplate(
                        port.send_friend_template(&player, &message, &template),
                    ),
                    None => {
                        CardGameEffectResult::PrivateDelivery(port.send_friend(&player, &message))
                    }
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
            CardGameEffect::FriendVerify { ref player, .. } if player == "甲"
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
                CardGameEffectResult::PrivateBatchTemplates(Ok(
                    CardGameFriendBatchOutcome::Failed {
                        retryable,
                        failure: FriendBatchFailure::new(
                            FriendBatchFailureKind::ConfirmedUnsent,
                            "second hand was not sent",
                        ),
                    },
                )),
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
            CardGameEffect::PrivateDelivery { ref player, ref template, .. }
                if player == "甲" && matches!(template, Some(CardGameReplyTemplate::Template { spec, .. }) if spec.key == "card_games.hand.current")
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
            CardGameEffect::PrivateDelivery { ref player, ref template, .. }
                if player == "甲" && matches!(template, Some(CardGameReplyTemplate::Template { spec, .. }) if spec.key == "card_games.hand.current")
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
                CardGameEffectResult::PrivateBatchTemplates(Ok(
                    CardGameFriendBatchOutcome::Complete,
                )),
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
                .resume_claimed_for_test(
                    stale_status.key,
                    CardGameEffectResult::HallDelivery(Ok(())),
                )
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
        for (command, expected_action) in [
            (LandlordCommand::Decline, "redealt"),
            (LandlordCommand::Rob, "landlord-selected"),
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
            for _ in 0..3 {
                let accepted = ["甲", "乙", "丙"]
                    .into_iter()
                    .map(|player| service.handle(player, &command, now).unwrap())
                    .find(|outcome| outcome.action != "wrong-bidder")
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
                CardGameEffectResult::PrivateBatchTemplates(Ok(
                    CardGameFriendBatchOutcome::Complete,
                )),
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
                CardGameEffectResult::PrivateBatchTemplates(Ok(
                    CardGameFriendBatchOutcome::Complete,
                )),
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

    fn ranks(text: &str) -> Vec<Rank> {
        parse_cards(text).unwrap()
    }

    fn finish_bidding_with_all_players_robbing(game: &mut LandlordGame, now: Instant) {
        for _ in 0..3 {
            let GameState::Bidding(bidding) = &game.state else {
                panic!("expected bidding")
            };
            let player = bidding.players[bidding.current].name.clone();
            game.handle(&player, &LandlordCommand::Rob, now);
        }
        assert!(matches!(game.state, GameState::Playing(_)));
    }

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
    fn last_player_to_rob_becomes_landlord_and_leads() {
        let now = Instant::now();
        let mut game = LandlordGame::with_seed(LandlordConfig::default(), 29);
        game.create("甲", now);
        game.join("乙", now);
        game.join("丙", now);

        let mut last_robber = String::new();
        for rob in [true, false, true] {
            let GameState::Bidding(bidding) = &game.state else {
                panic!("expected bidding")
            };
            let player = bidding.players[bidding.current].name.clone();
            if rob {
                last_robber = player.clone();
            }
            game.handle(
                &player,
                if rob {
                    &LandlordCommand::Rob
                } else {
                    &LandlordCommand::Decline
                },
                now,
            );
        }

        let GameState::Playing(playing) = &game.state else {
            panic!("expected playing")
        };
        assert_eq!(playing.players[playing.landlord].name, last_robber);
        assert_eq!(playing.current, playing.landlord);
        assert_eq!(playing.players[playing.landlord].hand.len(), 20);
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
        assert_eq!(outcome.action, "declined");
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
}
