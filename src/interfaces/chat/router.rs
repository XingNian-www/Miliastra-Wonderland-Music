use crate::features::administration::AdministrationCommand;
use crate::features::card_games::LandlordCommand;
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch, ModuleCommand,
    RoutedCommand,
};
use crate::features::custom_workflow::CustomWorkflowService;
use crate::features::entertainment::EntertainmentKind;
use crate::features::hall::HallCommand;
use crate::features::idiom_chain::IdiomChainCommand;
use crate::features::invite::InviteCommand;
use crate::features::moderation::ModerationCommand;
use crate::features::playback::PlaybackCommand;
use crate::features::song_request::SongCommand;
use crate::features::turtle_soup::TurtleSoupCommand;
use crate::features::undercover::UndercoverCommand;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChatCommandModule {
    SongRequest,
    Playback,
    Hall,
    Administration,
    IdiomChain,
    CardGame,
    TurtleSoup,
    Undercover,
    Invite,
    Moderation,
    CustomWorkflow,
}

type ModuleClaim = (ChatCommandModule, fn(&CommandEnvelope) -> bool);

/// Static module router for chat command envelopes.
///
/// Selection asks each vertical module only whether it owns the syntax. Once selected, only that
/// module parses its arguments. Hash commands use the active entertainment owner explicitly.
pub(crate) struct ChatCommandRouter<'a> {
    custom_workflow: Option<&'a CustomWorkflowService>,
}

impl<'a> ChatCommandRouter<'a> {
    pub(crate) const fn new(custom_workflow: &'a CustomWorkflowService) -> Self {
        Self {
            custom_workflow: Some(custom_workflow),
        }
    }

    #[cfg(test)]
    pub(crate) const fn without_custom_workflow() -> Self {
        Self {
            custom_workflow: None,
        }
    }

