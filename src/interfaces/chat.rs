#[cfg(test)]
use crate::features::administration::AdministrationCommand;
#[cfg(test)]
use crate::features::administration::ChatListenerModeCommand;
#[cfg(test)]
use crate::features::card_games::LandlordCommand;
use crate::features::chat_text::command_identity;
use crate::features::command::{CommandEnvelope, CommandObservation, ModuleCommand, RoutedCommand};
#[cfg(test)]
use crate::features::entertainment::EntertainmentKind;
#[cfg(test)]
use crate::features::hall::HallCommand;
#[cfg(test)]
use crate::features::idiom_chain::IdiomChainCommand;
#[cfg(test)]
use crate::features::idiom_chain::IdiomChainMode;
#[cfg(test)]
use crate::features::invite::InviteCommand;
pub use crate::features::moderation::{ModerationAction, ModerationCommand};
#[cfg(test)]
use crate::features::song_request::{SongCommand, SongSource};
#[cfg(test)]
use crate::features::turtle_soup::TurtleSoupCommand;
#[cfg(test)]
use crate::features::undercover::UndercoverCommand;
use std::collections::HashMap;

mod router;

pub(crate) use router::ChatCommandRouter;

#[derive(Clone, Debug)]
pub struct PendingCommand {
    pub lock_key: String,
    pub routed: RoutedCommand,
}

/// Observation context retained when a chat message becomes a queued command.
///
/// This is deliberately runtime-only metadata. It is useful for correlating execution with
/// the frame and message that produced it, but it is not part of the external command protocol.
pub(crate) fn parse_command_envelope(
    text: &str,
    message_type: &str,
    observation: CommandObservation,
) -> Option<CommandEnvelope> {
    if !matches!(message_type, "blue" | "pink") || is_feedback_text(text) {
        return None;
    }
    if message_type == "blue" && text.starts_with("播放") && text.contains(" - ") {
        return None;
    }

    let separator_index = text.find(['：', ':', ']', '】'])?;
    let username = if message_type == "pink" {
        extract_bracket_username(text)?
    } else {
        text[..separator_index]
            .trim_matches(['[', '【', ']', '】', ' ', '\t'])
            .to_string()
    };
    if username.is_empty() {
        return None;
    }
    let separator_len = text[separator_index..].chars().next()?.len_utf8();
    let raw_command_text = text[separator_index + separator_len..]
        .trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    CommandEnvelope::new(text, username, message_type, raw_command_text, observation)
}

/// A command submitted by a local control surface rather than read from chat.
///
/// Control surfaces should describe the business command and its display text; the chat-shaped
/// envelope is created here at the command boundary so HTTP and other adapters do not fabricate
/// `RoutedCommand` values independently.
#[derive(Clone, Debug)]
pub(crate) struct ConsoleCommandIntent {
    matched: String,
    raw: String,
    command: ModuleCommand,
}

impl ConsoleCommandIntent {
    pub(crate) fn new(
        raw: impl Into<String>,
        matched: impl Into<String>,
        command: ModuleCommand,
    ) -> Self {
        Self {
            matched: matched.into(),
            raw: raw.into(),
            command,
        }
    }

    pub(crate) fn into_pending(self) -> PendingCommand {
        let routed = RoutedCommand::console(self.matched, self.raw, self.command);
        PendingCommand {
            lock_key: lock_key(&routed),
            routed,
        }
    }
}

#[derive(Clone, Debug)]
struct CommandLock {
    command: RoutedCommand,
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
        visible_commands: &[RoutedCommand],
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
                routed: parsed.clone(),
            });
        }

        update
    }

    fn find_locked_command(&self, command: &RoutedCommand) -> Option<&RoutedCommand> {
        self.locks
            .values()
            .find(|lock| same_lock_command(&lock.command, command))
            .map(|lock| &lock.command)
    }
}

pub fn lock_key(command: &RoutedCommand) -> String {
    let key = command.command.lock_key();
    if command.command.scopes_lock_to_actor() {
        format!("{}:{}", key, command_identity(&command.username))
    } else {
        key
    }
}

