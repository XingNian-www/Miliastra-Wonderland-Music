use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use pinyin::ToPinyin;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

use super::chat_text::{
    command_identity, compact_command, shortcut_argument, split_numbered_chat_message,
};
use super::command::{CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch};
use super::entertainment::{AcquireOutcome, EntertainmentKind, EntertainmentState};
use super::reply_template::{idiom_chain as templates, ReplyVars};
use miliastra_kernel::timer::{DeadlineKind, DeadlineModule, DeadlineToken};

fn default_lexicon_path() -> std::path::PathBuf {
    std::path::PathBuf::from("deps/assets/idioms.txt")
}

#[derive(Debug)]
pub struct IdiomChainDeadlineModule;

impl DeadlineModule for IdiomChainDeadlineModule {
    const NAME: &'static str = "idiom-chain";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IdiomChainDeadlineKind {
    SessionIdle,
}

impl DeadlineKind for IdiomChainDeadlineKind {
    type Module = IdiomChainDeadlineModule;
}

pub type IdiomChainDeadlineToken = DeadlineToken<IdiomChainDeadlineKind>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdiomChainConfig {
    pub enabled: bool,
    #[serde(default = "default_lexicon_path")]
    pub lexicon_path: std::path::PathBuf,
    pub history_limit: usize,
    pub idle_timeout_seconds: u64,
    pub allow_consecutive_player: bool,
    pub allow_anyone_stop: bool,
}

impl Default for IdiomChainConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lexicon_path: default_lexicon_path(),
            history_limit: 200,
            idle_timeout_seconds: 300,
            allow_consecutive_player: false,
            allow_anyone_stop: false,
        }
    }
}

impl IdiomChainConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.enabled && (self.history_limit == 0 || self.idle_timeout_seconds == 0) {
            bail!("成语接龙历史上限和空闲超时必须大于 0");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdiomChainMode {
    Exact,
    Homophone,
}

impl IdiomChainMode {
    fn label(self) -> &'static str {
        match self {
            Self::Exact => "成语接龙",
            Self::Homophone => "同音接龙",
        }
    }