    pub(crate) fn select_module(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<ChatCommandModule> {
        match envelope.prefix() {
            CommandPrefix::At => self.select_at_module(envelope),
            CommandPrefix::Hash => self.select_hash_module(envelope, active_entertainment),
        }
    }

    pub(crate) fn route(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<RoutedCommand> {
        let module = self.select_module(envelope, active_entertainment)?;
        let matched =
            match module {
                ChatCommandModule::SongRequest => SongCommand::parse_chat(envelope)
                    .map(|matched| matched.map(ModuleCommand::SongRequest)),
                ChatCommandModule::Playback => PlaybackCommand::parse_chat(envelope)
                    .map(|matched| matched.map(ModuleCommand::Playback)),
                ChatCommandModule::Hall => HallCommand::parse_chat(envelope)
                    .map(|matched| matched.map(ModuleCommand::Hall)),
                ChatCommandModule::Administration => AdministrationCommand::parse_chat(envelope)
                    .map(|matched| matched.map(ModuleCommand::Administration)),
                ChatCommandModule::IdiomChain => {
                    route_idiom(envelope).map(|matched| matched.map(ModuleCommand::IdiomChain))
                }
                ChatCommandModule::CardGame => {
                    route_card_game(envelope).map(|matched| matched.map(ModuleCommand::CardGame))
                }
                ChatCommandModule::TurtleSoup => route_turtle_soup(envelope)
                    .map(|matched| matched.map(ModuleCommand::TurtleSoup)),
                ChatCommandModule::Undercover => {
                    route_undercover(envelope).map(|matched| matched.map(ModuleCommand::Undercover))
                }
                ChatCommandModule::Invite => InviteCommand::parse_chat(envelope)
                    .map(|matched| matched.map(ModuleCommand::Invite)),
                ChatCommandModule::Moderation => ModerationCommand::parse_chat(envelope)
                    .map(|matched| matched.map(ModuleCommand::Moderation)),
                ChatCommandModule::CustomWorkflow => self
                    .custom_workflow?
                    .parse_chat(envelope)
                    .map(|matched| matched.map(ModuleCommand::CustomWorkflow)),
            }?;
        Some(RoutedCommand::from_envelope(envelope, matched))
    }

    fn select_at_module(&self, envelope: &CommandEnvelope) -> Option<ChatCommandModule> {
        let candidates: &[ModuleClaim] = match envelope.authority() {
            CommandAuthority::HallMember => &[
                (ChatCommandModule::SongRequest, SongCommand::claims_chat),
                (ChatCommandModule::Playback, PlaybackCommand::claims_chat),
                (ChatCommandModule::Hall, HallCommand::claims_chat),
                (
                    ChatCommandModule::Administration,
                    AdministrationCommand::claims_chat,
                ),
            ],
            CommandAuthority::Friend => &[
                (
                    ChatCommandModule::Administration,
                    AdministrationCommand::claims_chat,
                ),
                (ChatCommandModule::SongRequest, SongCommand::claims_chat),
                (ChatCommandModule::Invite, InviteCommand::claims_chat),
                (
                    ChatCommandModule::Moderation,
                    ModerationCommand::claims_chat,
                ),
                (ChatCommandModule::Hall, HallCommand::claims_chat),
            ],
        };
        candidates
            .iter()
            .find_map(|(module, claims)| claims(envelope).then_some(*module))
            .or_else(|| {
                self.custom_workflow
                    .is_some_and(|service| service.claims_chat(envelope))
                    .then_some(ChatCommandModule::CustomWorkflow)
            })
    }

    fn select_hash_module(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<ChatCommandModule> {
        if envelope.authority() == CommandAuthority::HallMember {
            for (module, claims) in [
                (
                    ChatCommandModule::IdiomChain,
                    IdiomChainCommand::claims_start_chat as fn(&CommandEnvelope) -> bool,
                ),
                (
                    ChatCommandModule::CardGame,
                    LandlordCommand::claims_start_chat,
                ),
                (
                    ChatCommandModule::TurtleSoup,
                    TurtleSoupCommand::claims_start_chat,
                ),
                (
                    ChatCommandModule::Undercover,
                    UndercoverCommand::claims_start_chat,
                ),
                (
                    ChatCommandModule::Administration,
                    AdministrationCommand::claims_chat,
                ),
            ] {
                if claims(envelope) {
                    return Some(module);
                }
            }
        }
        match active_entertainment {
            Some(EntertainmentKind::IdiomChain)
                if IdiomChainCommand::claims_active_chat(envelope) =>
            {
                Some(ChatCommandModule::IdiomChain)
            }
            Some(EntertainmentKind::Landlord | EntertainmentKind::RunFast)
                if LandlordCommand::claims_active_chat(envelope) =>
            {
                Some(ChatCommandModule::CardGame)
            }
            Some(EntertainmentKind::TurtleSoup)
                if TurtleSoupCommand::claims_active_chat(envelope) =>
            {
                Some(ChatCommandModule::TurtleSoup)
            }
            Some(EntertainmentKind::Undercover)
                if UndercoverCommand::claims_active_chat(envelope) =>
            {
                Some(ChatCommandModule::Undercover)
            }
            _ => None,
        }
    }
}

fn route_idiom(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<IdiomChainCommand>> {
    IdiomChainCommand::parse_start_chat(envelope)
        .or_else(|| IdiomChainCommand::parse_active_chat(envelope))
}

fn route_card_game(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<LandlordCommand>> {
    LandlordCommand::parse_start_chat(envelope)
        .or_else(|| LandlordCommand::parse_active_chat(envelope))
}

fn route_turtle_soup(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<TurtleSoupCommand>> {
    TurtleSoupCommand::parse_start_chat(envelope)
        .or_else(|| TurtleSoupCommand::parse_active_chat(envelope))
}

fn route_undercover(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<UndercoverCommand>> {
    UndercoverCommand::parse_start_chat(envelope)
        .or_else(|| UndercoverCommand::parse_active_chat(envelope))
}
