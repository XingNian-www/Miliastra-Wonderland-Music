use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedCommand {
    pub matched: String,
    pub raw: String,
    pub command: UserCommand,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UserCommand {
    Song(SongCommand),
    Pause,
    Resume,
    Play,
    Next,
    Previous,
    Volume(String),
    Status,
    Lyrics,
    Queue,
    QueueDelete(Vec<usize>),
    QueueClear,
    HallDetect,
    HallTime,
    Help,
    Invite(InviteCommand),
    Microphone { username: String },
    DisableCommands { username: String },
    EnableCommands { username: String },
    IdleExit { minutes: u32 },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InviteCommand {
    pub username: String,
    pub seq: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SongCommand {
    pub keyword: String,
    pub source: SongSource,
    pub prefix: String,
    pub prefer_accompaniment: bool,
    pub ai_assisted: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SongSource {
    QqMusic,
    Netease,
    Bilibili,
}

impl SongSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QqMusic => "qqmusic",
            Self::Netease => "netease",
            Self::Bilibili => "bilibili",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PendingCommand {
    pub lock_key: String,
    pub parsed: ParsedCommand,
}

#[derive(Clone, Debug)]
struct CommandLock {
    command: ParsedCommand,
}

#[derive(Default, Debug)]
pub struct CommandLockState {
    locks: HashMap<String, CommandLock>,
}

#[derive(Default, Debug)]
pub struct LockUpdate {
    pub accepted: Vec<PendingCommand>,
    pub skipped: Vec<String>,
    pub unlocked: Vec<String>,
}

pub fn parse_text(text: &str, message_type: &str) -> Option<ParsedCommand> {
    if message_type == "pink" {
        return parse_pink_text(text);
    }
    if message_type != "blue" {
        return None;
    }

    let sep_index = text.find(['：', ':', ']', '】'])?;
    if text.starts_with("播放") && text.contains(" - ") {
        return None;
    }
    if is_feedback_text(text) {
        return None;
    }

    let after_sep = &text[sep_index + text[sep_index..].chars().next()?.len_utf8()..];
    let raw_command_text = after_sep.trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    let command_text = raw_command_text.strip_prefix('@')?.trim_start();
    let matched = COMMANDS
        .iter()
        .find(|command| command_text.starts_with(**command))?;
    if matched.len() < command_text.len() && command_text[matched.len()..].starts_with('/') {
        return None;
    }

    let after_match = command_text[matched.len()..]
        .trim_start_matches(['：', ':', ' ', '\t'])
        .trim_end_matches([']', '】'])
        .trim();
    if !allows_param(matched) && !after_match.is_empty() {
        return None;
    }

    let raw = if after_match.is_empty() {
        (*matched).to_string()
    } else {
        format!("{} {}", matched, after_match)
    };
    let command = parse_command(matched, after_match)?;
    Some(ParsedCommand {
        matched: (*matched).to_string(),
        raw,
        command,
    })
}

fn parse_pink_text(text: &str) -> Option<ParsedCommand> {
    if is_feedback_text(text) {
        return None;
    }
    let username = extract_bracket_username(text)?;
    let sep_index = text.find(['：', ':', ']', '】'])?;
    let after_sep = &text[sep_index + text[sep_index..].chars().next()?.len_utf8()..];
    let command_text = after_sep
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】'])
        .strip_prefix('@')?
        .trim_start();
    if let Some(rest) = command_text.strip_prefix("邀请") {
        let rest = rest.trim_start();
        let digits = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        let seq = digits.parse::<u32>().ok()?;
        if !command_boundary(rest[digits.len()..].chars().next()) {
            return None;
        }
        if !(1..=1000).contains(&seq) {
            return None;
        }
        return Some(ParsedCommand {
            matched: "邀请".to_string(),
            raw: format!("邀请 {} {}", username, seq),
            command: UserCommand::Invite(InviteCommand { username, seq }),
        });
    }
    if let Some(rest) = command_text.strip_prefix("麦克风") {
        let rest = rest.trim_start_matches(['：', ':', ' ', '\t']);
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(ParsedCommand {
                matched: "麦克风".to_string(),
                raw: format!("麦克风 {}", username),
                command: UserCommand::Microphone { username },
            });
        }
        return None;
    }
    if let Some(rest) = command_text.strip_prefix("禁用") {
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(ParsedCommand {
                matched: "禁用".to_string(),
                raw: format!("禁用 {}", username),
                command: UserCommand::DisableCommands { username },
            });
        }
        return None;
    }
    if let Some(rest) = command_text.strip_prefix("启用") {
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(ParsedCommand {
                matched: "启用".to_string(),
                raw: format!("启用 {}", username),
                command: UserCommand::EnableCommands { username },
            });
        }
        return None;
    }
    if let Some(rest) = command_text.strip_prefix("闲置退出") {
        let rest = rest.trim_start_matches(['：', ':', ' ', '\t']);
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(ParsedCommand {
                matched: "闲置退出".to_string(),
                raw: "闲置退出 30".to_string(),
                command: UserCommand::IdleExit { minutes: 30 },
            });
        }
        let digits = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            return None;
        }
        let suffix = rest[digits.len()..].trim_start();
        let suffix = suffix
            .strip_prefix("分钟")
            .or_else(|| suffix.strip_prefix('分'))
            .unwrap_or(suffix);
        if !command_boundary(suffix.chars().next()) {
            return None;
        }
        let minutes = digits.parse::<u32>().ok()?.max(15);
        return Some(ParsedCommand {
            matched: "闲置退出".to_string(),
            raw: format!("闲置退出 {}", minutes),
            command: UserCommand::IdleExit { minutes },
        });
    }
    None
}

