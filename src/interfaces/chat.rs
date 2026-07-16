use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::features::administration::AdministrationCommand;
use crate::features::card_games::LandlordCommand;
use crate::features::chat_text::{CommandSyntax, command_identity};
use crate::features::custom_workflow::CustomWorkflowMatch;
use crate::features::entertainment::EntertainmentKind;
use crate::features::hall::HallCommand;
use crate::features::idiom_chain::IdiomChainCommand;
#[cfg(test)]
use crate::features::idiom_chain::IdiomChainMode;
use crate::features::invite::InviteCommand;
use crate::features::moderation;
pub use crate::features::moderation::{ModerationAction, ModerationCommand};
use crate::features::playback::PlaybackCommand;
use crate::features::song_request;
#[cfg(test)]
use crate::features::song_request::{SongCommand, SongSource};
use crate::features::turtle_soup::TurtleSoupCommand;
use crate::features::undercover::UndercoverCommand;
use crate::observation::chat::{ObservationFrameId, ObservedChatMessageId};
use crate::runtime::business::BusinessIntent;
#[cfg(test)]
use crate::runtime::chat_listener::ChatListenerModeCommand;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParsedCommand {
    pub matched: String,
    pub raw: String,
    pub user_command: String,
    pub message_type: String,
    pub username: String,
    pub command: BusinessIntent,
}

#[derive(Clone, Debug)]
pub struct PendingCommand {
    pub lock_key: String,
    pub parsed: ParsedCommand,
    pub observation: CommandObservation,
}

pub(crate) fn from_custom_workflow_match(matched: CustomWorkflowMatch) -> ParsedCommand {
    ParsedCommand {
        matched: matched.matched,
        raw: matched.raw,
        user_command: matched.user_command,
        message_type: matched.message_type,
        username: matched.username,
        command: BusinessIntent::CustomWorkflow(matched.command),
    }
}

/// Observation context retained when a chat message becomes a queued command.
///
/// This is deliberately runtime-only metadata. It is useful for correlating execution with
/// the frame and message that produced it, but it is not part of the external command protocol.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommandObservation {
    pub(crate) frame_id: Option<ObservationFrameId>,
    pub(crate) captured_at: Option<Instant>,
    pub(crate) message_id: Option<ObservedChatMessageId>,
}

/// A command submitted by a local control surface rather than read from chat.
///
/// Control surfaces should describe the business command and its display text; the chat-shaped
/// envelope is created here at the command boundary so HTTP and other adapters do not fabricate
/// `ParsedCommand` values independently.
#[derive(Clone, Debug)]
pub(crate) struct ConsoleCommandIntent {
    matched: String,
    raw: String,
    command: BusinessIntent,
}

impl ConsoleCommandIntent {
    pub(crate) fn new(
        raw: impl Into<String>,
        matched: impl Into<String>,
        command: BusinessIntent,
    ) -> Self {
        Self {
            matched: matched.into(),
            raw: raw.into(),
            command,
        }
    }

    pub(crate) fn into_pending(self) -> PendingCommand {
        let user_command = format!("@{}", self.raw);
        let parsed = ParsedCommand {
            matched: self.matched,
            raw: self.raw,
            user_command,
            message_type: "控制台".to_string(),
            username: "控制台".to_string(),
            command: self.command,
        };
        PendingCommand {
            lock_key: lock_key(&parsed),
            parsed,
            observation: CommandObservation::default(),
        }
    }
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
    let parsed = parse_hall_command(command_text)?;
    let matched = parsed.matched;
    let after_match = parsed.argument;

    let username = text[..sep_index]
        .trim_matches(['[', '【', ']', '】', ' ', '\t'])
        .to_string();
    let raw = if after_match.is_empty() {
        matched.to_string()
    } else {
        format!("{} {}", matched, after_match)
    };
    Some(ParsedCommand {
        matched: matched.to_string(),
        raw,
        user_command,
        message_type: message_type.to_string(),
        username,
        command: parsed.command,
    })
}

