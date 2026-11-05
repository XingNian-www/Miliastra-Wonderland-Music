use std::collections::BTreeSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
use crate::features::entertainment::{AcquireOutcome, EntertainmentKind, EntertainmentState};
use crate::features::reply_template::{ReplyTemplate, ReplyVars, number_bomb as templates};

/// 一条待发送的数字炸弹文案。
///
/// 数字炸弹只有公屏结果，没有私聊/复合回复，因此保留模板与变量到组装层渲染：
/// 命令面板的覆盖与静音才能在真正发送时生效。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NumberBombReply {
    pub(crate) spec: &'static ReplyTemplate,
    pub(crate) vars: ReplyVars,
}

impl NumberBombReply {
    fn keyed(spec: &'static ReplyTemplate, vars: ReplyVars) -> Self {
        Self { spec, vars }
    }

    /// 测试用默认渲染；生产发送必须走组装层，才能读配置覆盖。
    #[cfg(test)]
    pub(crate) fn render_default(&self) -> String {
        crate::features::reply_template::render(self.spec, None, &self.vars).text
    }
}

/// 一条命令或一次超时推进产生的全部回复。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NumberBombCommandOutcome {
    pub(crate) action: &'static str,
    pub(crate) replies: Vec<NumberBombReply>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NumberBombTimeout {
    pub(crate) replies: Vec<NumberBombReply>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum NumberBombCommand {
    /// 大厅创建房间，发起人自动加入。
    Start,
    /// 加入等待中的房间。
    Join,
    /// 发起人开局。
    StartGame,
    /// 显示当前范围与轮到谁。
    Status,
    /// 发起人结束整局并释放娱乐占用。
    End,
    /// 发起人保留成员重开一局。
    Retry,
    /// 大厅里的一次猜测：必须带井号，例如 #42。
    Guess(u32),
    /// 当前玩家发了非数字内容；只提示本人，不消耗回合。
    Invalid,
}

impl NumberBombCommand {
    pub(crate) fn claims_start_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && envelope.authority() == CommandAuthority::HallMember
            && crate::features::chat_text::compact_command(envelope.command_text()) == "数字炸弹"
    }

    pub(crate) fn claims_active_chat(envelope: &CommandEnvelope) -> bool {
        if envelope.prefix() != CommandPrefix::Hash
            || envelope.authority() != CommandAuthority::HallMember
        {
            return false;
        }
        // 进行中由本玩法接管大厅的 # 输入：固定关键词、数字和“非数字提示”
        // 都必须在同一张表里认领，否则非数字会被静默丢弃。
        true
    }

    /// 只认固定控制词和数字，用于“另一个玩法进行中”时的日志归类；
    /// 实际会话认领故意更宽（任何 # 文本都要走非数字提示）。
    pub(crate) fn claims_recognized_session_chat(envelope: &CommandEnvelope) -> bool {
        if !Self::claims_active_chat(envelope) {
            return false;
        }
        let compact = crate::features::chat_text::compact_command(envelope.command_text());
        matches!(compact.as_str(), "加入" | "开始" | "状态" | "结束" | "重来")
            || parse_guess(&compact).is_some()
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
        let command = Self::parse_hall(envelope.command_text())?;
        Some(FeatureCommandMatch::new(
            "#",
            envelope.command_text(),
            command,
        ))
    }

    pub(crate) fn parse_start(payload: &str) -> Option<Self> {
        (crate::features::chat_text::compact_command(payload) == "数字炸弹").then_some(Self::Start)
    }

    pub(crate) fn parse_hall(payload: &str) -> Option<Self> {
        let compact = crate::features::chat_text::compact_command(payload);
        Some(match compact.as_str() {
            "加入" => Self::Join,
            "开始" => Self::StartGame,
            "状态" => Self::Status,
            "结束" => Self::End,
            "重来" => Self::Retry,
            _ => match parse_guess(&compact) {
                Some(number) => Self::Guess(number),
                None => Self::Invalid,
            },
        })
    }

    /// 大厅纯数字猜测的入口。
    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Start => "number_bomb:start".to_string(),
            Self::Join => "number_bomb:join".to_string(),
            Self::StartGame => "number_bomb:begin".to_string(),
            Self::Status => "number_bomb:status".to_string(),
            Self::End => "number_bomb:end".to_string(),
            Self::Retry => "number_bomb:retry".to_string(),
            Self::Guess(number) => format!("number_bomb:guess:{number}"),
            Self::Invalid => "number_bomb:invalid".to_string(),
        }
    }
}

