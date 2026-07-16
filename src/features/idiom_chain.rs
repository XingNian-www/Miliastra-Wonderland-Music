use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use pinyin::ToPinyin;
use serde::{Deserialize, Serialize};

use super::chat_text::{command_identity, compact_command, shortcut_argument};
use super::entertainment::{AcquireOutcome, EntertainmentKind, EntertainmentState};
use crate::runtime::timer::{DeadlineKind, DeadlineModule, DeadlineToken};

const PROJECT_IDIOM_ASSET_PATH: &str = "assets/idioms.txt";

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
#[serde(default, deny_unknown_fields)]
pub struct IdiomChainConfig {
    pub enabled: bool,
    pub history_limit: usize,
    pub idle_timeout_seconds: u64,
    pub allow_consecutive_player: bool,
    pub allow_anyone_stop: bool,
}

impl Default for IdiomChainConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            history_limit: 200,
            idle_timeout_seconds: 300,
            allow_consecutive_player: false,
            allow_anyone_stop: false,
        }
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
    pub(crate) fn parse_start(payload: &str) -> Option<Self> {
        if let Some(idiom) = shortcut_argument(payload, "同音接龙") {
            return Some(Self::Start {
                idiom: idiom.to_string(),
                mode: IdiomChainMode::Homophone,
            });
        }
        shortcut_argument(payload, "接龙").map(|idiom| Self::Start {
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

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        self.lock_key() == other.lock_key()
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

pub struct IdiomChainGame {
    enabled: bool,
    lexicon: IdiomLexicon,
    history_limit: usize,
    idle_timeout: Option<Duration>,
    allow_consecutive_player: bool,
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

    pub(crate) fn handle(
        &mut self,
        entertainment: &mut EntertainmentState,
        player: &str,
        command: &IdiomChainCommand,
    ) -> Result<IdiomChainOutcome> {
        if entertainment.active() == Some(EntertainmentKind::TurtleSoup) {
            return Ok(outcome(
                "occupied",
                "海龟汤正在进行，请结束后再开始成语接龙",
            ));
        }
        let acquired_for_start = if matches!(command, IdiomChainCommand::Start { .. }) {
            match entertainment.try_acquire(EntertainmentKind::IdiomChain)? {
                AcquireOutcome::Acquired => true,
                AcquireOutcome::AlreadyOwned => false,
                AcquireOutcome::Occupied(kind) => {
                    return Ok(outcome(
                        "occupied",
                        format!("{}正在进行，请结束后再开始成语接龙", kind.label()),
                    ));
                }
            }
        } else {
            false
        };

        let outcome = self.game.handle(player, command);
        if acquired_for_start && outcome.action != "started" {
            entertainment.release(EntertainmentKind::IdiomChain);
        }
        if matches!(outcome.action, "completed" | "stopped" | "expired") {
            entertainment.release(EntertainmentKind::IdiomChain);
        }
        Ok(outcome)
    }

    pub(crate) fn explain(
        &mut self,
        player: &str,
        command: &IdiomChainCommand,
    ) -> Result<IdiomChainOutcome> {
        Ok(self.game.handle(player, command))
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
            allow_consecutive_player: false,
            allow_anyone_stop: false,
            session: None,
        })
    }
}

impl IdiomChainGame {
    pub fn load(config: IdiomChainConfig) -> Result<Self> {
        let lexicon = if config.enabled {
            IdiomLexicon::load_project_asset()?
        } else {
            IdiomLexicon::default()
        };
        Ok(Self {
            enabled: config.enabled,
            lexicon,
            history_limit: config.history_limit.max(1),
            idle_timeout: (config.idle_timeout_seconds > 0)
                .then(|| Duration::from_secs(config.idle_timeout_seconds)),
            allow_consecutive_player: config.allow_consecutive_player,
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
            return outcome("disabled", "成语接龙未启用");
        }

        if !matches!(command, IdiomChainCommand::Start { .. }) && self.expire_if_idle(now) {
            return outcome("expired", "本局成语接龙已超时，请用 #接龙 成语 重新开局");
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
            return outcome("already-active", "已有成语接龙正在进行，请先 #状态");
        }
        let idiom = match self.lookup_idiom(raw_idiom) {
            Ok(idiom) => idiom,
            Err(reply) => return outcome("invalid-start", reply),
        };
        let expected_first = last_char(&idiom);
        let mut session = IdiomChainSession::new(player, idiom.clone(), expected_first, mode, now);
        session.remember(idiom.clone(), self.history_limit);
        if !self
            .lexicon
            .has_unused_successor(expected_first, mode, &session.used)
        {
            return outcome("dead-end-start", "这个成语没有可接词，请换一个开局成语");
        }
        self.session = Some(session);
        let expected = expected_start_description(expected_first, mode);
        outcome(
            "started",
            format!("{}开始：{}。请接{}的成语。", mode.label(), idiom, expected),
        )
    }

    fn submit(&mut self, player: &str, raw_idiom: &str, now: Instant) -> IdiomChainOutcome {
        let idiom = match self.lookup_idiom(raw_idiom) {
            Ok(idiom) => idiom,
            Err(reply) => return outcome("invalid-submission", reply),
        };
        let Some(session) = self.session.as_ref() else {
            return outcome("no-session", "还没有开局，请用 #接龙 成语");
        };
        if !self.allow_consecutive_player && same_player(player, &session.last_player) {
            return outcome("same-player", "请让其他玩家接下一个成语");
        }
        let first = first_char(&idiom);
        if !session.mode.accepts(session.expected_first, first) {
            return outcome(
                "wrong-first-char",
                format!(
                    "接龙失败：请用{}。",
                    expected_start_description(session.expected_first, session.mode)
                ),
            );
        }
        if session.used.contains(&idiom) {
            return outcome("duplicate", "这个成语已经出现过了");
        }

        let next_first = last_char(&idiom);
        let previous = session.last_idiom.clone();
        let session = self.session.as_mut().expect("session checked above");
        session.last_player = player.trim().to_string();
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
                format!("接龙封龙：{}。共接出 {} 个成语。", idiom, count),
            );
        }
        let expected = expected_start_description(next_first, session.mode);
        outcome(
            "accepted",
            format!("接龙成功：{} -> {}。请接{}。", previous, idiom, expected),
        )
    }

    fn status(&mut self) -> IdiomChainOutcome {
        let Some(session) = self.session.as_ref() else {
            return outcome("no-session", "当前没有进行中的成语接龙");
        };
        outcome(
            "status",
            format!(
                "{}当前第 {} 个：{}。请接{}。",
                session.mode.label(),
                session.total_count,
                session.last_idiom,
                expected_start_description(session.expected_first, session.mode)
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
                "请使用 #解释 成语，或先开始一局接龙",
            );
        };
        let Some(details) = self.lexicon.explanation(&idiom) else {
            return outcome("missing-explanation", "这个成语暂无来源和解释");
        };
        IdiomChainOutcome {
            reply: format!("成语：{}", idiom),
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
            return outcome("no-session", "当前没有进行中的成语接龙");
        };
        let Some(idiom) =
            self.lexicon
                .first_safe_hint(session.expected_first, session.mode, &session.used)
        else {
            return outcome("no-hint", "当前没有不会立即封龙的安全提示");
        };
        outcome("hint", format!("提示：{}", idiom))
    }

    fn stop(&mut self, player: &str) -> IdiomChainOutcome {
        let Some(session) = self.session.as_ref() else {
            return outcome("no-session", "当前没有进行中的成语接龙");
        };
        if !self.allow_anyone_stop && !same_player(player, &session.starter) {
            return outcome("stop-denied", "只有开局玩家可以结束本局接龙");
        }
        let count = session.total_count;
        self.session = None;
        outcome(
            "stopped",
            format!("成语接龙已结束，共接出 {} 个成语。", count),
        )
    }

    fn lookup_idiom(&self, raw_idiom: &str) -> std::result::Result<String, &'static str> {
        let Some(idiom) = normalize_idiom(raw_idiom) else {
            return Err("请输入只包含汉字的成语");
        };
        if !self.lexicon.contains(&idiom) {
            return Err("这个词不在当前成语词库中");
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
    last_player: String,
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
            last_player: player.trim().to_string(),
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
    fn load_project_asset() -> Result<Self> {
        let path = Path::new(PROJECT_IDIOM_ASSET_PATH);
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
                non_empty_or_unspecified(fields.next().unwrap_or_default().trim().to_string()),
                non_empty_or_unspecified(fields.next().unwrap_or_default().trim().to_string()),
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
                        source: non_empty_or_unspecified(source),
                        explanation: non_empty_or_unspecified(explanation),
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
        (IdiomChainMode::Homophone, Some(pinyin)) => {
            format!("与“{}”（{}）同音的字开头", expected, pinyin)
        }
        _ => format!("“{}”字开头", expected),
    }
}

fn same_player(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn non_empty_or_unspecified(value: String) -> String {
    if value.trim().is_empty() {
        "未注明".to_string()
    } else {
        value
    }
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

    fn game(entries: &[&str]) -> IdiomChainGame {
        IdiomChainGame {
            enabled: true,
            lexicon: IdiomLexicon::from_entries(entries.iter().map(|entry| entry.to_string()))
                .expect("valid test lexicon"),
            history_limit: 200,
            idle_timeout: Some(Duration::from_secs(300)),
            allow_consecutive_player: false,
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

    #[test]
    fn loads_the_complete_project_lexicon_asset() {
        let lexicon = IdiomLexicon::load_project_asset().expect("load project idioms");

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
    fn accepts_only_lexicon_entries_with_matching_characters() {
        let mut game = game(&["画蛇添足", "足智多谋", "谋事在人", "人山人海"]);

        let started = game.handle("Alice", &start_exact("画蛇添足！"));
        assert_eq!(started.action, "started");
        assert!(started.reply.contains('足'));

        let wrong_player = game.handle("Alice", &IdiomChainCommand::Submit("足智多谋".to_string()));
        assert_eq!(wrong_player.action, "same-player");

        let accepted = game.handle("Bob", &IdiomChainCommand::Submit("足智多谋".to_string()));
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
}
