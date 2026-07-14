use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::entertainment::EntertainmentKind;
use super::turtle_soup::TurtleSoupCommand;
use super::undercover::UndercoverCommand;
use crate::features::card_games::LandlordCommand;
use crate::features::idiom_chain::{IdiomChainCommand, IdiomChainMode};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedCommand {
    pub matched: String,
    pub raw: String,
    pub user_command: String,
    pub message_type: String,
    pub username: String,
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
    EntertainmentHelp,
    IdiomChain(IdiomChainCommand),
    Landlord(LandlordCommand),
    TurtleSoup(TurtleSoupCommand),
    Undercover(UndercoverCommand),
    Invite(InviteCommand),
    Moderation(ModerationCommand),
    Microphone { username: String },
    DisableCommands { username: String },
    EnableCommands { username: String },
    IdleExit { minutes: u32 },
    ChatListenerMode(ChatListenerModeCommand),
    CustomWorkflow(CustomWorkflowCommand),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChatListenerModeCommand {
    Primary,
    Secondary,
    Status,
}

impl ChatListenerModeCommand {
    pub fn label(self) -> &'static str {
        match self {
            Self::Primary => "一级",
            Self::Secondary => "二级",
            Self::Status => "状态",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomWorkflowCommand {
    pub name: String,
    pub workflow: String,
    pub args: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InviteCommand {
    pub username: String,
    pub seq: Option<u32>,
    pub password: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModerationCommand {
    pub action: ModerationAction,
    pub uid: String,
    pub requester: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ModerationAction {
    Blacklist,
    BlockChat,
}

impl ModerationAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Blacklist => "拉黑",
            Self::BlockChat => "屏蔽",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SongCommand {
    pub keyword: String,
    pub source: SongSource,
    pub prefix: String,
    pub prefer_accompaniment: bool,
    pub ai_assisted: bool,
    pub friend_username: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SongSource {
    All,
    QqMusic,
    Netease,
    Bilibili,
}

impl SongSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "",
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
    let user_command = user_command_text(raw_command_text);
    let command_text = raw_command_text.strip_prefix('@')?.trim_start();
    let matched = COMMANDS
        .iter()
        .find(|command| strip_ascii_case_prefix(command_text, command).is_some())?;
    let (matched, after_command) = (*matched, strip_ascii_case_prefix(command_text, matched)?);
    if !after_command.is_empty() && after_command.starts_with('/') {
        return None;
    }

    let after_match = after_command
        .trim_start_matches(['：', ':', ' ', '\t'])
        .trim_end_matches([']', '】'])
        .trim();
    if !allows_param(matched) && !after_match.is_empty() {
        return None;
    }

    let username = text[..sep_index]
        .trim_matches(['[', '【', ']', '】', ' ', '\t'])
        .to_string();
    let raw = if after_match.is_empty() {
        matched.to_string()
    } else {
        format!("{} {}", matched, after_match)
    };
    let command = parse_command(matched, after_match)?;
    Some(ParsedCommand {
        matched: matched.to_string(),
        raw,
        user_command,
        message_type: message_type.to_string(),
        username,
        command,
    })
}

pub(super) fn parse_entertainment_shortcut(
    text: &str,
    message_type: &str,
    active: Option<EntertainmentKind>,
) -> Option<ParsedCommand> {
    if !matches!(message_type, "blue" | "pink") || is_feedback_text(text) {
        return None;
    }
    let username = if message_type == "pink" {
        extract_bracket_username(text)?
    } else {
        let sep_index = text.find(['：', ':', ']', '】'])?;
        text[..sep_index]
            .trim_matches(['[', '【', ']', '】', ' ', '\t'])
            .to_string()
    };
    let sep_index = text.find(['：', ':', ']', '】'])?;
    let separator_len = text[sep_index..].chars().next()?.len_utf8();
    let raw_command_text =
        text[sep_index + separator_len..].trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    let payload = raw_command_text
        .strip_prefix('#')
        .or_else(|| raw_command_text.strip_prefix('＃'))?
        .trim_end_matches([']', '】'])
        .trim();
    if payload.is_empty() {
        return None;
    }
    let command = if message_type == "blue" {
        parse_entertainment_start(payload).or_else(|| match active {
            Some(EntertainmentKind::IdiomChain) => parse_idiom_shortcut(payload),
            Some(EntertainmentKind::Landlord | EntertainmentKind::RunFast) => {
                parse_card_shortcut(payload)
            }
            Some(EntertainmentKind::TurtleSoup) => parse_turtle_soup_shortcut(payload),
            Some(EntertainmentKind::Undercover) => parse_undercover_hall_shortcut(payload),
            None => None,
        })?
    } else {
        match active {
            Some(EntertainmentKind::Landlord | EntertainmentKind::RunFast)
                if normalized_shortcut(payload) == "手牌" =>
            {
                UserCommand::Landlord(LandlordCommand::Hand)
            }
            Some(EntertainmentKind::Undercover) => parse_undercover_friend_shortcut(payload)?,
            _ => return None,
        }
    };
    let raw = match &command {
        UserCommand::Undercover(UndercoverCommand::Vote(_)) => "投票".to_string(),
        UserCommand::Undercover(UndercoverCommand::Describe(_)) => "卧底描述".to_string(),
        _ => payload.to_string(),
    };
    Some(ParsedCommand {
        matched: "#".to_string(),
        raw,
        user_command: user_command_text(raw_command_text),
        message_type: message_type.to_string(),
        username,
        command,
    })
}

fn parse_entertainment_start(payload: &str) -> Option<UserCommand> {
    if let Some(idiom) = shortcut_argument(payload, "同音接龙") {
        return Some(UserCommand::IdiomChain(IdiomChainCommand::Start {
            idiom: idiom.to_string(),
            mode: IdiomChainMode::Homophone,
        }));
    }
    if let Some(idiom) = shortcut_argument(payload, "接龙") {
        return Some(UserCommand::IdiomChain(IdiomChainCommand::Start {
            idiom: idiom.to_string(),
            mode: IdiomChainMode::Exact,
        }));
    }
    match normalized_shortcut(payload).as_str() {
        "斗地主" => Some(UserCommand::Landlord(LandlordCommand::Start)),
        "跑得快" => Some(UserCommand::Landlord(LandlordCommand::RunFastStart)),
        "海龟汤" => Some(UserCommand::TurtleSoup(TurtleSoupCommand::Start)),
        "卧底" => Some(UserCommand::Undercover(UndercoverCommand::CreateSingle)),
        "卧底双" => Some(UserCommand::Undercover(UndercoverCommand::CreateDouble)),
        "娱乐" | "帮助" => Some(UserCommand::EntertainmentHelp),
        _ => None,
    }
}

fn shortcut_argument<'a>(payload: &'a str, prefix: &str) -> Option<&'a str> {
    let value = payload.strip_prefix(prefix)?;
    let value = value.trim_start_matches(['：', ':', ' ', '\t']).trim();
    (!value.is_empty()).then_some(value)
}

fn normalized_shortcut(payload: &str) -> String {
    payload.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn parse_idiom_shortcut(payload: &str) -> Option<UserCommand> {
    let normalized = normalized_shortcut(payload);
    let command = match normalized.as_str() {
        "提示" => IdiomChainCommand::Hint,
        "状态" => IdiomChainCommand::Status,
        "结束" => IdiomChainCommand::Stop,
        "解释" => IdiomChainCommand::Explain(None),
        _ => {
            if let Some(idiom) = shortcut_argument(payload, "解释") {
                IdiomChainCommand::Explain(Some(idiom.to_string()))
            } else {
                IdiomChainCommand::Submit(payload.to_string())
            }
        }
    };
    Some(UserCommand::IdiomChain(command))
}

fn parse_card_shortcut(payload: &str) -> Option<UserCommand> {
    let normalized = normalized_shortcut(payload);
    let command = match normalized.as_str() {
        "加入" => LandlordCommand::Join,
        "抢" => LandlordCommand::Rob,
        "不抢" => LandlordCommand::Decline,
        "过" => LandlordCommand::Pass,
        "状态" => LandlordCommand::Status,
        "结束" => LandlordCommand::Exit,
        "手牌" => return None,
        _ => {
            let cards = payload
                .strip_prefix('出')
                .unwrap_or(payload)
                .trim_start_matches(['：', ':', ' ', '\t'])
                .trim();
            if cards.is_empty() {
                return None;
            }
            LandlordCommand::Play(cards.to_string())
        }
    };
    Some(UserCommand::Landlord(command))
}

fn parse_turtle_soup_shortcut(payload: &str) -> Option<UserCommand> {
    match normalized_shortcut(payload).as_str() {
        "状态" => Some(UserCommand::TurtleSoup(TurtleSoupCommand::Status)),
        "结束" => Some(UserCommand::TurtleSoup(TurtleSoupCommand::End)),
        _ => None,
    }
}

fn parse_undercover_hall_shortcut(payload: &str) -> Option<UserCommand> {
    let normalized = normalized_shortcut(payload);
    let command = match normalized.as_str() {
        "开局" => UndercoverCommand::Start,
        "状态" => UndercoverCommand::Status,
        "退出" => UndercoverCommand::Exit,
        "结束" => UndercoverCommand::End,
        "加入" | "手牌" => return None,
        _ if parse_vote_position(&normalized).is_some() => return None,
        _ => UndercoverCommand::Describe(payload.to_string()),
    };
    Some(UserCommand::Undercover(command))
}

fn parse_undercover_friend_shortcut(payload: &str) -> Option<UserCommand> {
    let normalized = normalized_shortcut(payload);
    let command = match normalized.as_str() {
        "加入" => UndercoverCommand::Join,
        "退出" => UndercoverCommand::Exit,
        _ => UndercoverCommand::Vote(parse_vote_position(&normalized)?),
    };
    Some(UserCommand::Undercover(command))
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

fn parse_pink_text(text: &str) -> Option<ParsedCommand> {
    if is_feedback_text(text) {
        return None;
    }
    let username = extract_bracket_username(text)?;
    let sep_index = text.find(['：', ':', ']', '】'])?;
    let after_sep = &text[sep_index + text[sep_index..].chars().next()?.len_utf8()..];
    let raw_command_text = after_sep.trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    let user_command = user_command_text(raw_command_text);
    let command_text = raw_command_text.strip_prefix('@')?.trim_start();
    if let Some(rest) = strip_ascii_case_prefix(command_text, "监听模式") {
        let value = rest.trim_start_matches(['：', ':', ' ', '\t']).trim();
        let mode = match value {
            "一级" => ChatListenerModeCommand::Primary,
            "二级" => ChatListenerModeCommand::Secondary,
            "状态" => ChatListenerModeCommand::Status,
            _ => return None,
        };
        return Some(ParsedCommand {
            matched: "监听模式".to_string(),
            raw: format!("监听模式 {}", mode.label()),
            user_command,
            message_type: "pink".to_string(),
            username,
            command: UserCommand::ChatListenerMode(mode),
        });
    }
    if let Some(song) = parse_pink_song_command(command_text, &username) {
        return Some(ParsedCommand {
            matched: song.prefix.clone(),
            raw: format!("{} {} {}", username, song.prefix, song.keyword),
            user_command,
            message_type: "pink".to_string(),
            username,
            command: UserCommand::Song(song),
        });
    }
    if let Some(rest) = strip_ascii_case_prefix(command_text, "邀请") {
        let rest = rest.trim_start();
        let digits = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            return None;
        }
        if !invite_arg_trailing_is_empty(&rest[digits.len()..]) {
            return None;
        }
        let (seq, password, raw_param) = parse_invite_arg(&digits)?;
        return Some(ParsedCommand {
            matched: "邀请".to_string(),
            raw: format!("邀请 {} {}", username, raw_param),
            user_command,
            message_type: "pink".to_string(),
            username: username.clone(),
            command: UserCommand::Invite(InviteCommand {
                username,
                seq,
                password,
            }),
        });
    }
    if let Some(command) = parse_moderation_command(command_text, &username, &user_command) {
        return Some(command);
    }
    if let Some(rest) = strip_ascii_case_prefix(command_text, "麦克风") {
        let rest = rest.trim_start_matches(['：', ':', ' ', '\t']);
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(ParsedCommand {
                matched: "麦克风".to_string(),
                raw: format!("麦克风 {}", username),
                user_command,
                message_type: "pink".to_string(),
                username: username.clone(),
                command: UserCommand::Microphone { username },
            });
        }
        return None;
    }
    if let Some(rest) = strip_ascii_case_prefix(command_text, "禁用") {
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(ParsedCommand {
                matched: "禁用".to_string(),
                raw: format!("禁用 {}", username),
                user_command,
                message_type: "pink".to_string(),
                username: username.clone(),
                command: UserCommand::DisableCommands { username },
            });
        }
        return None;
    }
    if let Some(rest) = strip_ascii_case_prefix(command_text, "启用") {
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(ParsedCommand {
                matched: "启用".to_string(),
                raw: format!("启用 {}", username),
                user_command,
                message_type: "pink".to_string(),
                username: username.clone(),
                command: UserCommand::EnableCommands { username },
            });
        }
        return None;
    }
    if let Some(rest) = strip_ascii_case_prefix(command_text, "闲置退出") {
        let rest = rest.trim_start_matches(['：', ':', ' ', '\t']);
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(ParsedCommand {
                matched: "闲置退出".to_string(),
                raw: "闲置退出 30".to_string(),
                user_command,
                message_type: "pink".to_string(),
                username: username.clone(),
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
            user_command,
            message_type: "pink".to_string(),
            username,
            command: UserCommand::IdleExit { minutes },
        });
    }
    None
}

fn user_command_text(text: &str) -> String {
    text.trim()
        .trim_end_matches([']', '】'])
        .trim_end()
        .to_string()
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

fn invite_arg_trailing_is_empty(value: &str) -> bool {
    value
        .trim_start_matches([' ', '\t'])
        .trim_end_matches([']', '】'])
        .trim()
        .is_empty()
}

fn parse_invite_arg(digits: &str) -> Option<(Option<u32>, Option<String>, String)> {
    match digits.len() {
        1..=3 => {
            let seq = digits.parse::<u32>().ok()?;
            if seq == 0 {
                return None;
            }
            Some((Some(seq), None, seq.to_string()))
        }
        6 => Some((None, Some(digits.to_string()), "6位密码".to_string())),
        _ => None,
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
    let key = command_lock_key(&command.command);
    if matches!(
        command.command,
        UserCommand::Landlord(_) | UserCommand::Undercover(_)
    ) {
        format!("{}:{}", key, identity_text(&command.username))
    } else {
        key
    }
}

pub fn same_lock_command(left: &ParsedCommand, right: &ParsedCommand) -> bool {
    if matches!(left.command, UserCommand::Landlord(_))
        && matches!(right.command, UserCommand::Landlord(_))
        && identity_text(&left.username) != identity_text(&right.username)
    {
        return false;
    }
    if matches!(left.command, UserCommand::Undercover(_))
        && matches!(right.command, UserCommand::Undercover(_))
        && identity_text(&left.username) != identity_text(&right.username)
    {
        return false;
    }
    same_user_command(&left.command, &right.command)
}

fn same_user_command(left: &UserCommand, right: &UserCommand) -> bool {
    match (left, right) {
        (UserCommand::Song(left), UserCommand::Song(right)) => {
            identity_text(&left.friend_username) == identity_text(&right.friend_username)
                && left.source == right.source
                && left.prefer_accompaniment == right.prefer_accompaniment
                && same_lock_keyword(&left.keyword, &right.keyword)
        }
        (UserCommand::Invite(left), UserCommand::Invite(right)) => match (left.seq, right.seq) {
            (Some(left_seq), Some(right_seq)) => left_seq == right_seq,
            (None, None) => {
                identity_text(&left.username) == identity_text(&right.username)
                    && left.password == right.password
            }
            _ => false,
        },
        (UserCommand::Moderation(left), UserCommand::Moderation(right)) => {
            left.action == right.action && left.uid == right.uid
        }
        (
            UserCommand::Microphone { username: left },
            UserCommand::Microphone { username: right },
        ) => identity_text(left) == identity_text(right),
        (UserCommand::IdleExit { minutes: left }, UserCommand::IdleExit { minutes: right }) => {
            left == right
        }
        (UserCommand::ChatListenerMode(left), UserCommand::ChatListenerMode(right)) => {
            left == right
        }
        (UserCommand::CustomWorkflow(left), UserCommand::CustomWorkflow(right)) => {
            identity_text(&left.workflow) == identity_text(&right.workflow)
                && identity_text(&left.args) == identity_text(&right.args)
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
            identity_text(&song.friend_username),
            song.source.as_str(),
            if song.prefer_accompaniment { 1 } else { 0 },
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
        UserCommand::EntertainmentHelp => "entertainment_help".to_string(),
        UserCommand::IdiomChain(command) => match command {
            IdiomChainCommand::Start { idiom, mode } => {
                format!("idiom_chain:start:{}", identity_text(idiom))
                    + match mode {
                        IdiomChainMode::Exact => ":exact",
                        IdiomChainMode::Homophone => ":homophone",
                    }
            }
            IdiomChainCommand::Submit(idiom) => {
                format!("idiom_chain:submit:{}", identity_text(idiom))
            }
            IdiomChainCommand::Explain(idiom) => format!(
                "idiom_chain:explain:{}",
                idiom.as_deref().map(identity_text).unwrap_or_default()
            ),
            IdiomChainCommand::Hint => "idiom_chain:hint".to_string(),
            IdiomChainCommand::Status => "idiom_chain:status".to_string(),
            IdiomChainCommand::Stop => "idiom_chain:stop".to_string(),
        },
        UserCommand::Landlord(command) => match command {
            LandlordCommand::Start => "landlord:start".to_string(),
            LandlordCommand::RunFastStart => "run_fast:start".to_string(),
            LandlordCommand::Join => "landlord:join".to_string(),
            LandlordCommand::Rob => "landlord:rob".to_string(),
            LandlordCommand::Decline => "landlord:decline".to_string(),
            LandlordCommand::Status => "landlord:status".to_string(),
            LandlordCommand::Play(cards) => {
                format!("landlord:play:{}", identity_text(cards))
            }
            LandlordCommand::Pass => "landlord:pass".to_string(),
            LandlordCommand::Hand => "landlord:hand".to_string(),
            LandlordCommand::Exit => "landlord:exit".to_string(),
        },
        UserCommand::TurtleSoup(command) => match command {
            TurtleSoupCommand::Start => "turtle_soup:start".to_string(),
            TurtleSoupCommand::Status => "turtle_soup:status".to_string(),
            TurtleSoupCommand::End => "turtle_soup:end".to_string(),
        },
        UserCommand::Undercover(command) => match command {
            UndercoverCommand::CreateSingle => "undercover:create:single".to_string(),
            UndercoverCommand::CreateDouble => "undercover:create:double".to_string(),
            UndercoverCommand::Join => "undercover:join".to_string(),
            UndercoverCommand::Start => "undercover:start".to_string(),
            UndercoverCommand::Status => "undercover:status".to_string(),
            UndercoverCommand::Exit => "undercover:exit".to_string(),
            UndercoverCommand::End => "undercover:end".to_string(),
            UndercoverCommand::Describe(text) => {
                format!("undercover:describe:{}", identity_text(text))
            }
            UndercoverCommand::Vote(position) => format!("undercover:vote:{position}"),
        },
        UserCommand::Invite(invite) => {
            if let Some(seq) = invite.seq {
                format!("invite:{}", seq)
            } else {
                format!(
                    "invite_password:{}:{}",
                    identity_text(&invite.username),
                    invite.password.as_deref().unwrap_or_default()
                )
            }
        }
        UserCommand::Moderation(command) => {
            format!("moderation:{}:{}", command.action.label(), command.uid)
        }
        UserCommand::Microphone { username } => {
            format!("microphone:{}", identity_text(username))
        }
        UserCommand::DisableCommands { username: _ } => "disable_commands".to_string(),
        UserCommand::EnableCommands { username: _ } => "enable_commands".to_string(),
        UserCommand::IdleExit { minutes } => format!("idle_exit:{}", minutes),
        UserCommand::ChatListenerMode(mode) => format!("chat_listener:{}", mode.label()),
        UserCommand::CustomWorkflow(command) => {
            format!(
                "custom_workflow:{}:{}",
                identity_text(&command.workflow),
                identity_text(&command.args)
            )
        }
    }
}

fn parse_moderation_command(
    command_text: &str,
    username: &str,
    user_command: &str,
) -> Option<ParsedCommand> {
    for (prefix, action) in [
        ("拉黑UID", ModerationAction::Blacklist),
        ("屏蔽UID", ModerationAction::BlockChat),
        ("拉黑", ModerationAction::Blacklist),
        ("屏蔽", ModerationAction::BlockChat),
    ] {
        let Some(rest) = strip_ascii_case_prefix(command_text, prefix) else {
            continue;
        };
        let digits = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if digits.len() != 9 {
            return None;
        }
        if !command_boundary(rest[digits.len()..].chars().next()) {
            return None;
        }
        return Some(ParsedCommand {
            matched: prefix.to_string(),
            raw: format!("{} {} {}", prefix, username, digits),
            user_command: user_command.to_string(),
            message_type: "pink".to_string(),
            username: username.to_string(),
            command: UserCommand::Moderation(ModerationCommand {
                action,
                uid: digits,
                requester: username.to_string(),
            }),
        });
    }
    None
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

pub(super) fn strip_ascii_case_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

pub fn parse_song_command(command: &str) -> Option<SongCommand> {
    for (prefix, source, ai_assisted) in SONG_COMMANDS {
        if strip_ascii_case_prefix(command, prefix).is_some() {
            return parse_song_command_with_source(command, prefix, *source, *ai_assisted);
        }
    }
    None
}

fn parse_pink_song_command(command: &str, username: &str) -> Option<SongCommand> {
    for (prefix, source, ai_assisted) in PINK_SONG_COMMANDS {
        if strip_ascii_case_prefix(command, prefix).is_some() {
            let mut song = parse_song_command_with_source(command, prefix, *source, *ai_assisted)?;
            song.friend_username = username.to_string();
            return Some(song);
        }
    }
    None
}

fn parse_song_command_with_source(
    command: &str,
    prefix: &str,
    source: SongSource,
    ai_assisted: bool,
) -> Option<SongCommand> {
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
    Some(SongCommand {
        keyword,
        source,
        prefix: prefix.to_string(),
        prefer_accompaniment,
        ai_assisted,
        friend_username: String::new(),
    })
}

fn parse_command(matched: &str, param: &str) -> Option<UserCommand> {
    match matched {
        "AI点歌" | "AI搜索" | "QQ点歌" | "QQ搜索" | "网易点歌" | "网易搜索" | "点歌" | "搜索" => {
            parse_song_command(&format!("{} {}", matched, param)).map(UserCommand::Song)
        }
        "暂停" => Some(UserCommand::Pause),
        "继续" | "恢复" => Some(UserCommand::Resume),
        "播放" => Some(UserCommand::Play),
        "下一首" | "下一曲" => Some(UserCommand::Next),
        "上一首" | "上一曲" => Some(UserCommand::Previous),
        "音量" => Some(UserCommand::Volume(param.to_string())),
        "状态" => Some(UserCommand::Status),
        "歌词" => Some(UserCommand::Lyrics),
        "队列" | "列表" => Some(UserCommand::Queue),
        "队列删除" => Some(UserCommand::QueueDelete(parse_queue_indexes(param))),
        "队列清空" => Some(UserCommand::QueueClear),
        "大厅检测" => Some(UserCommand::HallDetect),
        "大厅时间" => Some(UserCommand::HallTime),
        "帮助" => Some(UserCommand::Help),
        "娱乐帮助" => Some(UserCommand::EntertainmentHelp),
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
        "AI点歌"
            | "AI搜索"
            | "点歌"
            | "搜索"
            | "QQ点歌"
            | "QQ搜索"
            | "网易点歌"
            | "网易搜索"
            | "音量"
            | "队列删除"
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
    "AI搜索",
    "QQ点歌",
    "QQ搜索",
    "网易点歌",
    "网易搜索",
    "点歌",
    "搜索",
    "暂停",
    "继续",
    "恢复",
    "播放",
    "下一首",
    "下一曲",
    "上一首",
    "上一曲",
    "音量",
    "状态",
    "帮助",
    "娱乐帮助",
    "歌词",
    "队列删除",
    "队列清空",
    "队列",
    "列表",
    "大厅检测",
    "大厅时间",
];

const SONG_COMMANDS: &[(&str, SongSource, bool)] = &[
    ("AI点歌", SongSource::QqMusic, true),
    ("AI搜索", SongSource::QqMusic, true),
    ("QQ点歌", SongSource::QqMusic, false),
    ("QQ搜索", SongSource::QqMusic, false),
    ("网易点歌", SongSource::Netease, false),
    ("网易搜索", SongSource::Netease, false),
    ("点歌", SongSource::QqMusic, false),
    ("搜索", SongSource::QqMusic, false),
];

const PINK_SONG_COMMANDS: &[(&str, SongSource, bool)] = &[
    ("AI点歌", SongSource::All, true),
    ("AI搜索", SongSource::All, true),
    ("QQ点歌", SongSource::QqMusic, false),
    ("QQ搜索", SongSource::QqMusic, false),
    ("网易点歌", SongSource::Netease, false),
    ("网易搜索", SongSource::Netease, false),
    ("B站点歌", SongSource::Bilibili, false),
    ("B站搜索", SongSource::Bilibili, false),
    ("点歌", SongSource::QqMusic, false),
    ("搜索", SongSource::QqMusic, false),
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
    fn parses_hash_idiom_chain_commands_by_active_game() {
        let started = parse_entertainment_shortcut("用户：#接龙 画蛇添足", "blue", None)
            .expect("parse idiom chain start");
        assert_eq!(
            started.command,
            UserCommand::IdiomChain(IdiomChainCommand::Start {
                idiom: "画蛇添足".to_string(),
                mode: IdiomChainMode::Exact,
            })
        );

        let homophone = parse_entertainment_shortcut("用户：＃同音接龙 画蛇添足", "blue", None)
            .expect("parse homophone idiom chain start");
        assert_eq!(
            homophone.command,
            UserCommand::IdiomChain(IdiomChainCommand::Start {
                idiom: "画蛇添足".to_string(),
                mode: IdiomChainMode::Homophone,
            })
        );

        let submitted = parse_entertainment_shortcut(
            "用户：#足智多谋",
            "blue",
            Some(EntertainmentKind::IdiomChain),
        )
        .expect("parse idiom chain submit");
        assert_eq!(
            submitted.command,
            UserCommand::IdiomChain(IdiomChainCommand::Submit("足智多谋".to_string()))
        );

        let explained = parse_entertainment_shortcut(
            "用户：#解释 画蛇添足",
            "blue",
            Some(EntertainmentKind::IdiomChain),
        )
        .expect("parse idiom explanation command");
        assert_eq!(
            explained.command,
            UserCommand::IdiomChain(IdiomChainCommand::Explain(Some("画蛇添足".to_string())))
        );
        let hint = parse_entertainment_shortcut(
            "用户：#提示",
            "blue",
            Some(EntertainmentKind::IdiomChain),
        )
        .expect("parse idiom hint command");
        assert_eq!(
            hint.command,
            UserCommand::IdiomChain(IdiomChainCommand::Hint)
        );
    }

    #[test]
    fn explicit_hash_starts_are_available_without_an_active_game() {
        for (text, expected) in [
            (
                "用户：#斗地主",
                UserCommand::Landlord(LandlordCommand::Start),
            ),
            (
                "用户：#跑得快",
                UserCommand::Landlord(LandlordCommand::RunFastStart),
            ),
            (
                "用户：#海龟汤",
                UserCommand::TurtleSoup(TurtleSoupCommand::Start),
            ),
            (
                "用户：#卧底",
                UserCommand::Undercover(UndercoverCommand::CreateSingle),
            ),
            (
                "用户：# 卧 底 双",
                UserCommand::Undercover(UndercoverCommand::CreateDouble),
            ),
            ("用户：#娱乐", UserCommand::EntertainmentHelp),
            ("用户：＃帮助", UserCommand::EntertainmentHelp),
        ] {
            assert_eq!(
                parse_entertainment_shortcut(text, "blue", None)
                    .unwrap_or_else(|| panic!("parse {text}"))
                    .command,
                expected,
                "{text}"
            );
        }
        assert_eq!(
            parse_text("用户：@娱乐帮助", "blue").unwrap().command,
            UserCommand::EntertainmentHelp
        );
        assert!(parse_text("用户：@娱乐", "blue").is_none());
    }

    #[test]
    fn parses_hash_card_commands_by_active_game_and_message_source() {
        for (text, expected) in [
            ("用户：#加入", LandlordCommand::Join),
            ("用户：#抢", LandlordCommand::Rob),
            ("用户：#不抢", LandlordCommand::Decline),
            ("用户：#出 3334", LandlordCommand::Play("3334".to_string())),
            ("用户：#10 10", LandlordCommand::Play("10 10".to_string())),
            ("用户：＃小王", LandlordCommand::Play("小王".to_string())),
            ("用户：#过", LandlordCommand::Pass),
            ("用户：#状态", LandlordCommand::Status),
            ("用户：#结束", LandlordCommand::Exit),
        ] {
            let parsed =
                parse_entertainment_shortcut(text, "blue", Some(EntertainmentKind::Landlord))
                    .expect("parse landlord hall command");
            assert_eq!(parsed.command, UserCommand::Landlord(expected), "{text}");
        }

        let hand =
            parse_entertainment_shortcut("[用户]：#手牌", "pink", Some(EntertainmentKind::RunFast))
                .expect("parse private hand command");
        assert_eq!(hand.command, UserCommand::Landlord(LandlordCommand::Hand));
        assert!(
            parse_entertainment_shortcut("用户：#手牌", "blue", Some(EntertainmentKind::Landlord))
                .is_none()
        );
    }

    #[test]
    fn parses_hash_undercover_commands_by_active_game_and_message_source() {
        let cases = [
            ("用户：#开局", UndercoverCommand::Start),
            ("用户：#状 态", UndercoverCommand::Status),
            ("用户：#退出", UndercoverCommand::Exit),
            ("用户：#结束", UndercoverCommand::End),
            (
                "用户：#一种常见事物",
                UndercoverCommand::Describe("一种常见事物".to_string()),
            ),
        ];
        for (text, expected) in cases {
            let parsed =
                parse_entertainment_shortcut(text, "blue", Some(EntertainmentKind::Undercover))
                    .unwrap_or_else(|| panic!("parse {text}"));
            assert_eq!(parsed.command, UserCommand::Undercover(expected));
        }
        assert!(
            parse_entertainment_shortcut(
                "用户：#加入",
                "blue",
                Some(EntertainmentKind::Undercover)
            )
            .is_none()
        );

        let join = parse_entertainment_shortcut(
            "[用户]：#加入",
            "pink",
            Some(EntertainmentKind::Undercover),
        )
        .unwrap();
        assert_eq!(
            join.command,
            UserCommand::Undercover(UndercoverCommand::Join)
        );

        for text in ["[用户]：#c", "[用户]：＃投 C"] {
            let vote =
                parse_entertainment_shortcut(text, "pink", Some(EntertainmentKind::Undercover))
                    .unwrap();
            assert_eq!(
                vote.command,
                UserCommand::Undercover(UndercoverCommand::Vote('C'))
            );
            assert_eq!(vote.raw, "投票");
        }
    }

    #[test]
    fn landlord_command_locks_are_scoped_to_the_player() {
        let left =
            parse_entertainment_shortcut("甲：#加入", "blue", Some(EntertainmentKind::Landlord))
                .unwrap();
        let right =
            parse_entertainment_shortcut("乙：#加入", "blue", Some(EntertainmentKind::Landlord))
                .unwrap();

        assert!(!same_lock_command(&left, &right));
        assert_ne!(lock_key(&left), lock_key(&right));
    }

    #[test]
    fn turtle_soup_hash_controls_do_not_consume_questions() {
        let status = parse_entertainment_shortcut(
            "用户：#状态",
            "blue",
            Some(EntertainmentKind::TurtleSoup),
        )
        .expect("parse turtle soup status");
        assert_eq!(
            status.command,
            UserCommand::TurtleSoup(TurtleSoupCommand::Status)
        );
        let stop = parse_entertainment_shortcut(
            "用户：#结束",
            "blue",
            Some(EntertainmentKind::TurtleSoup),
        )
        .expect("parse turtle soup stop");
        assert_eq!(
            stop.command,
            UserCommand::TurtleSoup(TurtleSoupCommand::End)
        );
        assert!(
            parse_entertainment_shortcut(
                "用户：#他认识死者吗？",
                "blue",
                Some(EntertainmentKind::TurtleSoup)
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_legacy_entertainment_syntax_and_hash_in_the_middle() {
        for text in [
            "用户：@接龙 画蛇添足",
            "用户：!画蛇添足",
            "用户：！画蛇添足",
            "用户：@斗地主",
            "用户：@加入",
            "用户：@出 345",
            "用户：$345",
            "用户：＄345",
            "用户：@手牌",
            "用户：@海龟汤",
            "用户：@卧底开始",
            "用户：@投A",
        ] {
            assert!(parse_text(text, "blue").is_none(), "{text}");
            assert!(
                parse_entertainment_shortcut(text, "blue", Some(EntertainmentKind::Landlord))
                    .is_none(),
                "{text}"
            );
        }
        assert!(
            parse_entertainment_shortcut(
                "用户：普通聊天 #加入",
                "blue",
                Some(EntertainmentKind::Landlord)
            )
            .is_none()
        );
    }

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
    fn parses_invite_command_without_password() {
        let parsed = parse_text("[Alice]：@邀请2", "pink").expect("parse invite");
        assert_eq!(
            parsed.command,
            UserCommand::Invite(InviteCommand {
                username: "Alice".to_string(),
                seq: Some(2),
                password: None,
            })
        );
    }

    #[test]
    fn parses_invite_command_with_password_as_only_argument() {
        let parsed = parse_text("[Alice]：@邀请123456", "pink").expect("parse invite password");
        assert_eq!(
            parsed.command,
            UserCommand::Invite(InviteCommand {
                username: "Alice".to_string(),
                seq: None,
                password: Some("123456".to_string()),
            })
        );
    }

    #[test]
    fn rejects_invite_command_with_invalid_password() {
        assert!(parse_text("[Alice]：@邀请2 12345", "pink").is_none());
        assert!(parse_text("[Alice]：@邀请2 123456", "pink").is_none());
        assert!(parse_text("[Alice]：@邀请2 1234567", "pink").is_none());
        assert!(parse_text("[Alice]：@邀请2 abcdef", "pink").is_none());
        assert!(parse_text("[Alice]：@邀请1000", "pink").is_none());
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
    fn parses_pink_blacklist_uid_command() {
        let parsed = parse_text("[Alice]：@拉黑UID123456789", "pink").expect("parse blacklist uid");
        assert_eq!(
            parsed.command,
            UserCommand::Moderation(ModerationCommand {
                action: ModerationAction::Blacklist,
                uid: "123456789".to_string(),
                requester: "Alice".to_string(),
            })
        );
    }

    #[test]
    fn parses_pink_blacklist_uid_command_case_insensitive() {
        let parsed = parse_text("[Alice]：@拉黑uid123456789", "pink")
            .expect("parse blacklist uid case insensitive");
        assert_eq!(
            parsed.command,
            UserCommand::Moderation(ModerationCommand {
                action: ModerationAction::Blacklist,
                uid: "123456789".to_string(),
                requester: "Alice".to_string(),
            })
        );
    }

    #[test]
    fn parses_pink_block_chat_uid_command() {
        let parsed = parse_text("[Alice]：@屏蔽UID123456789", "pink").expect("parse block uid");
        assert_eq!(
            parsed.command,
            UserCommand::Moderation(ModerationCommand {
                action: ModerationAction::BlockChat,
                uid: "123456789".to_string(),
                requester: "Alice".to_string(),
            })
        );
    }

    #[test]
    fn rejects_invalid_uid_length() {
        assert!(parse_text("[Alice]：@拉黑UID12345678", "pink").is_none());
        assert!(parse_text("[Alice]：@屏蔽UID1234567890", "pink").is_none());
        assert!(parse_text("[Alice]：@拉黑12345678", "pink").is_none());
        assert!(parse_text("[Alice]：@屏蔽1234567890", "pink").is_none());
    }

    #[test]
    fn parses_pink_blacklist_uid_alias() {
        let parsed = parse_text("[Alice]：@拉黑123456789", "pink").expect("parse blacklist alias");
        assert_eq!(
            parsed.command,
            UserCommand::Moderation(ModerationCommand {
                action: ModerationAction::Blacklist,
                uid: "123456789".to_string(),
                requester: "Alice".to_string(),
            })
        );
    }

    #[test]
    fn parses_pink_block_chat_uid_alias() {
        let parsed = parse_text("[Alice]：@屏蔽123456789", "pink").expect("parse block alias");
        assert_eq!(
            parsed.command,
            UserCommand::Moderation(ModerationCommand {
                action: ModerationAction::BlockChat,
                uid: "123456789".to_string(),
                requester: "Alice".to_string(),
            })
        );
    }

    #[test]
    fn parses_ai_song_command() {
        let parsed = parse_text("用户：@AI点歌 晴天 周杰伦", "blue").expect("parse ai song");
        assert_eq!(parsed.user_command, "@AI点歌 晴天 周杰伦");
        assert_eq!(
            parsed.command,
            UserCommand::Song(SongCommand {
                keyword: "晴天 周杰伦".to_string(),
                source: SongSource::QqMusic,
                prefix: "AI点歌".to_string(),
                prefer_accompaniment: false,
                ai_assisted: true,
                friend_username: String::new(),
            })
        );
    }

    #[test]
    fn parses_ai_song_command_case_insensitive() {
        let parsed = parse_text("用户：@ai点歌 晴天 周杰伦", "blue")
            .expect("parse ai song case insensitive");
        assert_eq!(parsed.user_command, "@ai点歌 晴天 周杰伦");
        assert_eq!(
            parsed.command,
            UserCommand::Song(SongCommand {
                keyword: "晴天 周杰伦".to_string(),
                source: SongSource::QqMusic,
                prefix: "AI点歌".to_string(),
                prefer_accompaniment: false,
                ai_assisted: true,
                friend_username: String::new(),
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
                friend_username: String::new(),
            })
        );
    }

    #[test]
    fn song_lock_treats_ai_and_plain_hall_song_as_same_request() {
        let plain = parse_text("用户：@点歌 晴天 周杰伦", "blue").expect("parse plain song");
        let ai = parse_text("用户：@AI点歌 晴天 周杰伦", "blue").expect("parse ai song");

        assert!(same_lock_command(&plain, &ai));
    }

    #[test]
    fn command_lock_accepts_only_one_ai_or_plain_hall_song_request() {
        let plain = parse_text("用户：@点歌 晴天 周杰伦", "blue").expect("parse plain song");
        let ai = parse_text("用户：@AI点歌 晴天 周杰伦", "blue").expect("parse ai song");
        let mut locks = CommandLockState::default();

        let update = locks.update(&[plain, ai], false);

        assert_eq!(update.accepted.len(), 1);
    }

    #[test]
    fn command_lock_treats_disable_and_enable_as_global_commands() {
        let alice_disable = parse_text("[Alice]：@禁用", "pink").expect("parse disable");
        let bob_disable = parse_text("[Bob]：@禁用", "pink").expect("parse disable");
        let alice_enable = parse_text("[Alice]：@启用", "pink").expect("parse enable");
        let bob_enable = parse_text("[Bob]：@启用", "pink").expect("parse enable");
        let mut locks = CommandLockState::default();

        assert!(same_lock_command(&alice_disable, &bob_disable));
        assert!(same_lock_command(&alice_enable, &bob_enable));

        let update = locks.update(&[alice_disable, bob_disable], false);

        assert_eq!(update.accepted.len(), 1);
    }

    #[test]
    fn song_lock_keeps_different_sources_separate() {
        let qq = parse_text("用户：@点歌 晴天 周杰伦", "blue").expect("parse qq song");
        let netease =
            parse_text("用户：@网易点歌 晴天 周杰伦", "blue").expect("parse netease song");

        assert!(!same_lock_command(&qq, &netease));
    }

    #[test]
    fn rejects_blue_bilibili_song_command() {
        assert!(parse_text("用户：@B站点歌 晴天 周杰伦", "blue").is_none());
    }

    #[test]
    fn parses_pink_bilibili_song_command() {
        let parsed = parse_text("[Alice]：@B站点歌 晴天 周杰伦", "pink")
            .expect("parse friend bilibili song");
        assert_eq!(
            parsed.command,
            UserCommand::Song(SongCommand {
                keyword: "晴天 周杰伦".to_string(),
                source: SongSource::Bilibili,
                prefix: "B站点歌".to_string(),
                prefer_accompaniment: false,
                ai_assisted: false,
                friend_username: "Alice".to_string(),
            })
        );
        assert_eq!(parsed.username, "Alice");
        assert_eq!(parsed.message_type, "pink");
        assert_eq!(parsed.user_command, "@B站点歌 晴天 周杰伦");
    }

    #[test]
    fn parses_pink_bilibili_song_command_case_insensitive() {
        let parsed = parse_text("[Alice]：@b站点歌 晴天 周杰伦", "pink")
            .expect("parse friend bilibili song case insensitive");
        assert_eq!(
            parsed.command,
            UserCommand::Song(SongCommand {
                keyword: "晴天 周杰伦".to_string(),
                source: SongSource::Bilibili,
                prefix: "B站点歌".to_string(),
                prefer_accompaniment: false,
                ai_assisted: false,
                friend_username: "Alice".to_string(),
            })
        );
    }

    #[test]
    fn parses_pink_chat_listener_mode_commands_only() {
        let primary = parse_text("[Alice]：@监听模式 一级", "pink").expect("primary mode");
        let secondary = parse_text("[Alice]：@监听模式：二级", "pink").expect("secondary mode");
        let status = parse_text("[Alice]：@监听模式 状态", "pink").expect("mode status");

        assert_eq!(
            primary.command,
            UserCommand::ChatListenerMode(ChatListenerModeCommand::Primary)
        );
        assert_eq!(
            secondary.command,
            UserCommand::ChatListenerMode(ChatListenerModeCommand::Secondary)
        );
        assert_eq!(
            status.command,
            UserCommand::ChatListenerMode(ChatListenerModeCommand::Status)
        );
        assert!(parse_text("大厅：@监听模式 二级", "blue").is_none());
    }

    #[test]
    fn parses_pink_ai_song_command_as_all_sources() {
        let parsed =
            parse_text("[Alice]：@AI点歌 晴天 周杰伦", "pink").expect("parse friend ai song");
        assert_eq!(
            parsed.command,
            UserCommand::Song(SongCommand {
                keyword: "晴天 周杰伦".to_string(),
                source: SongSource::All,
                prefix: "AI点歌".to_string(),
                prefer_accompaniment: false,
                ai_assisted: true,
                friend_username: "Alice".to_string(),
            })
        );
    }
}