/// 解析猜测里的数字；0 也先交给业务范围校验，避免解析层与配置下界各写一套门槛。
fn parse_guess(value: &str) -> Option<u32> {
    if value.is_empty() || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NumberBombConfig {
    pub enabled: bool,
    pub min: u32,
    pub max: u32,
    pub turn_timeout_seconds: u64,
    pub max_players: usize,
    pub min_players: usize,
    /// 超过 5 人后每多 1 人给上限加多少。
    pub extra_range_per_player: u32,
    /// 上限的绝对封顶值；人数再多也不会超过它。
    pub upper_bound_cap: u32,
}

impl Default for NumberBombConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min: 1,
            max: 100,
            turn_timeout_seconds: 60,
            max_players: 11,
            min_players: 2,
            extra_range_per_player: 100,
            upper_bound_cap: 1000,
        }
    }
}

impl NumberBombConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.min >= self.max {
            bail!("number_bomb.min 必须小于 number_bomb.max");
        }
        if self.min_players < 2 {
            bail!("number_bomb.min_players 必须至少为 2");
        }
        if self.max_players < self.min_players {
            bail!("number_bomb.max_players 不能小于 number_bomb.min_players");
        }
        if self.turn_timeout_seconds == 0 {
            bail!("number_bomb.turn_timeout_seconds 必须大于 0");
        }
        if self.upper_bound_cap < self.max {
            bail!("number_bomb.upper_bound_cap 不能小于 number_bomb.max");
        }
        Ok(())
    }
}

/// 房间阶段。没有房间时由 `session: None` 表达 Idle，
/// 因此不需要一个永远不会被构造的 Idle 变体。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Lobby,
    Playing,
}

#[derive(Clone, Debug)]
struct Player {
    /// 身份比较键（已登记身份是 UUID），绝不渲染进文案。
    key: String,
    /// 展示名，只用于渲染。
    display: String,
}

