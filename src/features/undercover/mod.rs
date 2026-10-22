use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
use miliastra_kernel::timer::{DeadlineKind, DeadlineModule, DeadlineToken};

pub(crate) mod repository;
mod service;

use super::chat_text::{command_identity, compact_command, display_width};
use super::reply_template::{ReplyTemplate, ReplyVars, undercover as templates};

pub use service::{
    UndercoverApplication, UndercoverCommandSource, UndercoverCommandStart, UndercoverDeliveryPort,
    UndercoverEffectClaim, UndercoverEffectKey, UndercoverEffectResult, UndercoverEffectTask,
    UndercoverFriendBatchOutcome, UndercoverResume, UndercoverRuntimePort,
    UndercoverRuntimeService, UndercoverTimedOutcome,
};

/// A reply that stays raw or keyed until the composition delivery boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndercoverReply {
    Raw(String),
    Keyed {
        spec: &'static ReplyTemplate,
        vars: ReplyVars,
    },
    Composed {
        spec: &'static ReplyTemplate,
        vars: ReplyVars,
        fragments: Vec<(&'static str, UndercoverReply)>,
    },
    Joined {
        fragments: Vec<UndercoverReply>,
    },
}

impl UndercoverReply {
    pub(crate) fn keyed(spec: &'static ReplyTemplate, vars: &ReplyVars) -> Self {
        Self::Keyed {
            spec,
            vars: vars.clone(),
        }
    }

    pub(crate) fn render_default(&self) -> String {
        match self {
            Self::Raw(text) => text.clone(),
            Self::Keyed { spec, vars } => {
                crate::features::reply_template::render(spec, None, vars).text
            }
            Self::Composed {
                spec,
                vars,
                fragments,
            } => {
                let nested = fragments
                    .iter()
                    .map(|(name, reply)| (*name, reply.render_default()))
                    .collect();
                crate::features::reply_template::render(
                    spec,
                    None,
                    &vars.clone().with_nested(nested),
                )
                .text
            }
            Self::Joined { fragments } => fragments
                .iter()
                .map(Self::render_default)
                .collect::<String>(),
        }
    }

    pub(crate) fn into_raw(self) -> Option<String> {
        match self {
            Self::Raw(message) => Some(message),
            Self::Keyed { .. } | Self::Composed { .. } | Self::Joined { .. } => None,
        }
    }
}

/// One private message, retaining the recipient beside its unresolved reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndercoverFriendDelivery {
    pub recipient: String,
    pub reply: UndercoverReply,
}

impl UndercoverFriendDelivery {
    pub(crate) fn new(recipient: impl Into<String>, reply: UndercoverReply) -> Self {
        Self {
            recipient: recipient.into(),
            reply,
        }
    }
}

/// 投票说明。它是多条投票提示里的公共片段，单独成键以便用户只改一处。
pub(crate) fn vote_instruction() -> UndercoverReply {
    UndercoverReply::keyed(&templates::VOTE_INSTRUCTION, &ReplyVars::new())
}

fn with_vote_instruction(spec: &'static ReplyTemplate) -> UndercoverReply {
    UndercoverReply::Composed {
        spec,
        vars: ReplyVars::new(),
        fragments: vec![("投票说明", vote_instruction())],
    }
}

#[derive(Debug)]
pub struct UndercoverDeadlineModule;

impl DeadlineModule for UndercoverDeadlineModule {
    const NAME: &'static str = "undercover";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UndercoverDeadlineKind {
    LobbyIdle,
    PhaseIdle,
    VoteReminder,
}

impl DeadlineKind for UndercoverDeadlineKind {
    type Module = UndercoverDeadlineModule;
}

pub type UndercoverDeadlineToken = DeadlineToken<UndercoverDeadlineKind>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UndercoverCommand {
    CreateSingle,
    CreateDouble,
    Join,
    Start,
    Status,
    Exit,
    End,
    Retry,
    Reveal,
    Describe(String),
    Vote(char),
    Abstain,
}

