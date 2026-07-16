use serde::{Deserialize, Serialize};

use crate::features::chat_text::{
    CommandSyntax, command_identity, parse_prefixed_command, strip_ascii_case_prefix,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum HallCommand {
    Detect,
    Time,
    ToggleMicrophone { username: String },
}

impl HallCommand {
    pub(crate) fn parse_hall(text: &str) -> Option<CommandSyntax<'_, Self>> {
        for prefix in ["大厅检测", "大厅时间"] {
            let Some(argument) = parse_prefixed_command(text, prefix, false) else {
                continue;
            };
            let command = match prefix {
                "大厅检测" => Self::Detect,
                "大厅时间" => Self::Time,
                _ => unreachable!("all hall prefixes are handled"),
            };
            return Some(CommandSyntax {
                matched: prefix,
                argument,
                command,
            });
        }
        None
    }

    pub(crate) fn parse_friend(text: &str, username: &str) -> Option<Self> {
        let rest = strip_ascii_case_prefix(text, "麦克风")?;
        let rest = rest.trim_start_matches(['：', ':', ' ', '\t']);
        (rest.is_empty() || rest.starts_with([']', '】'])).then(|| Self::ToggleMicrophone {
            username: username.to_string(),
        })
    }

    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::Detect => "hall_detect".to_string(),
            Self::Time => "hall_time".to_string(),
            Self::ToggleMicrophone { username } => {
                format!("microphone:{}", command_identity(username))
            }
        }
    }

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ToggleMicrophone { username: left },
                Self::ToggleMicrophone { username: right },
            ) => command_identity(left) == command_identity(right),
            _ => self.lock_key() == other.lock_key(),
        }
    }
}
