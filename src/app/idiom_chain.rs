use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum IdiomChainCommand {
    Start(String),
    Submit(String),
    Status,
    Stop,
    Help,
}

impl IdiomChainCommand {
    pub fn parse(args: &str) -> Self {
        let args = args.trim();
        if args.is_empty() || matches!(args, "帮助" | "?" | "？") {
            return Self::Help;
        }
        if let Some(word) = args.strip_prefix("开始") {
            let word = word.trim_start_matches(['：', ':', ' ', '\t']).trim();
            return if word.is_empty() {
                Self::Help
            } else {
                Self::Start(word.to_string())
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

        if !matches!(command, IdiomChainCommand::Start(_)) && self.expire_if_idle(now) {
            return outcome(
                "expired",
                "本局成语接龙已超时，请用 @接龙 开始 成语 重新开局",
            );
        }

        match command {
            IdiomChainCommand::Start(word) => self.start(player, word, now),
            IdiomChainCommand::Submit(word) => self.submit(player, word, now),
            IdiomChainCommand::Status => self.status(),
            IdiomChainCommand::Stop => self.stop(player),
            IdiomChainCommand::Help => outcome(
                "help",
                "用法：@接龙 开始 自相矛盾；随后 @接龙 成语；@接龙 状态/结束",
            ),
        }
    }

    fn start(&mut self, player: &str, raw_idiom: &str, now: Instant) -> IdiomChainOutcome {
        self.expire_if_idle(now);
        if self.session.is_some() {
            return outcome("already-active", "已有成语接龙正在进行，请先 @接龙 状态");
        }
        let idiom = match self.lookup_idiom(raw_idiom) {
            Ok(idiom) => idiom,
            Err(reply) => return outcome("invalid-start", reply),
        };
        let expected_first = last_char(&idiom);
        let mut session = IdiomChainSession::new(player, idiom.clone(), expected_first, now);
        session.remember(idiom.clone(), self.history_limit);
        if !self
            .lexicon
            .has_unused_starting_with(expected_first, &session.used)
        {
            return outcome("dead-end-start", "这个成语没有可接词，请换一个开局成语");
        }
        self.session = Some(session);
        outcome(
            "started",
            format!(
                "成语接龙开始：{}。请接“{}”字开头的成语。",
                idiom, expected_first
            ),
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
        if first != session.expected_first {
            return outcome(
                "wrong-first-char",
                format!("接龙失败：请用“{}”字开头。", session.expected_first),
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
        let count = session.history.len();
        let no_successor = !self
            .lexicon
            .has_unused_starting_with(next_first, &session.used);
        if no_successor {
            self.session = None;
            return outcome(
                "completed",
                format!("接龙封龙：{}。共接出 {} 个成语。", idiom, count),
            );
        }
        outcome(
            "accepted",
            format!(
                "接龙成功：{} -> {}。请接“{}”字开头。",
                previous, idiom, next_first
            ),
        )
    }

    fn status(&mut self) -> IdiomChainOutcome {
        let Some(session) = self.session.as_ref() else {
            return outcome("no-session", "当前没有进行中的成语接龙");
        };
        outcome(
            "status",
            format!(
                "当前第 {} 个：{}。请接“{}”字开头。",
                session.history.len(),
                session.last_idiom,
                session.expected_first
            ),
        )
    }

    fn stop(&mut self, player: &str) -> IdiomChainOutcome {
        let Some(session) = self.session.as_ref() else {
            return outcome("no-session", "当前没有进行中的成语接龙");
        };
        if !self.allow_anyone_stop && !same_player(player, &session.starter) {
            return outcome("stop-denied", "只有开局玩家可以结束本局接龙");
        }
        let count = session.history.len();
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
    history: VecDeque<String>,
    used: HashSet<String>,
    last_activity: Instant,
}

impl IdiomChainSession {
    fn new(player: &str, idiom: String, expected_first: char, now: Instant) -> Self {
        Self {
            starter: player.trim().to_string(),
            last_player: player.trim().to_string(),
            last_idiom: idiom,
            expected_first,
            history: VecDeque::new(),
            used: HashSet::new(),
            last_activity: now,
        }
    }

    fn remember(&mut self, idiom: String, history_limit: usize) {
        self.used.insert(idiom.clone());
        self.history.push_back(idiom);
        while self.history.len() > history_limit {
            if let Some(expired) = self.history.pop_front() {
                self.used.remove(&expired);
            }
        }
    }
}

#[derive(Default)]
struct IdiomLexicon {
    entries: HashSet<String>,
    entries_by_first: HashMap<char, Vec<String>>,
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
            let idiom = normalize_idiom(trimmed).ok_or_else(|| {
                anyhow::anyhow!(
                    "成语词库条目格式错误: {}:{} ({})",
                    path.display(),
                    line_number + 1,
                    trimmed
                )
            })?;
            entries.push(idiom);
        }
        Self::from_entries(entries)
    }

    fn from_entries(entries: impl IntoIterator<Item = String>) -> Result<Self> {
        let mut lexicon = Self::default();
        for idiom in entries {
            if idiom.chars().count() < 2 {
                bail!("成语词库条目至少需要两个汉字: {}", idiom);
            }
            if !lexicon.entries.insert(idiom.clone()) {
                continue;
            }
            lexicon
                .entries_by_first
                .entry(first_char(&idiom))
                .or_default()
                .push(idiom);
        }
        if lexicon.entries.is_empty() {
            bail!("成语词库没有可用条目");
        }
        Ok(lexicon)
    }

    fn contains(&self, idiom: &str) -> bool {
        self.entries.contains(idiom)
    }

    fn has_unused_starting_with(&self, first: char, used: &HashSet<String>) -> bool {
        self.entries_by_first
            .get(&first)
            .is_some_and(|entries| entries.iter().any(|entry| !used.contains(entry)))
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

fn same_player(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn outcome(action: &'static str, reply: impl Into<String>) -> IdiomChainOutcome {
    IdiomChainOutcome {
        reply: reply.into(),
        action,
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

    #[test]
    fn parses_control_words_and_submission() {
        assert_eq!(
            IdiomChainCommand::parse("开始：画蛇添足"),
            IdiomChainCommand::Start("画蛇添足".to_string())
        );
        assert_eq!(IdiomChainCommand::parse("状态"), IdiomChainCommand::Status);
        assert_eq!(IdiomChainCommand::parse("结束"), IdiomChainCommand::Stop);
        assert_eq!(
            IdiomChainCommand::parse("足智多谋"),
            IdiomChainCommand::Submit("足智多谋".to_string())
        );
    }

    #[test]
    fn loads_the_complete_project_lexicon_asset() {
        let lexicon = IdiomLexicon::load_project_asset().expect("load project idioms");

        assert_eq!(lexicon.len(), 30_345);
        assert!(lexicon.contains("阿鼻地狱"));
        assert!(lexicon.contains("画蛇添足"));
        assert!(lexicon.contains("足智多谋"));
    }

    #[test]
    fn default_config_enables_the_project_lexicon() {
        assert!(IdiomChainConfig::default().enabled);
    }

    #[test]
    fn accepts_only_lexicon_entries_with_matching_characters() {
        let mut game = game(&["画蛇添足", "足智多谋", "谋事在人", "人山人海"]);

        let started = game.handle("Alice", &IdiomChainCommand::Start("画蛇添足！".to_string()));
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
        game.handle("Alice", &IdiomChainCommand::Start("画蛇添足".to_string()));
        game.handle("Bob", &IdiomChainCommand::Submit("足智多谋".to_string()));

        let wrong = game.handle("Carol", &IdiomChainCommand::Submit("人山人海".to_string()));
        assert_eq!(wrong.action, "wrong-first-char");

        game.handle("Carol", &IdiomChainCommand::Submit("谋足谋足".to_string()));
        let repeated = game.handle("Dave", &IdiomChainCommand::Submit("足智多谋".to_string()));
        assert_eq!(repeated.action, "duplicate");
    }

    #[test]
    fn finishes_when_no_unused_successor_exists() {
        let mut game = game(&["画蛇添足", "足智多谋"]);
        game.handle("Alice", &IdiomChainCommand::Start("画蛇添足".to_string()));

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
        game.handle("Alice", &IdiomChainCommand::Start("画蛇添足".to_string()));

        assert_eq!(
            game.handle("Bob", &IdiomChainCommand::Stop).action,
            "stop-denied"
        );
        assert_eq!(
            game.handle("Alice", &IdiomChainCommand::Stop).action,
            "stopped"
        );
    }
}
