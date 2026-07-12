use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use pinyin::ToPinyin;
use serde::{Deserialize, Serialize};

const PROJECT_IDIOM_ASSET_PATH: &str = "assets/idioms.txt";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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
    Help,
}

impl IdiomChainCommand {
    pub fn parse(args: &str) -> Self {
        Self::parse_with_mode(args, IdiomChainMode::Exact)
    }

    pub fn parse_homophone(args: &str) -> Self {
        Self::parse_with_mode(args, IdiomChainMode::Homophone)
    }

    fn parse_with_mode(args: &str, mode: IdiomChainMode) -> Self {
        let args = args.trim();
        if args.is_empty() || matches!(args, "帮助" | "?" | "？") {
            return Self::Help;
        }
        if let Some(word) = args.strip_prefix("开始") {
            let word = word.trim_start_matches(['：', ':', ' ', '\t']).trim();
            return if word.is_empty() {
                Self::Help
            } else {
                Self::Start {
                    idiom: word.to_string(),
                    mode,
                }
            };
        }
        if matches!(args, "状态" | "查看") {
            return Self::Status;
        }
        if matches!(args, "结束" | "停止") {
            return Self::Stop;
        }
        Self::Submit(args.to_string())
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

    pub fn expire_idle_now(&mut self) -> bool {
        self.expire_if_idle(Instant::now())
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
            return outcome(
                "expired",
                "本局成语接龙已超时，请用 @接龙 开始 成语 重新开局",
            );
        }

        match command {
            IdiomChainCommand::Start { idiom, mode } => self.start(player, idiom, *mode, now),
            IdiomChainCommand::Submit(word) => self.submit(player, word, now),
            IdiomChainCommand::Explain(idiom) => self.explain(idiom.as_deref()),
            IdiomChainCommand::Hint => self.hint(),
            IdiomChainCommand::Status => self.status(),
            IdiomChainCommand::Stop => self.stop(player),
            IdiomChainCommand::Help => outcome(
                "help",
                "用法：@接龙 开始 自相矛盾；同音模式用 @同音接龙 开始 成语；随后 @接龙 成语；@接龙 状态/结束",
            ),
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
            return outcome("already-active", "已有成语接龙正在进行，请先 @接龙 状态");
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
            return outcome("no-session", "还没有开局，请用 @接龙 开始 成语");
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
                "请使用 @解释 成语，或先开始一局接龙",
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
        let entries = match mode {
            IdiomChainMode::Exact => self.entries_by_first.get(&first),
            IdiomChainMode::Homophone => pinyin_key(first)
                .as_ref()
                .and_then(|pinyin| self.entries_by_first_pinyin.get(pinyin))
                .or_else(|| self.entries_by_first.get(&first)),
        };
        entries
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
    fn parses_control_words_and_submission() {
        assert_eq!(
            IdiomChainCommand::parse("开始：画蛇添足"),
            start_exact("画蛇添足")
        );
        assert_eq!(IdiomChainCommand::parse("状态"), IdiomChainCommand::Status);
        assert_eq!(IdiomChainCommand::parse("结束"), IdiomChainCommand::Stop);
        assert_eq!(
            IdiomChainCommand::parse("足智多谋"),
            IdiomChainCommand::Submit("足智多谋".to_string())
        );
        assert_eq!(
            IdiomChainCommand::parse_homophone("开始 画蛇添足"),
            IdiomChainCommand::Start {
                idiom: "画蛇添足".to_string(),
                mode: IdiomChainMode::Homophone,
            }
        );
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
