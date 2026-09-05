use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::administration::AdministrationCommand;
use super::card_games::LandlordCommand;
use super::custom_workflow::CustomWorkflowCommand;
use super::hall::HallCommand;
use super::identity::IdentityRole;
use super::idiom_chain::IdiomChainCommand;
use super::invite::InviteCommand;
use super::moderation::ModerationCommand;
use super::playback::PlaybackCommand;
use super::song_request::SongCommand;
use super::turtle_soup::TurtleSoupCommand;
use super::undercover::UndercoverCommand;
use crate::observation::chat::{ObservationFrameId, ObservedChatMessageId};

/// 命令来源：大厅成员或好友私聊；身份角色在路由层单独判断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandAuthority {
    HallMember,
    Friend,
}

/// 命令前缀：@ 用于功能命令，# 用于娱乐玩法或玩法中的操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandPrefix {
    At,
    Hash,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommandObservation {
    pub(crate) frame_id: Option<ObservationFrameId>,
    pub(crate) captured_at: Option<Instant>,
    pub(crate) message_id: Option<ObservedChatMessageId>,
}

/// Chat input before a vertical feature has been selected or parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandEnvelope {
    original_text: String,
    user_command: String,
    command_text: String,
    message_type: String,
    username: String,
    prefix: CommandPrefix,
    authority: CommandAuthority,
    observation: CommandObservation,
}

impl CommandEnvelope {
    pub(crate) fn new(
        original_text: impl Into<String>,
        username: impl Into<String>,
        message_type: impl Into<String>,
        user_command: impl Into<String>,
        observation: CommandObservation,
    ) -> Option<Self> {
        let username = username.into();
        if username.trim().is_empty() {
            return None;
        }
        let message_type = message_type.into();
        let authority = match message_type.as_str() {
            "blue" => CommandAuthority::HallMember,
            "pink" => CommandAuthority::Friend,
            _ => return None,
        };
        let user_command = user_command.into();
        let user_command = user_command
            .trim()
            .trim_end_matches([']', '】'])
            .trim_end()
            .to_string();
        let (prefix, command_text) = if let Some(text) = user_command.strip_prefix('@') {
            (CommandPrefix::At, text)
        } else {
            (
                CommandPrefix::Hash,
                user_command
                    .strip_prefix('#')
                    .or_else(|| user_command.strip_prefix('＃'))?,
            )
        };
        let command_text = command_text.trim_start().to_string();
        if command_text.is_empty() {
            return None;
        }
        Some(Self {
            original_text: original_text.into(),
            user_command,
            command_text,
            message_type,
            username,
            prefix,
            authority,
            observation,
        })
    }

    /// 生成仅替换命令来源权限的副本，用于跨来源权限路由。
    pub(crate) fn with_authority(&self, authority: CommandAuthority) -> Self {
        Self {
            message_type: match authority {
                CommandAuthority::HallMember => "blue".to_string(),
                CommandAuthority::Friend => "pink".to_string(),
            },
            authority,
            ..self.clone()
        }
    }

    pub(crate) fn with_command_text(&self, command_text: impl Into<String>) -> Self {
        let command_text = command_text.into();
        let user_command = match self.prefix {
            CommandPrefix::At => format!("@{command_text}"),
            CommandPrefix::Hash => format!("#{command_text}"),
        };
        Self {
            original_text: self.original_text.clone(),
            user_command,
            command_text,
            message_type: self.message_type.clone(),
            username: self.username.clone(),
            prefix: self.prefix,
            authority: self.authority,
            observation: self.observation.clone(),
        }
    }

    pub(crate) fn username(&self) -> &str {
        &self.username
    }

    pub(crate) fn user_command(&self) -> &str {
        &self.user_command
    }

    pub(crate) fn command_text(&self) -> &str {
        &self.command_text
    }

    pub(crate) fn message_type(&self) -> &str {
        &self.message_type
    }

    pub(crate) const fn prefix(&self) -> CommandPrefix {
        self.prefix
    }

    pub(crate) const fn authority(&self) -> CommandAuthority {
        self.authority
    }

