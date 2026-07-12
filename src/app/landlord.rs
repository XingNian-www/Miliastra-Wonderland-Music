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
    Join,
    Status,
    Play(String),
    Pass,
    Hand,
    Exit,
    Help,
}

impl LandlordCommand {
    pub fn parse(args: &str) -> Self {
        match args.trim() {
            "开始" | "创建" => Self::Start,
            "状态" | "查看" => Self::Status,
            "退出" | "结束" | "取消" => Self::Exit,
            "帮助" | "?" | "？" | "" => Self::Help,
            _ => Self::Help,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandlordOutcome {
    pub action: &'static str,
    pub public_reply: Option<String>,
    pub private_reply: Option<String>,
    pub ended: bool,
}

impl LandlordOutcome {
    fn public(action: &'static str, reply: impl Into<String>) -> Self {
        Self {
            action,
            public_reply: Some(reply.into()),
            private_reply: None,
            ended: false,
        }
    }

    fn private(action: &'static str, reply: impl Into<String>) -> Self {
        Self {
            action,
            public_reply: None,
            private_reply: Some(reply.into()),
            ended: false,
        }
    }

    fn ended(action: &'static str, reply: impl Into<String>) -> Self {
        Self {
            action,
            public_reply: Some(reply.into()),
            private_reply: None,
            ended: true,
        }
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
    Playing(Playing),
}

struct Lobby {
    players: Vec<Player>,
    timer: ActiveTimer,
}

struct Playing {
    players: Vec<Player>,
    landlord: usize,
    current: usize,
    last_play: Option<LastPlay>,
    consecutive_passes: u8,
    timer: ActiveTimer,
    warning_sent: bool,
    turns: u32,
    bombs: u32,
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
        if let GameState::Playing(game) = &mut self.state
            && let Some(index) = find_player(&game.players, player)
        {
            game.players[index].last_hand_reply = None;
        }
    }

    pub fn create(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        if !self.config.enabled {
            return LandlordOutcome::public("disabled", "斗地主未启用");
        }
        if self.is_active() {
            return LandlordOutcome::public("already-active", "已有斗地主房间或牌局进行中");
        }
        self.state = GameState::Lobby(Lobby {
            players: vec![Player::new(player)],
            timer: ActiveTimer::new(now),
        });
        LandlordOutcome::public(
            "created",
            format!(
                "{}创建了斗地主房间，还需2人，发送 @加入 参加",
                player.trim()
            ),
        )
    }

    pub fn join(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        let GameState::Lobby(lobby) = &mut self.state else {
            return LandlordOutcome::public("no-lobby", "当前没有等待加入的斗地主房间");
        };
        let key = player_key(player);
        if lobby.players.iter().any(|item| item.key == key) {
            return LandlordOutcome::public("duplicate-player", "你已经加入本局斗地主");
        }
        lobby.players.push(Player::new(player));
        lobby.timer.reset(now);
        if lobby.players.len() < 3 {
            return LandlordOutcome::public(
                "joined",
                format!("{}加入斗地主，还需1人", player.trim()),
            );
        }

        let mut players = std::mem::take(&mut lobby.players);
        let (hands, landlord) = deal(&mut self.rng);
        for (player, hand) in players.iter_mut().zip(hands) {
            player.hand = hand;
        }
        let landlord_name = players[landlord].name.clone();
        self.state = GameState::Playing(Playing {
            players,
            landlord,
            current: landlord,
            last_play: None,
            consecutive_passes: 0,
            timer: ActiveTimer::new(now),
            warning_sent: false,
            turns: 0,
            bombs: 0,
        });
        LandlordOutcome::public(
            "started",
            format!(
                "斗地主开始，{}持有黑桃3成为地主并先出牌。请好友私聊 @手牌 查看手牌",
                landlord_name
            ),
        )
    }

    pub fn handle(
        &mut self,
        player: &str,
        command: &LandlordCommand,
        now: Instant,
    ) -> LandlordOutcome {
        match command {
            LandlordCommand::Start => self.create(player, now),
            LandlordCommand::Join => self.join(player, now),
            LandlordCommand::Status => self.status(),
            LandlordCommand::Play(cards) => self.play(player, cards, now),
            LandlordCommand::Pass => self.pass(player, now),
            LandlordCommand::Hand => self.hand(player, now),
            LandlordCommand::Exit => self.exit(player),
            LandlordCommand::Help => LandlordOutcome::public(
                "help",
                "斗地主: @斗地主 开始、@加入、@出 牌组/$牌组/＄牌组、@过、@斗地主 状态/退出；好友私聊 @手牌",
            ),
        }
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
                self.state = GameState::Idle;
                Some(LandlordOutcome::ended(
                    "lobby-timeout",
                    "斗地主组局等待超时，房间已取消",
                ))
            }
            GameState::Playing(game) => {
                game.timer.tick(now, clock_active);
                if !clock_active {
                    return None;
                }
                let current = game.current;
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
            let previous = &game
                .last_play
                .as_ref()
                .expect("can pass has last play")
                .pattern;
            if let Some((cards, pattern)) =
                lowest_beating_play(&game.players[current].hand, previous)
            {
                let outcome = play_cards(game, current, cards, pattern, now, true);
                if outcome.ended {
                    self.state = GameState::Idle;
                }
                outcome
            } else {
                pass_playing(game, current, now, true);
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
            let card = game.players[current]
                .hand
                .first()
                .copied()
                .expect("active player has cards");
            let outcome = play_cards(
                game,
                current,
                vec![card],
                PlayPattern::Single(card.rank),
                now,
                true,
            );
            if outcome.ended {
                self.state = GameState::Idle;
            }
            outcome
        }
    }

    fn play(&mut self, player: &str, text: &str, now: Instant) -> LandlordOutcome {
        let GameState::Playing(game) = &mut self.state else {
            return LandlordOutcome::public("no-game", "当前没有进行中的斗地主牌局");
        };
        let Some(player_index) = find_player(&game.players, player) else {
            return LandlordOutcome::public("not-player", "你不在本局斗地主中");
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
        let cards = match take_cards(&game.players[player_index].hand, &ranks) {
            Ok(cards) => cards,
            Err(error) => return LandlordOutcome::public("missing-card", error),
        };
        let pattern = match classify(&ranks) {
            Some(pattern) => pattern,
            None => return LandlordOutcome::public("invalid-pattern", "这组牌不是有效斗地主牌型"),
        };
        if let Some(last) = &game.last_play
            && last.player != player_index
            && !pattern.beats(&last.pattern)
        {
            return LandlordOutcome::public(
                "cannot-beat",
                format!("{}压不过上一手{}", pattern.label(), last.pattern.label()),
            );
        }
        let outcome = play_cards(game, player_index, cards, pattern, now, false);
        if outcome.ended {
            self.state = GameState::Idle;
        }
        outcome
    }

    fn pass(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        let GameState::Playing(game) = &mut self.state else {
            return LandlordOutcome::public("no-game", "当前没有进行中的斗地主牌局");
        };
        let Some(player_index) = find_player(&game.players, player) else {
            return LandlordOutcome::public("not-player", "你不在本局斗地主中");
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
        pass_playing(game, player_index, now, false)
    }

    fn hand(&mut self, player: &str, now: Instant) -> LandlordOutcome {
        let GameState::Playing(game) = &mut self.state else {
            return LandlordOutcome::private("no-game", "当前没有进行中的斗地主牌局");
        };
        let Some(index) = find_player(&game.players, player) else {
            return LandlordOutcome::private("not-player", "你不在本局斗地主中");
        };
        let cooldown = Duration::from_secs(self.config.hand_cooldown_seconds);
        if game.players[index]
            .last_hand_reply
            .is_some_and(|last| now.saturating_duration_since(last) < cooldown)
        {
            return LandlordOutcome {
                action: "hand-cooldown",
                public_reply: None,
                private_reply: None,
                ended: false,
            };
        }
        game.players[index].last_hand_reply = Some(now);
        LandlordOutcome::private(
            "hand",
            format!(
                "当前手牌({}张): {}",
                game.players[index].hand.len(),
                format_hand(&game.players[index].hand)
            ),
        )
    }

    fn status(&self) -> LandlordOutcome {
        match &self.state {
            GameState::Idle => LandlordOutcome::public("idle", "当前没有斗地主房间或牌局"),
            GameState::Lobby(lobby) => LandlordOutcome::public(
                "lobby-status",
                format!(
                    "斗地主等待加入: {}，还需{}人",
                    lobby
                        .players
                        .iter()
                        .map(|player| player.name.as_str())
                        .collect::<Vec<_>>()
                        .join("、"),
                    3usize.saturating_sub(lobby.players.len())
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
                LandlordOutcome::public(
                    "playing-status",
                    format!(
                        "地主:{}；轮到:{}；剩余:{}；上一手:{}",
                        game.players[game.landlord].name,
                        game.players[game.current].name,
                        game.players
                            .iter()
                            .map(|player| format!("{} {}张", player.name, player.hand.len()))
                            .collect::<Vec<_>>()
                            .join("、"),
                        last
                    ),
                )
            }
        }
    }

    fn exit(&mut self, player: &str) -> LandlordOutcome {
        match &mut self.state {
            GameState::Idle => LandlordOutcome::public("idle", "当前没有斗地主房间或牌局"),
            GameState::Lobby(lobby) => {
                let Some(index) = find_player(&lobby.players, player) else {
                    return LandlordOutcome::public("not-player", "你不在当前斗地主房间中");
                };
                let name = lobby.players[index].name.clone();
                if index == 0 {
                    self.state = GameState::Idle;
                    LandlordOutcome::ended("lobby-canceled", format!("{}取消了斗地主房间", name))
                } else {
                    lobby.players.remove(index);
                    LandlordOutcome::public("left-lobby", format!("{}退出了斗地主房间", name))
                }
            }
            GameState::Playing(game) => {
                let Some(index) = find_player(&game.players, player) else {
                    return LandlordOutcome::public("not-player", "你不在本局斗地主中");
                };
                let name = game.players[index].name.clone();
                self.state = GameState::Idle;
                LandlordOutcome::ended("game-aborted", format!("{}退出，斗地主牌局已结束", name))
            }
        }
    }
}

fn play_cards(
    game: &mut Playing,
    player: usize,
    cards: Vec<Card>,
    pattern: PlayPattern,
    now: Instant,
    automatic: bool,
) -> LandlordOutcome {
    if !automatic {
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
    let automatic_text = if automatic {
        format!("超时{}，自动出", trustee_suffix(game, player))
    } else {
        "出牌".to_string()
    };
    LandlordOutcome::public(
        if automatic { "auto-play" } else { "played" },
        format!(
            "{}({}){} {}[{}]，剩余{}张；轮到{}",
            name,
            role,
            automatic_text,
            played,
            pattern_name,
            game.players[player].hand.len(),
            game.players[game.current].name
        ),
    )
}

fn pass_playing(
    game: &mut Playing,
    player: usize,
    now: Instant,
    automatic: bool,
) -> LandlordOutcome {
    if !automatic {
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
    LandlordOutcome::public(
        if automatic { "auto-pass" } else { "passed" },
        if new_trick {
            format!(
                "{}过牌，重新由{}领出",
                name, game.players[game.current].name
            )
        } else {
            format!("{}过牌，轮到{}", name, game.players[game.current].name)
        },
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

fn deal(rng: &mut SplitMix64) -> ([Vec<Card>; 3], usize) {
    let landlord = rng.index(3);
    let spade_three = Card {
        rank: Rank::Three,
        spade_three: true,
    };
    let mut deck = Vec::with_capacity(53);
    for rank in Rank::NORMAL {
        let copies = if rank == Rank::Three { 3 } else { 4 };
        for _ in 0..copies {
            deck.push(Card::new(rank));
        }
    }
    deck.push(Card::new(Rank::SmallJoker));
    deck.push(Card::new(Rank::BigJoker));
    rng.shuffle(&mut deck);

    let mut hands: [Vec<Card>; 3] = std::array::from_fn(|_| Vec::with_capacity(20));
    hands[landlord].push(spade_three);
    let mut seat = (landlord + 1) % 3;
    while hands.iter().any(|hand| hand.len() < 17) {
        if hands[seat].len() < 17 {
            hands[seat].push(deck.pop().expect("deck has enough cards"));
        }
        seat = (seat + 1) % 3;
    }
    while let Some(card) = deck.pop() {
        hands[landlord].push(card);
    }
    for hand in &mut hands {
        sort_hand(hand);
    }
    (hands, landlord)
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

fn format_play(cards: &[Card]) -> String {
    let mut ranks = cards.iter().map(|card| card.rank).collect::<Vec<_>>();
    ranks.sort_unstable();
    ranks
        .into_iter()
        .map(|rank| rank.label())
        .collect::<Vec<_>>()
        .join(" ")
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

fn lowest_beating_play(hand: &[Card], previous: &PlayPattern) -> Option<(Vec<Card>, PlayPattern)> {
    let counts = rank_counts(&hand.iter().map(|card| card.rank).collect::<Vec<_>>());
    let groups = counts.into_iter().collect::<Vec<_>>();
    let mut selected = Vec::new();
    let mut best: Option<(Vec<Rank>, PlayPattern)> = None;
    collect_beating_plays(&groups, 0, &mut selected, previous, &mut best);
    let (ranks, pattern) = best?;
    let cards = take_cards(hand, &ranks).ok()?;
    Some((cards, pattern))
}

fn collect_beating_plays(
    groups: &[(Rank, usize)],
    index: usize,
    selected: &mut Vec<Rank>,
    previous: &PlayPattern,
    best: &mut Option<(Vec<Rank>, PlayPattern)>,
) {
    if index == groups.len() {
        let Some(pattern) = classify(selected) else {
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
        collect_beating_plays(groups, index + 1, selected, previous, best);
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

    fn ranks(text: &str) -> Vec<Rank> {
        parse_cards(text).unwrap()
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
    fn dealing_never_places_spade_three_in_bottom_cards() {
        for seed in 0..100 {
            let mut rng = SplitMix64::new(seed);
            let (hands, landlord) = deal(&mut rng);
            assert_eq!(hands[landlord].len(), 20);
            assert_eq!(hands.iter().filter(|hand| hand.len() == 17).count(), 2);
            assert!(hands[landlord].iter().any(|card| card.spade_three));
            assert_eq!(
                hands
                    .iter()
                    .flatten()
                    .filter(|card| card.spade_three)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn three_players_start_and_landlord_leads() {
        let now = Instant::now();
        let mut game = LandlordGame::with_seed(LandlordConfig::default(), 7);
        assert_eq!(game.create("甲", now).action, "created");
        assert_eq!(game.join("乙", now).action, "joined");
        let outcome = game.join("丙", now);
        assert_eq!(outcome.action, "started");
        let GameState::Playing(playing) = &game.state else {
            panic!("expected playing")
        };
        assert_eq!(playing.current, playing.landlord);
        assert!(
            playing.players[playing.landlord]
                .hand
                .iter()
                .any(|card| card.spade_three)
        );
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
        assert!(game.tick(now + Duration::from_secs(100), false).is_none());
        assert!(
            game.tick(now + Duration::from_secs(189), true)
                .is_some_and(|item| item.action == "turn-warning")
        );
        assert!(game.tick(now + Duration::from_secs(190), true).is_some());
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
            landlord: 0,
            current: 0,
            last_play: None,
            consecutive_passes: 0,
            timer: ActiveTimer::new(now),
            warning_sent: false,
            turns: 0,
            bombs: 0,
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
