use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::administration::AdministrationCommand;
use super::card_games::LandlordCommand;
use super::custom_workflow::CustomWorkflowCommand;
use super::hall::HallCommand;
use super::idiom_chain::IdiomChainCommand;
use super::invite::InviteCommand;
use super::moderation::ModerationCommand;
use super::playback::PlaybackCommand;
use super::song_request::SongCommand;
use super::turtle_soup::TurtleSoupCommand;
use super::undercover::UndercoverCommand;
use crate::observation::chat::{ObservationFrameId, ObservedChatMessageId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandAuthority {
    HallMember,
    Friend,
}

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
        } else if let Some(text) = user_command
            .strip_prefix('#')
            .or_else(|| user_command.strip_prefix('＃'))
        {
            (CommandPrefix::Hash, text)
        } else {
            return None;
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
            command,
            observation: CommandObservation::default(),
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

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::SongRequest(left), Self::SongRequest(right)) => left.same_request(right),
            (Self::Playback(left), Self::Playback(right)) => left.same_request(right),
            (Self::Hall(left), Self::Hall(right)) => left.same_request(right),
            (Self::Administration(left), Self::Administration(right)) => left.same_request(right),
            (Self::IdiomChain(left), Self::IdiomChain(right)) => left.same_request(right),
            (Self::CardGame(left), Self::CardGame(right)) => left.same_request(right),
            (Self::TurtleSoup(left), Self::TurtleSoup(right)) => left.same_request(right),
            (Self::Undercover(left), Self::Undercover(right)) => left.same_request(right),
            (Self::Invite(left), Self::Invite(right)) => left.same_request(right),
            (Self::Moderation(left), Self::Moderation(right)) => left.same_request(right),
            (Self::CustomWorkflow(left), Self::CustomWorkflow(right)) => left.same_request(right),
            _ => false,
        }
    }

    pub(crate) fn scopes_lock_to_actor(&self) -> bool {
        matches!(self, Self::CardGame(_) | Self::Undercover(_))
    }
}
