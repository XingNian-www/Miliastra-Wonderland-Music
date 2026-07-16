use serde::{Deserialize, Serialize};

use crate::features::chat_text::{
    CommandSyntax, is_command_boundary, parse_prefixed_command, strip_ascii_case_prefix,
};
use crate::runtime::chat_listener::ChatListenerModeCommand;

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

impl AdministrationCommand {
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