impl UndercoverCommand {
    pub(crate) fn claims_start_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && envelope.authority() == CommandAuthority::HallMember
            && matches!(
                compact_command(envelope.command_text()).as_str(),
                "卧底" | "卧底双"
            )
    }

    pub(crate) fn claims_active_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && match envelope.authority() {
                CommandAuthority::HallMember => true,
                CommandAuthority::Friend => {
                    let compact = compact_command(envelope.command_text());
                    matches!(compact.as_str(), "加入" | "退出" | "弃权" | "谜底")
                        || parse_vote_position(&compact).is_some()
                }
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
        let raw = match &command {
            Self::Vote(_) | Self::Abstain => "投票".to_string(),
            Self::Reveal => "谜底".to_string(),
            Self::Describe(_) => "卧底描述".to_string(),
            _ => envelope.command_text().to_string(),
        };
        Some(FeatureCommandMatch::new("#", raw, command))
    }

    pub(crate) fn parse_start(payload: &str) -> Option<Self> {
        match compact_command(payload).as_str() {
            "卧底" => Some(Self::CreateSingle),
            "卧底双" => Some(Self::CreateDouble),
            _ => None,
        }
    }

    pub(crate) fn parse_hall(payload: &str) -> Option<Self> {
        let normalized = compact_command(payload);
        Some(match normalized.as_str() {
            "开局" => Self::Start,
            "状态" => Self::Status,
            "退出" => Self::Exit,
            "结束" => Self::End,
            "重试" => Self::Retry,
            "加入" | "手牌" | "弃权" => return None,
            _ if parse_vote_position(&normalized).is_some() => return None,
            _ => Self::Describe(payload.to_string()),
        })
    }

    pub(crate) fn parse_friend(payload: &str) -> Option<Self> {
        let normalized = compact_command(payload);
        Some(match normalized.as_str() {
            "加入" => Self::Join,
            "退出" => Self::Exit,
            "弃权" => Self::Abstain,
            "谜底" => Self::Reveal,
            _ => Self::Vote(parse_vote_position(&normalized)?),
        })
    }

    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::CreateSingle => "undercover:create:single".to_string(),
            Self::CreateDouble => "undercover:create:double".to_string(),
            Self::Join => "undercover:join".to_string(),
            Self::Start => "undercover:start".to_string(),
            Self::Status => "undercover:status".to_string(),
            Self::Exit => "undercover:exit".to_string(),
            Self::End => "undercover:end".to_string(),
            Self::Retry => "undercover:retry".to_string(),
            Self::Reveal => "undercover:reveal".to_string(),
            Self::Describe(text) => format!("undercover:describe:{}", command_identity(text)),
            Self::Vote(position) => format!("undercover:vote:{position}"),
            Self::Abstain => "undercover:vote:abstain".to_string(),
        }
    }
}