    pub(crate) fn observation(&self) -> &CommandObservation {
        &self.observation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FeatureCommandMatch<T> {
    pub(crate) matched: String,
    pub(crate) raw: String,
    pub(crate) command: T,
}

impl<T> FeatureCommandMatch<T> {
    pub(crate) fn new(matched: impl Into<String>, raw: impl Into<String>, command: T) -> Self {
        Self {
            matched: matched.into(),
            raw: raw.into(),
            command,
        }
    }

    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> FeatureCommandMatch<U> {
        FeatureCommandMatch {
            matched: self.matched,
            raw: self.raw,
            command: map(self.command),
        }
    }
}

/// The small top-level routing enum described by ADR 0059.
///
/// Every payload type is owned by its vertical feature. This enum identifies the selected
/// module after chat routing or lets a non-chat adapter submit a typed module command directly.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum ModuleCommand {
    SongRequest(SongCommand),
    Playback(PlaybackCommand),
    Hall(HallCommand),
    Administration(AdministrationCommand),
    IdiomChain(IdiomChainCommand),
    CardGame(LandlordCommand),
    TurtleSoup(TurtleSoupCommand),
    Undercover(UndercoverCommand),
    Invite(InviteCommand),
    Moderation(ModerationCommand),
    CustomWorkflow(CustomWorkflowCommand),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutedCommand {
    pub(crate) matched: String,
    pub(crate) raw: String,
    pub(crate) user_command: String,
    pub(crate) message_type: String,
    pub(crate) username: String,
    pub(crate) authority: CommandAuthority,
    pub(crate) role: Option<IdentityRole>,
    pub(crate) permission_required: Option<IdentityRole>,
    pub(crate) command: ModuleCommand,
    pub(crate) observation: CommandObservation,
}

impl RoutedCommand {
    pub(crate) fn from_envelope(
        envelope: &CommandEnvelope,
        matched: FeatureCommandMatch<ModuleCommand>,
    ) -> Self {
        debug_assert!(!envelope.original_text.trim().is_empty());
        Self {
            matched: matched.matched,
            raw: matched.raw,
            user_command: envelope.user_command().to_string(),
            message_type: envelope.message_type.clone(),
            username: envelope.username.clone(),
            authority: envelope.authority,
            role: None,
            permission_required: None,
            command: matched.command,
            observation: envelope.observation().clone(),
        }
    }

    pub(crate) fn console(
        matched: impl Into<String>,
        raw: impl Into<String>,
        command: ModuleCommand,
    ) -> Self {
        let raw = raw.into();
        Self {
            matched: matched.into(),
            user_command: format!("@{raw}"),
            raw,
            message_type: "控制台".to_string(),
            username: "控制台".to_string(),
            authority: CommandAuthority::HallMember,
            role: None,
            permission_required: None,
            command,
            observation: CommandObservation {
                captured_at: Some(Instant::now()),
                ..CommandObservation::default()
            },
        }
    }
}

impl ModuleCommand {
    pub(crate) fn lock_key(&self) -> String {
        match self {
            Self::SongRequest(command) => command.lock_key(),
            Self::Playback(command) => command.lock_key(),
            Self::Hall(command) => command.lock_key(),
            Self::Administration(command) => command.lock_key(),
            Self::IdiomChain(command) => command.lock_key(),
            Self::CardGame(command) => command.lock_key(),
            Self::TurtleSoup(command) => command.lock_key().to_string(),
            Self::Undercover(command) => command.lock_key(),
            Self::Invite(command) => command.lock_key(),
            Self::Moderation(command) => command.lock_key(),
            Self::CustomWorkflow(command) => command.lock_key(),
        }
    }

    pub(crate) fn scopes_lock_to_actor(&self) -> bool {
        matches!(self, Self::CardGame(_) | Self::Undercover(_))
    }

    /// Whether a command observed in the current hall needs the actual speaker identity.
    ///
    /// Logging and audit metadata do not count as an identity dependency. This is reserved for
    /// commands whose business result, authorization, turn ownership, or delivery target changes
    /// with the speaker.
    pub(crate) fn requires_hall_sender(&self) -> bool {
        match self {
            Self::SongRequest(_) | Self::Playback(_) | Self::Hall(_) | Self::Administration(_) => {
                false
            }
            Self::IdiomChain(command) => matches!(
                command,
                IdiomChainCommand::Start { .. }
                    | IdiomChainCommand::Submit(_)
                    | IdiomChainCommand::Stop
            ),
            Self::CardGame(command) => {
                !matches!(command, LandlordCommand::Status | LandlordCommand::Retry)
            }
            Self::TurtleSoup(command) => matches!(command, TurtleSoupCommand::Start),
            Self::Undercover(command) => !matches!(command, UndercoverCommand::Retry),
            // These modules are friend-only today, or expose the triggering user as part of their
            // execution contract. Keep them conservative if a hall route is added later.
            Self::Invite(_) | Self::Moderation(_) | Self::CustomWorkflow(_) => true,
        }
    }
}
