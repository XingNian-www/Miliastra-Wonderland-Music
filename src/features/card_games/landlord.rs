//! 斗地主/跑得快对局状态机。
//!
//! 从 `card_games.rs` 拆出：坐庄叫分、发底牌、出牌判据、超时托管都在这里。
//! 规则判定（牌型/压制）在 `rules.rs`，发牌与展示在 `dealing.rs`。

use std::time::{Duration, Instant};

use crate::features::reply_template::{ReplyVars, card_games as templates};

use super::cards::{Card, UNKNOWN_PLAYER_DISPLAY, find_player, player_key};
use super::commands::LandlordCommand;
use super::config::LandlordConfig;
use super::deadline::CardGameDeadlineKind;
use super::dealing::{
    deal, deal_hunan_run_fast, format_hand, format_play, format_play_compact,
    initial_hand_deliveries, sort_hand,
};
use super::outcome::{CardGameReplyTemplate, LandlordOutcome, LandlordPrivateDelivery};
use super::reply_helpers::{keyed_reply, reply};
use super::rng::SplitMix64;
use super::rng::random_seed;
use super::rules::{
    PlayPattern, RunFastPlayFilter, classify_for_variant, collect_run_fast_legal_plays,
    lowest_beating_play, lowest_beating_play_for_variant, parse_cards, rank_counts, remove_cards,
    take_cards_for_play,
};

pub struct LandlordGame {
    pub(super) config: LandlordConfig,
    pub(super) state: GameState,
    pub(super) rng: SplitMix64,
}

pub(super) enum GameState {
    Idle,
    Lobby(Lobby),
    Bidding(Bidding),
    Playing(Playing),
}

pub(super) struct Lobby {
    pub(super) players: Vec<Player>,
    pub(super) variant: CardGameVariant,
    pub(super) timer: ActiveTimer,
}

pub(super) struct Bidding {
    pub(super) players: Vec<Player>,
    pub(super) bottom: Vec<Card>,
    pub(super) phase: BiddingPhase,
    pub(super) current: usize,
    /// 本阶段已表态的人数（叫地主阶段数"不叫"，抢地主阶段数"抢/不抢"）。
    pub(super) decisions: u8,
    /// 叫地主的人，也就是暂定地主。
    pub(super) caller: Option<usize>,
    /// 当前抢到的地主候选。
    pub(super) candidate: Option<usize>,
    pub(super) timer: ActiveTimer,
    pub(super) warning_sent: bool,
}

/// 抢地主的三个阶段：叫地主 → 抢地主 → 反抢。
///
/// 传统流程：随机起手依次「叫地主/不叫」，第一个叫的人成为暂定地主；
/// 其余两家依次「抢地主/不抢」；有人抢则原叫地主者获得一次「反抢」机会。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BiddingPhase {
    Calling,
    Robbing,
    CounterRobbing,
}

impl BiddingPhase {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Calling => "叫地主",
            Self::Robbing => "抢地主",
            Self::CounterRobbing => "反抢",
        }
    }
}

/// 一次抢地主表态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BidAction {
    Call,
    Decline,
    Rob,
}

/// 抢地主的结束方式，决定收尾文案。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BiddingFinish {
    /// 无人抢，叫地主者直接当地主。
    CallerLandlord,
    /// 有人抢，最后抢到的人当地主。
    RobberLandlord,
    /// 原叫地主者反抢成功。
    CounterRobbed,
}

pub(super) struct Playing {
    pub(super) players: Vec<Player>,
    pub(super) variant: CardGameVariant,
    pub(super) landlord: usize,
    pub(super) current: usize,
    pub(super) last_play: Option<LastPlay>,
    pub(super) consecutive_passes: u8,
    pub(super) timer: ActiveTimer,
    pub(super) warning_sent: bool,
    pub(super) turns: u32,
    pub(super) bombs: u32,
    pub(super) opening_spade_three_required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CardGameVariant {
    Landlord,
    HunanRunFast,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActionOrigin {
    Manual,
    Timeout,
    Forced,
}

pub(super) enum ForcedRunFastAction {
    Pass,
    Play {
        cards: Vec<Card>,
        pattern: PlayPattern,
    },
}

impl CardGameVariant {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Landlord => "斗地主",
            Self::HunanRunFast => "跑得快",
        }
    }
}

#[derive(Clone)]
pub(super) struct Player {
    /// 面向玩家的展示名（note 优先）。**绝不可是内部 UUID 归属键**：
    /// 它会被渲染进「{玩家} 抢地主」这类文案。
    pub(super) name: String,
    /// 身份比较键。同一玩家的 `name` 可能变（改备注、换映射），`key` 保持稳定。
    pub(super) key: String,
    pub(super) hand: Vec<Card>,
    pub(super) timeouts: u32,
    pub(super) trustee: bool,
    pub(super) last_hand_reply: Option<Instant>,
}

