use crate::features::administration::AdministrationCommand;
use crate::features::card_games::LandlordCommand;
use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch, ModuleCommand,
    RoutedCommand,
};
use crate::features::custom_workflow::CustomWorkflowService;
use crate::features::entertainment::EntertainmentKind;
use crate::features::hall::HallCommand;
use crate::features::identity::{IdentityAccess, IdentityRole};
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
    identity: Option<&'a IdentityAccess>,
}

impl<'a> ChatCommandRouter<'a> {
    pub(crate) const fn new(custom_workflow: &'a CustomWorkflowService) -> Self {
        Self {
            custom_workflow: Some(custom_workflow),
            identity: None,
        }
    }

    pub(crate) const fn with_identity(
        custom_workflow: &'a CustomWorkflowService,
        identity: &'a IdentityAccess,
    ) -> Self {
        Self {
            custom_workflow: Some(custom_workflow),
            identity: Some(identity),
        }
    }

    #[cfg(test)]
    pub(crate) const fn without_custom_workflow() -> Self {
        Self {
            custom_workflow: None,
            identity: None,
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
        let role = self
            .identity
            .and_then(|identity| identity.role_of(envelope.username()));
        let authority = match role {
            Some(IdentityRole::Friend | IdentityRole::Admin) => CommandAuthority::Friend,
            _ => envelope.authority(),
        };
        let primary =
            (authority != envelope.authority()).then(|| envelope.with_authority(authority));
        let alternate_authority = match role {
            Some(IdentityRole::Owner) => Some(match envelope.authority() {
                CommandAuthority::HallMember => CommandAuthority::Friend,
                CommandAuthority::Friend => CommandAuthority::HallMember,
            }),
            _ => None,
        };
        self.route_with_authorities(envelope, active_entertainment, primary, alternate_authority)
            .or_else(|| {
                (envelope.authority() == CommandAuthority::HallMember
                    && self.identity.is_some()
                    && role.is_none())
                .then(|| self.route_as_permission_denied(envelope, active_entertainment))
                .flatten()
            })
    }

    fn route_as_permission_denied(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<RoutedCommand> {
        if envelope.authority() != CommandAuthority::HallMember {
            return None;
        }
        let candidate = envelope.with_authority(CommandAuthority::Friend);
        let matched = self.route_match(&candidate, active_entertainment)?;
        let mut routed = RoutedCommand::from_envelope(envelope, matched);
        routed.permission_required = Some(IdentityRole::Admin);
        Some(routed)
    }

    fn route_with_authorities(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
        primary: Option<CommandEnvelope>,
        alternate_authority: Option<CommandAuthority>,
    ) -> Option<RoutedCommand> {
        let tolerant = ocr_tolerant_command_text(envelope.command_text());
        for candidate in [
            Some(envelope.clone()),
            primary.clone(),
            tolerant
                .as_ref()
                .map(|text| envelope.with_command_text(text)),
            tolerant.as_ref().map(|text| {
                primary.as_ref().map_or_else(
                    || envelope.with_command_text(text),
                    |candidate| candidate.with_command_text(text),
                )
            }),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(matched) = self.route_match(&candidate, active_entertainment) {
                let mut routed = RoutedCommand::from_envelope(envelope, matched);
                routed.authority = candidate.authority();
                routed.message_type = candidate.message_type().to_string();
                routed.role = role_for(self.identity, envelope.username());
                return Some(routed);
            }
            if let Some(authority) = alternate_authority {
                let candidate = candidate.with_authority(authority);
                if let Some(matched) = self.route_match(&candidate, active_entertainment) {
                    let mut routed = RoutedCommand::from_envelope(envelope, matched);
                    routed.authority = candidate.authority();
                    routed.message_type = candidate.message_type().to_string();
                    routed.role = role_for(self.identity, envelope.username());
                    return Some(routed);
                }
            }
        }
        None
    }

    fn route_match(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<FeatureCommandMatch<ModuleCommand>> {
        let module = self.select_module(envelope, active_entertainment)?;
        match module {
            ChatCommandModule::SongRequest => SongCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::SongRequest)),
            ChatCommandModule::Playback => PlaybackCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::Playback)),
            ChatCommandModule::Hall => {
                HallCommand::parse_chat(envelope).map(|matched| matched.map(ModuleCommand::Hall))
            }
            ChatCommandModule::Administration => AdministrationCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::Administration)),
            ChatCommandModule::IdiomChain => {
                route_idiom(envelope).map(|matched| matched.map(ModuleCommand::IdiomChain))
            }
            ChatCommandModule::CardGame => {
                route_card_game(envelope).map(|matched| matched.map(ModuleCommand::CardGame))
            }
            ChatCommandModule::TurtleSoup => {
                route_turtle_soup(envelope).map(|matched| matched.map(ModuleCommand::TurtleSoup))
            }
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
        }
    }

    fn select_at_module(&self, envelope: &CommandEnvelope) -> Option<ChatCommandModule> {
        // Decision replies belong only to the active exclusive reader. Keep their reserved
        // syntax out of configurable workflows when the same frame is also dispatched normally.
        if is_reserved_decision_command(envelope) {
            return None;
        }
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
                (ChatCommandModule::Playback, PlaybackCommand::claims_chat),
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

fn is_reserved_decision_command(envelope: &CommandEnvelope) -> bool {
    let command = envelope.command_text();
    if ["确认", "跳过", "换源", "AI"]
        .iter()
        .any(|prefix| command.strip_prefix(prefix).is_some_and(decision_boundary))
    {
        return true;
    }

    envelope.authority() == CommandAuthority::Friend
        && [
            "邀请确认",
            "邀请拒绝",
            "确认邀请",
            "拒绝邀请",
            "同意邀请",
            "同意",
            "不同意",
        ]
        .iter()
        .any(|prefix| command.strip_prefix(prefix).is_some_and(decision_boundary))
}

fn decision_boundary(rest: &str) -> bool {
    match rest.chars().next() {
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

fn role_for(identity: Option<&IdentityAccess>, username: &str) -> Option<IdentityRole> {
    identity.and_then(|identity| identity.role_of(username))
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

/// OCR 容错：去掉命令文本首尾的噪声标点后重试路由。
/// 覆盖常见抖动（如 `#海龟汤，` 尾部粘连标点），不处理文本内部的形近字差异。
fn ocr_tolerant_command_text(text: &str) -> Option<String> {
    let trimmed = text.trim_end_matches(OCR_TRAILING_NOISE);
    let trimmed = trimmed.trim_start_matches(OCR_LEADING_NOISE);
    (!trimmed.is_empty() && trimmed != text).then(|| trimmed.to_string())
}

/// 命令尾部常见 OCR 噪声：中文/英文标点、括号、引号、省略号与空白。
const OCR_TRAILING_NOISE: &[char] = &[
    '，', ',', '。', '.', '！', '!', '？', '?', '、', '；', ';', '：', ':', '·', '…', '~', '～',
    '—', '」', '』', '”', '’', '）', ')', '】', ']', ' ', '\t', '　',
];

/// 命令头部常见 OCR 噪声：空白与普通标点（# / @ 前缀在 envelope 中已剥离）。
const OCR_LEADING_NOISE: &[char] = &[
    ' ', '\t', '　', '，', ',', '。', '.', '！', '!', '？', '?', '、', '；', ';', '：', ':',
];

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::features::command::CommandObservation;
    use crate::features::custom_workflow::{
        CustomWorkflowConfig, CustomWorkflowDefinition, CustomWorkflowService, CustomWorkflowStep,
        WorkflowDefaults,
    };
    use crate::features::identity::{
        IdentityAccess, IdentityConfig, IdentityMapping, IdentityRole,
    };

    fn envelope(message_type: &str, command: &str) -> CommandEnvelope {
        CommandEnvelope::new(
            format!("用户：{command}"),
            "用户",
            message_type,
            command,
            CommandObservation::default(),
        )
        .expect("test command envelope")
    }

    #[test]
    fn reserves_decision_syntax_from_custom_workflow_routing() {
        for (message_type, command) in [
            ("blue", "@确认"),
            ("blue", "@跳过！"),
            ("blue", "@换源"),
            ("blue", "@AI"),
            ("pink", "@邀请确认"),
            ("pink", "@邀请拒绝"),
            ("pink", "@同意邀请"),
            ("pink", "@拒绝邀请"),
            ("pink", "@同意"),
            ("pink", "@不同意"),
        ] {
            assert!(
                is_reserved_decision_command(&envelope(message_type, command)),
                "decision syntax was not reserved: {message_type} {command}"
            );
        }

        for (message_type, command) in [("blue", "@确认其他"), ("blue", "@AI点歌 晴天")] {
            assert!(
                !is_reserved_decision_command(&envelope(message_type, command)),
                "ordinary command was reserved: {message_type} {command}"
            );
        }
    }

    #[test]
    fn delete_current_pool_track_is_friend_only() {
        let router = ChatCommandRouter::without_custom_workflow();

        let friend = router
            .route(&envelope("pink", "@删除"), None)
            .expect("好友私聊应允许删除当前播放池歌曲");
        assert_eq!(
            friend.command,
            ModuleCommand::Playback(PlaybackCommand::DeleteCurrentPoolTrack)
        );
        assert!(router.route(&envelope("blue", "@删除"), None).is_none());
        assert!(router.route(&envelope("pink", "@下一首"), None).is_none());
    }

    #[test]
    fn unmapped_hall_member_gets_permission_denied_command() {
        let identity = IdentityAccess::new(IdentityConfig::default());
        let service = CustomWorkflowService::new(
            crate::features::custom_workflow::CustomWorkflowConfig::default(),
            WorkflowDefaults {
                default_timeout_ms: 1_000,
                default_poll_ms: 100,
                default_step_wait_ms: 100,
                decision_timeout_ms: 1_000,
                decision_poll_ms: 100,
                after_activate_ms: 100,
                clipboard_hold_ms: 100,
                stability_mean_threshold: 1.0,
                stability_changed_ratio_threshold: 0.1,
            },
        );
        let routed = ChatCommandRouter::with_identity(&service, &identity)
            .route(&envelope("blue", "@删除"), None)
            .expect("应识别权限不足命令");
        assert_eq!(routed.permission_required, Some(IdentityRole::Admin));
        assert_eq!(
            routed.command,
            ModuleCommand::Playback(PlaybackCommand::DeleteCurrentPoolTrack)
        );
    }

    #[test]
    fn identity_roles_expand_command_sources() {
        let identity = IdentityAccess::new(IdentityConfig {
            mappings: vec![
                IdentityMapping {
                    nickname: "用户".to_string(),
                    id: uuid::Uuid::from_u128(1),
                    role: IdentityRole::Owner,
                    note: String::new(),
                },
                IdentityMapping {
                    nickname: "管理员".to_string(),
                    id: uuid::Uuid::from_u128(2),
                    role: IdentityRole::Admin,
                    note: String::new(),
                },
            ],
        });
        let service = CustomWorkflowService::new(
            crate::features::custom_workflow::CustomWorkflowConfig::default(),
            WorkflowDefaults {
                default_timeout_ms: 1_000,
                default_poll_ms: 100,
                default_step_wait_ms: 100,
                decision_timeout_ms: 1_000,
                decision_poll_ms: 100,
                after_activate_ms: 100,
                clipboard_hold_ms: 100,
                stability_mean_threshold: 1.0,
                stability_changed_ratio_threshold: 0.1,
            },
        );
        let router = ChatCommandRouter::with_identity(&service, &identity);
        let routed = router.route(&envelope("blue", "@删除"), None).unwrap();
        assert_eq!(routed.authority, CommandAuthority::Friend);
        assert_eq!(routed.message_type, "pink");
        assert_eq!(routed.role, Some(IdentityRole::Owner));

        let admin = CommandEnvelope::new(
            "用户：@删除",
            "管理员",
            "blue",
            "@删除",
            CommandObservation::default(),
        )
        .unwrap();
        let routed = router.route(&admin, None).unwrap();
        assert_eq!(routed.authority, CommandAuthority::Friend);
        assert_eq!(routed.role, Some(IdentityRole::Admin));
    }

    #[test]
    fn ocr_tolerant_command_text_strips_leading_and_trailing_noise() {
        assert_eq!(
            ocr_tolerant_command_text("海龟汤，").as_deref(),
            Some("海龟汤")
        );
        assert_eq!(
            ocr_tolerant_command_text("下一首。！").as_deref(),
            Some("下一首")
        );
        assert_eq!(ocr_tolerant_command_text("，帮助").as_deref(), Some("帮助"));
        assert_eq!(ocr_tolerant_command_text("海龟汤"), None);
        assert_eq!(ocr_tolerant_command_text("，"), None);
    }

    #[test]
    fn route_retries_with_ocr_tolerant_text() {
        let router = ChatCommandRouter::without_custom_workflow();
        // 尾部粘连标点：原文本无法匹配，容错重试后命中。
        let routed = router.route(&envelope("blue", "#海龟汤，"), None);
        assert!(routed.is_some(), "尾部标点应通过容错命中海龟汤开局");
        let routed = router.route(&envelope("blue", "@帮助。"), None);
        assert!(routed.is_some(), "尾部标点应通过容错命中帮助命令");
        // 原始 user_command 保留，便于回显与日志关联原文本。
        let routed = router.route(&envelope("blue", "@帮助。"), None).unwrap();
        assert_eq!(routed.user_command, "@帮助。");
    }

    #[test]
    fn custom_workflow_cannot_claim_reserved_decision_syntax() {
        let service = CustomWorkflowService::new(
            CustomWorkflowConfig {
                enabled: true,
                default_threshold: 0.9,
                wait_template_absent_stable_default: true,
                max_hold_key_seconds: 10,
                templates: HashMap::new(),
                workflows: vec![CustomWorkflowDefinition {
                    enabled: true,
                    name: "确认工作流".to_string(),
                    commands: vec!["确认".to_string()],
                    allow_args: false,
                    message_types: Vec::new(),
                    confirm_before_run: false,
                    confirm_message: String::new(),
                    confirm_message_types: Vec::new(),
                    confirm_timeout_ms: None,
                    confirm_poll_ms: None,
                    steps: vec![CustomWorkflowStep {
                        step_type: "key".to_string(),
                        template: None,
                        region: None,
                        point: None,
                        click_offset: None,
                        key: Some("F1".to_string()),
                        button: None,
                        target: None,
                        text: None,
                        message: None,
                        threshold: None,
                        timeout_ms: None,
                        poll_ms: None,
                        wait_ms: None,
                        hold_seconds_arg: None,
                        stable_after_absent: None,
                    }],
                    success_message: String::new(),
                }],
            },
            WorkflowDefaults {
                default_timeout_ms: 1_000,
                default_poll_ms: 100,
                default_step_wait_ms: 100,
                decision_timeout_ms: 1_000,
                decision_poll_ms: 100,
                after_activate_ms: 100,
                clipboard_hold_ms: 100,
                stability_mean_threshold: 1.0,
                stability_changed_ratio_threshold: 0.1,
            },
        );
        let envelope = envelope("blue", "@确认");
        assert!(service.claims_chat(&envelope));
        assert!(
            ChatCommandRouter::new(&service)
                .route(&envelope, None)
                .is_none()
        );
    }
}