    fn accepts(self, expected: char, candidate: char) -> bool {
        match self {
            Self::Exact => expected == candidate,
            Self::Homophone => match (pinyin_key(expected), pinyin_key(candidate)) {
                (Some(expected), Some(candidate)) => expected == candidate,
                _ => expected == candidate,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdiomChainCommand {
    Start { idiom: String, mode: IdiomChainMode },
    Submit(String),
    Explain(Option<String>),
    Hint,
    Status,
    Stop,
}

impl IdiomChainCommand {
    pub(crate) fn claims_start_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && envelope.authority() == CommandAuthority::HallMember
            && (start_argument(envelope.command_text(), "同音接龙").is_some()
                || start_argument(envelope.command_text(), "接龙").is_some())
    }

    pub(crate) fn claims_active_chat(envelope: &CommandEnvelope) -> bool {
        envelope.prefix() == CommandPrefix::Hash
            && envelope.authority() == CommandAuthority::HallMember
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
        Some(FeatureCommandMatch::new(
            "#",
            envelope.command_text(),
            Self::parse_active(envelope.command_text()),
        ))
    }

    pub(crate) fn parse_start(payload: &str) -> Option<Self> {
        if let Some(idiom) = start_argument(payload, "同音接龙") {
            return Some(Self::Start {
                idiom: idiom.to_string(),
                mode: IdiomChainMode::Homophone,
            });
        }
        start_argument(payload, "接龙").map(|idiom| Self::Start {
            idiom: idiom.to_string(),
            mode: IdiomChainMode::Exact,
        })
    }

    pub(crate) fn parse_active(payload: &str) -> Self {
        match compact_command(payload).as_str() {
            "提示" => Self::Hint,
            "状态" => Self::Status,
            "结束" => Self::Stop,
            "解释" => Self::Explain(None),
            _ => shortcut_argument(payload, "解释")
                .map(|idiom| Self::Explain(Some(idiom.to_string())))
                .unwrap_or_else(|| Self::Submit(payload.to_string())),
        }
    }

    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Start { idiom, mode } => format!(
                "idiom_chain:start:{}:{}",
                command_identity(idiom),
                match mode {
                    IdiomChainMode::Exact => "exact",
                    IdiomChainMode::Homophone => "homophone",
                }
            ),
            Self::Submit(idiom) => format!("idiom_chain:submit:{}", command_identity(idiom)),
            Self::Explain(idiom) => format!(
                "idiom_chain:explain:{}",
                idiom.as_deref().map(command_identity).unwrap_or_default()
            ),
            Self::Hint => "idiom_chain:hint".to_string(),
            Self::Status => "idiom_chain:status".to_string(),
            Self::Stop => "idiom_chain:stop".to_string(),
        }
    }

    pub(crate) const fn requires_executor(&self) -> bool {
        matches!(self, Self::Explain(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdiomChainOutcome {
    pub reply: String,
    pub action: &'static str,
    pub explanation: Option<IdiomExplanation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdiomExplanation {
    pub idiom: String,
    pub source: String,
    pub explanation: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdiomDeliveryOutcome {
    Added,
    DroppedEarlierMessage,
    Rejected,
}

pub(crate) trait IdiomChainDeferredPort {
    fn handle_command(
        &mut self,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Instant,
    ) -> Result<IdiomChainOutcome>;
    fn send_deferred(&mut self, message: String) -> Result<IdiomDeliveryOutcome>;
}

pub(crate) trait IdiomChainExplanationPort {
    fn explain(
        &mut self,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Instant,
    ) -> Result<IdiomChainOutcome>;
    fn send_batch(&mut self, messages: &[String], delay_ms: u64) -> Result<()>;
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct IdiomChainApplication {
    batch_delay_ms: u64,
}

impl IdiomChainApplication {
    pub(crate) const fn new(batch_delay_ms: u64) -> Self {
        Self { batch_delay_ms }
    }

    pub(crate) fn execute_deferred<P: IdiomChainDeferredPort + ?Sized>(
        &self,
        raw_command: &str,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Instant,
        port: &mut P,
    ) -> Result<()> {
        let outcome = port.handle_command(player, command, observed_at)?;
        let action = outcome.action;
        debug_assert!(outcome.explanation.is_none());
        match port.send_deferred(outcome.reply)? {
            IdiomDeliveryOutcome::Added => {}
            IdiomDeliveryOutcome::DroppedEarlierMessage => {
                log::warn!("延迟聊天发送队列已满，已丢弃一条较早的回复");
            }
            IdiomDeliveryOutcome::Rejected => {
                log::warn!("延迟聊天发送队列已被受保护批次占满，成语接龙回复已丢弃");
            }
        }
        log::info!(
            "成语接龙已处理，回复进入延迟发送队列: command={} action={}",
            raw_command,
            action
        );
        Ok(())
    }

    pub(crate) fn execute_explanation<P: IdiomChainExplanationPort + ?Sized>(
        &self,
        player: &str,
        command: &IdiomChainCommand,
        observed_at: Instant,
        port: &mut P,
    ) -> Result<()> {
        let outcome = port.explain(player, command, observed_at)?;
        let mut messages = vec![outcome.reply];
        if let Some(explanation) = outcome.explanation {
            messages.extend(split_numbered_chat_message("来源", &explanation.source));
            messages.extend(split_numbered_chat_message(
                "解释",
                &explanation.explanation,
            ));
        }
        port.send_batch(&messages, self.batch_delay_ms)
    }
}

pub struct IdiomChainGame {
    enabled: bool,
    lexicon: IdiomLexicon,
    history_limit: usize,
    idle_timeout: Option<Duration>,
    allow_anyone_stop: bool,
    session: Option<IdiomChainSession>,
}

pub(crate) struct IdiomChainService {
    game: IdiomChainGame,
}

impl IdiomChainService {
    pub(crate) fn load(config: IdiomChainConfig) -> Result<Self> {
        Ok(Self::from_game(IdiomChainGame::load(config)?))
    }

    fn from_game(game: IdiomChainGame) -> Self {
        Self { game }
    }

    pub(crate) fn lexicon_len(&self) -> usize {
        self.game.lexicon_len()
    }

    #[cfg(test)]
    pub(crate) fn handle(
        &mut self,
        entertainment: &mut EntertainmentState,
        player: &str,
        command: &IdiomChainCommand,
    ) -> Result<IdiomChainOutcome> {
        self.handle_at(entertainment, player, command, Instant::now())
    }

    pub(crate) fn handle_at(
        &mut self,
        entertainment: &mut EntertainmentState,
        player: &str,
        command: &IdiomChainCommand,
        now: Instant,
    ) -> Result<IdiomChainOutcome> {
        if entertainment.active() == Some(EntertainmentKind::TurtleSoup) {
            return Ok(outcome(
                "occupied",
                reply(&templates::OCCUPIED_TURTLE_SOUP, ReplyVars::new()),
            ));
        }
        let acquired_for_start = if matches!(command, IdiomChainCommand::Start { .. }) {
            match entertainment.try_acquire(EntertainmentKind::IdiomChain)? {
                AcquireOutcome::Acquired => true,
                AcquireOutcome::AlreadyOwned => false,
                AcquireOutcome::Occupied(kind) => {
                    return Ok(outcome(
                        "occupied",
                        reply(
                            &templates::OCCUPIED_OTHER,
                            ReplyVars::new().with("其他玩法", kind.label()),
                        ),
                    ));
                }
            }
        } else {
            false
        };

        let outcome = self.game.handle_at(player, command, now);
        if acquired_for_start && outcome.action != "started" {
            entertainment.release(EntertainmentKind::IdiomChain);
        }
        if matches!(outcome.action, "completed" | "stopped" | "expired") {
            entertainment.release(EntertainmentKind::IdiomChain);
        }
        Ok(outcome)
    }

    pub(crate) fn explain_at(
        &mut self,
        player: &str,
        command: &IdiomChainCommand,
        now: Instant,
    ) -> Result<IdiomChainOutcome> {
        Ok(self.game.handle_at(player, command, now))
    }

    pub(crate) fn abort(&mut self, entertainment: &mut EntertainmentState) -> Result<bool> {
        let aborted = self.game.abort();
        if aborted {
            entertainment.release(EntertainmentKind::IdiomChain);
        }
        Ok(aborted)
    }

    pub(crate) fn expire_idle_at(
        &mut self,
        entertainment: &mut EntertainmentState,
        now: Instant,
    ) -> Result<bool> {
        let expired = self.game.expire_idle_at(now);
        if expired {
            entertainment.release(EntertainmentKind::IdiomChain);
        }
        Ok(expired)
    }

    pub(crate) fn idle_deadline(&self) -> Option<Instant> {
        self.game.idle_deadline()
    }

    #[cfg(test)]
    pub(crate) fn from_entries_for_test(entries: &[&str], idle_timeout: Option<Duration>) -> Self {
        Self::from_game(IdiomChainGame {
            enabled: true,
            lexicon: IdiomLexicon::from_entries(entries.iter().map(|entry| entry.to_string()))
                .expect("valid test lexicon"),
            history_limit: 200,
            idle_timeout,
            allow_anyone_stop: false,
            session: None,
        })
    }
}

impl IdiomChainGame {
    pub fn load(config: IdiomChainConfig) -> Result<Self> {
        let lexicon = if config.enabled {
            IdiomLexicon::load(&config.lexicon_path)?
        } else {
            IdiomLexicon::default()
        };
        Ok(Self {
            enabled: config.enabled,
            lexicon,
            history_limit: config.history_limit.max(1),
            idle_timeout: (config.idle_timeout_seconds > 0)
                .then(|| Duration::from_secs(config.idle_timeout_seconds)),
            allow_anyone_stop: config.allow_anyone_stop,
            session: None,
        })
    }

    pub fn lexicon_len(&self) -> usize {
        self.lexicon.len()
    }

    pub fn abort(&mut self) -> bool {
        self.session.take().is_some()
    }

    pub fn expire_idle_at(&mut self, now: Instant) -> bool {
        self.expire_if_idle(now)
    }

    pub fn idle_deadline(&self) -> Option<Instant> {
        let timeout = self.idle_timeout?;
        let session = self.session.as_ref()?;
        Some(session.last_activity + timeout)
    }

    #[cfg(test)]
    pub fn handle(&mut self, player: &str, command: &IdiomChainCommand) -> IdiomChainOutcome {
        self.handle_at(player, command, Instant::now())
    }

    fn handle_at(
        &mut self,
        player: &str,
        command: &IdiomChainCommand,
        now: Instant,
    ) -> IdiomChainOutcome {
        if !self.enabled {
            return outcome("disabled", reply(&templates::DISABLED, ReplyVars::new()));
        }

        if !matches!(command, IdiomChainCommand::Start { .. }) && self.expire_if_idle(now) {
            return outcome("expired", reply(&templates::EXPIRED, ReplyVars::new()));
        }

        match command {
            IdiomChainCommand::Start { idiom, mode } => self.start(player, idiom, *mode, now),
            IdiomChainCommand::Submit(word) => self.submit(player, word, now),
            IdiomChainCommand::Explain(idiom) => self.explain(idiom.as_deref()),
            IdiomChainCommand::Hint => self.hint(),
            IdiomChainCommand::Status => self.status(),
            IdiomChainCommand::Stop => self.stop(player),
        }
    }

    fn start(
        &mut self,
        player: &str,
        raw_idiom: &str,
        mode: IdiomChainMode,
        now: Instant,
    ) -> IdiomChainOutcome {
        self.expire_if_idle(now);
        if self.session.is_some() {
            return outcome(
                "already-active",
                reply(&templates::ALREADY_ACTIVE, ReplyVars::new()),
            );
        }
        let idiom = if raw_idiom.trim().is_empty() {
            let eligible = self
                .lexicon
                .entries
                .iter()
                .filter(|idiom| {
                    self.lexicon
                        .successors(last_char(idiom), mode)
                        .is_some_and(|successors| {
                            successors.iter().any(|successor| *successor != **idiom)
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            let Some(idiom) = eligible.choose(&mut rand::thread_rng()) else {
                return outcome(
                    "no-start-candidate",
                    reply(&templates::NO_START_CANDIDATE, ReplyVars::new()),
                );
            };
            idiom.clone()
        } else {
            match self.lookup_idiom(raw_idiom) {
                Ok(idiom) => idiom,
                Err(reply) => return outcome("invalid-start", reply),
            }
        };
        let expected_first = last_char(&idiom);
        let mut session = IdiomChainSession::new(player, idiom.clone(), expected_first, mode, now);
        session.remember(idiom.clone(), self.history_limit);
        if !self
            .lexicon
            .has_unused_successor(expected_first, mode, &session.used)
        {
            return outcome(
                "dead-end-start",
                reply(&templates::DEAD_END_START, ReplyVars::new()),
            );
        }
        self.session = Some(session);
        let expected = expected_start_description(expected_first, mode);
        outcome(
            "started",
            reply(
                &templates::STARTED,
                ReplyVars::new()
                    .with("模式", mode.label())
                    .with("成语", idiom)
                    .with("开头描述", expected),
            ),
        )
    }

    fn submit(&mut self, player: &str, raw_idiom: &str, now: Instant) -> IdiomChainOutcome {
        let idiom = match self.lookup_idiom(raw_idiom) {
            Ok(idiom) => idiom,
            Err(reply) => return outcome("invalid-submission", reply),
        };
        let Some(session) = self.session.as_ref() else {
            return outcome(
                "no-session",
                reply(&templates::NO_SESSION_START, ReplyVars::new()),
            );
        };
        let first = first_char(&idiom);
        if !session.mode.accepts(session.expected_first, first) {
            return outcome(
                "wrong-first-char",
                reply(
                    &templates::WRONG_FIRST_CHAR,
                    ReplyVars::new().with(
                        "开头描述",
                        expected_start_description(session.expected_first, session.mode),
                    ),
                ),
            );
        }
        if session.used.contains(&idiom) {
            return outcome("duplicate", reply(&templates::DUPLICATE, ReplyVars::new()));
        }

        let next_first = last_char(&idiom);
        let previous = session.last_idiom.clone();
        let session = self.session.as_mut().expect("session checked above");
        session.last_idiom = idiom.clone();
        session.expected_first = next_first;
        session.last_activity = now;
        session.remember(idiom.clone(), self.history_limit);
        let count = session.total_count;
        let no_successor =
            !self
                .lexicon
                .has_unused_successor(next_first, session.mode, &session.used);
        if no_successor {
            self.session = None;
            return outcome(
                "completed",
                reply(
                    &templates::COMPLETED,
                    ReplyVars::new()
                        .with("成语", idiom)
                        .with("数量", count.to_string()),
                ),
            );
        }
        let expected = expected_start_description(next_first, session.mode);
        outcome(
            "accepted",
            reply(
                &templates::ACCEPTED,
                ReplyVars::new()
                    .with("上一个", previous)
                    .with("成语", idiom)
                    .with("开头描述", expected),
            ),
        )
    }

    fn status(&mut self) -> IdiomChainOutcome {
        let Some(session) = self.session.as_ref() else {
            return outcome("no-session", reply(&templates::NO_SESSION, ReplyVars::new()));
        };
        outcome(
            "status",
            reply(
                &templates::STATUS,
                ReplyVars::new()
                    .with("模式", session.mode.label())
                    .with("数量", session.total_count.to_string())
                    .with("成语", session.last_idiom.clone())
                    .with(
                        "开头描述",
                        expected_start_description(session.expected_first, session.mode),
                    ),
            ),
        )
    }

    fn explain(&self, requested: Option<&str>) -> IdiomChainOutcome {
        let idiom = if let Some(requested) = requested.filter(|value| !value.trim().is_empty()) {
            match self.lookup_idiom(requested) {
                Ok(idiom) => idiom,
                Err(reply) => return outcome("invalid-explanation", reply),
            }
        } else if let Some(session) = self.session.as_ref() {
            session.last_idiom.clone()
        } else {
            return outcome(
                "no-explanation-target",
                reply(&templates::NO_EXPLANATION_TARGET, ReplyVars::new()),
            );
        };
        let Some(details) = self.lexicon.explanation(&idiom) else {
            return outcome(
                "missing-explanation",
                reply(&templates::MISSING_EXPLANATION, ReplyVars::new()),
            );
        };
        IdiomChainOutcome {
            reply: reply(
                &templates::EXPLANATION_TITLE,
                ReplyVars::new().with("成语", idiom.clone()),
            ),
            action: "explained",
            explanation: Some(IdiomExplanation {
                idiom,
                source: details.source.clone(),
                explanation: details.explanation.clone(),
            }),
        }
    }

    fn hint(&self) -> IdiomChainOutcome {
        let Some(session) = self.session.as_ref() else {
            return outcome("no-session", reply(&templates::NO_SESSION, ReplyVars::new()));
        };
        let Some(idiom) =
            self.lexicon
                .first_safe_hint(session.expected_first, session.mode, &session.used)
        else {
            return outcome("no-hint", reply(&templates::NO_HINT, ReplyVars::new()));
        };
        outcome(
            "hint",
            reply(&templates::HINT, ReplyVars::new().with("成语", idiom)),
        )
    }

    fn stop(&mut self, player: &str) -> IdiomChainOutcome {
        let Some(session) = self.session.as_ref() else {
            return outcome("no-session", reply(&templates::NO_SESSION, ReplyVars::new()));
        };
        if !self.allow_anyone_stop && !same_player(player, &session.starter) {
            return outcome(
                "stop-denied",
                reply(&templates::STOP_DENIED, ReplyVars::new()),
            );
        }
        let count = session.total_count;
        self.session = None;
        outcome(
            "stopped",
            reply(
                &templates::STOPPED,
                ReplyVars::new().with("数量", count.to_string()),
            ),
        )
    }

    fn lookup_idiom(&self, raw_idiom: &str) -> std::result::Result<String, String> {
        let Some(idiom) = normalize_idiom(raw_idiom) else {
            return Err(reply(&templates::INVALID_IDIOM, ReplyVars::new()));
        };
        if !self.lexicon.contains(&idiom) {
            return Err(reply(&templates::IDIOM_NOT_IN_LEXICON, ReplyVars::new()));
        }
        Ok(idiom)
    }

    fn expire_if_idle(&mut self, now: Instant) -> bool {
        let Some(timeout) = self.idle_timeout else {
            return false;
        };
        let expired = self
            .session
            .as_ref()
            .is_some_and(|session| now.duration_since(session.last_activity) >= timeout);
        if expired {
            self.session = None;
        }
        expired
    }
}

struct IdiomChainSession {
    starter: String,
    last_idiom: String,
    expected_first: char,
    mode: IdiomChainMode,
    history: VecDeque<String>,
    used: HashSet<String>,
    total_count: usize,
    last_activity: Instant,
}

impl IdiomChainSession {
    fn new(
        player: &str,
        idiom: String,
        expected_first: char,
        mode: IdiomChainMode,
        now: Instant,
    ) -> Self {
        Self {
            starter: player.trim().to_string(),
            last_idiom: idiom,
            expected_first,
            mode,
            history: VecDeque::new(),
            used: HashSet::new(),
            total_count: 0,
            last_activity: now,
        }
    }

    fn remember(&mut self, idiom: String, history_limit: usize) {
        if self.used.insert(idiom.clone()) {
            self.total_count = self.total_count.saturating_add(1);
        }
        self.history.push_back(idiom);
        while self.history.len() > history_limit {
            self.history.pop_front();
        }
    }
}

#[derive(Default)]
struct IdiomLexicon {
    entries: HashSet<String>,
    entries_by_first: HashMap<char, Vec<String>>,
    entries_by_first_pinyin: HashMap<String, Vec<String>>,
    explanations: HashMap<String, IdiomDetails>,
}

#[derive(Clone, Debug)]
struct IdiomDetails {
    source: String,
    explanation: String,
}

impl IdiomLexicon {
    fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("读取项目成语词库失败: {}", path.display()))?;
        Self::from_text(&text, path)
    }

    fn from_text(text: &str, path: &Path) -> Result<Self> {
        let mut entries = Vec::new();
        for (line_number, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut fields = trimmed.splitn(3, ':');
            let raw_idiom = fields.next().unwrap_or_default().trim();
            let idiom = normalize_idiom(raw_idiom).ok_or_else(|| {
                anyhow::anyhow!(
                    "成语词库条目格式错误: {}:{} ({})",
                    path.display(),
                    line_number + 1,
                    trimmed
                )
            })?;
            entries.push((
                idiom,
                with_default_label(fields.next().unwrap_or_default().trim().to_string()),
                with_default_label(fields.next().unwrap_or_default().trim().to_string()),
            ));
        }
        Self::from_definitions(entries)
    }

    #[cfg(test)]
    fn from_entries(entries: impl IntoIterator<Item = String>) -> Result<Self> {
        Self::from_definitions(
            entries
                .into_iter()
                .map(|idiom| (idiom, String::new(), String::new())),
        )
    }

    fn from_definitions(
        entries: impl IntoIterator<Item = (String, String, String)>,
    ) -> Result<Self> {
        let mut lexicon = Self::default();
        for (idiom, source, explanation) in entries {
            if idiom.chars().count() < 2 {
                bail!("成语词库条目至少需要两个汉字: {}", idiom);
            }
            if !lexicon.entries.insert(idiom.clone()) {
                continue;
            }
            if !source.is_empty() || !explanation.is_empty() {
                lexicon.explanations.insert(
                    idiom.clone(),
                    IdiomDetails {
                        source: with_default_label(source),
                        explanation: with_default_label(explanation),
                    },
                );
            }
            lexicon
                .entries_by_first
                .entry(first_char(&idiom))
                .or_default()
                .push(idiom.clone());
            if let Some(pinyin) = pinyin_key(first_char(&idiom)) {
                lexicon
                    .entries_by_first_pinyin
                    .entry(pinyin)
                    .or_default()
                    .push(idiom);
            }
        }
        if lexicon.entries.is_empty() {
            bail!("成语词库没有可用条目");
        }
        Ok(lexicon)
    }

    fn contains(&self, idiom: &str) -> bool {
        self.entries.contains(idiom)
    }

    fn explanation(&self, idiom: &str) -> Option<&IdiomDetails> {
        self.explanations.get(idiom)
    }

    fn has_unused_successor(
        &self,
        first: char,
        mode: IdiomChainMode,
        used: &HashSet<String>,
    ) -> bool {
        let entries = self.successors(first, mode);
        entries.is_some_and(|entries| entries.iter().any(|entry| !used.contains(entry)))
    }

    fn first_safe_hint(
        &self,
        first: char,
        mode: IdiomChainMode,
        used: &HashSet<String>,
    ) -> Option<&str> {
        self.successors(first, mode)?
            .iter()
            .filter(|candidate| !used.contains(*candidate))
            .find(|candidate| {
                self.successors(last_char(candidate), mode)
                    .is_some_and(|successors| {
                        successors
                            .iter()
                            .any(|successor| successor != *candidate && !used.contains(successor))
                    })
            })
            .map(String::as_str)
    }

    fn successors(&self, first: char, mode: IdiomChainMode) -> Option<&Vec<String>> {
        match mode {
            IdiomChainMode::Exact => self.entries_by_first.get(&first),
            IdiomChainMode::Homophone => pinyin_key(first)
                .as_ref()
                .and_then(|pinyin| self.entries_by_first_pinyin.get(pinyin))
                .or_else(|| self.entries_by_first.get(&first)),
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

fn start_argument<'a>(payload: &'a str, prefix: &str) -> Option<&'a str> {
    let value = payload.strip_prefix(prefix)?;
    Some(value.trim_start_matches(['：', ':', ' ', '\t']).trim())
}

fn normalize_idiom(value: &str) -> Option<String> {
    let idiom = value
        .chars()
        .filter(|ch| !ch.is_whitespace() && !is_punctuation(*ch))
        .collect::<String>();
    (!idiom.is_empty() && idiom.chars().all(is_han_character)).then_some(idiom)
}

fn is_han_character(ch: char) -> bool {
    matches!(
        ch,
        '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}'
    )
}

fn is_punctuation(ch: char) -> bool {
    ch.is_ascii_punctuation()
        || matches!(
            ch,
            '，' | '。'
                | '、'
                | '；'
                | '：'
                | '？'
                | '！'
                | '（'
                | '）'
                | '【'
                | '】'
                | '《'
                | '》'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '·'
                | '—'
                | '～'
                | '…'
        )
}

fn first_char(value: &str) -> char {
    value.chars().next().expect("validated idiom is non-empty")
}

fn last_char(value: &str) -> char {
    value
        .chars()
        .next_back()
        .expect("validated idiom is non-empty")
}

fn pinyin_key(ch: char) -> Option<String> {
    ch.to_pinyin().map(|pinyin| pinyin.plain().to_string())
}

fn expected_start_description(expected: char, mode: IdiomChainMode) -> String {
    match (mode, pinyin_key(expected)) {
        (IdiomChainMode::Homophone, Some(pinyin)) => reply(
            &templates::EXPECTED_HOMOPHONE,
            ReplyVars::new()
                .with("字", expected.to_string())
                .with("拼音", pinyin),
        ),
        _ => reply(
            &templates::EXPECTED_EXACT,
            ReplyVars::new().with("字", expected.to_string()),
        ),
    }
}

fn same_player(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn with_default_label(value: String) -> String {
    if value.trim().is_empty() {
        "未注明".to_string()
    } else {
        value
    }
}

/// 渲染一条回复文案。
///
/// 成语接龙的回复在业务层就拼成成品字符串（`IdiomChainOutcome.reply`），
/// 组装层拿不到模板键，因此这里只渲染内置默认文案；用户覆盖文案要生效，
/// 需要在组装层增加模板直通通道（见任务汇报）。
fn reply(spec: &super::reply_template::ReplyTemplate, vars: ReplyVars) -> String {
    super::reply_template::render(spec, None, &vars).text
}

fn outcome(action: &'static str, reply: impl Into<String>) -> IdiomChainOutcome {
    IdiomChainOutcome {
        reply: reply.into(),
        action,
        explanation: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::entertainment::EntertainmentState;
    use miliastra_kernel::clock::{Clock, ManualClock};

    fn game(entries: &[&str]) -> IdiomChainGame {
        IdiomChainGame {
            enabled: true,
            lexicon: IdiomLexicon::from_entries(entries.iter().map(|entry| entry.to_string()))
                .expect("valid test lexicon"),
            history_limit: 200,
            idle_timeout: Some(Duration::from_secs(300)),
            allow_anyone_stop: false,
            session: None,
        }
    }

    fn start_exact(idiom: &str) -> IdiomChainCommand {
        IdiomChainCommand::Start {
            idiom: idiom.to_string(),
            mode: IdiomChainMode::Exact,
        }
    }

    struct ExplanationPort {
        batches: Vec<Vec<String>>,
    }

    impl IdiomChainExplanationPort for ExplanationPort {
        fn explain(
            &mut self,
            _player: &str,
            _command: &IdiomChainCommand,
            _observed_at: Instant,
        ) -> Result<IdiomChainOutcome> {
            Ok(IdiomChainOutcome {
                reply: "成语：画蛇添足".to_string(),
                action: "explained",
                explanation: Some(IdiomExplanation {
                    idiom: "画蛇添足".to_string(),
                    source: "故事来源".to_string(),
                    explanation: "比喻多此一举".to_string(),
                }),
            })
        }

        fn send_batch(&mut self, messages: &[String], _delay_ms: u64) -> Result<()> {
            self.batches.push(messages.to_vec());
            Ok(())
        }
    }

    #[test]
    fn explanation_application_sends_reply_source_and_explanation_as_one_batch() {
        let mut port = ExplanationPort {
            batches: Vec::new(),
        };

        IdiomChainApplication::new(100)
            .execute_explanation(
                "Alice",
                &IdiomChainCommand::Explain(Some("画蛇添足".to_string())),
                Instant::now(),
                &mut port,
            )
            .expect("explanation delivery");

        assert_eq!(port.batches.len(), 1);
        assert_eq!(
            port.batches[0],
            [
                "成语：画蛇添足",
                "来源1/1：故事来源",
                "解释1/1：比喻多此一举"
            ]
        );
    }

    #[test]
    fn loads_the_complete_project_lexicon_asset() {
        // 仓库根 assets/ 保存源资源；发布包中位于 deps/assets/。
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/idioms.txt");
        let lexicon = IdiomLexicon::load(&path).expect("load project idioms");

        assert!(lexicon.len() >= 30_000);
        assert!(lexicon.contains("阿鼻地狱"));
        assert!(lexicon.contains("画蛇添足"));
        assert!(lexicon.contains("足智多谋"));
    }

    #[test]
    fn default_config_enables_the_project_lexicon() {
        assert!(IdiomChainConfig::default().enabled);
    }

    #[test]
    fn application_service_releases_entertainment_after_an_invalid_start() {
        let mut entertainment = EntertainmentState::new();
        let mut service = IdiomChainService::from_game(game(&["画蛇添足", "足智多谋"]));

        let outcome = service
            .handle(&mut entertainment, "Alice", &start_exact("不是成语"))
            .expect("service should handle invalid input");

        assert_eq!(outcome.action, "invalid-start");
        assert_eq!(entertainment.active(), None);
    }

    #[test]
    fn lexicon_splits_only_the_first_two_ascii_colons_and_defaults_missing_details() {
        let lexicon = IdiomLexicon::from_text(
            "画蛇添足:《出处：卷一》:解释：保留全角冒号，也保留后续:半角冒号\n足智多谋::",
            Path::new("test-idioms.txt"),
        )
        .expect("parse idiom details");

        let details = lexicon.explanation("画蛇添足").expect("details");
        assert_eq!(details.source, "《出处：卷一》");
        assert_eq!(
            details.explanation,
            "解释：保留全角冒号，也保留后续:半角冒号"
        );
        let missing = lexicon.explanation("足智多谋").expect("missing defaults");
        assert_eq!(missing.source, "未注明");
        assert_eq!(missing.explanation, "未注明");
    }

    #[test]
    fn explanation_uses_requested_idiom_or_the_current_session_idiom() {
        let lexicon = IdiomLexicon::from_text(
            "画蛇添足:故事来源:比喻做了多余的事，反而不恰当。\n足智多谋:未注明:富有智慧，善于谋划。",
            Path::new("test-idioms.txt"),
        )
        .expect("parse explanation lexicon");
        let mut game = game(&["画蛇添足", "足智多谋"]);
        game.lexicon = lexicon;

        let requested = game.handle(
            "Alice",
            &IdiomChainCommand::Explain(Some("画蛇添足".to_string())),
        );
        assert_eq!(requested.action, "explained");
        assert_eq!(requested.explanation.expect("details").source, "故事来源");

        game.handle("Alice", &start_exact("画蛇添足"));
        let current = game.handle("Bob", &IdiomChainCommand::Explain(None));
        assert_eq!(current.action, "explained");
        assert_eq!(current.explanation.expect("details").idiom, "画蛇添足");
    }

    #[test]
    fn start_commands_allow_random_openers_in_both_modes() {
        assert_eq!(
            IdiomChainCommand::parse_start("接龙"),
            Some(start_exact(""))
        );
        assert_eq!(
            IdiomChainCommand::parse_start("同音接龙"),
            Some(IdiomChainCommand::Start {
                idiom: String::new(),
                mode: IdiomChainMode::Homophone,
            })
        );
        assert_eq!(
            IdiomChainCommand::parse_start("接龙后续"),
            Some(start_exact("后续"))
        );

        let mut exact = game(&["画蛇添足", "足智多谋", "天马行空"]);
        let started = exact.handle("Alice", &start_exact(""));
        assert_eq!(started.action, "started");
        assert!(started.reply.contains("画蛇添足"));

        let mut homophone = game(&["画蛇添足", "组词造句", "天马行空"]);
        let started = homophone.handle(
            "Alice",
            &IdiomChainCommand::Start {
                idiom: String::new(),
                mode: IdiomChainMode::Homophone,
            },
        );
        assert_eq!(started.action, "started");
        assert!(started.reply.contains("画蛇添足"));
    }

    #[test]
    fn accepts_only_lexicon_entries_with_matching_characters() {
        let mut game = game(&["画蛇添足", "足智多谋", "谋事在人", "人山人海"]);

        let started = game.handle("Alice", &start_exact("画蛇添足！"));
        assert_eq!(started.action, "started");
        assert!(started.reply.contains('足'));

        let accepted = game.handle("Alice", &IdiomChainCommand::Submit("足智多谋".to_string()));
        assert_eq!(accepted.action, "accepted");
        assert!(accepted.reply.contains('谋'));

        let invalid = game.handle("Carol", &IdiomChainCommand::Submit("谋定后动".to_string()));
        assert_eq!(invalid.action, "invalid-submission");
    }

    #[test]
    fn rejects_repeated_idioms_and_wrong_first_character() {
        let mut game = game(&["画蛇添足", "足智多谋", "谋足谋足", "足不出户", "人山人海"]);
        game.handle("Alice", &start_exact("画蛇添足"));
        game.handle("Bob", &IdiomChainCommand::Submit("足智多谋".to_string()));

        let wrong = game.handle("Carol", &IdiomChainCommand::Submit("人山人海".to_string()));
        assert_eq!(wrong.action, "wrong-first-char");

        game.handle("Carol", &IdiomChainCommand::Submit("谋足谋足".to_string()));
        let repeated = game.handle("Dave", &IdiomChainCommand::Submit("足智多谋".to_string()));
        assert_eq!(repeated.action, "duplicate");
    }

    #[test]
    fn homophone_mode_accepts_same_pronunciation_with_a_different_character() {
        let entries = ["画蛇添足", "足智多谋", "组词造句", "聚精会神"];
        let mut exact_game = game(&entries);
        assert_eq!(
            exact_game.handle("Alice", &start_exact("画蛇添足")).action,
            "started"
        );
        assert_eq!(
            exact_game
                .handle("Bob", &IdiomChainCommand::Submit("组词造句".to_string()))
                .action,
            "wrong-first-char"
        );

        let mut homophone_game = game(&entries);
        let started = homophone_game.handle(
            "Alice",
            &IdiomChainCommand::Start {
                idiom: "画蛇添足".to_string(),
                mode: IdiomChainMode::Homophone,
            },
        );
        assert_eq!(started.action, "started");
        assert!(started.reply.contains("zu"));

        let accepted =
            homophone_game.handle("Bob", &IdiomChainCommand::Submit("组词造句".to_string()));
        assert_eq!(accepted.action, "accepted");
        assert!(accepted.reply.contains("ju"));
    }

    #[test]
    fn hint_uses_the_active_mode_and_never_recommends_a_used_idiom() {
        let entries = ["画蛇添足", "足智多谋", "族群兴旺", "望子成龙", "龙腾虎跃"];
        let mut game = game(&entries);
        game.handle(
            "Alice",
            &IdiomChainCommand::Start {
                idiom: "画蛇添足".to_string(),
                mode: IdiomChainMode::Homophone,
            },
        );

        let hint = game.handle("Bob", &IdiomChainCommand::Hint);
        assert_eq!(hint.action, "hint");
        assert!(!hint.reply.contains("画蛇添足"));
        assert!(hint.reply.contains("足智多谋") || hint.reply.contains("族群兴旺"));
    }

    #[test]
    fn hint_skips_dead_ends_and_reports_when_only_dead_ends_remain() {
        let mut safe_game = game(&["画蛇添足", "足智多谋", "足不出户", "户枢不蠹"]);
        safe_game.handle("Alice", &start_exact("画蛇添足"));

        let hint = safe_game.handle("Bob", &IdiomChainCommand::Hint);
        assert_eq!(hint.action, "hint");
        assert!(hint.reply.contains("足不出户"));
        assert!(!hint.reply.contains("足智多谋"));

        let mut dead_end_game = game(&["画蛇添足", "足智多谋"]);
        dead_end_game.handle("Alice", &start_exact("画蛇添足"));
        let no_hint = dead_end_game.handle("Bob", &IdiomChainCommand::Hint);
        assert_eq!(no_hint.action, "no-hint");
        assert!(no_hint.reply.contains("安全提示"));
    }

    #[test]
    fn duplicate_idioms_remain_blocked_after_the_recent_history_limit() {
        let mut game = game(&["甲乙", "乙丙", "丙甲", "甲丁", "丁甲"]);
        game.history_limit = 2;
        game.handle("Alice", &start_exact("甲乙"));
        game.handle("Bob", &IdiomChainCommand::Submit("乙丙".to_string()));
        game.handle("Carol", &IdiomChainCommand::Submit("丙甲".to_string()));

        let repeated = game.handle("Dave", &IdiomChainCommand::Submit("甲乙".to_string()));
        assert_eq!(repeated.action, "duplicate");
    }

    #[test]
    fn finishes_when_no_unused_successor_exists() {
        let mut game = game(&["画蛇添足", "足智多谋"]);
        game.handle("Alice", &start_exact("画蛇添足"));

        let completed = game.handle("Bob", &IdiomChainCommand::Submit("足智多谋".to_string()));
        assert_eq!(completed.action, "completed");
        assert!(completed.reply.contains("封龙"));
        assert_eq!(
            game.handle("Alice", &IdiomChainCommand::Status).action,
            "no-session"
        );
    }

    #[test]
    fn only_starter_can_stop_by_default() {
        let mut game = game(&["画蛇添足", "足智多谋"]);
        game.handle("Alice", &start_exact("画蛇添足"));

        assert_eq!(
            game.handle("Bob", &IdiomChainCommand::Stop).action,
            "stop-denied"
        );
        assert_eq!(
            game.handle("Alice", &IdiomChainCommand::Stop).action,
            "stopped"
        );
    }

    #[test]
    fn context_abort_clears_the_current_session() {
        let mut game = game(&["画蛇添足", "足智多谋"]);
        game.handle("Alice", &start_exact("画蛇添足"));

        assert!(game.abort());
        assert!(!game.abort());
        assert_eq!(
            game.handle("Alice", &IdiomChainCommand::Status).action,
            "no-session"
        );
    }

    #[test]
    fn idle_timeout_is_driven_by_a_manual_clock() {
        let clock = ManualClock::new(Instant::now());
        let mut game = game(&["画蛇添足", "足智多谋"]);
        assert_eq!(
            game.handle_at("Alice", &start_exact("画蛇添足"), clock.now())
                .action,
            "started"
        );

        clock.advance(Duration::from_secs(299)).unwrap();
        assert!(!game.expire_idle_at(clock.now()));
        clock.advance(Duration::from_secs(1)).unwrap();
        assert!(game.expire_idle_at(clock.now()));
    }
}