impl Player {
    /// 仅供测试与「无身份映射」场景；生产路径用 `with_display` 分别传展示名与比较键。
    /// 两者相等会让 UUID 渲染进玩家文案——见 `uid_key_never_leaks_into_card_game_copy`。
    pub(super) fn new(name: &str) -> Self {
        Self::with_display(name, name)
    }

    /// 建玩家，但**展示名与身份键分开**。
    ///
    /// `key_source` 是内部归属键（已登记身份为 UUID），只参与比较与查找；
    /// `display` 是玩家看得懂的名字，只参与渲染。
    pub(super) fn with_display(key_source: &str, display: &str) -> Self {
        let display = display.trim();
        Self {
            name: if display.trim().is_empty() {
                // 绝不回退到内部键：已登记身份的 key_source 是 UUID，
                // 一旦回退就会把 UUID 显示给玩家。
                UNKNOWN_PLAYER_DISPLAY.to_string()
            } else {
                display.to_string()
            },
            key: player_key(key_source),
            hand: Vec::new(),
            timeouts: 0,
            trustee: false,
            last_hand_reply: None,
        }
    }
}

pub(super) struct LastPlay {
    pub(super) player: usize,
    pub(super) cards: Vec<Card>,
    pub(super) pattern: PlayPattern,
}

pub(super) struct ActiveTimer {
    pub(super) elapsed: Duration,
    pub(super) last_tick: Instant,
}

