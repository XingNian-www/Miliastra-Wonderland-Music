use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::features::chat_text::{
    CommandSyntax, is_command_boundary, parse_prefixed_command, strip_ascii_case_prefix,
};
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
};
use crate::runtime::chat_listener::{ChatListenerMode, ChatListenerSnapshot};

const IDLE_EXIT_MIN_MINUTES: u32 = 15;

const SONG_REQUEST_HELP: [&str; 3] = [
    "点歌示例: @点歌/@AI点歌 歌名 歌手 伴奏,输入伴奏时优先匹配伴奏",
    "切换网易平台: @网易点歌/@网易云点歌 歌名 歌手 伴奏,默认为QQ平台",
    "可用 @QQ点歌/@网易点歌/@网易云点歌 指定来源,@AI点歌用于智能识别歌名歌手",
];

const ENTERTAINMENT_HELP: [&str; 7] = [
    "成语接龙: #接龙 成语;同音模式用 #同音接龙 成语;进行中用 #成语/#提示/#解释",
    "斗地主: #斗地主,#加入,#抢/#不抢,#牌组/#出牌组,#过;好友私聊 #手牌",
    "跑得快: #跑得快,#加入,#牌组/#出牌组,#过;好友私聊 #手牌",
    "海龟汤: #海龟汤;进行中 #状态/#结束;其他 #内容 作为问题",
    "海龟汤长答案: ##1第一段,##2第二段,最后发送##提交",
    "谁是卧底: #卧底/#卧底双;好友私聊 #加入;公屏 #开局/#状态/#退出",
    "谁是卧底: 描述用公屏 #内容;投票用好友私聊 #A 或 #投A;局外好友私聊 #谜底 查看答案",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AdministrationCommandContext {
    pub(crate) message_type: String,
    pub(crate) username: String,
    pub(crate) user_command: String,
}

pub(crate) trait AdministrationImmediatePort {
    fn set_commands_enabled(&mut self, enabled: bool) -> Result<()>;
    fn configure_idle_exit(&mut self, minutes: u32) -> Result<()>;
    fn record_command_activity(&mut self) -> Result<()>;
    fn log_executed(
        &mut self,
        context: &AdministrationCommandContext,
        final_command: &str,
    ) -> Result<()>;
}

pub(crate) trait AdministrationApplicationPort: AdministrationImmediatePort {
    fn send_hall(&mut self, message: &str) -> Result<()>;
    fn send_hall_batch(&mut self, messages: &[&str], delay_ms: u64) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImmediateAdministrationOutcome {
    ContinueFormal,
    Handled,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AdministrationApplication {
    help_batch_ms: u64,
}

impl AdministrationApplication {
    pub(crate) const fn new(help_batch_ms: u64) -> Self {
        Self { help_batch_ms }
    }

    pub(crate) fn execute<P: AdministrationApplicationPort + ?Sized>(
        &self,
        context: &AdministrationCommandContext,
        command: &AdministrationCommand,
        port: &mut P,
    ) -> Result<()> {
        match command {
            AdministrationCommand::Help => {
                port.log_executed(context, "help")?;
                port.send_hall_batch(&SONG_REQUEST_HELP, self.help_batch_ms)
            }
            AdministrationCommand::EntertainmentHelp => {
                port.log_executed(context, "entertainment help")?;
                port.send_hall_batch(&ENTERTAINMENT_HELP, self.help_batch_ms)
            }
            AdministrationCommand::SetCommandsEnabled { enabled, .. } => {
                log::info!("收到{}命令", if *enabled { "启用" } else { "禁用" });
                port.set_commands_enabled(*enabled)?;
                port.log_executed(
                    context,
                    if *enabled {
                        "enable commands"
                    } else {
                        "disable commands"
                    },
                )?;
                port.send_hall(if *enabled {
                    "管理员已启用大厅命令识别功能"
                } else {
                    "管理员已禁用大厅命令识别功能"
                })
            }
            AdministrationCommand::IdleExit { minutes } => {
                port.configure_idle_exit((*minutes).max(IDLE_EXIT_MIN_MINUTES))?;
                port.log_executed(context, &format!("idle exit {}", minutes))
            }
            AdministrationCommand::ChatListenerMode(command) => {
                port.log_executed(context, &format!("chat listener {}", command.label()))?;
                log::warn!(
                    "监听模式命令未经过专用队列分发，已只记录: {}",
                    command.label()
                );
                Ok(())
            }
        }
    }

    pub(crate) fn apply_immediate<P: AdministrationImmediatePort + ?Sized>(
        &self,
        context: &AdministrationCommandContext,
        command: &AdministrationCommand,
        propagate_log_error: bool,
        port: &mut P,
    ) -> Result<ImmediateAdministrationOutcome> {
        match command.dispatch() {
            AdministrationDispatch::ApplyCommandAvailabilityThenFormal { enabled } => {
                port.set_commands_enabled(enabled)?;
                Ok(ImmediateAdministrationOutcome::ContinueFormal)
            }
            AdministrationDispatch::ConfigureIdleExit { minutes } => {
                port.record_command_activity()?;
                port.configure_idle_exit(minutes.max(IDLE_EXIT_MIN_MINUTES))?;
                let log_result = port.log_executed(context, &format!("idle exit {minutes}"));
                if propagate_log_error {
                    log_result?;
                } else if let Err(error) = log_result {
                    log::error!("写入执行命令日志失败: {error:#}");
                }
                Ok(ImmediateAdministrationOutcome::Handled)
            }
            AdministrationDispatch::FormalTask | AdministrationDispatch::ChatListenerMode(_) => {
                Ok(ImmediateAdministrationOutcome::ContinueFormal)
            }
        }
    }
}

pub(crate) enum AdministrationMutationIntent {
    RequestChatListenerMode(ChatListenerMode),
    CancelChatListenerModeRequest(ChatListenerMode),
}

pub(crate) enum AdministrationMutationOutcome {
    ChatListenerModeRequested {
        queued: bool,
        snapshot: ChatListenerSnapshot,
    },
    ChatListenerModeRequestCancelled,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum ChatListenerModeCommand {
    Primary,
    Secondary,
    Status,
}

impl ChatListenerModeCommand {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Primary => "一级",
            Self::Secondary => "二级",
            Self::Status => "状态",
        }
    }

    fn parse(text: &str) -> Option<CommandSyntax<'_, Self>> {
        let argument = parse_prefixed_command(text, "监听模式", true)?;
        let command = match argument {
            "一级" => Self::Primary,
            "二级" => Self::Secondary,
            "状态" => Self::Status,
            _ => return None,
        };
        Some(CommandSyntax {
            matched: "监听模式",
            argument,
            command,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum AdministrationCommand {
    Help,
    EntertainmentHelp,
    SetCommandsEnabled { enabled: bool, username: String },
    IdleExit { minutes: u32 },
    ChatListenerMode(ChatListenerModeCommand),
}

pub(crate) struct FriendAdministrationMatch {
    pub(crate) matched: &'static str,
    pub(crate) raw: String,
    pub(crate) command: AdministrationCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdministrationDispatch {
    FormalTask,
    ApplyCommandAvailabilityThenFormal { enabled: bool },
    ConfigureIdleExit { minutes: u32 },
    ChatListenerMode(ChatListenerModeCommand),
}

impl AdministrationCommand {
    pub(crate) fn claims_chat(envelope: &CommandEnvelope) -> bool {
        match (envelope.prefix(), envelope.authority()) {
            (CommandPrefix::At, CommandAuthority::HallMember) => ["娱乐帮助", "帮助"]
                .iter()
                .any(|prefix| envelope.command_text().starts_with(prefix)),
            (CommandPrefix::At, CommandAuthority::Friend) => {
                ["监听模式", "禁用", "启用", "闲置退出"]
                    .iter()
                    .any(|prefix| {
                        strip_ascii_case_prefix(envelope.command_text(), prefix).is_some()
                    })
            }
            (CommandPrefix::Hash, CommandAuthority::HallMember) => {
                matches!(
                    crate::features::chat_text::compact_command(envelope.command_text()).as_str(),
                    "娱乐" | "帮助"
                )
            }
            _ => false,
        }
    }

    pub(crate) fn parse_chat(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<Self>> {
        if !Self::claims_chat(envelope) {
            return None;
        }
        match (envelope.prefix(), envelope.authority()) {
            (CommandPrefix::At, CommandAuthority::HallMember) => {
                let parsed = Self::parse_hall(envelope.command_text())?;
                let raw = if parsed.argument.is_empty() {
                    parsed.matched.to_string()
                } else {
                    format!("{} {}", parsed.matched, parsed.argument)
                };
                Some(FeatureCommandMatch::new(
                    parsed.matched,
                    raw,
                    parsed.command,
                ))
            }
            (CommandPrefix::At, CommandAuthority::Friend) => {
                let parsed = Self::parse_friend(envelope.command_text(), envelope.username())?;
                Some(FeatureCommandMatch::new(
                    parsed.matched,
                    parsed.raw,
                    parsed.command,
                ))
            }
            (CommandPrefix::Hash, CommandAuthority::HallMember) => {
                Self::parse_entertainment_start(envelope.command_text())
                    .map(|command| FeatureCommandMatch::new("#", envelope.command_text(), command))
            }
            _ => None,
        }
    }

    pub(crate) const fn dispatch(&self) -> AdministrationDispatch {
        match self {
            Self::Help | Self::EntertainmentHelp => AdministrationDispatch::FormalTask,
            Self::SetCommandsEnabled { enabled, .. } => {
                AdministrationDispatch::ApplyCommandAvailabilityThenFormal { enabled: *enabled }
            }
            Self::IdleExit { minutes } => {
                AdministrationDispatch::ConfigureIdleExit { minutes: *minutes }
            }
            Self::ChatListenerMode(command) => AdministrationDispatch::ChatListenerMode(*command),
        }
    }

    pub(crate) fn parse_hall(text: &str) -> Option<CommandSyntax<'_, Self>> {
        for prefix in ["娱乐帮助", "帮助"] {
            let Some(argument) = parse_prefixed_command(text, prefix, false) else {
                continue;
            };
            let command = match prefix {
                "娱乐帮助" => Self::EntertainmentHelp,
                "帮助" => Self::Help,
                _ => unreachable!("all administration prefixes are handled"),
            };
            return Some(CommandSyntax {
                matched: prefix,
                argument,
                command,
            });
        }
        None
    }

    pub(crate) fn parse_entertainment_start(payload: &str) -> Option<Self> {
        matches!(
            crate::features::chat_text::compact_command(payload).as_str(),
            "娱乐" | "帮助"
        )
        .then_some(Self::EntertainmentHelp)
    }

    pub(crate) fn parse_friend(text: &str, username: &str) -> Option<FriendAdministrationMatch> {
        if let Some(parsed) = ChatListenerModeCommand::parse(text) {
            return Some(FriendAdministrationMatch {
                matched: parsed.matched,
                raw: format!("监听模式 {}", parsed.command.label()),
                command: Self::ChatListenerMode(parsed.command),
            });
        }
        for (prefix, enabled) in [("禁用", false), ("启用", true)] {
            let Some(rest) = strip_ascii_case_prefix(text, prefix) else {
                continue;
            };
            if !rest.is_empty() && !rest.starts_with([']', '】']) {
                return None;
            }
            return Some(FriendAdministrationMatch {
                matched: prefix,
                raw: format!("{} {}", prefix, username),
                command: Self::SetCommandsEnabled {
                    enabled,
                    username: username.to_string(),
                },
            });
        }
        let rest = strip_ascii_case_prefix(text, "闲置退出")?;
        let rest = rest.trim_start_matches(['：', ':', ' ', '\t']);
        if rest.is_empty() || rest.starts_with([']', '】']) {
            return Some(FriendAdministrationMatch {
                matched: "闲置退出",
                raw: "闲置退出 30".to_string(),
                command: Self::IdleExit { minutes: 30 },
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
        if !is_command_boundary(suffix.chars().next()) {
            return None;
        }
        let minutes = digits.parse::<u32>().ok()?.max(15);
        Some(FriendAdministrationMatch {
            matched: "闲置退出",
            raw: format!("闲置退出 {}", minutes),
            command: Self::IdleExit { minutes },
        })
    }

    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Help => "help".to_string(),
            Self::EntertainmentHelp => "entertainment_help".to_string(),
            Self::SetCommandsEnabled { enabled, .. } => {
                if *enabled {
                    "enable_commands".to_string()
                } else {
                    "disable_commands".to_string()
                }
            }
            Self::IdleExit { minutes } => format!("idle_exit:{minutes}"),
            Self::ChatListenerMode(mode) => format!("chat_listener:{}", mode.label()),
        }
    }

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::IdleExit { minutes: left }, Self::IdleExit { minutes: right }) => left == right,
            (Self::ChatListenerMode(left), Self::ChatListenerMode(right)) => left == right,
            _ => self.lock_key() == other.lock_key(),
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;

    #[derive(Default)]
    struct RecordingPort {
        batches: Vec<Vec<String>>,
    }

    impl AdministrationImmediatePort for RecordingPort {
        fn set_commands_enabled(&mut self, _enabled: bool) -> Result<()> {
            Ok(())
        }

        fn configure_idle_exit(&mut self, _minutes: u32) -> Result<()> {
            Ok(())
        }

        fn record_command_activity(&mut self) -> Result<()> {
            Ok(())
        }

        fn log_executed(
            &mut self,
            _context: &AdministrationCommandContext,
            _final_command: &str,
        ) -> Result<()> {
            Ok(())
        }
    }

    impl AdministrationApplicationPort for RecordingPort {
        fn send_hall(&mut self, _message: &str) -> Result<()> {
            Ok(())
        }

        fn send_hall_batch(&mut self, messages: &[&str], _delay_ms: u64) -> Result<()> {
            self.batches.push(
                messages
                    .iter()
                    .map(|message| (*message).to_string())
                    .collect(),
            );
            Ok(())
        }
    }

    #[test]
    fn ordinary_help_contains_only_song_request_guidance() {
        let mut port = RecordingPort::default();
        let context = AdministrationCommandContext {
            message_type: "blue".to_string(),
            username: "测试".to_string(),
            user_command: "@帮助".to_string(),
        };
        let application = AdministrationApplication::new(150);

        application
            .execute(&context, &AdministrationCommand::Help, &mut port)
            .expect("help command");

        assert_eq!(port.batches.len(), 1);
        assert_eq!(port.batches[0].len(), 3);
        assert!(port.batches[0].iter().all(|line| line.contains("点歌")));
        assert!(port.batches[0].iter().all(|line| !line.contains("斗地主")));
    }
}