fn extract_bracket_username(text: &str) -> Option<String> {
    let (start, close) = if let Some(start) = text.find('[') {
        (start, ']')
    } else {
        (text.find('【')?, '】')
    };
    let end = text[start + 1..].find(close)? + start + 1;
    let username = text[start + 1..end].trim();
    if username.is_empty() {
        None
    } else {
        Some(username.to_string())
    }
}

fn command_boundary(ch: Option<char>) -> bool {
    match ch {
        None => true,
        Some(ch) => {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '，' | ',' | '。' | '.' | '!' | '！' | '?' | '？' | ']' | '】'
                )
        }
    }
}

impl CommandLockState {
    pub fn update(
        &mut self,
        visible_commands: &[ParsedCommand],
        command_executing: bool,
    ) -> LockUpdate {
        let mut update = LockUpdate::default();

        let lock_keys = self.locks.keys().cloned().collect::<Vec<_>>();
        for lock_key in lock_keys {
            let Some(lock) = self.locks.get_mut(&lock_key) else {
                continue;
            };
            let visible = visible_commands
                .iter()
                .any(|item| same_lock_command(&lock.command, item));
            if visible {
                continue;
            }
            if command_executing {
                continue;
            }
            let removed = self.locks.remove(&lock_key).map(|lock| lock.command.raw);
            if let Some(command) = removed {
                update.unlocked.push(command);
            }
        }

        for parsed in visible_commands {
            if let Some(locked) = self.find_locked_command(parsed) {
                update.skipped.push(locked.raw.clone());
                continue;
            }
            let lock_key = lock_key(parsed);
            self.locks.insert(
                lock_key.clone(),
                CommandLock {
                    command: parsed.clone(),
                },
            );
            update.accepted.push(PendingCommand {
                lock_key,
                parsed: parsed.clone(),
            });
        }

        update
    }

    fn find_locked_command(&self, command: &ParsedCommand) -> Option<&ParsedCommand> {
        self.locks
            .values()
            .find(|lock| same_lock_command(&lock.command, command))
            .map(|lock| &lock.command)
    }
}

pub fn lock_key(command: &ParsedCommand) -> String {
    command_lock_key(&command.command)
}

pub fn same_lock_command(left: &ParsedCommand, right: &ParsedCommand) -> bool {
    same_user_command(&left.command, &right.command)
}