pub(crate) fn parse_entertainment_shortcut(
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
            Some(EntertainmentKind::IdiomChain) => Some(BusinessIntent::IdiomChain(
                IdiomChainCommand::parse_active(payload),
            )),
            Some(EntertainmentKind::Landlord | EntertainmentKind::RunFast) => {
                LandlordCommand::parse_hall(payload).map(BusinessIntent::CardGame)
            }
            Some(EntertainmentKind::TurtleSoup) => {
                TurtleSoupCommand::parse_hall(payload).map(BusinessIntent::TurtleSoup)
            }
            Some(EntertainmentKind::Undercover) => {
                UndercoverCommand::parse_hall(payload).map(BusinessIntent::Undercover)
            }
            None => None,
        })?
    } else {
        match active {
            Some(EntertainmentKind::Landlord | EntertainmentKind::RunFast) => {
                BusinessIntent::CardGame(LandlordCommand::parse_friend(payload)?)
            }
            Some(EntertainmentKind::Undercover) => {
                BusinessIntent::Undercover(UndercoverCommand::parse_friend(payload)?)
            }
            _ => return None,
        }
    };
    let raw = match &command {
        BusinessIntent::Undercover(UndercoverCommand::Vote(_)) => "投票".to_string(),
        BusinessIntent::Undercover(UndercoverCommand::Describe(_)) => "卧底描述".to_string(),
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

fn parse_entertainment_start(payload: &str) -> Option<BusinessIntent> {
    IdiomChainCommand::parse_start(payload)
        .map(BusinessIntent::IdiomChain)
        .or_else(|| LandlordCommand::parse_start(payload).map(BusinessIntent::CardGame))
        .or_else(|| TurtleSoupCommand::parse_start(payload).map(BusinessIntent::TurtleSoup))
        .or_else(|| UndercoverCommand::parse_start(payload).map(BusinessIntent::Undercover))
        .or_else(|| {
            AdministrationCommand::parse_entertainment_start(payload)
                .map(BusinessIntent::Administration)
        })
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
    if let Some(parsed) = AdministrationCommand::parse_friend(command_text, &username) {
        return Some(ParsedCommand {
            matched: parsed.matched.to_string(),
            raw: parsed.raw,
            user_command,
            message_type: "pink".to_string(),
            username,
            command: BusinessIntent::Administration(parsed.command),
        });
    }
    if let Some(song) = song_request::parse_friend_command(command_text, &username) {
        return Some(ParsedCommand {
            matched: song.prefix.clone(),
            raw: format!("{} {} {}", username, song.prefix, song.keyword),
            user_command,
            message_type: "pink".to_string(),
            username,
            command: BusinessIntent::SongRequest(song),
        });
    }
    if let Some(parsed) = InviteCommand::parse_friend(command_text, &username) {
        return Some(ParsedCommand {
            matched: "邀请".to_string(),
            raw: format!("邀请 {} {}", username, parsed.raw_parameter),
            user_command,
            message_type: "pink".to_string(),
            username,
            command: BusinessIntent::Invite(parsed.command),
        });
    }
    if let Some(command) = parse_moderation_command(command_text, &username, &user_command) {
        return Some(command);
    }
    if let Some(command) = HallCommand::parse_friend(command_text, &username) {
        return Some(ParsedCommand {
            matched: "麦克风".to_string(),
            raw: format!("麦克风 {}", username),
            user_command,
            message_type: "pink".to_string(),
            username,
            command: BusinessIntent::Hall(command),
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
                observation: CommandObservation::default(),
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
    let key = command.command.lock_key();
    if command.command.scopes_lock_to_actor() {
        format!("{}:{}", key, command_identity(&command.username))
    } else {
        key
    }
}

pub fn same_lock_command(left: &ParsedCommand, right: &ParsedCommand) -> bool {
    if left.command.scopes_lock_to_actor()
        && right.command.scopes_lock_to_actor()
        && command_identity(&left.username) != command_identity(&right.username)
    {
        return false;
    }
    left.command.same_request(&right.command)
}

fn parse_moderation_command(
    command_text: &str,
    username: &str,
    user_command: &str,
) -> Option<ParsedCommand> {
    let parsed = moderation::parse_command(command_text, username)?;
    Some(ParsedCommand {
        matched: parsed.matched.to_string(),
        raw: format!("{} {} {}", parsed.matched, username, parsed.command.uid),
        user_command: user_command.to_string(),
        message_type: "pink".to_string(),
        username: username.to_string(),
        command: BusinessIntent::Moderation(parsed.command),
    })
}

pub(crate) fn strip_ascii_case_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

fn parse_hall_command(text: &str) -> Option<CommandSyntax<'_, BusinessIntent>> {
    if let Some(parsed) = song_request::parse_hall_syntax(text) {
        return Some(CommandSyntax {
            matched: parsed.matched,
            argument: parsed.argument,
            command: BusinessIntent::SongRequest(parsed.command),
        });
    }
    if let Some(parsed) = PlaybackCommand::parse_hall(text) {
        return Some(CommandSyntax {
            matched: parsed.matched,
            argument: parsed.argument,
            command: BusinessIntent::Playback(parsed.command),
        });
    }
    if let Some(parsed) = HallCommand::parse_hall(text) {
        return Some(CommandSyntax {
            matched: parsed.matched,
            argument: parsed.argument,
            command: BusinessIntent::Hall(parsed.command),
        });
    }
    AdministrationCommand::parse_hall(text).map(|parsed| CommandSyntax {
        matched: parsed.matched,
        argument: parsed.argument,
        command: BusinessIntent::Administration(parsed.command),
    })
}

fn is_feedback_text(text: &str) -> bool {
    FEEDBACK_TEXT_PATTERNS
        .iter()
        .any(|pattern| text.contains(pattern))
}

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
            BusinessIntent::IdiomChain(IdiomChainCommand::Start {
                idiom: "画蛇添足".to_string(),
                mode: IdiomChainMode::Exact,
            })
        );

        let homophone = parse_entertainment_shortcut("用户：＃同音接龙 画蛇添足", "blue", None)
            .expect("parse homophone idiom chain start");
        assert_eq!(
            homophone.command,
            BusinessIntent::IdiomChain(IdiomChainCommand::Start {
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
            BusinessIntent::IdiomChain(IdiomChainCommand::Submit("足智多谋".to_string()))
        );

        let explained = parse_entertainment_shortcut(
            "用户：#解释 画蛇添足",
            "blue",
            Some(EntertainmentKind::IdiomChain),
        )
        .expect("parse idiom explanation command");
        assert_eq!(
            explained.command,
            BusinessIntent::IdiomChain(IdiomChainCommand::Explain(Some("画蛇添足".to_string())))
        );
        let hint = parse_entertainment_shortcut(
            "用户：#提示",
            "blue",
            Some(EntertainmentKind::IdiomChain),
        )
        .expect("parse idiom hint command");
        assert_eq!(
            hint.command,
            BusinessIntent::IdiomChain(IdiomChainCommand::Hint)
        );
    }

    #[test]
    fn explicit_hash_starts_are_available_without_an_active_game() {
        for (text, expected) in [
            (
                "用户：#斗地主",
                BusinessIntent::CardGame(LandlordCommand::Start),
            ),
            (
                "用户：#跑得快",
                BusinessIntent::CardGame(LandlordCommand::RunFastStart),
            ),
            (
                "用户：#海龟汤",
                BusinessIntent::TurtleSoup(TurtleSoupCommand::Start),
            ),
            (
                "用户：#卧底",
                BusinessIntent::Undercover(UndercoverCommand::CreateSingle),
            ),
            (
                "用户：# 卧 底 双",
                BusinessIntent::Undercover(UndercoverCommand::CreateDouble),
            ),
            (
                "用户：#娱乐",
                BusinessIntent::Administration(AdministrationCommand::EntertainmentHelp),
            ),
            (
                "用户：＃帮助",
                BusinessIntent::Administration(AdministrationCommand::EntertainmentHelp),
            ),
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
            BusinessIntent::Administration(AdministrationCommand::EntertainmentHelp)
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
            ("用户：#重试", LandlordCommand::Retry),
            ("用户：#结束", LandlordCommand::Exit),
        ] {
            let parsed =
                parse_entertainment_shortcut(text, "blue", Some(EntertainmentKind::Landlord))
                    .expect("parse landlord hall command");
            assert_eq!(parsed.command, BusinessIntent::CardGame(expected), "{text}");
        }

        let hand =
            parse_entertainment_shortcut("[用户]：#手牌", "pink", Some(EntertainmentKind::RunFast))
                .expect("parse private hand command");
        assert_eq!(
            hand.command,
            BusinessIntent::CardGame(LandlordCommand::Hand)
        );
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
            assert_eq!(parsed.command, BusinessIntent::Undercover(expected));
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
            BusinessIntent::Undercover(UndercoverCommand::Join)
        );

        for text in ["[用户]：#c", "[用户]：＃投 C"] {
            let vote =
                parse_entertainment_shortcut(text, "pink", Some(EntertainmentKind::Undercover))
                    .unwrap();
            assert_eq!(
                vote.command,
                BusinessIntent::Undercover(UndercoverCommand::Vote('C'))
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
            BusinessIntent::TurtleSoup(TurtleSoupCommand::Status)
        );
        let stop = parse_entertainment_shortcut(
            "用户：#结束",
            "blue",
            Some(EntertainmentKind::TurtleSoup),
        )
        .expect("parse turtle soup stop");
        assert_eq!(
            stop.command,
            BusinessIntent::TurtleSoup(TurtleSoupCommand::End)
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
            BusinessIntent::Hall(HallCommand::ToggleMicrophone {
                username: "Alice".to_string(),
            })
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
            BusinessIntent::Administration(AdministrationCommand::SetCommandsEnabled {
                enabled: false,
                username: "Alice".to_string(),
            })
        );
    }

    #[test]
    fn parses_enable_commands() {
        let parsed = parse_text("[Alice]：@启用", "pink").expect("parse enable");
        assert_eq!(
            parsed.command,
            BusinessIntent::Administration(AdministrationCommand::SetCommandsEnabled {
                enabled: true,
                username: "Alice".to_string(),
            })
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
            BusinessIntent::Invite(InviteCommand {
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
            BusinessIntent::Invite(InviteCommand {
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
        assert_eq!(
            parsed.command,
            BusinessIntent::Administration(AdministrationCommand::IdleExit { minutes: 30 })
        );
    }

    #[test]
    fn parses_idle_exit_with_minimum() {
        let parsed = parse_text("[Alice]：@闲置退出 5", "pink").expect("parse idle exit");
        assert_eq!(
            parsed.command,
            BusinessIntent::Administration(AdministrationCommand::IdleExit { minutes: 15 })
        );
    }

    #[test]
    fn parses_idle_exit_with_minutes_suffix() {
        let parsed = parse_text("[Alice]：@闲置退出 20分钟", "pink").expect("parse idle exit");
        assert_eq!(
            parsed.command,
            BusinessIntent::Administration(AdministrationCommand::IdleExit { minutes: 20 })
        );
    }

    #[test]
    fn parses_pink_blacklist_uid_command() {
        let parsed = parse_text("[Alice]：@拉黑UID123456789", "pink").expect("parse blacklist uid");
        assert_eq!(
            parsed.command,
            BusinessIntent::Moderation(ModerationCommand {
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
            BusinessIntent::Moderation(ModerationCommand {
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
            BusinessIntent::Moderation(ModerationCommand {
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
            BusinessIntent::Moderation(ModerationCommand {
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
            BusinessIntent::Moderation(ModerationCommand {
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
            BusinessIntent::SongRequest(SongCommand {
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
            BusinessIntent::SongRequest(SongCommand {
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
            BusinessIntent::SongRequest(SongCommand {
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
    fn pending_command_keeps_observation_context_with_the_command() {
        let parsed = parse_text("用户：@状态", "blue").expect("parse status");
        let mut ledger = crate::observation::chat::ChatObservationLedger::new();
        let frame = ledger.begin_frame(Instant::now());
        let message_id = ObservedChatMessageId::new(
            crate::observation::chat::VisualSessionId::new(7),
            crate::observation::chat::ChatIdentity::PrimaryHall,
            crate::observation::chat::BubbleSequence::new(3),
        );
        let observation = CommandObservation {
            frame_id: Some(frame.id()),
            captured_at: Some(frame.captured_at()),
            message_id: Some(message_id),
        };
        let pending = PendingCommand {
            lock_key: lock_key(&parsed),
            parsed,
            observation: observation.clone(),
        };

        assert_eq!(pending.observation.frame_id, observation.frame_id);
        assert_eq!(pending.observation.captured_at, observation.captured_at);
        assert_eq!(pending.observation.message_id, observation.message_id);
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
            BusinessIntent::SongRequest(SongCommand {
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
            BusinessIntent::SongRequest(SongCommand {
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
            BusinessIntent::Administration(AdministrationCommand::ChatListenerMode(
                ChatListenerModeCommand::Primary,
            ))
        );
        assert_eq!(
            secondary.command,
            BusinessIntent::Administration(AdministrationCommand::ChatListenerMode(
                ChatListenerModeCommand::Secondary,
            ))
        );
        assert_eq!(
            status.command,
            BusinessIntent::Administration(AdministrationCommand::ChatListenerMode(
                ChatListenerModeCommand::Status,
            ))
        );
        assert!(parse_text("大厅：@监听模式 二级", "blue").is_none());
    }

    #[test]
    fn parses_pink_ai_song_command_as_all_sources() {
        let parsed =
            parse_text("[Alice]：@AI点歌 晴天 周杰伦", "pink").expect("parse friend ai song");
        assert_eq!(
            parsed.command,
            BusinessIntent::SongRequest(SongCommand {
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