pub fn same_lock_command(left: &RoutedCommand, right: &RoutedCommand) -> bool {
    if left.command.scopes_lock_to_actor()
        && right.command.scopes_lock_to_actor()
        && command_identity(&left.username) != command_identity(&right.username)
    {
        return false;
    }
    left.command.same_request(&right.command)
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
    use std::time::Instant;

    use super::*;
    use crate::features::command::{CommandAuthority, CommandPrefix};
    use crate::observation::chat::ObservedChatMessageId;

    fn parse_text(text: &str, message_type: &str) -> Option<RoutedCommand> {
        let envelope = parse_command_envelope(text, message_type, CommandObservation::default())?;
        (envelope.prefix() == CommandPrefix::At).then_some(())?;
        ChatCommandRouter::without_custom_workflow().route(&envelope, None)
    }

    fn parse_entertainment_shortcut(
        text: &str,
        message_type: &str,
        active: Option<EntertainmentKind>,
    ) -> Option<RoutedCommand> {
        let envelope = parse_command_envelope(text, message_type, CommandObservation::default())?;
        (envelope.prefix() == CommandPrefix::Hash).then_some(())?;
        ChatCommandRouter::without_custom_workflow().route(&envelope, active)
    }

    #[test]
    fn chat_ingress_produces_an_unparsed_command_envelope() {
        let envelope = parse_command_envelope(
            "用户：@点歌 晴天 周杰伦",
            "blue",
            CommandObservation::default(),
        )
        .expect("command envelope");

        assert_eq!(envelope.username(), "用户");
        assert_eq!(envelope.user_command(), "@点歌 晴天 周杰伦");
        assert_eq!(envelope.command_text(), "点歌 晴天 周杰伦");
        assert_eq!(envelope.prefix(), CommandPrefix::At);
        assert_eq!(envelope.authority(), CommandAuthority::HallMember);
    }

    #[test]
    fn structured_secondary_friend_input_routes_without_fabricating_chat_text() {
        let message_id = ObservedChatMessageId::new(
            crate::observation::chat::VisualSessionId::new(9),
            crate::observation::chat::ChatIdentity::Friend(std::sync::Arc::from("芦荟")),
            crate::observation::chat::BubbleSequence::new(4),
        );
        let observation = CommandObservation {
            frame_id: None,
            captured_at: Some(Instant::now()),
            message_id: Some(message_id.clone()),
        };
        let envelope =
            CommandEnvelope::new("@邀请2", "芦荟", "pink", "@邀请2", observation.clone())
                .expect("structured secondary envelope");

        let routed = ChatCommandRouter::without_custom_workflow()
            .route(&envelope, None)
            .expect("route secondary friend command");

        assert_eq!(routed.username, "芦荟");
        assert_eq!(routed.message_type, "pink");
        assert_eq!(routed.observation, observation);
        assert_eq!(
            routed.command,
            ModuleCommand::Invite(InviteCommand {
                username: "芦荟".to_string(),
                seq: Some(2),
                password: None,
            })
        );
    }

    #[test]
    fn parses_hash_idiom_chain_commands_by_active_game() {
        let started = parse_entertainment_shortcut("用户：#接龙 画蛇添足", "blue", None)
            .expect("parse idiom chain start");
        assert_eq!(
            started.command,
            ModuleCommand::IdiomChain(IdiomChainCommand::Start {
                idiom: "画蛇添足".to_string(),
                mode: IdiomChainMode::Exact,
            })
        );

        let homophone = parse_entertainment_shortcut("用户：＃同音接龙 画蛇添足", "blue", None)
            .expect("parse homophone idiom chain start");
        assert_eq!(
            homophone.command,
            ModuleCommand::IdiomChain(IdiomChainCommand::Start {
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
            ModuleCommand::IdiomChain(IdiomChainCommand::Submit("足智多谋".to_string()))
        );

        let explained = parse_entertainment_shortcut(
            "用户：#解释 画蛇添足",
            "blue",
            Some(EntertainmentKind::IdiomChain),
        )
        .expect("parse idiom explanation command");
        assert_eq!(
            explained.command,
            ModuleCommand::IdiomChain(IdiomChainCommand::Explain(Some("画蛇添足".to_string())))
        );
        let hint = parse_entertainment_shortcut(
            "用户：#提示",
            "blue",
            Some(EntertainmentKind::IdiomChain),
        )
        .expect("parse idiom hint command");
        assert_eq!(
            hint.command,
            ModuleCommand::IdiomChain(IdiomChainCommand::Hint)
        );
    }

    #[test]
    fn explicit_hash_starts_are_available_without_an_active_game() {
        for (text, expected) in [
            (
                "用户：#斗地主",
                ModuleCommand::CardGame(LandlordCommand::Start),
            ),
            (
                "用户：#跑得快",
                ModuleCommand::CardGame(LandlordCommand::RunFastStart),
            ),
            (
                "用户：#海龟汤",
                ModuleCommand::TurtleSoup(TurtleSoupCommand::Start),
            ),
            (
                "用户：#卧底",
                ModuleCommand::Undercover(UndercoverCommand::CreateSingle),
            ),
            (
                "用户：# 卧 底 双",
                ModuleCommand::Undercover(UndercoverCommand::CreateDouble),
            ),
            (
                "用户：#娱乐",
                ModuleCommand::Administration(AdministrationCommand::EntertainmentHelp),
            ),
            (
                "用户：＃帮助",
                ModuleCommand::Administration(AdministrationCommand::EntertainmentHelp),
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
            ModuleCommand::Administration(AdministrationCommand::EntertainmentHelp)
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
            assert_eq!(parsed.command, ModuleCommand::CardGame(expected), "{text}");
        }

        let hand =
            parse_entertainment_shortcut("[用户]：#手牌", "pink", Some(EntertainmentKind::RunFast))
                .expect("parse private hand command");
        assert_eq!(hand.command, ModuleCommand::CardGame(LandlordCommand::Hand));
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
            assert_eq!(parsed.command, ModuleCommand::Undercover(expected));
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
            ModuleCommand::Undercover(UndercoverCommand::Join)
        );

        let reveal = parse_entertainment_shortcut(
            "[用户]：#谜底",
            "pink",
            Some(EntertainmentKind::Undercover),
        )
        .expect("parse private reveal command");
        assert_eq!(
            reveal.command,
            ModuleCommand::Undercover(UndercoverCommand::Reveal)
        );
        assert_eq!(reveal.raw, "谜底");

        let public_reveal = parse_entertainment_shortcut(
            "用户：#谜底",
            "blue",
            Some(EntertainmentKind::Undercover),
        )
        .expect("public answer word remains a description payload");
        assert_eq!(
            public_reveal.command,
            ModuleCommand::Undercover(UndercoverCommand::Describe("谜底".to_string()))
        );

        for text in ["[用户]：#c", "[用户]：＃投 C"] {
            let vote =
                parse_entertainment_shortcut(text, "pink", Some(EntertainmentKind::Undercover))
                    .unwrap();
            assert_eq!(
                vote.command,
                ModuleCommand::Undercover(UndercoverCommand::Vote('C'))
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
            ModuleCommand::TurtleSoup(TurtleSoupCommand::Status)
        );
        let stop = parse_entertainment_shortcut(
            "用户：#结束",
            "blue",
            Some(EntertainmentKind::TurtleSoup),
        )
        .expect("parse turtle soup stop");
        assert_eq!(
            stop.command,
            ModuleCommand::TurtleSoup(TurtleSoupCommand::End)
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
            ModuleCommand::Hall(HallCommand::ToggleMicrophone {
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
            ModuleCommand::Administration(AdministrationCommand::SetCommandsEnabled {
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
            ModuleCommand::Administration(AdministrationCommand::SetCommandsEnabled {
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
            ModuleCommand::Invite(InviteCommand {
                username: "Alice".to_string(),
                seq: Some(2),
                password: None,
            })
        );
    }

    #[test]
    fn parses_invite_command_with_password_as_only_argument() {
        let parsed = parse_text("[Alice]：@邀请654321", "pink").expect("parse invite password");
        assert_eq!(
            parsed.command,
            ModuleCommand::Invite(InviteCommand {
                username: "Alice".to_string(),
                seq: None,
                password: Some("654321".to_string()),
            })
        );
    }

    #[test]
    fn rejects_invite_command_with_invalid_password() {
        assert!(parse_text("[Alice]：@邀请2 12345", "pink").is_none());
        assert!(parse_text("[Alice]：@邀请2 654321", "pink").is_none());
        assert!(parse_text("[Alice]：@邀请2 1234567", "pink").is_none());
        assert!(parse_text("[Alice]：@邀请2 abcdef", "pink").is_none());
        assert!(parse_text("[Alice]：@邀请1000", "pink").is_none());
    }

    #[test]
    fn parses_idle_exit_default() {
        let parsed = parse_text("[Alice]：@闲置退出", "pink").expect("parse idle exit");
        assert_eq!(
            parsed.command,
            ModuleCommand::Administration(AdministrationCommand::IdleExit { minutes: 30 })
        );
    }

    #[test]
    fn parses_idle_exit_with_minimum() {
        let parsed = parse_text("[Alice]：@闲置退出 5", "pink").expect("parse idle exit");
        assert_eq!(
            parsed.command,
            ModuleCommand::Administration(AdministrationCommand::IdleExit { minutes: 15 })
        );
    }

    #[test]
    fn parses_idle_exit_with_minutes_suffix() {
        let parsed = parse_text("[Alice]：@闲置退出 20分钟", "pink").expect("parse idle exit");
        assert_eq!(
            parsed.command,
            ModuleCommand::Administration(AdministrationCommand::IdleExit { minutes: 20 })
        );
    }

    #[test]
    fn parses_pink_blacklist_uid_command() {
        let parsed = parse_text("[Alice]：@拉黑UID123456789", "pink").expect("parse blacklist uid");
        assert_eq!(
            parsed.command,
            ModuleCommand::Moderation(ModerationCommand {
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
            ModuleCommand::Moderation(ModerationCommand {
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
            ModuleCommand::Moderation(ModerationCommand {
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
            ModuleCommand::Moderation(ModerationCommand {
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
            ModuleCommand::Moderation(ModerationCommand {
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
            ModuleCommand::SongRequest(SongCommand {
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
            ModuleCommand::SongRequest(SongCommand {
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
            ModuleCommand::SongRequest(SongCommand {
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
            routed: RoutedCommand {
                observation: observation.clone(),
                ..parsed
            },
        };

        assert_eq!(pending.routed.observation.frame_id, observation.frame_id);
        assert_eq!(
            pending.routed.observation.captured_at,
            observation.captured_at
        );
        assert_eq!(
            pending.routed.observation.message_id,
            observation.message_id
        );
    }

    #[test]
    fn song_lock_keeps_different_sources_separate() {
        let qq = parse_text("用户：@点歌 晴天 周杰伦", "blue").expect("parse qq song");
        let netease =
            parse_text("用户：@网易点歌 晴天 周杰伦", "blue").expect("parse netease song");

        assert!(!same_lock_command(&qq, &netease));
    }

    #[test]
    fn parses_netease_cloud_song_alias_in_hall_and_friend_chat() {
        let hall = parse_text("用户：@网易云点歌 晴天 周杰伦", "blue")
            .expect("parse hall netease cloud alias");
        let friend = parse_text("[Alice]：@网易云点歌 晴天 周杰伦", "pink")
            .expect("parse friend netease cloud alias");

        for parsed in [hall, friend] {
            assert_eq!(
                parsed.command,
                ModuleCommand::SongRequest(SongCommand {
                    keyword: "晴天 周杰伦".to_string(),
                    source: SongSource::Netease,
                    prefix: "网易云点歌".to_string(),
                    prefer_accompaniment: false,
                    ai_assisted: false,
                    friend_username: if parsed.message_type == "pink" {
                        "Alice".to_string()
                    } else {
                        String::new()
                    },
                })
            );
        }
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
            ModuleCommand::SongRequest(SongCommand {
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
            ModuleCommand::SongRequest(SongCommand {
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
            ModuleCommand::Administration(AdministrationCommand::ChatListenerMode(
                ChatListenerModeCommand::Primary,
            ))
        );
        assert_eq!(
            secondary.command,
            ModuleCommand::Administration(AdministrationCommand::ChatListenerMode(
                ChatListenerModeCommand::Secondary,
            ))
        );
        assert_eq!(
            status.command,
            ModuleCommand::Administration(AdministrationCommand::ChatListenerMode(
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
            ModuleCommand::SongRequest(SongCommand {
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