fn same_user_command(left: &UserCommand, right: &UserCommand) -> bool {
    match (left, right) {
        (UserCommand::Song(left), UserCommand::Song(right)) => {
            left.source == right.source
                && left.prefer_accompaniment == right.prefer_accompaniment
                && left.ai_assisted == right.ai_assisted
                && same_lock_keyword(&left.keyword, &right.keyword)
        }
        (UserCommand::Invite(left), UserCommand::Invite(right)) => left.seq == right.seq,
        (
            UserCommand::Microphone { username: left },
            UserCommand::Microphone { username: right },
        ) => identity_text(left) == identity_text(right),
        (
            UserCommand::DisableCommands { username: left },
            UserCommand::DisableCommands { username: right },
        ) => identity_text(left) == identity_text(right),
        (
            UserCommand::EnableCommands { username: left },
            UserCommand::EnableCommands { username: right },
        ) => identity_text(left) == identity_text(right),
        (UserCommand::IdleExit { minutes: left }, UserCommand::IdleExit { minutes: right }) => {
            left == right
        }
        (UserCommand::Volume(left), UserCommand::Volume(right)) => {
            identity_text(left) == identity_text(right)
        }
        (UserCommand::QueueDelete(left), UserCommand::QueueDelete(right)) => left == right,
        _ => command_lock_key(left) == command_lock_key(right),
    }
}

fn command_lock_key(command: &UserCommand) -> String {
    match command {
        UserCommand::Song(song) => format!(
            "song:{}:{}:{}:{}",
            song.source.as_str(),
            if song.prefer_accompaniment { 1 } else { 0 },
            if song.ai_assisted { 1 } else { 0 },
            identity_text(&song.keyword)
        ),
        UserCommand::Pause => "pause".to_string(),
        UserCommand::Resume | UserCommand::Play => "play".to_string(),
        UserCommand::Next => "next".to_string(),
        UserCommand::Previous => "previous".to_string(),
        UserCommand::Volume(volume) => format!("volume:{}", identity_text(volume)),
        UserCommand::Status => "status".to_string(),
        UserCommand::Lyrics => "lyrics".to_string(),
        UserCommand::Queue => "queue".to_string(),
        UserCommand::QueueDelete(indexes) => format!(
            "queue_delete:{}",
            indexes
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        UserCommand::QueueClear => "queue_clear".to_string(),
        UserCommand::HallDetect => "hall_detect".to_string(),
        UserCommand::HallTime => "hall_time".to_string(),
        UserCommand::Help => "help".to_string(),
        UserCommand::Invite(invite) => format!("invite:{}", invite.seq),
        UserCommand::Microphone { username } => {
            format!("microphone:{}", identity_text(username))
        }
        UserCommand::DisableCommands { username: _ } => "disable_commands".to_string(),
        UserCommand::EnableCommands { username: _ } => "enable_commands".to_string(),
        UserCommand::IdleExit { minutes } => format!("idle_exit:{}", minutes),
    }
}

fn identity_text(text: &str) -> String {
    let normalized = normalize_lock_text(text);
    if normalized.is_empty() {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        normalized
    }
}

pub fn same_lock_keyword(left: &str, right: &str) -> bool {
    let left = normalize_lock_text(left);
    let right = normalize_lock_text(right);
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }
    let min_length = left.chars().count().min(right.chars().count());
    if left.contains(&right) || right.contains(&left) {
        return min_length >= 2;
    }
    if min_length < 4 {
        return false;
    }
    if left.contains(&right) || right.contains(&left) {
        return true;
    }
    let prefix_length = 16.min(min_length);
    let left_prefix = left.chars().take(prefix_length).collect::<String>();
    let right_prefix = right.chars().take(prefix_length).collect::<String>();
    let distance = levenshtein_distance(&left_prefix, &right_prefix);
    distance <= 1.max(prefix_length / 4)
}