fn parse_vote_position(value: &str) -> Option<char> {
    let value = value
        .strip_prefix('投')
        .unwrap_or(value)
        .to_ascii_uppercase();
    let mut chars = value.chars();
    let position = chars.next()?;
    (chars.next().is_none() && ('A'..='K').contains(&position)).then_some(position)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
            word_bank_path: PathBuf::from("deps/undercover.yaml"),
            used_state_path: PathBuf::from("deps/data/undercover-used.yaml"),
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

impl UndercoverConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if !(4..=11).contains(&self.max_players) {
            bail!("undercover.max_players 必须在 4 到 11 之间");
        }
        if !(4..=self.max_players).contains(&self.min_players) {
            bail!("undercover.min_players 必须在 4 到 max_players 之间");
        }
        if !(6..=self.max_players).contains(&self.double_min_players) {
            bail!("undercover.double_min_players 必须在 6 到 max_players 之间");
        }
        if self.lobby_timeout_seconds == 0
            || self.phase_timeout_seconds == 0
            || self.progress_interval_seconds == 0
        {
            bail!("谁是卧底的大厅、阶段和进度间隔必须大于 0 秒");
        }
        if self.description_max_width == 0 {
            bail!("undercover.description_max_width 必须大于 0");
        }
        if self.word_bank_path.as_os_str().is_empty() || self.used_state_path.as_os_str().is_empty()
        {
            bail!("谁是卧底词库和使用记录路径不能为空");
        }
        Ok(())
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
    KeyedHall(UndercoverReply),
    KeyedHallBatch(Vec<UndercoverReply>),
    KeyedFriend(UndercoverFriendDelivery),
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

impl Default for UndercoverSnapshot {
    fn default() -> Self {
        Self {
            enabled: false,
            phase: "idle",
            mode: None,
            round: 0,
            players: Vec::new(),
            completed: 0,
            total: 0,
            remaining_seconds: 0,
        }
    }
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
    fn reply(self) -> UndercoverReply {
        let spec = match self {
            Self::Civilian => &templates::ROLE_CIVILIAN,
            Self::Undercover => &templates::ROLE_UNDERCOVER,
        };
        UndercoverReply::keyed(spec, &ReplyVars::new())
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
        votes: Votes,
        reminder: VoteReminder,
    },
    RunoffDescribing {
        candidates: Vec<usize>,
        descriptions: BTreeMap<usize, String>,
    },
    RunoffVoting {
        candidates: Vec<usize>,
        votes: Votes,
        reminder: VoteReminder,
    },
}

type Votes = BTreeMap<usize, Option<usize>>;

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
            .map(|player| {
                UndercoverDelivery::KeyedFriend(UndercoverFriendDelivery::new(
                    player.name.clone(),
                    UndercoverReply::keyed(
                        &templates::SECRET,
                        &&ReplyVars::new()
                            .with("位置", player.position.to_string())
                            .with(
                                "词语",
                                if player.role == Role::Civilian {
                                    words.civilian.as_str()
                                } else {
                                    words.undercover.as_str()
                                },
                            ),
                    ),
                ))
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
        let mut lines = vec![UndercoverReply::keyed(
            &templates::OPENING,
            &ReplyVars::new()
                .with("模式", game.mode.label())
                .with("人数", game.players.len().to_string()),
        )];
        lines.extend(position_mapping_lines(&game.players));
        lines.push(UndercoverReply::keyed(
            &templates::DESCRIBE_PROMPT,
            &ReplyVars::new(),
        ));
        Ok(vec![UndercoverDelivery::KeyedHallBatch(lines)])
    }

    pub fn cancel_delivery(&mut self) -> Vec<UndercoverDelivery> {
        if matches!(self.state, GameState::Playing(_)) {
            self.state = GameState::Idle;
            vec![UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                &templates::DELIVERY_CANCELED,
                &ReplyVars::new(),
            ))]
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
                let voter_count = alive_count(&game.players).saturating_sub(candidates.len());
                if voter_count == 0 {
                    start_next_round(game);
                    deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                        &templates::RUNOFF_NO_VOTERS,
                        &ReplyVars::new().with("轮次", game.round.to_string()),
                    )));
                } else {
                    game.phase = Phase::RunoffVoting {
                        candidates,
                        votes: BTreeMap::new(),
                        reminder: VoteReminder::new(now),
                    };
                    deliveries.push(UndercoverDelivery::KeyedHall(with_vote_instruction(
                        &templates::RUNOFF_DESCRIBED,
                    )));
                }
            } else {
                game.phase = Phase::Voting {
                    votes: BTreeMap::new(),
                    reminder: VoteReminder::new(now),
                };
                deliveries.push(UndercoverDelivery::KeyedHall(with_vote_instruction(
                    &templates::DESCRIBE_COMPLETED,
                )));
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
        self.cast_vote(player, Some(target), now)
    }

    pub fn abstain(&mut self, player: &str, now: Instant) -> Result<Vec<UndercoverDelivery>> {
        self.cast_vote(player, None, now)
    }

    fn cast_vote(
        &mut self,
        player: &str,
        target: Option<char>,
        now: Instant,
    ) -> Result<Vec<UndercoverDelivery>> {
        let GameState::Playing(game) = &mut self.state else {
            bail!("当前没有进行中的谁是卧底牌局");
        };
        let voter = active_player_index(&game.players, player)?;
        let target_index = target
            .map(|target| {
                game.players
                    .iter()
                    .position(|item| item.alive && item.position.eq_ignore_ascii_case(&target))
                    .ok_or_else(|| anyhow::anyhow!("投票位置不存在或已淘汰"))
            })
            .transpose()?;
        let runoff_candidates = match &game.phase {
            Phase::RunoffVoting { candidates, .. } => Some(candidates.clone()),
            Phase::Voting { .. } => None,
            _ => bail!("当前不是投票阶段"),
        };
        if let Some(candidates) = &runoff_candidates {
            if candidates.contains(&voter) {
                bail!("并列玩家不参与加赛投票");
            }
            if target_index.is_some_and(|target| !candidates.contains(&target)) {
                bail!("加赛只能投给并列玩家");
            }
        } else if target_index == Some(voter) {
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

    pub fn status(&mut self, requester: &str, now: Instant) -> UndercoverReply {
        let requester = player_key(requester);
        match &mut self.state {
            GameState::Idle => UndercoverReply::keyed(&templates::STATUS_IDLE, &ReplyVars::new()),
            GameState::Lobby(lobby) => {
                if lobby.players.iter().any(|player| player.key == requester) {
                    lobby.last_activity = now;
                }
                UndercoverReply::keyed(
                    &templates::STATUS_LOBBY,
                    &ReplyVars::new()
                        .with("模式", lobby.mode.label())
                        .with("人数", lobby.players.len().to_string())
                        .with("上限", self.config.max_players.clamp(4, 11).to_string())
                        .with(
                            "玩家列表",
                            lobby
                                .players
                                .iter()
                                .map(|player| player.name.as_str())
                                .collect::<Vec<_>>()
                                .join("、"),
                        ),
                )
            }
            GameState::Playing(game) => compact_status(game),
        }
    }

    /// 返回当前游戏的私有答案，不改变游戏状态。
    pub fn reveal(&self, requester: &str) -> Result<Vec<UndercoverReply>> {
        let GameState::Playing(game) = &self.state else {
            bail!("当前没有正在进行的谁是卧底牌局");
        };
        let requester = player_key(requester);
        if game.players.iter().any(|player| player.key == requester) {
            bail!("对局玩家不能使用#谜底");
        }

        let undercover_positions = game
            .players
            .iter()
            .filter(|player| player.role == Role::Undercover)
            .map(|player| player.position.to_string())
            .collect::<Vec<_>>();
        let undercover_positions = if undercover_positions.is_empty() {
            "无".to_string()
        } else {
            undercover_positions.join("")
        };
        Ok(vec![UndercoverReply::keyed(
            &templates::REVEAL,
            &ReplyVars::new()
                .with("平民词", game.words.civilian.as_str())
                .with("卧底词", game.words.undercover.as_str())
                .with("卧底位置", undercover_positions),
        )])
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
                    Ok(vec![UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                        &templates::EXIT_CREATOR,
                        &ReplyVars::new().with("玩家", name),
                    ))])
                } else {
                    lobby.players.remove(index);
                    lobby.last_activity = now;
                    Ok(vec![UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                        &templates::EXIT_LOBBY,
                        &ReplyVars::new()
                            .with("玩家", name)
                            .with("人数", lobby.players.len().to_string()),
                    ))])
                }
            }
            GameState::Playing(game) => {
                let index = active_player_index(&game.players, player)?;
                game.players[index].alive = false;
                game.last_activity = now;
                let mut deliveries =
                    vec![UndercoverDelivery::KeyedHall(UndercoverReply::Composed {
                        spec: &templates::EXIT_PLAYING,
                        vars: ReplyVars::new()
                            .with("位置", game.players[index].position.to_string()),
                        fragments: vec![("身份", game.players[index].role.reply())],
                    })];
                if let Some(result) = winner(game) {
                    deliveries.push(UndercoverDelivery::KeyedHallBatch(settlement_lines(
                        game, result,
                    )));
                    self.state = GameState::Idle;
                } else {
                    start_next_round(game);
                    deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                        &templates::ROUND_RESTART,
                        &ReplyVars::new().with("轮次", game.round.to_string()),
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
                Ok(vec![UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                    &templates::LOBBY_CANCELED,
                    &ReplyVars::new(),
                ))])
            }
            GameState::Playing(game) => {
                if let Some(requester) = requester
                    && game.creator_key != player_key(requester)
                {
                    bail!("只有房间创建者可以结束");
                }
                let deliveries = vec![UndercoverDelivery::KeyedHallBatch(settlement_lines(
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

    pub(crate) fn next_deadline(
        &self,
        _now: Instant,
        clock_active: bool,
    ) -> Option<(UndercoverDeadlineKind, Instant)> {
        if !clock_active {
            return None;
        }
        match &self.state {
            GameState::Idle => None,
            GameState::Lobby(lobby) => Some((
                UndercoverDeadlineKind::LobbyIdle,
                lobby
                    .last_activity
                    .checked_add(Duration::from_secs(
                        self.config.lobby_timeout_seconds.max(1),
                    ))
                    .unwrap_or(lobby.last_activity),
            )),
            GameState::Playing(game) => {
                let phase_deadline = game
                    .last_activity
                    .checked_add(Duration::from_secs(
                        self.config.phase_timeout_seconds.max(1),
                    ))
                    .unwrap_or(game.last_activity);
                let reminder_deadline = match &game.phase {
                    Phase::Voting { reminder, .. } | Phase::RunoffVoting { reminder, .. } => Some(
                        reminder
                            .last_announcement
                            .checked_add(Duration::from_secs(
                                self.config.progress_interval_seconds.max(1),
                            ))
                            .unwrap_or(reminder.last_announcement),
                    ),
                    _ => None,
                };
                match reminder_deadline {
                    Some(deadline) if deadline <= phase_deadline => {
                        Some((UndercoverDeadlineKind::VoteReminder, deadline))
                    }
                    _ => Some((UndercoverDeadlineKind::PhaseIdle, phase_deadline)),
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
                    deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                        &templates::LOBBY_TIMEOUT,
                        &ReplyVars::new(),
                    )));
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
                    deliveries.push(UndercoverDelivery::KeyedHall(message));
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
                deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                    &templates::DELIVERY_TIMEOUT,
                    &ReplyVars::new(),
                )));
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
                    deliveries.push(UndercoverDelivery::KeyedHallBatch(settlement_lines(
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
                    deliveries.push(UndercoverDelivery::KeyedHallBatch(settlement_lines(
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

fn compact_status(game: &Playing) -> UndercoverReply {
    let (phase, eligible, completed) = match &game.phase {
        Phase::AwaitingDelivery => (
            UndercoverReply::keyed(&templates::PHASE_DESCRIBING, &ReplyVars::new()),
            alive_indices(&game.players),
            BTreeSet::new(),
        ),
        Phase::Describing { descriptions } => (
            UndercoverReply::keyed(&templates::PHASE_DESCRIBING, &ReplyVars::new()),
            alive_indices(&game.players),
            descriptions.keys().copied().collect(),
        ),
        Phase::Voting { .. } => {
            let eligible = alive_indices(&game.players);
            let completed = eligible.iter().copied().collect();
            (
                UndercoverReply::keyed(&templates::PHASE_VOTING, &ReplyVars::new()),
                eligible,
                completed,
            )
        }
        Phase::RunoffDescribing {
            candidates,
            descriptions,
        } => (
            UndercoverReply::keyed(&templates::PHASE_DESCRIBING, &ReplyVars::new()),
            candidates
                .iter()
                .copied()
                .filter(|index| game.players[*index].alive)
                .collect(),
            descriptions.keys().copied().collect(),
        ),
        Phase::RunoffVoting { candidates, .. } => {
            let eligible = alive_indices(&game.players)
                .into_iter()
                .filter(|index| !candidates.contains(index))
                .collect::<Vec<_>>();
            let completed = eligible.iter().copied().collect();
            (
                UndercoverReply::keyed(&templates::PHASE_VOTING, &ReplyVars::new()),
                eligible,
                completed,
            )
        }
    };
    let positions = eligible
        .iter()
        .map(|index| {
            let position = game.players[*index].position;
            if completed.contains(index) {
                position
            } else {
                position.to_ascii_lowercase()
            }
        })
        .collect::<String>();
    let mut fragments = vec![("阶段", phase)];
    if matches!(
        game.phase,
        Phase::Voting { .. } | Phase::RunoffVoting { .. }
    ) {
        fragments.push(("投票说明", vote_instruction()));
    }
    UndercoverReply::Composed {
        spec: &templates::STATUS_PLAYING,
        vars: ReplyVars::new()
            .with("模式", game.mode.label())
            .with("轮次", game.round.to_string())
            .with("位置", positions),
        fragments,
    }
}

fn alive_indices(players: &[Player]) -> Vec<usize> {
    players
        .iter()
        .enumerate()
        .filter_map(|(index, player)| player.alive.then_some(index))
        .collect()
}

fn remaining_seconds(started: Instant, timeout_seconds: u64, now: Instant) -> u64 {
    timeout_seconds
        .max(1)
        .saturating_sub(now.saturating_duration_since(started).as_secs())
}

fn missing_vote_message(
    players: &[Player],
    votes: &Votes,
    excluded_voters: Option<&[usize]>,
) -> Option<UndercoverReply> {
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
    (!positions.is_empty()).then(|| {
        UndercoverReply::keyed(
            &templates::VOTE_MISSING,
            &ReplyVars::new().with("位置列表", positions.join("、")),
        )
    })
}

fn resolve_description_timeout(
    game: &mut Playing,
    descriptions: BTreeMap<usize, String>,
    runoff_candidates: Option<Vec<usize>>,
    now: Instant,
    deliveries: &mut Vec<UndercoverDelivery>,
) -> bool {
    if descriptions.is_empty() {
        deliveries.push(UndercoverDelivery::KeyedHallBatch(settlement_lines(
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
            deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::Composed {
                spec: &templates::DESCRIBE_TIMEOUT,
                vars: ReplyVars::new().with("位置", game.players[index].position.to_string()),
                fragments: vec![("身份", game.players[index].role.reply())],
            }));
        }
    }
    if let Some(result) = winner(game) {
        deliveries.push(UndercoverDelivery::KeyedHallBatch(settlement_lines(
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
            deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                &templates::RUNOFF_DESCRIBE_TIMEOUT,
                &ReplyVars::new().with("轮次", game.round.to_string()),
            )));
        } else {
            game.phase = Phase::RunoffVoting {
                candidates,
                votes: BTreeMap::new(),
                reminder: VoteReminder::new(now),
            };
            deliveries.push(UndercoverDelivery::KeyedHall(with_vote_instruction(
                &templates::RUNOFF_DESCRIBE_CUTOFF,
            )));
        }
    } else {
        game.phase = Phase::Voting {
            votes: BTreeMap::new(),
            reminder: VoteReminder::new(now),
        };
        deliveries.push(UndercoverDelivery::KeyedHall(with_vote_instruction(
            &templates::DESCRIBE_CUTOFF,
        )));
    }
    false
}

fn resolve_votes(
    game: &mut Playing,
    votes: Votes,
    runoff_candidates: Option<Vec<usize>>,
    deliveries: &mut Vec<UndercoverDelivery>,
) -> bool {
    let leaders = highest_targets(&votes);
    if leaders.len() > 1 {
        if runoff_candidates.is_some() {
            start_next_round(game);
            deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                &templates::RUNOFF_TIED,
                &ReplyVars::new().with("轮次", game.round.to_string()),
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
            deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
                &templates::RUNOFF_ENTER,
                &ReplyVars::new().with("位置列表", positions),
            )));
        }
        return false;
    }
    let Some(target) = leaders.first().copied() else {
        start_next_round(game);
        deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
            &templates::VOTE_NONE,
            &ReplyVars::new().with("轮次", game.round.to_string()),
        )));
        return false;
    };
    game.players[target].alive = false;
    deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::Composed {
        spec: &templates::ELIMINATED,
        vars: ReplyVars::new().with("位置", game.players[target].position.to_string()),
        fragments: vec![("身份", game.players[target].role.reply())],
    }));
    if let Some(result) = winner(game) {
        deliveries.push(UndercoverDelivery::KeyedHallBatch(settlement_lines(
            game, result,
        )));
        true
    } else {
        start_next_round(game);
        deliveries.push(UndercoverDelivery::KeyedHall(UndercoverReply::keyed(
            &templates::ROUND_START,
            &ReplyVars::new().with("轮次", game.round.to_string()),
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
    fn reply(self) -> UndercoverReply {
        let spec = match self {
            Self::Civilian => &templates::WINNER_CIVILIAN,
            Self::Undercover => &templates::WINNER_UNDERCOVER,
            Self::None => &templates::WINNER_NONE,
        };
        UndercoverReply::keyed(spec, &ReplyVars::new())
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

fn highest_targets(votes: &Votes) -> Vec<usize> {
    let mut counts = BTreeMap::<usize, usize>::new();
    for target in votes.values().flatten() {
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

fn position_mapping_lines(players: &[Player]) -> Vec<UndercoverReply> {
    players
        .chunks(3)
        .map(|chunk| {
            let mut fragments = Vec::with_capacity(chunk.len() * 2 - 1);
            for (index, player) in chunk.iter().enumerate() {
                if index > 0 {
                    fragments.push(UndercoverReply::Raw("|".to_string()));
                }
                fragments.push(UndercoverReply::keyed(
                    &templates::POSITION_MAPPING,
                    &ReplyVars::new()
                        .with("位置", player.position.to_string())
                        .with("昵称", player.name.as_str()),
                ));
            }
            UndercoverReply::Joined { fragments }
        })
        .collect()
}

fn settlement_lines(game: &Playing, winner: Winner) -> Vec<UndercoverReply> {
    let mut lines = vec![UndercoverReply::Composed {
        spec: &templates::SETTLEMENT_HEADER,
        vars: ReplyVars::new()
            .with("平民词", game.words.civilian.as_str())
            .with("卧底词", game.words.undercover.as_str()),
        fragments: vec![("胜方", winner.reply())],
    }];
    lines.extend(game.players.chunks(3).map(|chunk| {
        let mut fragments = Vec::with_capacity(chunk.len() * 2 - 1);
        for (index, player) in chunk.iter().enumerate() {
            if index > 0 {
                fragments.push(UndercoverReply::Raw("|".to_string()));
            }
            fragments.push(UndercoverReply::Composed {
                spec: &templates::SETTLEMENT_PLAYER,
                vars: ReplyVars::new()
                    .with("位置", player.position.to_string())
                    .with("昵称", player.name.as_str()),
                fragments: vec![("身份", player.role.reply())],
            });
        }
        UndercoverReply::Joined { fragments }
    }));
    lines.push(UndercoverReply::keyed(
        &templates::SETTLEMENT_ROUNDS,
        &ReplyVars::new().with("轮次", game.round.to_string()),
    ));
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

    fn delivery_default_text(deliveries: &[UndercoverDelivery]) -> Vec<String> {
        deliveries
            .iter()
            .flat_map(|delivery| match delivery {
                UndercoverDelivery::Hall(message) => vec![message.clone()],
                UndercoverDelivery::HallBatch(messages) => messages.clone(),
                UndercoverDelivery::Friend { .. } | UndercoverDelivery::KeyedFriend(_) => {
                    Vec::new()
                }
                UndercoverDelivery::KeyedHall(reply) => vec![reply.render_default()],
                UndercoverDelivery::KeyedHallBatch(replies) => replies
                    .iter()
                    .map(UndercoverReply::render_default)
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn position_mapping_preserves_order_pipe_separators_and_keyed_entries() {
        let players = ['A', 'B', 'C', 'D']
            .into_iter()
            .zip(["甲", "乙", "丙", "丁"])
            .map(|(position, name)| {
                let mut player = Player::lobby(name);
                player.position = position;
                player
            })
            .collect::<Vec<_>>();

        let lines = position_mapping_lines(&players);
        assert_eq!(
            lines
                .iter()
                .map(UndercoverReply::render_default)
                .collect::<Vec<_>>(),
            ["A=甲|B=乙|C=丙", "D=丁"]
        );
        for (line, chunk) in lines.iter().zip(players.chunks(3)) {
            let UndercoverReply::Joined { fragments } = line else {
                panic!("position mappings must retain joined keyed entries");
            };
            assert_eq!(fragments.len(), chunk.len() * 2 - 1);
            for (index, player) in chunk.iter().enumerate() {
                assert!(
                    matches!(&fragments[index * 2], UndercoverReply::Keyed { spec, vars }
                    if spec.key == templates::POSITION_MAPPING.key
                        && vars == &ReplyVars::new()
                            .with("位置", player.position.to_string())
                            .with("昵称", player.name.as_str()))
                );
                if index > 0 {
                    assert_eq!(
                        fragments[index * 2 - 1],
                        UndercoverReply::Raw("|".to_string())
                    );
                }
            }
        }
    }

    #[test]
    fn abstain_is_a_private_vote_command_and_never_a_public_description() {
        assert_eq!(
            UndercoverCommand::parse_friend("弃 权"),
            Some(UndercoverCommand::Abstain)
        );
        assert_eq!(
            UndercoverCommand::parse_friend("谜 底"),
            Some(UndercoverCommand::Reveal)
        );
        assert_eq!(UndercoverCommand::parse_hall("弃权"), None);
    }

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
        assert!(deliveries.iter().all(|delivery| {
            let UndercoverDelivery::KeyedFriend(delivery) = delivery else {
                return false;
            };
            let message = delivery.reply.render_default();
            matches!(&delivery.reply, UndercoverReply::Keyed { spec, .. }
                if spec.key == templates::SECRET.key)
                && message.contains("你的位置：")
                && message.contains("你的词语：")
                && !message.contains("平民")
                && !message.contains("卧底")
        }));
    }

    #[test]
    fn status_uses_compact_phase_and_speech_case_markers() {
        let now = Instant::now();
        let mut game = enabled_game(11);
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }
        let players = game
            .start(UndercoverWordPair::new("苹果", "梨"), now)
            .unwrap()
            .into_iter()
            .filter_map(|delivery| match delivery {
                UndercoverDelivery::KeyedFriend(delivery) => Some((
                    delivery.recipient,
                    message_position(&delivery.reply.render_default()),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            game.status("旁观者", now).render_default(),
            "单卧底,第1轮,发言,abcd"
        );
        game.complete_delivery(now).unwrap();

        let player_a = players
            .iter()
            .find(|(_, position)| *position == 'A')
            .map(|(player, _)| player.clone())
            .expect("position A");
        game.describe(&player_a, "第一条描述", now).unwrap();
        assert_eq!(
            game.status("旁观者", now).render_default(),
            "单卧底,第1轮,发言,Abcd"
        );

        for (index, (player, _)) in players.iter().enumerate() {
            if player != &player_a {
                game.describe(player, &format!("描述{}", index), now)
                    .unwrap();
            }
        }
        assert_eq!(
            game.status("旁观者", now).render_default(),
            "单卧底,第1轮,投票,ABCD"
        );

        game.vote(&player_a, 'B', now).unwrap();
        assert_eq!(
            game.status("旁观者", now).render_default(),
            "单卧底,第1轮,投票,ABCD"
        );
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
                UndercoverDelivery::KeyedFriend(delivery)
                    if delivery.reply.render_default().contains("梨") =>
                {
                    Some((
                        delivery.recipient.clone(),
                        message_position(&delivery.reply.render_default()),
                    ))
                }
                _ => None,
            })
            .unwrap();
        let participants = words
            .iter()
            .filter_map(|delivery| match delivery {
                UndercoverDelivery::KeyedFriend(delivery) => Some((
                    delivery.recipient.clone(),
                    message_position(&delivery.reply.render_default()),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();

        let opening = game.complete_delivery(now).unwrap();
        assert!(
            matches!(&opening[0], UndercoverDelivery::KeyedHallBatch(lines)
            if matches!(lines.first(), Some(UndercoverReply::Keyed { spec, .. }) if spec.key == templates::OPENING.key)
                && lines.len() == 4
                && lines[1..3].iter().all(|reply| matches!(reply, UndercoverReply::Joined { .. }))
                && matches!(lines.last(), Some(UndercoverReply::Keyed { spec, .. }) if spec.key == templates::DESCRIBE_PROMPT.key))
        );

        for (index, (player, _)) in participants.iter().enumerate() {
            let deliveries = game
                .describe(player, &format!("第{}条描述", index + 1), now)
                .unwrap();
            if index < participants.len() - 1 {
                assert!(deliveries.is_empty());
            } else {
                assert_eq!(
                    delivery_default_text(&deliveries),
                    vec!["所有存活玩家已描述，请好友私聊#怀疑对象编号，例如#A，或发送#弃权"]
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
            UndercoverDelivery::KeyedHallBatch(lines)
                if lines.first().is_some_and(|line| line.render_default().contains("平民胜利"))
                    && lines.last().is_some_and(|line| line.render_default() == "共进行1轮"))));
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
                UndercoverDelivery::KeyedFriend(delivery) => Some((
                    delivery.recipient.clone(),
                    message_position(&delivery.reply.render_default()),
                )),
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
            UndercoverDelivery::KeyedHall(UndercoverReply::Keyed { spec, .. })
                if spec.key == templates::RUNOFF_ENTER.key)));

        game.describe(&players[0].0, "补充描述一", now).unwrap();
        let completed = game.describe(&players[1].0, "补充描述二", now).unwrap();
        assert_eq!(
            delivery_default_text(&completed),
            vec![
                "并列玩家已完成公屏描述，其他存活玩家请好友私聊#怀疑对象编号，例如#A，或发送#弃权"
            ]
        );

        let voters = [&players[2], &players[3]];
        game.vote(&voters[0].0, first, now).unwrap();
        let tied = game.vote(&voters[1].0, second, now).unwrap();
        assert!(tied.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::KeyedHall(UndercoverReply::Keyed { spec, vars })
                if spec.key == templates::RUNOFF_TIED.key && vars.get("轮次") == Some("2"))));
    }

    #[test]
    fn runoff_with_no_eligible_voters_advances_to_next_round() {
        let now = Instant::now();
        let mut game = enabled_game(19);
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
                UndercoverDelivery::KeyedFriend(delivery) => Some((
                    delivery.recipient.clone(),
                    message_position(&delivery.reply.render_default()),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        game.complete_delivery(now).unwrap();
        for (index, (player, _)) in players.iter().enumerate() {
            game.describe(player, &format!("描述{index}"), now).unwrap();
        }

        let first = players[0].1;
        let second = players[1].1;
        for ((player, _), target) in players.iter().zip([second, first, first, second]) {
            game.vote(player, target, now).unwrap();
        }

        let (first_name, second_name) = {
            let GameState::Playing(playing) = &mut game.state else {
                panic!("tie should enter runoff description");
            };
            playing.players[2].alive = false;
            playing.players[3].alive = false;
            let candidates = match &playing.phase {
                Phase::RunoffDescribing { candidates, .. } => candidates.clone(),
                _ => panic!("tie should enter runoff description"),
            };
            let first_candidate = candidates[0];
            let second_candidate = candidates[1];
            (
                playing.players[first_candidate].name.clone(),
                playing.players[second_candidate].name.clone(),
            )
        };
        game.describe(&first_name, "补充描述一", now).unwrap();
        let result = game.describe(&second_name, "补充描述二", now).unwrap();

        assert!(result.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::KeyedHall(UndercoverReply::Keyed { spec, vars })
                if spec.key == templates::RUNOFF_NO_VOTERS.key && vars.get("轮次") == Some("2"))));
        assert!(matches!(
            game.state,
            GameState::Playing(Playing {
                phase: Phase::Describing { .. },
                round: 2,
                ..
            })
        ));
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
                UndercoverDelivery::KeyedFriend(delivery) => Some((
                    delivery.recipient.clone(),
                    message_position(&delivery.reply.render_default()),
                )),
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
        let UndercoverDelivery::KeyedHall(reply) = &first_reminder[0] else {
            panic!("expected hall reminder");
        };
        let message = reply.render_default();
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
    fn all_players_abstaining_completes_the_vote_without_eliminating_anyone() {
        let now = Instant::now();
        let mut game = enabled_game(47);
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }
        let players = game
            .start(UndercoverWordPair::new("苹果", "梨"), now)
            .unwrap()
            .into_iter()
            .filter_map(|delivery| match delivery {
                UndercoverDelivery::KeyedFriend(delivery) => Some((
                    delivery.recipient,
                    message_position(&delivery.reply.render_default()),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        game.complete_delivery(now).unwrap();
        for (index, (player, _)) in players.iter().enumerate() {
            game.describe(player, &format!("描述{index}"), now).unwrap();
        }

        game.abstain(&players[0].0, now).unwrap();
        assert!(game.vote(&players[0].0, players[1].1, now).is_err());
        for (player, _) in players.iter().skip(1).take(players.len() - 2) {
            assert!(game.abstain(player, now).unwrap().is_empty());
        }
        let result = game.abstain(&players.last().unwrap().0, now).unwrap();

        assert!(result.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::KeyedHall(reply)
                if reply.render_default().contains("本轮无人获得票数")
                    && reply.render_default().contains("第2轮"))));
        let GameState::Playing(playing) = &game.state else {
            panic!("all abstentions must keep the game active");
        };
        assert_eq!(playing.round, 2);
        assert!(playing.players.iter().all(|player| player.alive));
        assert!(matches!(playing.phase, Phase::Describing { .. }));
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
            UndercoverDelivery::KeyedFriend(delivery) => delivery.recipient.clone(),
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
            UndercoverDelivery::KeyedHallBatch(lines)
                if lines.first().is_some_and(|line| line.render_default().contains("谁是卧底结束")))));

        let _ = game.status("甲", now + std::time::Duration::from_secs(290));

        let settled = game.tick(now + std::time::Duration::from_secs(300));
        assert!(!game.is_active());
        assert!(settled.iter().any(|delivery| matches!(delivery,
            UndercoverDelivery::KeyedHallBatch(lines)
                if lines.first().is_some_and(|line| line.render_default().contains("谁是卧底结束")))));
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
                UndercoverDelivery::KeyedFriend(delivery) => Some((
                    delivery.recipient.clone(),
                    message_position(&delivery.reply.render_default()),
                    delivery.reply.render_default().contains("梨"),
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
                    UndercoverDelivery::KeyedHallBatch(lines)
                        if lines.first().is_some_and(|line| line.render_default().contains("卧底胜利")))));
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
                UndercoverDelivery::KeyedFriend(delivery) => Some((
                    delivery.recipient.clone(),
                    message_position(&delivery.reply.render_default()),
                    delivery.reply.render_default().contains("梨"),
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
            UndercoverDelivery::KeyedHallBatch(lines)
                if lines.first().is_some_and(|line| line.render_default().contains("平民胜利")))));
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

    #[test]
    fn reveal_is_read_only_and_rejects_current_or_eliminated_players() {
        let now = Instant::now();
        let mut game = enabled_game(43);
        game.create("甲", UndercoverMode::Single, now).unwrap();
        for player in ["乙", "丙", "丁"] {
            game.join(player, now).unwrap();
        }
        game.start(UndercoverWordPair::new("绝密平民词", "绝密卧底词"), now)
            .unwrap();
        game.complete_delivery(now).unwrap();

        let before = serde_json::to_string(&game.snapshot(now)).unwrap();
        let deadline = game.next_deadline(now, true);
        let lines = game.reveal("旁观者").unwrap();
        let answer = lines
            .iter()
            .map(UndercoverReply::render_default)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(lines.len(), 1);
        assert!(answer.starts_with("平民词:绝密平民词,卧底词:绝密卧底词,卧底:"));
        assert!(!answer.contains("内鬼位置"));
        assert!(!answer.contains("仅私聊"));
        assert!(!answer.contains("存活"));
        assert!(!answer.contains("淘汰"));
        assert!(!answer.contains('='));
        assert_eq!(before, serde_json::to_string(&game.snapshot(now)).unwrap());
        assert_eq!(deadline, game.next_deadline(now, true));

        assert!(game.reveal("甲").is_err());
        if let GameState::Playing(playing) = &mut game.state {
            playing
                .players
                .iter_mut()
                .find(|player| player.name == "甲")
                .expect("creator remains in the game roster")
                .alive = false;
        } else {
            panic!("game should still be playing");
        }
        assert!(game.reveal("甲").is_err());
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