impl Player {
    fn new(key: &str, display: &str) -> Self {
        let display = display.trim();
        Self {
            key: player_key(key),
            // 展示名缺失时也不能回退到内部键，否则 UUID 会进玩家可见文案。
            display: if display.is_empty() {
                "玩家".to_string()
            } else {
                display.to_string()
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RoundResult {
    loser: usize,
    bomb: u32,
}

#[derive(Clone, Debug)]
struct Session {
    creator_key: String,
    players: Vec<Player>,
    phase: Phase,
    lower: u32,
    upper: u32,
    bomb: Option<u32>,
    guessed: BTreeSet<u32>,
    turn: usize,
    timer: ActiveTimer,
    last_result: Option<RoundResult>,
}

#[derive(Clone, Copy, Debug)]
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

    /// 只在 active 时累计：调度繁忙或程序暂停时，不能把真实等待算成玩家超时。
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

pub(crate) struct NumberBombService {
    config: NumberBombConfig,
    session: Option<Session>,
    rng: SplitMix64,
}

impl NumberBombService {
    pub(crate) fn new(config: NumberBombConfig) -> Self {
        Self::with_seed(config, random_seed())
    }

    /// 带显式种子的构造：生产用 new()（随机种子），测试用它固定随机源。
    ///
    /// 不能加 #[cfg(test)]：new() 本身要调它，只在测试配置里存在会让
    /// 非测试构建直接编译失败（发布构建就是非测试配置）。
    pub(crate) fn with_seed(config: NumberBombConfig, seed: u64) -> Self {
        Self {
            config,
            session: None,
            rng: SplitMix64::new(seed),
        }
    }

    pub(crate) fn handle_command(
        &mut self,
        entertainment: &mut EntertainmentState,
        player: &str,
        player_display: &str,
        command: &NumberBombCommand,
        now: Instant,
    ) -> Result<NumberBombCommandOutcome> {
        let outcome = match command {
            NumberBombCommand::Start => {
                self.start_room(entertainment, player, player_display, now)?
            }
            NumberBombCommand::Join => self.join(player, player_display),
            NumberBombCommand::StartGame => self.start_game(player, now),
            NumberBombCommand::Status => self.status(),
            NumberBombCommand::End => self.end(entertainment, player),
            NumberBombCommand::Retry => self.retry(player, now),
            NumberBombCommand::Guess(number) => self.guess(player, *number, now),
            NumberBombCommand::Invalid => self.invalid_guess(player),
        };
        Ok(outcome)
    }

    /// 回合超时推进。返回的是这批应发送的文案；状态已在调用线程内推进，
    /// 调用方只负责把文案送出去，不需要额外的 claim/resume 协议。
    pub(crate) fn tick(&mut self, now: Instant, clock_active: bool) -> Option<NumberBombTimeout> {
        let session = self.session.as_mut()?;
        if session.phase != Phase::Playing {
            return None;
        }
        session.timer.tick(now, clock_active);
        let timeout = Duration::from_secs(self.config.turn_timeout_seconds);
        if session.timer.elapsed_at(now, clock_active) < timeout {
            return None;
        }
        let player = session.players.get(session.turn)?.display.clone();
        let number = random_available(&mut self.rng, session);
        let mut replies = vec![NumberBombReply::keyed(
            &templates::TIMEOUT_AUTO,
            ReplyVars::new()
                .with("玩家", player)
                .with("数字", number.to_string()),
        )];
        replies.extend(apply_guess(&self.config, session, number, now));
        Some(NumberBombTimeout { replies })
    }

    pub(crate) fn abort(&mut self, entertainment: &mut EntertainmentState) -> bool {
        let aborted = self.session.take().is_some();
        if aborted {
            entertainment.release(EntertainmentKind::NumberBomb);
        }
        aborted
    }

    fn start_room(
        &mut self,
        entertainment: &mut EntertainmentState,
        player: &str,
        player_display: &str,
        now: Instant,
    ) -> Result<NumberBombCommandOutcome> {
        if !self.config.enabled {
            return Ok(action(
                "disabled",
                vec![keyed(&templates::DISABLED, ReplyVars::new())],
            ));
        }
        if self.session.is_some() {
            return Ok(action(
                "already-active",
                vec![keyed(&templates::ALREADY_ACTIVE, ReplyVars::new())],
            ));
        }
        match entertainment.try_acquire(EntertainmentKind::NumberBomb)? {
            AcquireOutcome::Acquired => {}
            AcquireOutcome::AlreadyOwned => {
                return Ok(action(
                    "already-active",
                    vec![keyed(&templates::ALREADY_ACTIVE, ReplyVars::new())],
                ));
            }
            AcquireOutcome::Occupied(kind) => {
                return Ok(action(
                    "occupied",
                    vec![keyed(
                        &templates::OCCUPIED,
                        ReplyVars::new().with("其他玩法", kind.label()),
                    )],
                ));
            }
        }
        let creator = Player::new(player, player_display);
        self.session = Some(Session {
            creator_key: creator.key.clone(),
            players: vec![creator.clone()],
            phase: Phase::Lobby,
            lower: self.config.min,
            upper: self.config.max,
            bomb: None,
            guessed: BTreeSet::new(),
            turn: 0,
            timer: ActiveTimer::new(now),
            last_result: None,
        });
        Ok(action(
            "lobby-created",
            vec![keyed(
                &templates::LOBBY_CREATED,
                ReplyVars::new()
                    .with("玩家", creator.display)
                    .with("人数", 1usize.to_string())
                    .with("上限", self.config.max_players.to_string())
                    .with("最少人数", self.config.min_players.to_string()),
            )],
        ))
    }

    fn join(&mut self, player: &str, player_display: &str) -> NumberBombCommandOutcome {
        let Some(session) = self.session.as_mut() else {
            return action(
                "no-session",
                vec![keyed(&templates::NO_SESSION, ReplyVars::new())],
            );
        };
        let player = Player::new(player, player_display);
        if session.phase == Phase::Playing {
            return action(
                "already-started",
                vec![keyed(&templates::LOBBY_ALREADY_STARTED, ReplyVars::new())],
            );
        }
        if session.players.iter().any(|item| item.key == player.key) {
            return action(
                "already-joined",
                vec![keyed(
                    &templates::LOBBY_ALREADY_JOINED,
                    ReplyVars::new().with("玩家", player.display),
                )],
            );
        }
        if session.players.len() >= self.config.max_players {
            return action(
                "full",
                vec![keyed(
                    &templates::LOBBY_FULL,
                    ReplyVars::new().with("上限", self.config.max_players.to_string()),
                )],
            );
        }
        session.players.push(player.clone());
        action(
            "joined",
            vec![keyed(
                &templates::LOBBY_JOINED,
                ReplyVars::new()
                    .with("玩家", player.display)
                    .with("人数", session.players.len().to_string())
                    .with("上限", self.config.max_players.to_string()),
            )],
        )
    }

    fn start_game(&mut self, player: &str, now: Instant) -> NumberBombCommandOutcome {
        let Some(session) = self.session.as_ref() else {
            return action(
                "no-session",
                vec![keyed(&templates::NO_SESSION, ReplyVars::new())],
            );
        };
        if session.creator_key != player_key(player) {
            return only_creator();
        }
        if session.phase == Phase::Playing {
            return action(
                "already-playing",
                vec![keyed(&templates::START_ALREADY_PLAYING, ReplyVars::new())],
            );
        }
        if session.players.len() < self.config.min_players {
            return too_few(session.players.len(), &self.config);
        }
        if session.players.len() > self.config.max_players {
            return too_many(session.players.len(), &self.config);
        }
        self.start_round(now)
    }

    fn retry(&mut self, player: &str, now: Instant) -> NumberBombCommandOutcome {
        let Some(session) = self.session.as_ref() else {
            return action(
                "no-session",
                vec![keyed(&templates::NO_SESSION, ReplyVars::new())],
            );
        };
        if session.creator_key != player_key(player) {
            return only_creator();
        }
        if session.phase == Phase::Playing {
            return action(
                "already-playing",
                vec![keyed(&templates::START_ALREADY_PLAYING, ReplyVars::new())],
            );
        }
        if session.players.len() < self.config.min_players {
            return too_few(session.players.len(), &self.config);
        }
        if session.players.len() > self.config.max_players {
            return too_many(session.players.len(), &self.config);
        }
        self.start_round(now)
    }

    fn start_round(&mut self, now: Instant) -> NumberBombCommandOutcome {
        let config = self.config.clone();
        let Some(session) = self.session.as_mut() else {
            return action(
                "no-session",
                vec![keyed(&templates::NO_SESSION, ReplyVars::new())],
            );
        };
        let upper = range_upper_bound(&config, session.players.len());
        let bomb = self.rng.gen_range(config.min, upper);
        session.phase = Phase::Playing;
        session.lower = config.min;
        session.upper = upper;
        session.bomb = Some(bomb);
        session.guessed.clear();
        session.turn = 0;
        session.last_result = None;
        session.timer.reset(now);
        let player = session
            .players
            .first()
            .map(|item| item.display.clone())
            .unwrap_or_else(|| "玩家".to_string());
        let players = session.players.len();
        action(
            "started",
            vec![keyed(
                &templates::STARTED,
                ReplyVars::new()
                    .with("下限", config.min.to_string())
                    .with("上限", upper.to_string())
                    .with("人数", players.to_string())
                    .with("玩家", player),
            )],
        )
    }

    fn status(&self) -> NumberBombCommandOutcome {
        let Some(session) = self.session.as_ref() else {
            return action(
                "no-session",
                vec![keyed(&templates::STATUS_NO_SESSION, ReplyVars::new())],
            );
        };
        match session.phase {
            Phase::Playing => action(
                "status-playing",
                vec![keyed(
                    &templates::STATUS_PLAYING,
                    ReplyVars::new()
                        .with("下限", session.lower.to_string())
                        .with("上限", session.upper.to_string())
                        .with(
                            "玩家",
                            session
                                .players
                                .get(session.turn)
                                .map(|item| item.display.clone())
                                .unwrap_or_else(|| "玩家".to_string()),
                        )
                        .with("人数", session.players.len().to_string()),
                )],
            ),
            _ => {
                if let Some(result) = session.last_result {
                    return action(
                        "status-settled",
                        vec![keyed(
                            &templates::STATUS_SETTLED,
                            ReplyVars::new()
                                .with("炸弹数字", result.bomb.to_string())
                                .with(
                                    "玩家",
                                    session
                                        .players
                                        .get(result.loser)
                                        .map(|item| item.display.clone())
                                        .unwrap_or_else(|| "玩家".to_string()),
                                )
                                .with("人数", session.players.len().to_string())
                                .with("上限", self.config.max_players.to_string()),
                        )],
                    );
                }
                action(
                    "status-lobby",
                    vec![keyed(
                        &templates::STATUS_LOBBY,
                        ReplyVars::new()
                            .with("人数", session.players.len().to_string())
                            .with("上限", self.config.max_players.to_string())
                            .with(
                                "玩家列表",
                                session
                                    .players
                                    .iter()
                                    .map(|item| item.display.as_str())
                                    .collect::<Vec<_>>()
                                    .join("、"),
                            ),
                    )],
                )
            }
        }
    }

    fn end(
        &mut self,
        entertainment: &mut EntertainmentState,
        player: &str,
    ) -> NumberBombCommandOutcome {
        let Some(session) = self.session.as_ref() else {
            return action(
                "no-session",
                vec![keyed(&templates::END_NO_SESSION, ReplyVars::new())],
            );
        };
        // 结束不做发起人限制：房间没有大厅超时，若只允许发起人结束，
        // 发起人离开后整局会一直占着娱乐互斥位。参与者都能收尾。
        let Some(ender) = session
            .players
            .iter()
            .find(|item| item.key == player_key(player))
            .map(|item| item.display.clone())
        else {
            return action(
                "not-in-game",
                vec![keyed(&templates::GUESS_NOT_IN_GAME, ReplyVars::new())],
            );
        };
        self.session = None;
        entertainment.release(EntertainmentKind::NumberBomb);
        action(
            "ended",
            vec![keyed(
                &templates::END_DONE,
                ReplyVars::new().with("玩家", ender),
            )],
        )
    }

    /// 非数字只提示当前玩家；其他人闲聊不回应，避免整局把大厅所有话都变成报错。
    fn invalid_guess(&self, player: &str) -> NumberBombCommandOutcome {
        let Some(session) = self.session.as_ref() else {
            return action("ignored", Vec::new());
        };
        if session.phase != Phase::Playing
            || session
                .players
                .get(session.turn)
                .is_none_or(|current| current.key != player_key(player))
        {
            return action("ignored", Vec::new());
        }
        action(
            "not-a-number",
            vec![keyed(
                &templates::GUESS_NOT_NUMBER,
                ReplyVars::new()
                    .with("下限", session.lower.to_string())
                    .with("上限", session.upper.to_string()),
            )],
        )
    }

    fn guess(&mut self, player: &str, number: u32, now: Instant) -> NumberBombCommandOutcome {
        let key = player_key(player);
        let Some(session) = self.session.as_mut() else {
            return action(
                "no-session",
                vec![keyed(&templates::NO_SESSION, ReplyVars::new())],
            );
        };
        if session.phase != Phase::Playing {
            return action(
                "not-playing",
                vec![keyed(&templates::GUESS_NOT_PLAYING, ReplyVars::new())],
            );
        }
        let Some(player_index) = session.players.iter().position(|item| item.key == key) else {
            return action(
                "not-in-game",
                vec![keyed(&templates::GUESS_NOT_IN_GAME, ReplyVars::new())],
            );
        };
        if player_index != session.turn {
            let current = session
                .players
                .get(session.turn)
                .map(|item| item.display.clone())
                .unwrap_or_else(|| "玩家".to_string());
            return action(
                "not-your-turn",
                vec![keyed(
                    &templates::GUESS_NOT_YOUR_TURN,
                    ReplyVars::new().with("玩家", current),
                )],
            );
        }
        if session.guessed.contains(&number) {
            return action(
                "duplicate",
                vec![keyed(
                    &templates::GUESS_DUPLICATE,
                    ReplyVars::new().with("数字", number.to_string()),
                )],
            );
        }
        if number < session.lower || number > session.upper {
            return action(
                "out-of-range",
                vec![keyed(
                    &templates::GUESS_OUT_OF_RANGE,
                    ReplyVars::new()
                        .with("下限", session.lower.to_string())
                        .with("上限", session.upper.to_string()),
                )],
            );
        }
        let replies = apply_guess(&self.config, session, number, now);
        let exploded = session.phase == Phase::Lobby;
        action(if exploded { "exploded" } else { "guessed" }, replies)
    }
}

fn range_upper_bound(config: &NumberBombConfig, players: usize) -> u32 {
    let extra_players = players.saturating_sub(5) as u32;
    config
        .max
        .saturating_add(extra_players.saturating_mul(config.extra_range_per_player))
        .min(config.upper_bound_cap.max(config.max))
}

/// 在剩余范围内随机取一个没猜过的数字。
///
/// 不能先把范围展开成 Vec：上限可配置到十亿级。正常收窄保证已猜数字都在
/// 当前范围外，碰撞只可能是极端配置；先随机试几次，再按下界顺序跳过已猜值兜底。
fn random_available(rng: &mut SplitMix64, session: &Session) -> u32 {
    let span = u64::from(session.upper - session.lower) + 1;
    for _ in 0..16 {
        let number = session.lower + (rng.next_u64() % span) as u32;
        if !session.guessed.contains(&number) {
            return number;
        }
    }
    let mut number = session.lower;
    while number < session.upper && session.guessed.contains(&number) {
        number = number.saturating_add(1);
    }
    number
}

/// 已通过越界/重复/轮次校验的猜测。超时自动猜测也走这里，保证规则只有一份。
fn apply_guess(
    config: &NumberBombConfig,
    session: &mut Session,
    number: u32,
    now: Instant,
) -> Vec<NumberBombReply> {
    let Some(bomb) = session.bomb else {
        return vec![keyed(&templates::GUESS_NOT_PLAYING, ReplyVars::new())];
    };
    let player = session
        .players
        .get(session.turn)
        .map(|item| item.display.clone())
        .unwrap_or_else(|| "玩家".to_string());
    session.guessed.insert(number);
    if number == bomb {
        let loser = session.turn;
        session.phase = Phase::Lobby;
        session.bomb = None;
        session.last_result = Some(RoundResult { loser, bomb });
        session.lower = config.min;
        session.upper = config.max;
        session.guessed.clear();
        session.turn = 0;
        session.timer.reset(now);
        return vec![keyed(
            &templates::EXPLODED,
            ReplyVars::new()
                .with("玩家", player)
                .with("炸弹数字", bomb.to_string()),
        )];
    }
    if number < bomb {
        session.lower = number.saturating_add(1);
    } else {
        session.upper = number.saturating_sub(1);
    }
    session.turn = (session.turn + 1) % session.players.len();
    session.timer.reset(now);
    let next = session
        .players
        .get(session.turn)
        .map(|item| item.display.clone())
        .unwrap_or_else(|| "玩家".to_string());
    vec![keyed(
        &templates::GUESS_ACCEPTED,
        ReplyVars::new()
            .with("玩家", player)
            .with("数字", number.to_string())
            .with("下限", session.lower.to_string())
            .with("上限", session.upper.to_string())
            .with("下一位", next),
    )]
}

fn keyed(spec: &'static ReplyTemplate, vars: ReplyVars) -> NumberBombReply {
    NumberBombReply::keyed(spec, vars)
}

fn action(action: &'static str, replies: Vec<NumberBombReply>) -> NumberBombCommandOutcome {
    NumberBombCommandOutcome { action, replies }
}

fn only_creator() -> NumberBombCommandOutcome {
    action(
        "only-creator",
        vec![keyed(&templates::ONLY_CREATOR, ReplyVars::new())],
    )
}

fn too_few(players: usize, config: &NumberBombConfig) -> NumberBombCommandOutcome {
    action(
        "too-few",
        vec![keyed(
            &templates::START_TOO_FEW,
            ReplyVars::new()
                .with("人数", players.to_string())
                .with("最少人数", config.min_players.to_string()),
        )],
    )
}

fn too_many(players: usize, config: &NumberBombConfig) -> NumberBombCommandOutcome {
    action(
        "too-many",
        vec![keyed(
            &templates::START_TOO_MANY,
            ReplyVars::new()
                .with("人数", players.to_string())
                .with("上限", config.max_players.to_string()),
        )],
    )
}

fn player_key(value: &str) -> String {
    crate::text::player_match_key(value)
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

    fn gen_range(&mut self, lower: u32, upper: u32) -> u32 {
        debug_assert!(lower <= upper);
        let span = u64::from(upper - lower) + 1;
        lower + (self.next_u64() % span) as u32
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
    use super::*;
    use crate::features::command::CommandObservation;

    fn config() -> NumberBombConfig {
        NumberBombConfig {
            enabled: true,
            min: 1,
            max: 100,
            turn_timeout_seconds: 60,
            max_players: 11,
            min_players: 2,
            extra_range_per_player: 100,
            upper_bound_cap: 1000,
        }
    }

    fn service() -> (NumberBombService, EntertainmentState) {
        (
            NumberBombService::with_seed(config(), 7),
            EntertainmentState::new(),
        )
    }

    fn text(reply: &NumberBombReply) -> String {
        reply.render_default()
    }

    fn last_text(outcome: &NumberBombCommandOutcome) -> String {
        outcome.replies.last().map(text).unwrap_or_default()
    }

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn default_config_matches_the_agreed_rule() {
        let config = NumberBombConfig::default();
        assert!(config.enabled);
        assert_eq!(config.min, 1);
        assert_eq!(config.max, 100);
        assert_eq!(config.turn_timeout_seconds, 60);
        assert_eq!(config.min_players, 2);
        assert_eq!(config.max_players, 11);
        assert_eq!(config.extra_range_per_player, 100);
        assert_eq!(config.upper_bound_cap, 1000);
        config.validate().expect("默认配置必须合法");
    }

    #[test]
    fn upper_bound_is_capped_by_configuration() {
        let config = NumberBombConfig {
            extra_range_per_player: 100,
            upper_bound_cap: 300,
            ..NumberBombConfig::default()
        };
        // 6 人按加成是 200，7 人是 300，8 人本该 400 但被封顶到 300。
        assert_eq!(range_upper_bound(&config, 6), 200);
        assert_eq!(range_upper_bound(&config, 7), 300);
        assert_eq!(range_upper_bound(&config, 8), 300);
        let mut invalid = config.clone();
        invalid.upper_bound_cap = 50;
        assert!(invalid.validate().is_err(), "封顶不能小于默认上限");
    }

    #[test]
    fn invalid_ranges_and_player_limits_are_rejected() {
        let mut config = NumberBombConfig::default();
        config.min = 100;
        assert!(config.validate().is_err());
        let mut config = NumberBombConfig::default();
        config.min_players = 1;
        assert!(config.validate().is_err());
        let mut config = NumberBombConfig::default();
        config.max_players = 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn command_parsing_covers_keywords_and_explicit_guesses() {
        let envelope = |text: &str| {
            CommandEnvelope::new(text, "玩家甲", "blue", text, CommandObservation::default())
                .expect("envelope")
        };
        assert_eq!(
            NumberBombCommand::parse_start_chat(&envelope("#数字炸弹")),
            Some(FeatureCommandMatch::new(
                "#",
                "数字炸弹",
                NumberBombCommand::Start
            ))
        );
        for (text, command) in [
            ("#加入", NumberBombCommand::Join),
            ("#开始", NumberBombCommand::StartGame),
            ("#状态", NumberBombCommand::Status),
            ("#结束", NumberBombCommand::End),
            ("#重来", NumberBombCommand::Retry),
            ("#42", NumberBombCommand::Guess(42)),
            ("#不是数字", NumberBombCommand::Invalid),
        ] {
            let matched = NumberBombCommand::parse_active_chat(&envelope(text))
                .unwrap_or_else(|| panic!("{text} 应被数字炸弹认领"));
            assert_eq!(matched.command, command, "{text}");
        }
        // 猜测必须显式带井号：裸数字根本不会形成命令信封，自然也不会被认领。
        assert!(
            CommandEnvelope::new("42", "玩家甲", "blue", "42", CommandObservation::default())
                .is_none(),
            "裸数字不构成命令"
        );
    }

    #[test]
    fn lock_keys_do_not_collide() {
        let keys = [
            NumberBombCommand::Start.lock_key(),
            NumberBombCommand::Join.lock_key(),
            NumberBombCommand::StartGame.lock_key(),
            NumberBombCommand::Status.lock_key(),
            NumberBombCommand::End.lock_key(),
            NumberBombCommand::Retry.lock_key(),
            NumberBombCommand::Guess(42).lock_key(),
            NumberBombCommand::Invalid.lock_key(),
        ];
        let unique: BTreeSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), keys.len());
    }

    #[test]
    fn lobby_requires_two_players_and_only_the_creator_starts() {
        let (mut service, mut entertainment) = service();
        let now = now();
        let created = service
            .handle_command(
                &mut entertainment,
                "a",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        assert_eq!(created.action, "lobby-created");
        assert!(text(&created.replies[0]).contains("甲"));
        assert_eq!(entertainment.active(), Some(EntertainmentKind::NumberBomb));

        let too_few = service
            .handle_command(
                &mut entertainment,
                "a",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        assert_eq!(too_few.action, "too-few");
        assert!(last_text(&too_few).contains("2"));

        let joined = service
            .handle_command(&mut entertainment, "b", "乙", &NumberBombCommand::Join, now)
            .unwrap();
        assert_eq!(joined.action, "joined");
        let not_creator = service
            .handle_command(
                &mut entertainment,
                "b",
                "乙",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        assert_eq!(not_creator.action, "only-creator");

        let started = service
            .handle_command(
                &mut entertainment,
                "a",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        assert_eq!(started.action, "started");
        assert_eq!(
            service.session.as_ref().map(|session| session.phase),
            Some(Phase::Playing)
        );
    }

    #[test]
    fn upper_bound_grows_after_five_players() {
        let (mut service, mut entertainment) = service();
        let now = now();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        for index in 2..=6 {
            service
                .handle_command(
                    &mut entertainment,
                    &format!("p{index}"),
                    &format!("玩家{index}"),
                    &NumberBombCommand::Join,
                    now,
                )
                .unwrap();
        }
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        let session = service.session.as_ref().expect("session");
        // 6 人：100 + 1×100。
        assert_eq!(session.upper, 200);
        assert!((1..=200).contains(&session.bomb.expect("bomb")));
    }

    #[test]
    fn guesses_narrow_the_range_and_advance_the_turn() {
        let (mut service, mut entertainment) = service();
        let now = now();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Join,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        let bomb = service.session.as_ref().unwrap().bomb.unwrap();
        let guess = if bomb > 10 { 10 } else { 1 };
        let outcome = service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Guess(guess),
                now,
            )
            .unwrap();
        assert_eq!(outcome.action, "guessed");
        let session = service.session.as_ref().unwrap();
        assert_eq!(session.lower, guess + 1);
        assert_eq!(session.turn, 1);
        assert!(last_text(&outcome).contains("乙"));
    }

    #[test]
    fn out_of_range_and_wrong_turn_do_not_consume_a_turn() {
        let (mut service, mut entertainment) = service();
        let now = now();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Join,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        let turn = service.session.as_ref().unwrap().turn;

        let out = service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Guess(0),
                now,
            )
            .unwrap();
        assert_eq!(out.action, "out-of-range");
        assert_eq!(service.session.as_ref().unwrap().turn, turn);

        let wrong = service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Guess(1),
                now,
            )
            .unwrap();
        assert_eq!(wrong.action, "not-your-turn");
        assert_eq!(service.session.as_ref().unwrap().turn, turn);
    }

    #[test]
    fn non_number_chat_only_prompts_the_current_player() {
        let (mut service, mut entertainment) = service();
        let now = now();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Join,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();

        let bystander = service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Invalid,
                now,
            )
            .unwrap();
        assert_eq!(bystander.action, "ignored");
        assert!(bystander.replies.is_empty());

        let current = service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Invalid,
                now,
            )
            .unwrap();
        assert_eq!(current.action, "not-a-number");
        assert!(last_text(&current).contains("#数字"));
        assert_eq!(service.session.as_ref().unwrap().turn, 0);
    }

    #[test]
    fn repeated_guesses_are_rejected_without_advancing() {
        let (mut service, mut entertainment) = service();
        let now = now();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Join,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        // 直接构造一个已猜集合，验证重复检查本身；正常收窄后旧数字已越界，
        // 但玩家仍可能重复输入，提示要区分“猜过”与“越界”。
        let turn = service.session.as_ref().unwrap().turn;
        service.session.as_mut().unwrap().guessed.insert(50);
        let duplicate = service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Guess(50),
                now,
            )
            .unwrap();
        assert_eq!(duplicate.action, "duplicate");
        assert_eq!(service.session.as_ref().unwrap().turn, turn);
    }

    #[test]
    fn guessing_the_bomb_ends_the_round_but_keeps_the_roster_for_retry() {
        let (mut service, mut entertainment) = service();
        let now = now();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Join,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        let bomb = service.session.as_ref().unwrap().bomb.unwrap();
        let exploded = service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Guess(bomb),
                now,
            )
            .unwrap();
        assert_eq!(exploded.action, "exploded");
        assert!(last_text(&exploded).contains(&bomb.to_string()));
        assert_eq!(
            service.session.as_ref().map(|session| session.phase),
            Some(Phase::Lobby)
        );
        assert_eq!(service.session.as_ref().unwrap().players.len(), 2);
        assert_eq!(entertainment.active(), Some(EntertainmentKind::NumberBomb));

        let retried = service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Retry,
                now,
            )
            .unwrap();
        assert_eq!(retried.action, "started");
        assert_eq!(
            service.session.as_ref().map(|session| session.phase),
            Some(Phase::Playing)
        );
    }