impl ActiveTimer {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            elapsed: Duration::ZERO,
            last_tick: now,
        }
    }

    pub(super) fn tick(&mut self, now: Instant, active: bool) {
        if active {
            self.elapsed = self
                .elapsed
                .saturating_add(now.saturating_duration_since(self.last_tick));
        }
        self.last_tick = now;
    }

    pub(super) fn reset(&mut self, now: Instant) {
        self.elapsed = Duration::ZERO;
        self.last_tick = now;
    }

    pub(super) fn elapsed_at(&self, now: Instant, active: bool) -> Duration {
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

    pub(super) fn with_seed(config: LandlordConfig, seed: u64) -> Self {
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
                if forced_run_fast_action(game).is_some()
                    || landlord_must_pass(game)
                    || game.players[game.current].trustee
                {
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
        self.create_variant_with_display(player, player, variant, now)
    }

    fn create_variant_with_display(
        &mut self,
        player: &str,
        player_display: &str,
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
            players: vec![Player::with_display(player, player_display)],
            variant,
            timer: ActiveTimer::new(now),
        });
        LandlordOutcome::public(
            "created",
            keyed_reply(
                &templates::LOBBY_CREATED,
                ReplyVars::new()
                    .with("玩家", player_display.trim())
                    .with("玩法", variant.label()),
            ),
        )
    }

    /// 兼容入口：展示名与身份键相同。
    pub fn create_with_display(
        &mut self,
        player: &str,
        player_display: &str,
        now: Instant,
    ) -> LandlordOutcome {
        self.create_variant_with_display(player, player_display, CardGameVariant::Landlord, now)
    }

    pub fn join(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        self.join_with_display(player, player, now)
    }

    pub fn join_with_display(
        &mut self,
        player: &str,
        player_display: &str,
        now: Instant,
    ) -> LandlordOutcome {
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
        lobby
            .players
            .push(Player::with_display(player, player_display));
        lobby.timer.reset(now);
        if lobby.players.len() < 3 {
            return LandlordOutcome::public(
                "joined",
                keyed_reply(
                    &templates::LOBBY_JOINED,
                    ReplyVars::new()
                        .with("玩家", player_display.trim())
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
            phase: BiddingPhase::Calling,
            current: first,
            decisions: 0,
            caller: None,
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
        // 未区分展示名与身份键时两者相同：保留既有签名给测试与内部调用。
        self.handle_with_display(player, player, command, now)
    }

    /// `player` 是身份键（参与比较与查找），`player_display` 是给玩家看的名字。
    ///
    /// 两者必须分开：已登记身份的 `player` 是 UUID，直接渲染会让玩家看到
    /// 一长串十六进制。
    pub fn handle_with_display(
        &mut self,
        player: &str,
        player_display: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> LandlordOutcome {
        match command {
            LandlordCommand::Start => self.create_with_display(player, player_display, now),
            LandlordCommand::RunFastStart => self.create_variant_with_display(
                player,
                player_display,
                CardGameVariant::HunanRunFast,
                now,
            ),
            LandlordCommand::Join => self.join_with_display(player, player_display, now),
            LandlordCommand::Call => self.bid(player, BidAction::Call, now),
            LandlordCommand::Rob => self.bid(player, BidAction::Rob, now),
            LandlordCommand::Decline => self.bid(player, BidAction::Decline, now),
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

    /// 抢地主一次表态。传统流程：叫地主 → 抢地主 → 反抢。
    fn bid(&mut self, player: &str, action: BidAction, now: Instant) -> LandlordOutcome {
        let GameState::Bidding(bidding) = &self.state else {
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
        self.bid_at(index, action, now)
    }

    /// 按下标表态。
    ///
    /// 超时路径必须走这里：`Player.name` 是展示名、`Player.key` 是身份键，
    /// 已登记玩家两者不同，拿展示名回灌 `bid()` 会 `find_player` 落空。
    fn bid_at(&mut self, index: usize, action: BidAction, now: Instant) -> LandlordOutcome {
        let GameState::Bidding(bidding) = &self.state else {
            return LandlordOutcome::public(
                "not-bidding",
                keyed_reply(&templates::BIDDING_NOT_BIDDING, ReplyVars::new()),
            );
        };
        if index >= bidding.players.len() {
            return LandlordOutcome::public(
                "not-player",
                keyed_reply(&templates::NOT_IN_LANDLORD_GAME, ReplyVars::new()),
            );
        }
        if index != bidding.current {
            return LandlordOutcome::public(
                "wrong-bidder",
                keyed_reply(
                    &templates::BIDDING_TURN,
                    ReplyVars::new().with("玩家", bidding.players[bidding.current].name.clone()),
                ),
            );
        }
        let allowed = match bidding.phase {
            BiddingPhase::Calling => matches!(action, BidAction::Call | BidAction::Decline),
            BiddingPhase::Robbing => matches!(action, BidAction::Rob | BidAction::Decline),
            // 反抢阶段直接复用 #抢：逻辑不变，玩家不用记新命令。
            BiddingPhase::CounterRobbing => matches!(action, BidAction::Rob | BidAction::Decline),
        };
        if !allowed {
            return LandlordOutcome::public(
                "wrong-phase",
                keyed_reply(
                    &templates::BIDDING_WRONG_PHASE,
                    ReplyVars::new().with("阶段", bidding.phase.label()),
                ),
            );
        }

        let GameState::Bidding(mut bidding) = std::mem::replace(&mut self.state, GameState::Idle)
        else {
            unreachable!("bidding state was checked")
        };
        let name = bidding.players[index].name.clone();
        match bidding.phase {
            BiddingPhase::Calling => {
                if action == BidAction::Call {
                    bidding.caller = Some(index);
                    bidding.candidate = Some(index);
                    bidding.phase = BiddingPhase::Robbing;
                    bidding.decisions = 0;
                    bidding.current = Self::next_robbing_player(&bidding, index);
                    bidding.timer.reset(now);
                    bidding.warning_sent = false;
                    let next = bidding.players[bidding.current].name.clone();
                    self.state = GameState::Bidding(bidding);
                    return LandlordOutcome::public(
                        "called",
                        keyed_reply(
                            &templates::BIDDING_CALLED,
                            ReplyVars::new().with("玩家", name).with("下一位", next),
                        ),
                    );
                }
                bidding.decisions = bidding.decisions.saturating_add(1);
                if bidding.decisions >= bidding.players.len() as u8 {
                    // 三家都不叫：流局重发。
                    return self.redeal(bidding, now);
                }
                bidding.current = (bidding.current + 1) % bidding.players.len();
                bidding.timer.reset(now);
                bidding.warning_sent = false;
                let next = bidding.players[bidding.current].name.clone();
                self.state = GameState::Bidding(bidding);
                LandlordOutcome::public(
                    "call-declined",
                    keyed_reply(
                        &templates::BIDDING_CALL_DECLINED,
                        ReplyVars::new().with("玩家", name).with("下一位", next),
                    ),
                )
            }
            BiddingPhase::Robbing => {
                if action == BidAction::Rob {
                    bidding.candidate = Some(index);
                }
                bidding.decisions = bidding.decisions.saturating_add(1);
                // 除叫地主者外，其余两家各表态一次。
                if bidding.decisions < bidding.players.len().saturating_sub(1) as u8 {
                    bidding.current = Self::next_robbing_player(&bidding, index);
                    bidding.timer.reset(now);
                    bidding.warning_sent = false;
                    let next = bidding.players[bidding.current].name.clone();
                    self.state = GameState::Bidding(bidding);
                    let spec = if action == BidAction::Rob {
                        &templates::BIDDING_ROBBED
                    } else {
                        &templates::BIDDING_ROB_DECLINED
                    };
                    return LandlordOutcome::public(
                        if action == BidAction::Rob {
                            "robbed"
                        } else {
                            "rob-declined"
                        },
                        keyed_reply(
                            spec,
                            ReplyVars::new().with("玩家", name).with("下一位", next),
                        ),
                    );
                }
                if bidding.candidate == bidding.caller {
                    // 没人抢：叫地主者直接当地主。
                    return self.finish_bidding(bidding, now, BiddingFinish::CallerLandlord);
                }
                // 有人抢：原叫地主者获得一次反抢机会。
                bidding.phase = BiddingPhase::CounterRobbing;
                bidding.decisions = 0;
                bidding.current = bidding.caller.expect("caller recorded when calling");
                bidding.timer.reset(now);
                bidding.warning_sent = false;
                let caller_name = bidding.players[bidding.current].name.clone();
                self.state = GameState::Bidding(bidding);
                LandlordOutcome::public(
                    "counter-prompt",
                    keyed_reply(
                        &templates::BIDDING_COUNTER_PROMPT,
                        ReplyVars::new()
                            .with("玩家", caller_name)
                            .with("抢地主", name),
                    ),
                )
            }
            BiddingPhase::CounterRobbing => {
                if action == BidAction::Rob {
                    bidding.candidate = bidding.caller;
                    self.finish_bidding(bidding, now, BiddingFinish::CounterRobbed)
                } else {
                    self.finish_bidding(bidding, now, BiddingFinish::RobberLandlord)
                }
            }
        }
    }

    /// 三家都不叫：重新发牌并重新开始叫地主。
    fn redeal(&mut self, mut bidding: Bidding, now: Instant) -> LandlordOutcome {
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
            phase: BiddingPhase::Calling,
            current: first,
            decisions: 0,
            caller: None,
            candidate: None,
            timer: ActiveTimer::new(now),
            warning_sent: false,
        });
        LandlordOutcome::public(
            "redealt",
            keyed_reply(
                &templates::BIDDING_REDEALT,
                ReplyVars::new().with("玩家", first_name),
            ),
        )
        .with_private_deliveries(deliveries)
    }

    /// 抢地主结束：定地主、发底牌、进入出牌阶段。
    fn finish_bidding(
        &mut self,
        mut bidding: Bidding,
        now: Instant,
        reason: BiddingFinish,
    ) -> LandlordOutcome {
        let landlord = bidding
            .candidate
            .expect("bidding always has a landlord candidate when finishing");
        let bottom_text = format_play(&bidding.bottom);
        bidding.players[landlord].hand.append(&mut bidding.bottom);
        sort_hand(&mut bidding.players[landlord].hand);
        let landlord_name = bidding.players[landlord].name.clone();
        let hand_text = format_hand(&bidding.players[landlord].hand);
        let hand_count = bidding.players[landlord].hand.len().to_string();
        let landlord_delivery = LandlordPrivateDelivery {
            player: landlord_name.clone(),
            message: reply(
                &templates::HAND_LANDLORD_INITIAL,
                ReplyVars::new()
                    .with("张数", hand_count.clone())
                    .with("手牌", hand_text.clone()),
            ),
            template: Some(keyed_reply(
                &templates::HAND_LANDLORD_INITIAL,
                ReplyVars::new()
                    .with("张数", hand_count)
                    .with("手牌", hand_text),
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
        let (action, spec) = match reason {
            BiddingFinish::CallerLandlord => {
                ("caller-landlord", &templates::BIDDING_CALLER_LANDLORD)
            }
            BiddingFinish::CounterRobbed => ("counter-robbed", &templates::BIDDING_COUNTER_ROBBED),
            BiddingFinish::RobberLandlord => ("landlord-selected", &templates::LANDLORD_SELECTED),
        };
        LandlordOutcome::public(
            action,
            keyed_reply(
                spec,
                ReplyVars::new()
                    .with("玩家", landlord_name)
                    .with("底牌", bottom_text),
            ),
        )
        .with_private_deliveries(vec![landlord_delivery])
    }

    /// 抢地主：从 from 之后找下一个表态者，跳过叫地主者。
    fn next_robbing_player(bidding: &Bidding, from: usize) -> usize {
        let len = bidding.players.len();
        let mut next = (from + 1) % len;
        if Some(next) == bidding.caller {
            next = (next + 1) % len;
        }
        next
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
                // 超时按**下标**表态：已登记玩家的 name 是展示名、key 是身份键，
                // 拿展示名回灌 bid() 会 find_player 落空；而失败分支不重置计时器，
                // 于是每次 tick 重复报错、抢地主永远推进不下去。
                let index = game.current;
                let mut outcome = self.bid_at(index, BidAction::Decline, now);
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
                // 斗地主「要不起」：轮到自己就立即跳过，不等玩家输入也不等超时。
                // 连跳在 auto_pass_landlord 里合并成一条消息。
                if landlord_must_pass(game) {
                    return Some(self.auto_pass_landlord(now));
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

    /// 斗地主「要不起」自动跳过：连续两家都压不过时合并成一条消息。
    ///
    /// 立即生效（不等超时）；合并是为了避免一次连跳刷两条消息。
    fn auto_pass_landlord(&mut self, now: Instant) -> LandlordOutcome {
        let GameState::Playing(game) = &mut self.state else {
            unreachable!("auto pass requires an active game");
        };
        let mut skipped: Vec<String> = Vec::new();
        let mut new_trick = false;
        // 最多循环一圈，防止状态异常时死循环。
        for _ in 0..game.players.len() {
            if !landlord_must_pass(game) {
                break;
            }
            let current = game.current;
            skipped.push(game.players[current].name.clone());
            pass_playing(game, current, now, ActionOrigin::Forced);
            new_trick = game.last_play.is_none();
        }
        let next = game.players[game.current].name.clone();
        let spec = if new_trick {
            &templates::PASS_FORCED_MERGED_LEAD
        } else {
            &templates::PASS_FORCED_MERGED_NEXT
        };
        LandlordOutcome::public(
            "forced-pass",
            reply(
                spec,
                ReplyVars::new()
                    .with("玩家列表", skipped.join("、"))
                    .with("下一位", next),
            ),
        )
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

    pub(super) fn play(&mut self, player: &str, text: &str, now: Instant) -> LandlordOutcome {
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

    pub(super) fn pass(&mut self, player: &str, now: Instant) -> LandlordOutcome {
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

    pub(super) fn hand(&mut self, player: &str, now: Instant) -> LandlordOutcome {
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

    pub(super) fn exit(&mut self, player: &str) -> LandlordOutcome {
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

pub(super) fn deadline_after(now: Instant, elapsed: Duration, target: Duration) -> Instant {
    now + target.saturating_sub(elapsed)
}

pub(super) fn report_one_largest_single(game: &Playing, player: usize) -> Option<Card> {
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

/// 斗地主：当前玩家是否「要不起」（手里没有任何能压过上家的牌）。
///
/// 只对斗地主生效。跑得快走 `forced_run_fast_action`，那边还会自动出唯一解，
/// 这里只处理"压不过就自动跳过"，不替玩家选牌。
pub(super) fn landlord_must_pass(game: &Playing) -> bool {
    if game.variant != CardGameVariant::Landlord {
        return false;
    }
    let current = game.current;
    let Some(previous) = game
        .last_play
        .as_ref()
        .filter(|last| last.player != current)
        .map(|last| &last.pattern)
    else {
        return false;
    };
    lowest_beating_play_for_variant(&game.players[current].hand, previous, game.variant).is_none()
}

pub(super) fn forced_run_fast_action(game: &Playing) -> Option<ForcedRunFastAction> {
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

pub(super) fn play_cards(
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
        keyed_reply(
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
            ActionOrigin::Manual => keyed_reply(&templates::LANDLORD_ACTION_PLAY, ReplyVars::new()),
            ActionOrigin::Timeout => keyed_reply(
                &templates::LANDLORD_ACTION_TIMEOUT,
                ReplyVars::new().with("托管后缀", trustee_suffix(game, player)),
            ),
            ActionOrigin::Forced => unreachable!("forced play only applies to run fast"),
        };
        CardGameReplyTemplate::Composed {
            spec: &templates::PLAY_LANDLORD,
            vars: ReplyVars::new()
                .with("玩家", name)
                .with("角色", role)
                .with("牌面", played)
                .with("牌型", pattern_name)
                .with("剩余", game.players[player].hand.len().to_string())
                .with("下一位", game.players[game.current].name.clone()),
            nested: vec![("动作", action_text)],
        }
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

pub(super) fn pass_playing(
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

pub(super) fn advance_turn(game: &mut Playing, now: Instant) {
    game.current = (game.current + 1) % game.players.len();
    game.timer.reset(now);
    game.warning_sent = false;
}

pub(super) fn trustee_suffix(game: &Playing, player: usize) -> &'static str {
    if game.players[player].trustee {
        "并进入托管"
    } else {
        ""
    }
}
