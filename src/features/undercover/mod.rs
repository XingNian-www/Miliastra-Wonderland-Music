use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub(crate) mod repository;

use super::chat_text::display_width;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UndercoverCommand {
    CreateSingle,
    CreateDouble,
    Join,
    Start,
    Status,
    Exit,
    End,
    Describe(String),
    Vote(char),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UndercoverConfig {
    pub enabled: bool,
    pub word_bank_path: PathBuf,
    pub used_state_path: PathBuf,
    pub min_players: usize,
    pub double_min_players: usize,
    pub max_players: usize,
    pub lobby_timeout_seconds: u64,
    pub phase_timeout_seconds: u64,
    pub progress_interval_seconds: u64,
    pub description_max_width: usize,
}

impl Default for UndercoverConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            word_bank_path: PathBuf::from("undercover.yaml"),
            used_state_path: PathBuf::from("data/undercover-used.yaml"),
            min_players: 4,
            double_min_players: 6,
            max_players: 11,
            lobby_timeout_seconds: 180,
            phase_timeout_seconds: 180,
            progress_interval_seconds: 20,
            description_max_width: 70,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndercoverMode {
    Single,
    Double,
}

impl UndercoverMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Single => "单卧底",
            Self::Double => "双卧底",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndercoverWordPair {
    pub civilian: String,
    pub undercover: String,
}

impl UndercoverWordPair {
    pub fn new(civilian: impl Into<String>, undercover: impl Into<String>) -> Self {
        Self {
            civilian: civilian.into(),
            undercover: undercover.into(),
        }
    }

    pub(super) fn unordered_key(&self) -> String {
        unordered_word_key(&self.civilian, &self.undercover)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndercoverDelivery {
    Hall(String),
    HallBatch(Vec<String>),
    Friend { player: String, message: String },
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndercoverSnapshot {
    pub enabled: bool,
    pub phase: &'static str,
    pub mode: Option<&'static str>,
    pub round: u32,
    pub players: Vec<UndercoverPlayerSnapshot>,
    pub completed: usize,
    pub total: usize,
    pub remaining_seconds: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndercoverPlayerSnapshot {
    pub position: Option<char>,
    pub name: String,
    pub alive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Civilian,
    Undercover,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Self::Civilian => "平民",
            Self::Undercover => "卧底",
        }
    }
}

#[derive(Clone, Debug)]
struct Player {
    name: String,
    key: String,
    position: char,
    role: Role,
    alive: bool,
}

impl Player {
    fn lobby(name: &str) -> Self {
        Self {
            name: name.trim().to_string(),
            key: player_key(name),
            position: '?',
            role: Role::Civilian,
            alive: true,
        }
    }
}

enum GameState {
    Idle,
    Lobby(Lobby),
    Playing(Playing),
}

struct Lobby {
    mode: UndercoverMode,
    creator_key: String,
    players: Vec<Player>,
    last_activity: Instant,
}

struct Playing {
    mode: UndercoverMode,
    creator_key: String,
    players: Vec<Player>,
    words: UndercoverWordPair,
    round: u32,
    last_activity: Instant,
    phase: Phase,
}

enum Phase {
    AwaitingDelivery,
    Describing {
        descriptions: BTreeMap<usize, String>,
    },
    Voting {
        votes: BTreeMap<usize, usize>,
        reminder: VoteReminder,
    },
    RunoffDescribing {
        candidates: Vec<usize>,
        descriptions: BTreeMap<usize, String>,
    },
    RunoffVoting {
        candidates: Vec<usize>,
        votes: BTreeMap<usize, usize>,
        reminder: VoteReminder,
    },
}

struct VoteReminder {
    last_announcement: Instant,
}

impl VoteReminder {
    fn new(now: Instant) -> Self {
        Self {
            last_announcement: now,
        }
    }
}

pub struct UndercoverGame {
    config: UndercoverConfig,
    state: GameState,
    rng: SplitMix64,
}

impl UndercoverGame {
    pub fn new(config: UndercoverConfig) -> Self {
        Self::with_seed(config, random_seed())
    }

    fn with_seed(config: UndercoverConfig, seed: u64) -> Self {
        Self {
            config,
            state: GameState::Idle,
            rng: SplitMix64::new(seed),
        }
    }

    pub fn create(&mut self, player: &str, mode: UndercoverMode, now: Instant) -> Result<()> {
        if !self.config.enabled {
            bail!("谁是卧底未启用");
        }
        if !matches!(self.state, GameState::Idle) {
            bail!("已有谁是卧底房间或牌局进行中");
        }
        let player = Player::lobby(player);
        self.state = GameState::Lobby(Lobby {
            mode,
            creator_key: player.key.clone(),
            players: vec![player],
            last_activity: now,
        });
        Ok(())
    }

    pub fn join(&mut self, player: &str, now: Instant) -> Result<()> {
        let GameState::Lobby(lobby) = &mut self.state else {
            bail!("当前没有等待加入的谁是卧底房间");
        };
        let key = player_key(player);
        if lobby.players.iter().any(|item| item.key == key) {
            bail!("你已经加入本局谁是卧底");
        }
        if lobby.players.len() >= self.config.max_players.clamp(4, 11) {
            bail!("谁是卧底房间已满");
        }
        lobby.players.push(Player::lobby(player));
        lobby.last_activity = now;
        Ok(())
    }

    pub fn start(
        &mut self,
        words: UndercoverWordPair,
        now: Instant,
    ) -> Result<Vec<UndercoverDelivery>> {
        let GameState::Lobby(lobby) = &mut self.state else {
            bail!("当前没有可开局的谁是卧底房间");
        };
        let minimum = match lobby.mode {
            UndercoverMode::Single => self.config.min_players.max(4),
            UndercoverMode::Double => self.config.double_min_players.max(6),
        };
        if lobby.players.len() < minimum {
            bail!("{}至少需要{}人", lobby.mode.label(), minimum);
        }
        if words.civilian.trim().is_empty() || words.undercover.trim().is_empty() {
            bail!("谁是卧底词语不能为空");
        }
        if normalized_word(&words.civilian) == normalized_word(&words.undercover) {
            bail!("平民词和卧底词不能相同");
        }

        let mode = lobby.mode;
        let creator_key = lobby.creator_key.clone();
        let mut players = std::mem::take(&mut lobby.players);
        self.rng.shuffle(&mut players);
        let undercover_count = match mode {
            UndercoverMode::Single => 1,
            UndercoverMode::Double => 2,
        };
        let mut roles = vec![Role::Civilian; players.len()];
        for role in roles.iter_mut().take(undercover_count) {
            *role = Role::Undercover;
        }
        self.rng.shuffle(&mut roles);
        for (index, (player, role)) in players.iter_mut().zip(roles).enumerate() {
            player.position = (b'A' + index as u8) as char;
            player.role = role;
        }

        let deliveries = players
            .iter()
            .map(|player| UndercoverDelivery::Friend {
                player: player.name.clone(),
                message: format!(
                    "你的位置：{}；你的词语：{}",
                    player.position,
                    if player.role == Role::Civilian {
                        words.civilian.as_str()
                    } else {
                        words.undercover.as_str()
                    }
                ),
            })
            .collect();
        self.state = GameState::Playing(Playing {
            mode,
            creator_key,
            players,
            words,
            round: 1,
            last_activity: now,
            phase: Phase::AwaitingDelivery,
        });
        Ok(deliveries)
    }

    pub fn complete_delivery(&mut self, now: Instant) -> Result<Vec<UndercoverDelivery>> {
        let GameState::Playing(game) = &mut self.state else {
            bail!("当前没有正在发词的谁是卧底牌局");
        };
        if !matches!(game.phase, Phase::AwaitingDelivery) {
            bail!("谁是卧底发词阶段已经结束");
        }
        game.phase = Phase::Describing {
            descriptions: BTreeMap::new(),
        };
        game.last_activity = now;
        let mut lines = vec![format!(
            "谁是卧底开始：{}，共{}人",
            game.mode.label(),
            game.players.len()
        )];
        lines.extend(position_mapping_lines(&game.players));
        lines.push("请存活玩家在公屏发送 #内容".to_string());
        Ok(vec![UndercoverDelivery::HallBatch(lines)])
    }

    pub fn cancel_delivery(&mut self) -> Vec<UndercoverDelivery> {
        if matches!(self.state, GameState::Playing(_)) {
            self.state = GameState::Idle;
            vec![UndercoverDelivery::Hall(
                "谁是卧底发词失败，牌局已取消".to_string(),
            )]
        } else {
            Vec::new()
        }
    }

    pub fn describe(
        &mut self,
        player: &str,
        description: &str,
        now: Instant,
    ) -> Result<Vec<UndercoverDelivery>> {
        let GameState::Playing(game) = &mut self.state else {
            bail!("当前没有进行中的谁是卧底牌局");
        };
        let index = active_player_index(&game.players, player)?;
        let description = description.trim();
        if description.is_empty() {
            bail!("描述不能为空");
        }
        if display_width(description) > self.config.description_max_width.max(1) {
            bail!(
                "描述不能超过{}显示宽度",
                self.config.description_max_width.max(1)
            );
        }
        let own_word = if game.players[index].role == Role::Civilian {
            &game.words.civilian
        } else {
            &game.words.undercover
        };
        if normalized_word_component(description).contains(&normalized_word_component(own_word)) {
            bail!("描述不能直接包含自己的完整词语");
        }
        let runoff_candidates = match &game.phase {
            Phase::RunoffDescribing { candidates, .. } => Some(candidates.clone()),
            Phase::Describing { .. } => None,
            _ => bail!("当前不是描述阶段"),
        };
        if runoff_candidates
            .as_ref()
            .is_some_and(|candidates| !candidates.contains(&index))
        {
            bail!("只有并列玩家需要补充描述");
        }
        let total = runoff_candidates
            .as_ref()
            .map_or_else(|| alive_count(&game.players), Vec::len);
        let descriptions = match &mut game.phase {
            Phase::Describing { descriptions } | Phase::RunoffDescribing { descriptions, .. } => {
                descriptions
            }
            _ => unreachable!("phase checked above"),
        };
        if descriptions.contains_key(&index) {
            bail!("本轮已描述");
        }
        descriptions.insert(index, description.to_string());
        game.last_activity = now;
        let completed = descriptions.len();
        let mut deliveries = Vec::new();
        if completed == total {
            descriptions.clear();
            if let Some(candidates) = runoff_candidates {
                game.phase = Phase::RunoffVoting {
                    candidates,
                    votes: BTreeMap::new(),
                    reminder: VoteReminder::new(now),
                };
                deliveries.push(UndercoverDelivery::Hall(
                    "并列玩家已完成公屏描述，请其他存活玩家好友私聊 #A".to_string(),
                ));
            } else {
                game.phase = Phase::Voting {
                    votes: BTreeMap::new(),
                    reminder: VoteReminder::new(now),
                };
                deliveries.push(UndercoverDelivery::Hall(
                    "所有存活玩家已描述，请好友私聊 #A".to_string(),
                ));
            }
        }
        Ok(deliveries)
    }

    pub fn vote(
        &mut self,
        player: &str,
        target: char,
        now: Instant,
    ) -> Result<Vec<UndercoverDelivery>> {
        let GameState::Playing(game) = &mut self.state else {
            bail!("当前没有进行中的谁是卧底牌局");
        };
        let voter = active_player_index(&game.players, player)?;
        let target_index = game
            .players
            .iter()
            .position(|item| item.alive && item.position.eq_ignore_ascii_case(&target))
            .ok_or_else(|| anyhow::anyhow!("投票位置不存在或已淘汰"))?;
        let runoff_candidates = match &game.phase {
            Phase::RunoffVoting { candidates, .. } => Some(candidates.clone()),
            Phase::Voting { .. } => None,
            _ => bail!("当前不是投票阶段"),
        };
        if let Some(candidates) = &runoff_candidates {
            if candidates.contains(&voter) {
                bail!("并列玩家不参与加赛投票");
            }
            if !candidates.contains(&target_index) {
                bail!("加赛只能投给并列玩家");
            }
        } else if voter == target_index {
            bail!("不能投自己");
        }
        let total = runoff_candidates.as_ref().map_or_else(
            || alive_count(&game.players),
            |candidates| alive_count(&game.players).saturating_sub(candidates.len()),
        );
        let votes = match &mut game.phase {
            Phase::Voting { votes, .. } | Phase::RunoffVoting { votes, .. } => votes,
            _ => unreachable!("phase checked above"),
        };
        if votes.contains_key(&voter) {
            bail!("本轮已投票");
        }
        votes.insert(voter, target_index);
        game.last_activity = now;
        let completed = votes.len();
        let mut deliveries = Vec::new();
        if completed != total {
            return Ok(deliveries);
        }
        let votes = std::mem::take(votes);
        let ended = resolve_votes(game, votes, runoff_candidates, &mut deliveries);
        if ended {
            self.state = GameState::Idle;
        }
        Ok(deliveries)
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.state, GameState::Idle)
    }

    pub fn is_lobby(&self) -> bool {
        matches!(self.state, GameState::Lobby(_))
    }

    pub fn lobby_contains(&self, player: &str) -> bool {
        let key = player_key(player);
        matches!(&self.state, GameState::Lobby(lobby)
            if lobby.players.iter().any(|item| item.key == key))
    }

    pub fn lobby_is_full(&self) -> bool {
        matches!(&self.state, GameState::Lobby(lobby)
            if lobby.players.len() >= self.config.max_players.clamp(4, 11))
    }

    pub fn authorize_start(&self, requester: Option<&str>) -> Result<()> {
        let GameState::Lobby(lobby) = &self.state else {
            bail!("当前没有可开局的谁是卧底房间");
        };
        if let Some(requester) = requester
            && lobby.creator_key != player_key(requester)
        {
            bail!("只有房间创建者可以开局");
        }
        let minimum = match lobby.mode {
            UndercoverMode::Single => self.config.min_players.max(4),
            UndercoverMode::Double => self.config.double_min_players.max(6),
        };
        if lobby.players.len() < minimum {
            bail!("{}至少需要{}人", lobby.mode.label(), minimum);
        }
        Ok(())
    }

    pub fn status(&mut self, requester: &str, now: Instant) -> String {
        let requester = player_key(requester);
        match &mut self.state {
            GameState::Idle => "当前没有谁是卧底房间或牌局".to_string(),
            GameState::Lobby(lobby) => {
                if lobby.players.iter().any(|player| player.key == requester) {
                    lobby.last_activity = now;
                }
                format!(
                    "谁是卧底等待开局：{}，{}/{}人，玩家：{}",
                    lobby.mode.label(),
                    lobby.players.len(),
                    self.config.max_players.clamp(4, 11),
                    lobby
                        .players
                        .iter()
                        .map(|player| player.name.as_str())
                        .collect::<Vec<_>>()
                        .join("、")
                )
            }
            GameState::Playing(game) => {
                let snapshot = playing_progress(game);
                format!(
                    "谁是卧底：{}，第{}轮，阶段：{}，存活：{}，进度：{}/{}",
                    game.mode.label(),
                    game.round,
                    snapshot.0,
                    game.players
                        .iter()
                        .filter(|player| player.alive)
                        .map(|player| player.position.to_string())
                        .collect::<Vec<_>>()
                        .join("、"),
                    snapshot.1,
                    snapshot.2
                )
            }
        }
    }

    pub fn exit(&mut self, player: &str, now: Instant) -> Result<Vec<UndercoverDelivery>> {
        let key = player_key(player);
        match &mut self.state {
            GameState::Idle => bail!("当前没有谁是卧底房间或牌局"),
            GameState::Lobby(lobby) => {
                let index = lobby
                    .players
                    .iter()
                    .position(|item| item.key == key)
                    .ok_or_else(|| anyhow::anyhow!("你不在当前谁是卧底房间中"))?;
                let name = lobby.players[index].name.clone();
                if lobby.creator_key == key {
                    self.state = GameState::Idle;
                    Ok(vec![UndercoverDelivery::Hall(format!(
                        "{}退出，谁是卧底房间已取消",
                        name
                    ))])
                } else {
                    lobby.players.remove(index);
                    lobby.last_activity = now;
                    Ok(vec![UndercoverDelivery::Hall(format!(
                        "{}退出谁是卧底房间，当前{}人",
                        name,
                        lobby.players.len()
                    ))])
                }
            }
            GameState::Playing(game) => {
                let index = active_player_index(&game.players, player)?;
                game.players[index].alive = false;
                game.last_activity = now;
                let mut deliveries = vec![UndercoverDelivery::Hall(format!(
                    "{}主动退出：{}",
                    game.players[index].position,
                    game.players[index].role.label()
                ))];
                if let Some(result) = winner(game) {
                    deliveries.push(UndercoverDelivery::HallBatch(settlement_lines(
                        game, result,
                    )));
                    self.state = GameState::Idle;
                } else {
                    start_next_round(game);
                    deliveries.push(UndercoverDelivery::Hall(format!(
                        "第{}轮重新开始，请存活玩家在公屏发送 #内容",
                        game.round
                    )));
                }
                Ok(deliveries)
            }
        }
    }

    pub fn end(&mut self, requester: Option<&str>) -> Result<Vec<UndercoverDelivery>> {
        match &self.state {
            GameState::Idle => bail!("当前没有可结束的谁是卧底房间或牌局"),
            GameState::Lobby(lobby) => {
                if let Some(requester) = requester
                    && lobby.creator_key != player_key(requester)
                {
                    bail!("只有房间创建者可以结束");
                }
                self.state = GameState::Idle;
                Ok(vec![UndercoverDelivery::Hall(
                    "谁是卧底房间已取消".to_string(),
                )])
            }
            GameState::Playing(game) => {
                if let Some(requester) = requester
                    && game.creator_key != player_key(requester)
                {
                    bail!("只有房间创建者可以结束");
                }
                let deliveries = vec![UndercoverDelivery::HallBatch(settlement_lines(
                    game,
                    Winner::None,
                ))];
                self.state = GameState::Idle;
                Ok(deliveries)
            }
        }
    }

    pub fn abort(&mut self) -> bool {
        if matches!(self.state, GameState::Idle) {
            false
        } else {
            self.state = GameState::Idle;
            true
        }
    }

    pub fn snapshot(&self, now: Instant) -> UndercoverSnapshot {
        match &self.state {
            GameState::Idle => UndercoverSnapshot {
                enabled: self.config.enabled,
                phase: "idle",
                mode: None,
                round: 0,
                players: Vec::new(),
                completed: 0,
                total: 0,
                remaining_seconds: 0,
            },
            GameState::Lobby(lobby) => UndercoverSnapshot {
                enabled: self.config.enabled,
                phase: "lobby",
                mode: Some(lobby.mode.label()),
                round: 0,
                players: lobby
                    .players
                    .iter()
                    .map(|player| UndercoverPlayerSnapshot {
                        position: None,
                        name: player.name.clone(),
                        alive: true,
                    })
                    .collect(),
                completed: lobby.players.len(),
                total: self.config.max_players.clamp(4, 11),
                remaining_seconds: remaining_seconds(
                    lobby.last_activity,
                    self.config.lobby_timeout_seconds,
                    now,
                ),
            },
            GameState::Playing(game) => {
                let (phase, completed, total) = playing_progress(game);
                UndercoverSnapshot {
                    enabled: self.config.enabled,
                    phase,
                    mode: Some(game.mode.label()),
                    round: game.round,
                    players: game
                        .players
                        .iter()
                        .map(|player| UndercoverPlayerSnapshot {
                            position: Some(player.position),
                            name: player.name.clone(),
                            alive: player.alive,
                        })
                        .collect(),
                    completed,
                    total,
                    remaining_seconds: remaining_seconds(
                        game.last_activity,
                        self.config.phase_timeout_seconds,
                        now,
                    ),
                }
            }
        }
    }

    pub fn tick(&mut self, now: Instant) -> Vec<UndercoverDelivery> {
        let mut deliveries = Vec::new();
        match &mut self.state {
            GameState::Idle => return deliveries,
            GameState::Lobby(lobby) => {
                if now.saturating_duration_since(lobby.last_activity)
                    >= std::time::Duration::from_secs(self.config.lobby_timeout_seconds.max(1))
                {
                    self.state = GameState::Idle;
                    deliveries.push(UndercoverDelivery::Hall(
                        "谁是卧底报名等待超时，房间已取消".to_string(),
                    ));
                }
                return deliveries;
            }
            GameState::Playing(game) => {
                let interval =
                    std::time::Duration::from_secs(self.config.progress_interval_seconds.max(1));
                let message = match &mut game.phase {
                    Phase::Voting { votes, reminder }
                        if now.saturating_duration_since(reminder.last_announcement)
                            >= interval =>
                    {
                        reminder.last_announcement = now;
                        missing_vote_message(&game.players, votes, None)
                    }
                    Phase::RunoffVoting {
                        candidates,
                        votes,
                        reminder,
                    } if now.saturating_duration_since(reminder.last_announcement) >= interval => {
                        reminder.last_announcement = now;
                        missing_vote_message(&game.players, votes, Some(candidates))
                    }
                    _ => None,
                };
                if let Some(message) = message {
                    deliveries.push(UndercoverDelivery::Hall(message));
                }
                if now.saturating_duration_since(game.last_activity)
                    < std::time::Duration::from_secs(self.config.phase_timeout_seconds.max(1))
                {
                    return deliveries;
                }
            }
        }

        let GameState::Playing(mut game) = std::mem::replace(&mut self.state, GameState::Idle)
        else {
            return deliveries;
        };
        let phase = std::mem::replace(&mut game.phase, Phase::AwaitingDelivery);
        let ended = match phase {
            Phase::AwaitingDelivery => {
                deliveries.push(UndercoverDelivery::Hall(
                    "谁是卧底发词超时，牌局已取消".to_string(),
                ));
                true
            }
            Phase::Describing { descriptions, .. } => {
                resolve_description_timeout(&mut game, descriptions, None, now, &mut deliveries)
            }
            Phase::RunoffDescribing {
                candidates,
                descriptions,
                ..
            } => resolve_description_timeout(
                &mut game,
                descriptions,
                Some(candidates),
                now,
                &mut deliveries,
            ),
            Phase::Voting { votes, .. } => {
                if votes.is_empty() {
                    deliveries.push(UndercoverDelivery::HallBatch(settlement_lines(
                        &game,
                        Winner::None,
                    )));
                    true
                } else {
                    resolve_votes(&mut game, votes, None, &mut deliveries)
                }
            }
            Phase::RunoffVoting {
                candidates, votes, ..
            } => {
                if votes.is_empty() {
                    deliveries.push(UndercoverDelivery::HallBatch(settlement_lines(
                        &game,
                        Winner::None,
                    )));
                    true
                } else {
                    resolve_votes(&mut game, votes, Some(candidates), &mut deliveries)
                }
            }
        };
        if !ended {
            self.state = GameState::Playing(game);
        }
        deliveries
    }
}

fn playing_progress(game: &Playing) -> (&'static str, usize, usize) {
    let alive = alive_count(&game.players);
    match &game.phase {
        Phase::AwaitingDelivery => ("delivering", 0, game.players.len()),
        Phase::Describing { descriptions } => ("describing", descriptions.len(), alive),
        Phase::Voting { votes, .. } => ("voting", votes.len(), alive),
        Phase::RunoffDescribing {
            candidates,
            descriptions,
        } => ("runoff_describing", descriptions.len(), candidates.len()),
        Phase::RunoffVoting {
            candidates, votes, ..
        } => (
            "runoff_voting",
            votes.len(),
            alive.saturating_sub(candidates.len()),
        ),
    }
}

fn remaining_seconds(started: Instant, timeout_seconds: u64, now: Instant) -> u64 {
    timeout_seconds
        .max(1)
        .saturating_sub(now.saturating_duration_since(started).as_secs())
}

fn missing_vote_message(
    players: &[Player],
    votes: &BTreeMap<usize, usize>,
    excluded_voters: Option<&[usize]>,
) -> Option<String> {
    let positions = players
        .iter()
        .enumerate()
        .filter(|(index, player)| {
            player.alive
                && !votes.contains_key(index)
                && excluded_voters.is_none_or(|excluded| !excluded.contains(index))
        })
        .map(|(_, player)| player.position.to_string())
        .collect::<Vec<_>>();
    (!positions.is_empty()).then(|| format!("未投票：{}", positions.join("、")))
}

fn resolve_description_timeout(
    game: &mut Playing,
    descriptions: BTreeMap<usize, String>,
    runoff_candidates: Option<Vec<usize>>,
    now: Instant,
    deliveries: &mut Vec<UndercoverDelivery>,
) -> bool {
    if descriptions.is_empty() {
        deliveries.push(UndercoverDelivery::HallBatch(settlement_lines(
            game,
            Winner::None,
        )));
        return true;
    }
    let required = runoff_candidates.clone().unwrap_or_else(|| {
        game.players
            .iter()
            .enumerate()
            .filter_map(|(index, player)| player.alive.then_some(index))
            .collect()
    });
    for index in required {
        if !descriptions.contains_key(&index) && game.players[index].alive {
            game.players[index].alive = false;
            deliveries.push(UndercoverDelivery::Hall(format!(
                "{}描述超时，视为退出：{}",
                game.players[index].position,
                game.players[index].role.label()
            )));
        }
    }
    if let Some(result) = winner(game) {
        deliveries.push(UndercoverDelivery::HallBatch(settlement_lines(
            game, result,
        )));
        return true;
    }

    if let Some(candidates) = runoff_candidates {
        let candidates = candidates
            .into_iter()
            .filter(|index| game.players[*index].alive)
            .collect::<Vec<_>>();
        if candidates.len() < 2 {
            start_next_round(game);
            deliveries.push(UndercoverDelivery::Hall(format!(
                "加赛描述超时，无法继续重投；第{}轮开始，请在公屏发送 #内容",
                game.round
            )));
        } else {
            game.phase = Phase::RunoffVoting {
                candidates,
                votes: BTreeMap::new(),
                reminder: VoteReminder::new(now),
            };
            deliveries.push(UndercoverDelivery::Hall(
                "加赛描述阶段已截止，请其他存活玩家好友私聊 #A".to_string(),
            ));
        }
    } else {
        game.phase = Phase::Voting {
            votes: BTreeMap::new(),
            reminder: VoteReminder::new(now),
        };
        deliveries.push(UndercoverDelivery::Hall(
            "描述阶段已截止，请存活玩家好友私聊 #A".to_string(),
        ));
    }
    false
}

fn resolve_votes(
    game: &mut Playing,
    votes: BTreeMap<usize, usize>,
    runoff_candidates: Option<Vec<usize>>,
    deliveries: &mut Vec<UndercoverDelivery>,
) -> bool {
    let leaders = highest_targets(&votes);
    if leaders.len() > 1 {
        if runoff_candidates.is_some() {
            start_next_round(game);
            deliveries.push(UndercoverDelivery::Hall(format!(
                "加赛仍并列，本轮无人淘汰；第{}轮开始，请在公屏发送 #内容",
                game.round
            )));
        } else {
            let positions = leaders
                .iter()
                .map(|index| game.players[*index].position.to_string())
                .collect::<Vec<_>>()
                .join("、");
            game.phase = Phase::RunoffDescribing {
                candidates: leaders,
                descriptions: BTreeMap::new(),
            };
            deliveries.push(UndercoverDelivery::Hall(format!(
                "{}最高票并列，进入并列加赛，请并列玩家在公屏发送 #内容",
                positions
            )));
        }
        return false;
    }
    let Some(target) = leaders.first().copied() else {
        deliveries.push(UndercoverDelivery::HallBatch(settlement_lines(
            game,
            Winner::None,
        )));
        return true;
    };
    game.players[target].alive = false;
    deliveries.push(UndercoverDelivery::Hall(format!(
        "{}已淘汰：{}",
        game.players[target].position,
        game.players[target].role.label()
    )));
    if let Some(result) = winner(game) {
        deliveries.push(UndercoverDelivery::HallBatch(settlement_lines(
            game, result,
        )));
        true
    } else {
        start_next_round(game);
        deliveries.push(UndercoverDelivery::Hall(format!(
            "第{}轮开始，请存活玩家在公屏发送 #内容",
            game.round
        )));
        false
    }
}

fn start_next_round(game: &mut Playing) {
    game.round = game.round.saturating_add(1);
    game.phase = Phase::Describing {
        descriptions: BTreeMap::new(),
    };
}

#[derive(Clone, Copy)]
enum Winner {
    Civilian,
    Undercover,
    None,
}

impl Winner {
    fn label(self) -> &'static str {
        match self {
            Self::Civilian => "平民胜利",
            Self::Undercover => "卧底胜利",
            Self::None => "无胜方",
        }
    }
}

fn active_player_index(players: &[Player], name: &str) -> Result<usize> {
    let key = player_key(name);
    let index = players
        .iter()
        .position(|player| player.key == key)
        .ok_or_else(|| anyhow::anyhow!("你不在本局谁是卧底中"))?;
    if !players[index].alive {
        bail!("你已被淘汰");
    }
    Ok(index)
}

fn alive_count(players: &[Player]) -> usize {
    players.iter().filter(|player| player.alive).count()
}

fn highest_targets(votes: &BTreeMap<usize, usize>) -> Vec<usize> {
    let mut counts = BTreeMap::<usize, usize>::new();
    for target in votes.values() {
        *counts.entry(*target).or_default() += 1;
    }
    let Some(highest) = counts.values().copied().max() else {
        return Vec::new();
    };
    counts
        .into_iter()
        .filter_map(|(target, count)| (count == highest).then_some(target))
        .collect()
}

fn winner(game: &Playing) -> Option<Winner> {
    let civilians = game
        .players
        .iter()
        .filter(|player| player.alive && player.role == Role::Civilian)
        .count();
    let undercovers = game
        .players
        .iter()
        .filter(|player| player.alive && player.role == Role::Undercover)
        .count();
    if undercovers == 0 {
        Some(Winner::Civilian)
    } else if civilians <= undercovers {
        Some(Winner::Undercover)
    } else {
        None
    }
}

fn position_mapping_lines(players: &[Player]) -> Vec<String> {
    players
        .chunks(3)
        .map(|chunk| {
            chunk
                .iter()
                .map(|player| format!("{}={}", player.position, player.name))
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn settlement_lines(game: &Playing, winner: Winner) -> Vec<String> {
    let mut lines = vec![format!(
        "谁是卧底结束：{}  平民词：{}  卧底词：{}",
        winner.label(),
        game.words.civilian,
        game.words.undercover
    )];
    lines.extend(game.players.chunks(3).map(|chunk| {
        chunk
            .iter()
            .map(|player| {
                format!(
                    "{}：{}（{}）",
                    player.position,
                    player.name,
                    player.role.label()
                )
            })
            .collect::<Vec<_>>()
            .join("|")
    }));
    lines.push(format!("共进行{}轮", game.round));
    lines
}

fn player_key(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalized_word(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) fn normalized_word_component(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter_map(|ch| match ch {
            '\u{3000}' => None,
            '\u{FF01}'..='\u{FF5E}' => char::from_u32(ch as u32 - 0xFEE0),
            ch if ch.is_whitespace() => None,
            ch => Some(ch),
        })
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) fn unordered_word_key(left: &str, right: &str) -> String {
    let mut words = [
        normalized_word_component(left),
        normalized_word_component(right),
    ];
    words.sort();
    format!("{}\0{}", words[0], words[1])
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn shuffle<T>(&mut self, values: &mut [T]) {
        for index in (1..values.len()).rev() {
            values.swap(index, (self.next_u64() as usize) % (index + 1));
        }
    }
}

fn random_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn single_undercover_game_starts_with_four_players_and_hides_roles() {
        let now = Instant::now();
        let mut game = UndercoverGame::with_seed(
            UndercoverConfig {
                enabled: true,
                ..UndercoverConfig::default()
            },
            7,
        );
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }

        let deliveries = game
            .start(UndercoverWordPair::new("苹果", "梨"), now)
            .unwrap();

        assert_eq!(deliveries.len(), 4);
        assert!(deliveries.iter().all(|delivery| matches!(
            delivery,
            UndercoverDelivery::Friend { message, .. }
                if message.contains("你的位置：")
                    && message.contains("你的词语：")
                    && !message.contains("平民")
                    && !message.contains("卧底")
        )));
    }

    #[test]
    fn public_descriptions_are_not_relayed_and_voting_eliminates_the_undercover() {
        let now = Instant::now();
        let mut game = enabled_game(11);
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }
        let words = game
            .start(UndercoverWordPair::new("苹果", "梨"), now)
            .unwrap();
        let undercover = words
            .iter()
            .find_map(|delivery| match delivery {
                UndercoverDelivery::Friend { player, message } if message.contains("梨") => {
                    Some((player.clone(), message_position(message)))
                }
                _ => None,
            })
            .unwrap();
        let participants = words
            .iter()
            .filter_map(|delivery| match delivery {
                UndercoverDelivery::Friend { player, message } => {
                    Some((player.clone(), message_position(message)))
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        let opening = game.complete_delivery(now).unwrap();
        assert!(matches!(&opening[0], UndercoverDelivery::HallBatch(lines)
            if lines.iter().any(|line| line.contains("A="))));

        for (index, (player, _)) in participants.iter().enumerate() {
            let deliveries = game
                .describe(player, &format!("第{}条描述", index + 1), now)
                .unwrap();
            if index < participants.len() - 1 {
                assert!(deliveries.is_empty());
            } else {
                assert_eq!(
                    deliveries,
                    vec![UndercoverDelivery::Hall(
                        "所有存活玩家已描述，请好友私聊 #A".to_string()
                    )]
                );
            }
        }

        let civilian_target = participants
            .iter()
            .find(|(player, _)| *player != undercover.0)
            .unwrap()
            .1;
        let mut final_deliveries = Vec::new();
        for (player, _) in &participants {
            let target = if *player == undercover.0 {
                civilian_target
            } else {
                undercover.1
            };
            final_deliveries = game.vote(player, target, now).unwrap();
        }
        assert!(final_deliveries.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::HallBatch(lines)
                if lines.first().is_some_and(|line| line.contains("平民胜利"))
                    && lines.last().is_some_and(|line| line == "共进行1轮"))));
    }

    #[test]
    fn one_runoff_is_held_and_a_second_tie_eliminates_nobody() {
        let now = Instant::now();
        let mut game = enabled_game(17);
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }
        let deliveries = game
            .start(UndercoverWordPair::new("苹果", "梨"), now)
            .unwrap();
        let players = deliveries
            .iter()
            .filter_map(|delivery| match delivery {
                UndercoverDelivery::Friend { player, message } => {
                    Some((player.clone(), message_position(message)))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        game.complete_delivery(now).unwrap();
        for (index, (player, _)) in players.iter().enumerate() {
            game.describe(player, &format!("描述{}", index), now)
                .unwrap();
        }
        let first = players[0].1;
        let second = players[1].1;
        let normal_votes = [second, first, first, second];
        let mut runoff_started = Vec::new();
        for ((player, _), target) in players.iter().zip(normal_votes) {
            runoff_started = game.vote(player, target, now).unwrap();
        }
        assert!(runoff_started.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::Hall(message) if message.contains("进入并列加赛"))));

        game.describe(&players[0].0, "补充描述一", now).unwrap();
        let completed = game.describe(&players[1].0, "补充描述二", now).unwrap();
        assert_eq!(
            completed,
            vec![UndercoverDelivery::Hall(
                "并列玩家已完成公屏描述，请其他存活玩家好友私聊 #A".to_string()
            )]
        );

        let voters = [&players[2], &players[3]];
        game.vote(&voters[0].0, first, now).unwrap();
        let tied = game.vote(&voters[1].0, second, now).unwrap();
        assert!(tied.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::Hall(message)
                if message.contains("加赛仍并列") && message.contains("第2轮"))));
    }

    #[test]
    fn voting_reminds_missing_positions_every_twenty_seconds() {
        let now = Instant::now();
        let mut game = enabled_game(23);
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }
        let deliveries = game
            .start(UndercoverWordPair::new("苹果", "梨"), now)
            .unwrap();
        let players = deliveries
            .iter()
            .filter_map(|delivery| match delivery {
                UndercoverDelivery::Friend { player, message } => {
                    Some((player.clone(), message_position(message)))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        game.complete_delivery(now).unwrap();
        for (index, (player, _)) in players.iter().enumerate() {
            game.describe(player, &format!("第{}人描述", index + 1), now)
                .unwrap();
        }
        game.vote(&players[0].0, players[1].1, now).unwrap();

        let before = game.tick(now + std::time::Duration::from_secs(19));
        assert!(before.is_empty());

        let first_reminder = game.tick(now + std::time::Duration::from_secs(20));
        assert_eq!(first_reminder.len(), 1);
        let UndercoverDelivery::Hall(message) = &first_reminder[0] else {
            panic!("expected hall reminder");
        };
        assert!(message.starts_with("未投票："));
        assert!(!message.contains(players[0].1));
        for (_, position) in players.iter().skip(1) {
            assert!(message.contains(*position));
        }

        assert!(
            game.tick(now + std::time::Duration::from_secs(39))
                .is_empty()
        );
        let second_reminder = game.tick(now + std::time::Duration::from_secs(40));
        assert_eq!(second_reminder, first_reminder);
    }

    #[test]
    fn valid_activity_extends_the_three_minute_idle_deadline() {
        let now = Instant::now();
        let mut game = enabled_game(29);
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }
        let deliveries = game
            .start(UndercoverWordPair::new("苹果", "梨"), now)
            .unwrap();
        let first = match &deliveries[0] {
            UndercoverDelivery::Friend { player, .. } => player.clone(),
            _ => unreachable!(),
        };
        game.complete_delivery(now).unwrap();
        game.describe(
            &first,
            "一种常见事物",
            now + std::time::Duration::from_secs(120),
        )
        .unwrap();

        let early = game.tick(now + std::time::Duration::from_secs(180));
        assert!(game.is_active());
        assert!(!early.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::HallBatch(lines)
                if lines.first().is_some_and(|line| line.contains("谁是卧底结束")))));

        let _ = game.status("甲", now + std::time::Duration::from_secs(290));

        let settled = game.tick(now + std::time::Duration::from_secs(300));
        assert!(!game.is_active());
        assert!(settled.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::HallBatch(lines)
                if lines.first().is_some_and(|line| line.contains("谁是卧底结束")))));
    }

    #[test]
    fn double_undercover_wins_when_civilian_and_undercover_counts_are_equal() {
        let now = Instant::now();
        let mut game = enabled_game(31);
        game.create("甲", UndercoverMode::Double, now).unwrap();
        for player in ["乙", "丙", "丁", "戊", "己"] {
            game.join(player, now).unwrap();
        }
        let deliveries = game
            .start(UndercoverWordPair::new("苹果", "梨"), now)
            .unwrap();
        let players = deliveries
            .iter()
            .filter_map(|delivery| match delivery {
                UndercoverDelivery::Friend { player, message } => Some((
                    player.clone(),
                    message_position(message),
                    message.contains("梨"),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(players.iter().filter(|(_, _, role)| *role).count(), 2);
        game.complete_delivery(now).unwrap();

        let civilians = players
            .iter()
            .filter(|(_, _, role)| !role)
            .map(|(player, position, _)| (player.clone(), *position))
            .collect::<Vec<_>>();
        for (round, target) in civilians.iter().take(2).enumerate() {
            let alive = game
                .snapshot(now)
                .players
                .into_iter()
                .filter(|player| player.alive)
                .map(|player| player.name)
                .collect::<Vec<_>>();
            for (index, player) in alive.iter().enumerate() {
                game.describe(player, &format!("第{}人描述", index), now)
                    .unwrap();
            }
            let fallback = players
                .iter()
                .find(|(player, _, _)| player != &target.0 && alive.contains(player))
                .unwrap()
                .1;
            let mut result = Vec::new();
            for player in &alive {
                result = game
                    .vote(
                        player,
                        if player == &target.0 {
                            fallback
                        } else {
                            target.1
                        },
                        now,
                    )
                    .unwrap();
            }
            if round == 1 {
                assert!(result.iter().any(|delivery| matches!(delivery,
                    UndercoverDelivery::HallBatch(lines)
                        if lines.first().is_some_and(|line| line.contains("卧底胜利")))));
            }
        }
        assert!(!game.is_active());
    }

    #[test]
    fn voting_timeout_uses_received_votes_and_missing_players_abstain() {
        let now = Instant::now();
        let mut game = enabled_game(37);
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }
        let deliveries = game
            .start(UndercoverWordPair::new("苹果", "梨"), now)
            .unwrap();
        let players = deliveries
            .iter()
            .filter_map(|delivery| match delivery {
                UndercoverDelivery::Friend { player, message } => Some((
                    player.clone(),
                    message_position(message),
                    message.contains("梨"),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        let undercover = players.iter().find(|(_, _, role)| *role).unwrap();
        game.complete_delivery(now).unwrap();
        for (index, (player, _, _)) in players.iter().enumerate() {
            game.describe(player, &format!("描述{}", index), now)
                .unwrap();
        }
        let voter = players
            .iter()
            .find(|(player, _, _)| player != &undercover.0)
            .unwrap();
        game.vote(&voter.0, undercover.1, now).unwrap();

        let result = game.tick(now + std::time::Duration::from_secs(180));
        assert!(result.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::HallBatch(lines)
                if lines.first().is_some_and(|line| line.contains("平民胜利")))));
    }

    #[test]
    fn snapshot_never_exposes_words_or_roles() {
        let now = Instant::now();
        let mut game = enabled_game(41);
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }
        game.start(UndercoverWordPair::new("绝密平民词", "绝密卧底词"), now)
            .unwrap();
        game.complete_delivery(now).unwrap();

        let json = serde_json::to_string(&game.snapshot(now)).unwrap();
        assert!(!json.contains("绝密平民词"));
        assert!(!json.contains("绝密卧底词"));
        assert!(!json.contains("civilian"));
        assert!(!json.contains("role"));
    }

    fn enabled_game(seed: u64) -> UndercoverGame {
        UndercoverGame::with_seed(
            UndercoverConfig {
                enabled: true,
                ..UndercoverConfig::default()
            },
            seed,
        )
    }

    fn message_position(message: &str) -> char {
        message
            .strip_prefix("你的位置：")
            .and_then(|rest| rest.chars().next())
            .unwrap()
    }
}
