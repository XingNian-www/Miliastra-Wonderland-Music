use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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
    Exit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandlordOutcome {
    pub action: &'static str,
    pub public_reply: Option<String>,
    pub private_reply: Option<String>,
    pub private_deliveries: Vec<LandlordPrivateDelivery>,
    pub ended: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandlordPrivateDelivery {
    pub player: String,
    pub message: String,
}

impl LandlordOutcome {
    fn public(action: &'static str, reply: impl Into<String>) -> Self {
        Self {
            action,
            public_reply: Some(reply.into()),
            private_reply: None,
            private_deliveries: Vec::new(),
            ended: false,
        }
    }

    fn private(action: &'static str, reply: impl Into<String>) -> Self {
        Self {
            action,
            public_reply: None,
            private_reply: Some(reply.into()),
            private_deliveries: Vec::new(),
            ended: false,
        }
    }

    fn ended(action: &'static str, reply: impl Into<String>) -> Self {
        Self {
            action,
            public_reply: Some(reply.into()),
            private_reply: None,
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
            return LandlordOutcome::public("disabled", format!("{}未启用", variant.label()));
        }
        if self.is_active() {
            return LandlordOutcome::public("already-active", "已有牌局或房间进行中");
        }
        self.state = GameState::Lobby(Lobby {
            players: vec![Player::new(player)],
            variant,
            timer: ActiveTimer::new(now),
        });
        LandlordOutcome::public(
            "created",
            format!(
                "{}创建了{}房间，还需2人，发送 #加入 参加",
                player.trim(),
                variant.label()
            ),
        )
    }

    pub fn join(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        let GameState::Lobby(lobby) = &mut self.state else {
            return LandlordOutcome::public("no-lobby", "当前没有等待加入的牌局房间");
        };
        let key = player_key(player);
        if lobby.players.iter().any(|item| item.key == key) {
            return LandlordOutcome::public(
                "duplicate-player",
                format!("你已经加入本局{}", lobby.variant.label()),
            );
        }
        lobby.players.push(Player::new(player));
        lobby.timer.reset(now);
        if lobby.players.len() < 3 {
            return LandlordOutcome::public(
                "joined",
                format!("{}加入{}，还需1人", player.trim(), lobby.variant.label()),
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
                format!(
                    "跑得快已发手牌，{}持有黑桃3并先出；第一手必须包含黑桃3",
                    first_name
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
            format!(
                "斗地主已发手牌，随机由{}开始抢地主；发送 #抢 或 #不抢",
                first_name
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
            LandlordCommand::Exit => self.exit(player),
        }
    }

    fn bid(&mut self, player: &str, rob: bool, now: Instant) -> LandlordOutcome {
        let GameState::Bidding(bidding) = &mut self.state else {
            return LandlordOutcome::public("not-bidding", "当前不在抢地主阶段");
        };
        let Some(index) = find_player(&bidding.players, player) else {
            return LandlordOutcome::public("not-player", "你不在本局斗地主中");
        };
        if index != bidding.current {
            return LandlordOutcome::public(
                "wrong-bidder",
                format!("现在轮到{}抢地主", bidding.players[bidding.current].name),
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
                format!(
                    "{}{}；轮到{}抢地主",
                    name,
                    if rob { "抢地主" } else { "不抢" },
                    next
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
                format!("三人均未抢地主，已重新发牌；随机由{}开始", first_name),
            )
            .with_private_deliveries(deliveries);
        };

        let bottom_text = format_play(&bidding.bottom);
        bidding.players[landlord].hand.append(&mut bidding.bottom);
        sort_hand(&mut bidding.players[landlord].hand);
        let landlord_name = bidding.players[landlord].name.clone();
        let landlord_delivery = LandlordPrivateDelivery {
            player: landlord_name.clone(),
            message: format!(
                "地主手牌({}张): {}",
                bidding.players[landlord].hand.len(),
                format_hand(&bidding.players[landlord].hand)
            ),
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
            format!("{}成为地主并先出牌；底牌: {}", landlord_name, bottom_text),
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
                    format!("{}组局等待超时，房间已取消", label),
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
                        format!("{}抢地主剩余30秒", game.players[game.current].name),
                    ));
                }
                if game.timer.elapsed < timeout {
                    return None;
                }
                let player = game.players[game.current].name.clone();
                let mut outcome = self.bid(&player, false, now);
                if let Some(reply) = outcome.public_reply.take() {
                    outcome.public_reply =
                        Some(format!("{}抢地主超时，按不抢处理；{}", player, reply));
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
                        format!("{}出牌剩余30秒", game.players[current].name),
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
                    format!("{}超时且无牌可压，自动过牌；轮到{}", name, next),
                );
            }
            if !game.players[current].trustee {
                pass_playing(game, current, now, ActionOrigin::Timeout);
                let next = game.players[game.current].name.clone();
                return LandlordOutcome::public(
                    "auto-pass",
                    format!("{}超时，自动过牌；轮到{}", name, next),
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
                    format!(
                        "{}超时{}，自动过牌；轮到{}",
                        name,
                        trustee_suffix(game, current),
                        next
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
            return LandlordOutcome::public("no-game", "当前没有进行中的牌局");
        };
        let Some(player_index) = find_player(&game.players, player) else {
            return LandlordOutcome::public(
                "not-player",
                format!("你不在本局{}中", game.variant.label()),
            );
        };
        if player_index != game.current {
            return LandlordOutcome::public(
                "wrong-turn",
                format!("现在轮到{}出牌", game.players[game.current].name),
            );
        }
        let ranks = match parse_cards(text) {
            Ok(ranks) if !ranks.is_empty() => ranks,
            Ok(_) => return LandlordOutcome::public("empty-play", "请输入要出的牌"),
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
            return LandlordOutcome::public("spade-three-required", "首手必须包含黑桃3");
        }
        let pattern = match classify_for_variant(&ranks, game.variant) {
            Some(pattern) => pattern,
            None => {
                return LandlordOutcome::public("invalid-pattern", "这组牌不是当前玩法的有效牌型");
            }
        };
        if game.variant == CardGameVariant::HunanRunFast
            && matches!(pattern, PlayPattern::Single(_))
            && let Some(largest) = report_one_largest_single(game, player_index)
            && !matches!(pattern, PlayPattern::Single(rank) if rank == largest.rank)
        {
            return LandlordOutcome::public(
                "must-play-largest-single",
                "下家已报单，出单张时必须出手中最大牌",
            );
        }
        if let Some(last) = &game.last_play
            && last.player != player_index
            && !pattern.beats(&last.pattern)
        {
            return LandlordOutcome::public(
                "cannot-beat",
                format!("{}压不过上一手{}", pattern.label(), last.pattern.label()),
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
            return LandlordOutcome::public("no-game", "当前没有进行中的牌局");
        };
        let Some(player_index) = find_player(&game.players, player) else {
            return LandlordOutcome::public(
                "not-player",
                format!("你不在本局{}中", game.variant.label()),
            );
        };
        if player_index != game.current {
            return LandlordOutcome::public(
                "wrong-turn",
                format!("现在轮到{}出牌", game.players[game.current].name),
            );
        }
        if game
            .last_play
            .as_ref()
            .is_none_or(|last| last.player == player_index)
        {
            return LandlordOutcome::public("cannot-pass", "你是本轮领出者，不能过牌");
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
                return LandlordOutcome::public("must-play", "跑得快有牌能压时不能过牌");
            }
        }
        pass_playing(game, player_index, now, ActionOrigin::Manual)
    }

    fn hand(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        let (players, label) = match &mut self.state {
            GameState::Bidding(game) => (&mut game.players, CardGameVariant::Landlord.label()),
            GameState::Playing(game) => (&mut game.players, game.variant.label()),
            GameState::Idle | GameState::Lobby(_) => {
                return LandlordOutcome::private("no-game", "当前没有已发牌的牌局");
            }
        };
        let Some(index) = find_player(players, player) else {
            return LandlordOutcome::private("not-player", format!("你不在本局{}中", label));
        };
        let cooldown = Duration::from_secs(self.config.hand_cooldown_seconds);
        if players[index]
            .last_hand_reply
            .is_some_and(|last| now.saturating_duration_since(last) < cooldown)
        {
            return LandlordOutcome {
                action: "hand-cooldown",
                public_reply: None,
                private_reply: None,
                private_deliveries: Vec::new(),
                ended: false,
            };
        }
        players[index].last_hand_reply = Some(now);
        LandlordOutcome::private(
            "hand",
            format!(
                "当前手牌({}张): {}",
                players[index].hand.len(),
                format_hand(&players[index].hand)
            ),
        )
    }

    fn status(&self) -> LandlordOutcome {
        match &self.state {
            GameState::Idle => LandlordOutcome::public("idle", "当前没有牌局房间或进行中的牌局"),
            GameState::Lobby(lobby) => LandlordOutcome::public(
                "lobby-status",
                format!(
                    "{}等待加入: {}，还需{}人",
                    lobby.variant.label(),
                    lobby
                        .players
                        .iter()
                        .map(|player| player.name.as_str())
                        .collect::<Vec<_>>()
                        .join("、"),
                    3usize.saturating_sub(lobby.players.len())
                ),
            ),
            GameState::Bidding(game) => LandlordOutcome::public(
                "bidding-status",
                format!(
                    "斗地主抢地主中；轮到:{}；当前抢地主:{}",
                    game.players[game.current].name,
                    game.candidate
                        .map(|index| game.players[index].name.as_str())
                        .unwrap_or("暂无")
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
                    .map(|player| format!("{} {}张", player.name, player.hand.len()))
                    .collect::<Vec<_>>()
                    .join("、");
                let reply = if game.variant == CardGameVariant::HunanRunFast {
                    format!(
                        "跑得快；轮到:{}；剩余:{}；上一手:{}",
                        game.players[game.current].name, remaining, last
                    )
                } else {
                    format!(
                        "地主:{}；轮到:{}；剩余:{}；上一手:{}",
                        game.players[game.landlord].name,
                        game.players[game.current].name,
                        remaining,
                        last
                    )
                };
                LandlordOutcome::public("playing-status", reply)
            }
        }
    }

    fn exit(&mut self, player: &str) -> LandlordOutcome {
        match &mut self.state {
            GameState::Idle => LandlordOutcome::public("idle", "当前没有牌局房间或进行中的牌局"),
            GameState::Lobby(lobby) => {
                let Some(index) = find_player(&lobby.players, player) else {
                    return LandlordOutcome::public(
                        "not-player",
                        format!("你不在当前{}房间中", lobby.variant.label()),
                    );
                };
                let name = lobby.players[index].name.clone();
                let variant = lobby.variant;
                if index == 0 {
                    self.state = GameState::Idle;
                    LandlordOutcome::ended(
                        "lobby-canceled",
                        format!("{}取消了{}房间", name, variant.label()),
                    )
                } else {
                    lobby.players.remove(index);
                    LandlordOutcome::public(
                        "left-lobby",
                        format!("{}退出了{}房间", name, variant.label()),
                    )
                }
            }
            GameState::Bidding(game) => {
                let Some(index) = find_player(&game.players, player) else {
                    return LandlordOutcome::public("not-player", "你不在本局斗地主中");
                };
                let name = game.players[index].name.clone();
                self.state = GameState::Idle;
                LandlordOutcome::ended("game-aborted", format!("{}退出，斗地主牌局已结束", name))
            }
            GameState::Playing(game) => {
                let Some(index) = find_player(&game.players, player) else {
                    return LandlordOutcome::public(
                        "not-player",
                        format!("你不在本局{}中", game.variant.label()),
                    );
                };
                let name = game.players[index].name.clone();
                let label = game.variant.label();
                self.state = GameState::Idle;
                LandlordOutcome::ended("game-aborted", format!("{}退出，{}牌局已结束", name, label))
            }
        }
    }
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
                format!(
                    "{}{}:{}[{}]，获胜；其余:{}张/{}张",
                    name,
                    action_text,
                    format_play_compact(&cards),
                    pattern_name,
                    remaining[0],
                    remaining[1]
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
            .map(|item| format!("{} {}张", item.name, item.hand.len()))
            .collect::<Vec<_>>()
            .join("、");
        return LandlordOutcome::ended(
            "won",
            format!(
                "{}({})出完{}[{}]，{}获胜。剩余:{}；炸弹{}次；共{}手",
                name, role, played, pattern_name, winner, summary, game.bombs, game.turns
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
        format!(
            "{}{}:{}[{}] 余{}，轮到{}",
            name,
            action_text,
            format_play_compact(&game.last_play.as_ref().expect("play was recorded").cards),
            pattern_name,
            game.players[player].hand.len(),
            game.players[game.current].name
        )
    } else {
        let action_text = match origin {
            ActionOrigin::Manual => "出牌".to_string(),
            ActionOrigin::Timeout => format!("超时{}，自动出", trustee_suffix(game, player)),
            ActionOrigin::Forced => unreachable!("forced play only applies to run fast"),
        };
        format!(
            "{}({}){} {}[{}]，剩余{}张；轮到{}",
            name,
            role,
            action_text,
            played,
            pattern_name,
            game.players[player].hand.len(),
            game.players[game.current].name
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
        match (origin, new_trick) {
            (ActionOrigin::Manual, false) => format!("{}过，轮到{}", name, next),
            (ActionOrigin::Manual, true) => format!("{}过，{}领出", name, next),
            (ActionOrigin::Timeout, false) => format!("{}超时过，轮到{}", name, next),
            (ActionOrigin::Timeout, true) => format!("{}超时过，{}领出", name, next),
            (ActionOrigin::Forced, false) => format!("{}无牌可压，轮到{}", name, next),
            (ActionOrigin::Forced, true) => format!("双方无牌可压，{}领出", next),
        }
    } else if new_trick {
        format!("{}过牌，重新由{}领出", name, next)
    } else {
        format!("{}过牌，轮到{}", name, next)
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
    name.chars()
        .filter(|ch| !ch.is_whitespace() && !ch.is_ascii_punctuation())
        .flat_map(char::to_lowercase)
        .collect()
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

fn deal(rng: &mut SplitMix64) -> ([Vec<Card>; 3], Vec<Card>) {
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

fn sort_hand(hand: &mut [Card]) {
    hand.sort_by_key(|card| (card.rank, card.spade_three));
}

fn format_hand(hand: &[Card]) -> String {
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

fn initial_hand_deliveries(
    players: &[Player],
    variant: CardGameVariant,
) -> Vec<LandlordPrivateDelivery> {
    players
        .iter()
        .map(|player| LandlordPrivateDelivery {
            player: player.name.clone(),
            message: format!(
                "{}初始手牌({}张): {}",
                variant.label(),
                player.hand.len(),
                format_hand(&player.hand)
            ),
        })
        .collect()
}

fn deal_hunan_run_fast(rng: &mut SplitMix64) -> ([Vec<Card>; 3], usize) {
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

fn format_play(cards: &[Card]) -> String {
    let mut ranks = cards.iter().map(|card| card.rank).collect::<Vec<_>>();
    ranks.sort_unstable();
    ranks
        .into_iter()
        .map(|rank| rank.label())
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_play_compact(cards: &[Card]) -> String {
    let mut ranks = cards.iter().map(|card| card.rank).collect::<Vec<_>>();
    ranks.sort_unstable();
    ranks
        .into_iter()
        .map(|rank| rank.label())
        .collect::<Vec<_>>()
        .join("")
}

fn parse_cards(text: &str) -> Result<Vec<Rank>, String> {
    let compact = text
        .chars()
        .filter(|ch| !ch.is_whitespace() && !matches!(ch, ',' | '，' | '、'))
        .collect::<String>();
    let mut rest = compact.as_str();
    let mut ranks = Vec::new();
    while !rest.is_empty() {
        let (rank, consumed) = if rest.starts_with("小王") {
            (Rank::SmallJoker, "小王".len())
        } else if rest.starts_with("大王") {
            (Rank::BigJoker, "大王".len())
        } else if rest.starts_with("10") {
            (Rank::Ten, 2)
        } else {
            let ch = rest.chars().next().expect("rest is not empty");
            let rank = match ch.to_ascii_uppercase() {
                '3' => Rank::Three,
                '4' => Rank::Four,
                '5' => Rank::Five,
                '6' => Rank::Six,
                '7' => Rank::Seven,
                '8' => Rank::Eight,
                '9' => Rank::Nine,
                'J' => Rank::Jack,
                'Q' => Rank::Queen,
                'K' => Rank::King,
                'A' => Rank::Ace,
                '2' => Rank::Two,
                _ => return Err(format!("无法识别牌面: {}", ch)),
            };
            (rank, ch.len_utf8())
        };
        ranks.push(rank);
        rest = &rest[consumed..];
    }
    ranks.sort_unstable();
    Ok(ranks)
}

fn take_cards(hand: &[Card], ranks: &[Rank]) -> Result<Vec<Card>, String> {
    let mut available = hand.to_vec();
    let mut cards = Vec::with_capacity(ranks.len());
    for rank in ranks {
        let index = available
            .iter()
            .position(|card| card.rank == *rank && !card.spade_three)
            .or_else(|| available.iter().position(|card| card.rank == *rank))
            .ok_or_else(|| format!("手牌中缺少 {}", rank.label()))?;
        cards.push(available.remove(index));
    }
    Ok(cards)
}

fn take_cards_for_play(
    hand: &[Card],
    ranks: &[Rank],
    requires_spade_three: bool,
) -> Result<Vec<Card>, String> {
    let mut cards = take_cards(hand, ranks)?;
    if requires_spade_three
        && ranks.contains(&Rank::Three)
        && !cards.iter().any(|card| card.spade_three)
        && let Some(spade_three) = hand.iter().find(|card| card.spade_three).copied()
        && let Some(normal_three) = cards.iter_mut().find(|card| card.rank == Rank::Three)
    {
        *normal_three = spade_three;
    }
    Ok(cards)
}

fn remove_cards(hand: &mut Vec<Card>, cards: &[Card]) {
    for card in cards {
        let index = hand
            .iter()
            .position(|candidate| candidate == card)
            .expect("validated card exists in hand");
        hand.remove(index);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PlayPattern {
    Single(Rank),
    Pair(Rank),
    Triple(Rank),
    TripleSingle(Rank),
    TriplePair(Rank),
    Straight { high: Rank, length: usize },
    PairStraight { high: Rank, length: usize },
    Airplane { high: Rank, triples: usize },
    AirplaneSingles { high: Rank, triples: usize },
    AirplanePairs { high: Rank, triples: usize },
    FourTwoSingles(Rank),
    FourTwoPairs(Rank),
    Bomb(Rank),
    JokerBomb,
}

impl PlayPattern {
    fn label(&self) -> &'static str {
        match self {
            Self::Single(_) => "单张",
            Self::Pair(_) => "对子",
            Self::Triple(_) => "三张",
            Self::TripleSingle(_) => "三带一",
            Self::TriplePair(_) => "三带二",
            Self::Straight { .. } => "顺子",
            Self::PairStraight { .. } => "连对",
            Self::Airplane { .. } => "飞机",
            Self::AirplaneSingles { .. } => "飞机带单",
            Self::AirplanePairs { .. } => "飞机带对",
            Self::FourTwoSingles(_) => "四带二单",
            Self::FourTwoPairs(_) => "四带二对",
            Self::Bomb(_) => "炸弹",
            Self::JokerBomb => "王炸",
        }
    }

    fn is_bomb(&self) -> bool {
        matches!(self, Self::Bomb(_) | Self::JokerBomb)
    }

    fn strength(&self) -> u8 {
        match self {
            Self::Single(rank)
            | Self::Pair(rank)
            | Self::Triple(rank)
            | Self::TripleSingle(rank)
            | Self::TriplePair(rank)
            | Self::FourTwoSingles(rank)
            | Self::FourTwoPairs(rank)
            | Self::Bomb(rank) => *rank as u8,
            Self::Straight { high, .. }
            | Self::PairStraight { high, .. }
            | Self::Airplane { high, .. }
            | Self::AirplaneSingles { high, .. }
            | Self::AirplanePairs { high, .. } => *high as u8,
            Self::JokerBomb => u8::MAX,
        }
    }

    fn beats(&self, previous: &Self) -> bool {
        match (self, previous) {
            (Self::JokerBomb, _) => !matches!(previous, Self::JokerBomb),
            (_, Self::JokerBomb) => false,
            (Self::Bomb(left), Self::Bomb(right)) => left > right,
            (Self::Bomb(_), _) => true,
            (_, Self::Bomb(_)) => false,
            (Self::Single(left), Self::Single(right))
            | (Self::Pair(left), Self::Pair(right))
            | (Self::Triple(left), Self::Triple(right))
            | (Self::TripleSingle(left), Self::TripleSingle(right))
            | (Self::TriplePair(left), Self::TriplePair(right))
            | (Self::FourTwoSingles(left), Self::FourTwoSingles(right))
            | (Self::FourTwoPairs(left), Self::FourTwoPairs(right)) => left > right,
            (
                Self::Straight {
                    high: left,
                    length: left_len,
                },
                Self::Straight {
                    high: right,
                    length: right_len,
                },
            )
            | (
                Self::PairStraight {
                    high: left,
                    length: left_len,
                },
                Self::PairStraight {
                    high: right,
                    length: right_len,
                },
            ) => left_len == right_len && left > right,
            (
                Self::Airplane {
                    high: left,
                    triples: left_len,
                },
                Self::Airplane {
                    high: right,
                    triples: right_len,
                },
            )
            | (
                Self::AirplaneSingles {
                    high: left,
                    triples: left_len,
                },
                Self::AirplaneSingles {
                    high: right,
                    triples: right_len,
                },
            )
            | (
                Self::AirplanePairs {
                    high: left,
                    triples: left_len,
                },
                Self::AirplanePairs {
                    high: right,
                    triples: right_len,
                },
            ) => left_len == right_len && left > right,
            _ => false,
        }
    }
}

fn classify(ranks: &[Rank]) -> Option<PlayPattern> {
    let counts = rank_counts(ranks);
    let len = ranks.len();
    if len == 1 {
        return Some(PlayPattern::Single(ranks[0]));
    }
    if len == 2 {
        if counts.len() == 2
            && counts.contains_key(&Rank::SmallJoker)
            && counts.contains_key(&Rank::BigJoker)
        {
            return Some(PlayPattern::JokerBomb);
        }
        return single_group(&counts, 2).map(PlayPattern::Pair);
    }
    if len == 3 {
        return single_group(&counts, 3).map(PlayPattern::Triple);
    }
    if len == 4 {
        if let Some(rank) = single_group(&counts, 4) {
            return Some(PlayPattern::Bomb(rank));
        }
        if let Some(rank) = group_rank(&counts, 3) {
            return Some(PlayPattern::TripleSingle(rank));
        }
    }
    if len == 5
        && let Some(rank) = group_rank(&counts, 3)
        && counts.values().any(|count| *count == 2)
    {
        return Some(PlayPattern::TriplePair(rank));
    }
    if len >= 5 && counts.values().all(|count| *count == 1) && consecutive(&counts, 1) {
        return Some(PlayPattern::Straight {
            high: *counts.keys().next_back()?,
            length: counts.len(),
        });
    }
    if len >= 6
        && len.is_multiple_of(2)
        && counts.values().all(|count| *count == 2)
        && consecutive(&counts, 2)
    {
        return Some(PlayPattern::PairStraight {
            high: *counts.keys().next_back()?,
            length: counts.len(),
        });
    }
    if len == 6
        && let Some(rank) = group_rank(&counts, 4)
    {
        return Some(PlayPattern::FourTwoSingles(rank));
    }
    if len == 8
        && let Some(rank) = group_rank(&counts, 4)
    {
        let pairs = counts
            .iter()
            .filter(|(candidate, count)| **candidate != rank && **count == 2)
            .count();
        if pairs == 2 {
            return Some(PlayPattern::FourTwoPairs(rank));
        }
    }
    classify_airplane(&counts, len)
}

fn classify_for_variant(ranks: &[Rank], variant: CardGameVariant) -> Option<PlayPattern> {
    if variant == CardGameVariant::HunanRunFast {
        let counts = rank_counts(ranks);
        if ranks.len() == 4 && counts.values().all(|count| *count == 2) && consecutive(&counts, 2) {
            return Some(PlayPattern::PairStraight {
                high: *counts.keys().next_back()?,
                length: counts.len(),
            });
        }
    }
    classify(ranks)
}

fn lowest_beating_play(hand: &[Card], previous: &PlayPattern) -> Option<(Vec<Card>, PlayPattern)> {
    lowest_beating_play_for_variant(hand, previous, CardGameVariant::Landlord)
}

fn lowest_beating_play_for_variant(
    hand: &[Card],
    previous: &PlayPattern,
    variant: CardGameVariant,
) -> Option<(Vec<Card>, PlayPattern)> {
    let counts = rank_counts(&hand.iter().map(|card| card.rank).collect::<Vec<_>>());
    let groups = counts.into_iter().collect::<Vec<_>>();
    let mut selected = Vec::new();
    let mut best: Option<(Vec<Rank>, PlayPattern)> = None;
    collect_beating_plays(&groups, 0, &mut selected, previous, variant, &mut best);
    let (ranks, pattern) = best?;
    let cards = take_cards(hand, &ranks).ok()?;
    Some((cards, pattern))
}

struct RunFastPlayFilter<'a> {
    previous: Option<&'a PlayPattern>,
    requires_spade_three: bool,
    required_single: Option<Rank>,
}

fn collect_run_fast_legal_plays(
    groups: &[(Rank, usize)],
    index: usize,
    selected: &mut Vec<Rank>,
    filter: &RunFastPlayFilter<'_>,
    options: &mut Vec<(Vec<Rank>, PlayPattern)>,
) {
    if options.len() >= 2 {
        return;
    }
    if index == groups.len() {
        let Some(pattern) = classify_for_variant(selected, CardGameVariant::HunanRunFast) else {
            return;
        };
        if filter.requires_spade_three && !selected.contains(&Rank::Three) {
            return;
        }
        if let Some(previous) = filter.previous
            && !pattern.beats(previous)
        {
            return;
        }
        if let (PlayPattern::Single(rank), Some(required)) = (&pattern, filter.required_single)
            && *rank != required
        {
            return;
        }
        options.push((selected.clone(), pattern));
        return;
    }

    let (rank, count) = groups[index];
    for take in 0..=count {
        selected.extend(std::iter::repeat_n(rank, take));
        collect_run_fast_legal_plays(groups, index + 1, selected, filter, options);
        selected.truncate(selected.len() - take);
        if options.len() >= 2 {
            break;
        }
    }
}

fn collect_beating_plays(
    groups: &[(Rank, usize)],
    index: usize,
    selected: &mut Vec<Rank>,
    previous: &PlayPattern,
    variant: CardGameVariant,
    best: &mut Option<(Vec<Rank>, PlayPattern)>,
) {
    if index == groups.len() {
        let Some(pattern) = classify_for_variant(selected, variant) else {
            return;
        };
        if !pattern.beats(previous) {
            return;
        }
        let score = (
            u8::from(pattern.is_bomb()),
            pattern.strength(),
            selected.len(),
        );
        let replace = best.as_ref().is_none_or(|(current_ranks, current)| {
            score
                < (
                    u8::from(current.is_bomb()),
                    current.strength(),
                    current_ranks.len(),
                )
        });
        if replace {
            *best = Some((selected.clone(), pattern));
        }
        return;
    }

    let (rank, count) = groups[index];
    for take in 0..=count {
        selected.extend(std::iter::repeat_n(rank, take));
        collect_beating_plays(groups, index + 1, selected, previous, variant, best);
        selected.truncate(selected.len() - take);
    }
}

fn classify_airplane(counts: &BTreeMap<Rank, usize>, len: usize) -> Option<PlayPattern> {
    for (unit, kind) in [(3usize, 0u8), (4, 1), (5, 2)] {
        if !len.is_multiple_of(unit) {
            continue;
        }
        let triples = len / unit;
        if triples < 2 {
            continue;
        }
        for start in Rank::Three as u8..=Rank::Ace as u8 {
            let end = start + triples as u8 - 1;
            if end > Rank::Ace as u8 {
                break;
            }
            let body = (start..=end)
                .map(rank_from_value)
                .collect::<Option<Vec<_>>>()?;
            if !body.iter().all(|rank| counts.get(rank).copied() == Some(3)) {
                continue;
            }
            let wings = counts
                .iter()
                .filter(|(rank, _)| !body.contains(rank))
                .map(|(rank, count)| (*rank, *count))
                .collect::<Vec<_>>();
            let valid = match kind {
                0 => wings.is_empty(),
                1 => {
                    wings.iter().map(|(_, count)| count).sum::<usize>() == triples
                        && wings.iter().all(|(_, count)| *count <= 2)
                }
                2 => wings.len() == triples && wings.iter().all(|(_, count)| *count == 2),
                _ => unreachable!(),
            };
            if valid {
                let high = *body.last()?;
                return Some(match kind {
                    0 => PlayPattern::Airplane { high, triples },
                    1 => PlayPattern::AirplaneSingles { high, triples },
                    2 => PlayPattern::AirplanePairs { high, triples },
                    _ => unreachable!(),
                });
            }
        }
    }
    None
}

fn rank_counts(ranks: &[Rank]) -> BTreeMap<Rank, usize> {
    let mut counts = BTreeMap::new();
    for rank in ranks {
        *counts.entry(*rank).or_insert(0) += 1;
    }
    counts
}

fn single_group(counts: &BTreeMap<Rank, usize>, size: usize) -> Option<Rank> {
    (counts.len() == 1)
        .then(|| counts.iter().next())
        .flatten()
        .filter(|(_, count)| **count == size)
        .map(|(rank, _)| *rank)
}

fn group_rank(counts: &BTreeMap<Rank, usize>, size: usize) -> Option<Rank> {
    counts
        .iter()
        .find(|(_, count)| **count == size)
        .map(|(rank, _)| *rank)
}

fn consecutive(counts: &BTreeMap<Rank, usize>, expected_count: usize) -> bool {
    if counts.values().any(|count| *count != expected_count) {
        return false;
    }
    let Some(first) = counts.keys().next().and_then(|rank| rank.sequence_value()) else {
        return false;
    };
    counts
        .keys()
        .enumerate()
        .all(|(index, rank)| rank.sequence_value() == Some(first.saturating_add(index as u8)))
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
    use super::*;
    use crate::features::chat_text::{MAX_CHAT_WIDTH, display_width};

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
        let now = Instant::now();
        let config = LandlordConfig {
            turn_timeout_seconds: 90,
            ..LandlordConfig::default()
        };
        let mut game = LandlordGame::with_seed(config, 13);
        game.create("甲", now);
        game.join("乙", now);
        game.join("丙", now);
        finish_bidding_with_all_players_robbing(&mut game, now);
        assert!(game.tick(now + Duration::from_secs(100), false).is_none());
        assert!(
            game.tick(now + Duration::from_secs(189), true)
                .is_some_and(|item| item.action == "turn-warning")
        );
        assert!(game.tick(now + Duration::from_secs(190), true).is_some());
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