    #[test]
    fn end_releases_the_entertainment_slot() {
        let (mut service, mut entertainment) = service();
        let now = now();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Join,
                now,
            )
            .unwrap();
        let outsider = service
            .handle_command(&mut entertainment, "p9", "丙", &NumberBombCommand::End, now)
            .unwrap();
        assert_eq!(outsider.action, "not-in-game");

        let ended = service
            .handle_command(&mut entertainment, "p2", "乙", &NumberBombCommand::End, now)
            .unwrap();
        assert_eq!(ended.action, "ended");
        assert_eq!(entertainment.active(), None);
        assert!(service.session.is_none());
    }

    #[test]
    fn timeout_auto_guesses_and_advances_without_a_new_thread() {
        let (mut service, mut entertainment) = service();
        let now = now();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Join,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        let turn = service.session.as_ref().unwrap().turn;
        let later = now + Duration::from_secs(60);
        let timeout = service.tick(later, true).expect("应触发回合超时");
        assert!(timeout.replies.len() >= 2);
        assert!(text(&timeout.replies[0]).contains("自动猜"));
        let session = service.session.as_ref().unwrap();
        if session.phase == Phase::Playing {
            assert_ne!(session.turn, turn);
        }
        // 暂停时不计时：同样的时长在 inactive 下不应再次触发。
        let paused = later + Duration::from_secs(120);
        assert!(service.tick(paused, false).is_none());
    }

    #[test]
    fn status_reports_the_current_range_and_player() {
        let (mut service, mut entertainment) = service();
        let now = now();
        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Start,
                now,
            )
            .unwrap();
        service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Join,
                now,
            )
            .unwrap();
        let lobby = service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::Status,
                now,
            )
            .unwrap();
        assert!(last_text(&lobby).contains("甲、乙"));

        service
            .handle_command(
                &mut entertainment,
                "p1",
                "甲",
                &NumberBombCommand::StartGame,
                now,
            )
            .unwrap();
        let playing = service
            .handle_command(
                &mut entertainment,
                "p2",
                "乙",
                &NumberBombCommand::Status,
                now,
            )
            .unwrap();
        let rendered = last_text(&playing);
        assert!(rendered.contains("[1, 100]"), "{rendered}");
        assert!(rendered.contains("甲"), "{rendered}");
    }
}