pub fn normalize_lock_text(text: &str) -> String {
    text.chars()
        .filter_map(normalize_char)
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

pub fn parse_song_command(command: &str) -> Option<SongCommand> {
    for (prefix, source, ai_assisted) in SONG_COMMANDS {
        if command.starts_with(prefix) {
            let raw_keyword = command[prefix.len()..].trim();
            let prefer_accompaniment = raw_keyword.contains("伴奏");
            let keyword = raw_keyword
                .replace("伴奏", "")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let keyword = keyword.trim().to_string();
            if keyword.is_empty() {
                return None;
            }
            return Some(SongCommand {
                keyword,
                source: *source,
                prefix: (*prefix).to_string(),
                prefer_accompaniment,
                ai_assisted: *ai_assisted,
            });
        }
    }
    None
}

fn parse_command(matched: &str, param: &str) -> Option<UserCommand> {
    match matched {
        "AI点歌" | "QQ点歌" | "网易点歌" | "B站点歌" | "点歌" => {
            parse_song_command(&format!("{} {}", matched, param)).map(UserCommand::Song)
        }
        "暂停" => Some(UserCommand::Pause),
        "继续" | "恢复" => Some(UserCommand::Resume),
        "播放" => Some(UserCommand::Play),
        "下一首" => Some(UserCommand::Next),
        "上一首" => Some(UserCommand::Previous),
        "音量" => Some(UserCommand::Volume(param.to_string())),
        "状态" => Some(UserCommand::Status),
        "歌词" => Some(UserCommand::Lyrics),
        "队列" => Some(UserCommand::Queue),
        "队列删除" => Some(UserCommand::QueueDelete(parse_queue_indexes(param))),
        "队列清空" => Some(UserCommand::QueueClear),
        "大厅检测" => Some(UserCommand::HallDetect),
        "大厅时间" => Some(UserCommand::HallTime),
        "帮助" => Some(UserCommand::Help),
        _ => None,
    }
}

fn parse_queue_indexes(param: &str) -> Vec<usize> {
    param
        .chars()
        .filter_map(|ch| ch.to_digit(10))
        .filter(|value| (1..=9).contains(value))
        .map(|value| value as usize - 1)
        .collect()
}

fn normalize_char(ch: char) -> Option<char> {
    if ch.is_whitespace() || is_punctuation(ch) {
        return None;
    }
    if ('\u{ff01}'..='\u{ff5e}').contains(&ch) {
        return char::from_u32(ch as u32 - 0xfee0);
    }
    Some(ch)
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
                | '￥'
                | '·'
                | '—'
                | '～'
                | '…'
        )
}

fn is_feedback_text(text: &str) -> bool {
    FEEDBACK_TEXT_PATTERNS
        .iter()
        .any(|pattern| text.contains(pattern))
}

fn allows_param(command: &str) -> bool {
    matches!(
        command,
        "AI点歌" | "点歌" | "QQ点歌" | "网易点歌" | "B站点歌" | "音量" | "队列删除"
    )
}

fn levenshtein_distance(left: &str, right: &str) -> usize {
    let right_chars = right.chars().collect::<Vec<_>>();
    let mut costs = (0..=right_chars.len()).collect::<Vec<_>>();
    for (i, left_ch) in left.chars().enumerate() {
        let mut last = i;
        costs[0] = i + 1;
        for (j, right_ch) in right_chars.iter().enumerate() {
            let old = costs[j + 1];
            costs[j + 1] = if left_ch == *right_ch {
                last
            } else {
                1 + last.min(costs[j]).min(costs[j + 1])
            };
            last = old;
        }
    }
    costs[right_chars.len()]
}

const COMMANDS: &[&str] = &[
    "AI点歌",
    "QQ点歌",
    "网易点歌",
    "B站点歌",
    "点歌",
    "暂停",
    "继续",
    "恢复",
    "播放",
    "下一首",
    "上一首",
    "音量",
    "状态",
    "帮助",
    "歌词",
    "队列删除",
    "队列清空",
    "队列",
    "大厅检测",
    "大厅时间",
];

const SONG_COMMANDS: &[(&str, SongSource, bool)] = &[
    ("AI点歌", SongSource::QqMusic, true),
    ("QQ点歌", SongSource::QqMusic, false),
    ("网易点歌", SongSource::Netease, false),
    ("B站点歌", SongSource::Bilibili, false),
    ("点歌", SongSource::QqMusic, false),
];

const FEEDBACK_TEXT_PATTERNS: &[&str] = &[
    "可用命令",
    "正在搜索",
    "平台无",
    "已暂停",
    "已恢复",
    "音量已设置",
    "队列已加入",
    "队列已满",
    "状态未知",
    "当前正在播放",
    "队列已有",
    "匹配失败",
    "换源",
    "AI自动匹配",
    "AI点歌未启用",
    "AI点歌识别失败",
    "AI匹配中",
    "搜索到:",
    "默认通过",
    "麦克风状态切换",
    "麦克风状态设为",
    "管理员已禁用",
    "管理员已启用",
    "大厅到期时间",
    "大厅时间未知",
    "公共大厅无时间限制",
    "大厅即将到期",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_microphone() {
        let parsed = parse_text("[Alice]：@麦克风", "pink").expect("parse microphone");
        assert_eq!(
            parsed.command,
            UserCommand::Microphone {
                username: "Alice".to_string(),
            }
        );
    }

    #[test]
    fn rejects_microphone_with_param() {
        assert!(parse_text("[Alice]：@麦克风关", "pink").is_none());
        assert!(parse_text("[Alice]：@麦克风开", "pink").is_none());
    }

    #[test]
    fn parses_disable_commands() {
        let parsed = parse_text("[Alice]：@禁用", "pink").expect("parse disable");
        assert_eq!(
            parsed.command,
            UserCommand::DisableCommands {
                username: "Alice".to_string(),
            }
        );
    }

    #[test]
    fn parses_enable_commands() {
        let parsed = parse_text("[Alice]：@启用", "pink").expect("parse enable");
        assert_eq!(
            parsed.command,
            UserCommand::EnableCommands {
                username: "Alice".to_string(),
            }
        );
    }

    #[test]
    fn rejects_disable_with_param() {
        assert!(parse_text("[Alice]：@禁用命令", "pink").is_none());
    }

    #[test]
    fn parses_idle_exit_default() {
        let parsed = parse_text("[Alice]：@闲置退出", "pink").expect("parse idle exit");
        assert_eq!(parsed.command, UserCommand::IdleExit { minutes: 30 });
    }

    #[test]
    fn parses_idle_exit_with_minimum() {
        let parsed = parse_text("[Alice]：@闲置退出 5", "pink").expect("parse idle exit");
        assert_eq!(parsed.command, UserCommand::IdleExit { minutes: 15 });
    }

    #[test]
    fn parses_idle_exit_with_minutes_suffix() {
        let parsed = parse_text("[Alice]：@闲置退出 20分钟", "pink").expect("parse idle exit");
        assert_eq!(parsed.command, UserCommand::IdleExit { minutes: 20 });
    }

    #[test]
    fn parses_ai_song_command() {
        let parsed = parse_text("用户：@AI点歌 晴天 周杰伦", "blue").expect("parse ai song");
        assert_eq!(
            parsed.command,
            UserCommand::Song(SongCommand {
                keyword: "晴天 周杰伦".to_string(),
                source: SongSource::QqMusic,
                prefix: "AI点歌".to_string(),
                prefer_accompaniment: false,
                ai_assisted: true,
            })
        );
    }

    #[test]
    fn rejects_yellow_hall_command() {
        assert!(parse_text("用户：@帮助", "yellow").is_none());
    }

    #[test]
    fn parses_default_song_command() {
        let parsed = parse_text("用户：@点歌 晴天 周杰伦", "blue").expect("parse default song");
        assert_eq!(
            parsed.command,
            UserCommand::Song(SongCommand {
                keyword: "晴天 周杰伦".to_string(),
                source: SongSource::QqMusic,
                prefix: "点歌".to_string(),
                prefer_accompaniment: false,
                ai_assisted: false,
            })
        );
    }

    #[test]
    fn parses_hidden_bilibili_song_command() {
        let parsed = parse_text("用户：@B站点歌 晴天 周杰伦", "blue").expect("parse bilibili song");
        assert_eq!(
            parsed.command,
            UserCommand::Song(SongCommand {
                keyword: "晴天 周杰伦".to_string(),
                source: SongSource::Bilibili,
                prefix: "B站点歌".to_string(),
                prefer_accompaniment: false,
                ai_assisted: false,
            })
        );
    }
}
