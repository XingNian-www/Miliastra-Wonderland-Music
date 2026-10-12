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
use crate::features::idiom_chain::{IdiomChainCommand, IdiomChainMode};
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
    Tarot,
    Playlist,
    Undercover,
    Invite,
    Moderation,
    CustomWorkflow,
}

type ModuleClaim = (ChatCommandModule, fn(&CommandEnvelope) -> bool);

/// 聊天命令信封的静态模块路由器。
///
/// 选择阶段只询问各业务模块是否拥有该语法；选中后仅由该模块解析参数。井号命令显式使用当前娱乐模块所有者。
pub(crate) struct ChatCommandRouter<'a> {
    custom_workflow: Option<&'a CustomWorkflowService>,
    identity: Option<&'a IdentityAccess>,
}

impl<'a> ChatCommandRouter<'a> {
    #[cfg(test)]
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
            // 测试专用前缀：不认领任何模块，永不执行。
            CommandPrefix::Amp => None,
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
        let alternate_authority = match role {
            Some(IdentityRole::Friend | IdentityRole::Admin | IdentityRole::Owner)
                if envelope.authority() == CommandAuthority::HallMember =>
            {
                Some(CommandAuthority::Friend)
            }
            Some(IdentityRole::Owner) if envelope.authority() == CommandAuthority::Friend => {
                Some(CommandAuthority::HallMember)
            }
            _ => None,
        };
        self.route_with_authorities(envelope, active_entertainment, None, alternate_authority)
            .or_else(|| {
                (envelope.authority() == CommandAuthority::HallMember
                    && self.identity.is_some()
                    && role.is_none())
                .then(|| self.route_as_permission_denied(envelope, active_entertainment))
                .flatten()
            })
    }

    /// 仅用于“读到命令却没执行”的排查日志：解释一个已可解析的命令信封为什么没有模块认领。
    /// 不参与路由，也不改变任何路由/认领结果；`@选择`/`@本地` 只作为决策语法用于日志归类。
    pub(crate) fn unclaimed_command_reason(
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> &'static str {
        if looks_like_reserved_decision_syntax(envelope) {
            return "保留的决策语法（由独占决策窗口消费，正常分发排除）";
        }
        if hash_entertainment_session_mismatch(envelope, active_entertainment) {
            return "娱乐 # 命令与当前会话不匹配（没有进行中的对应玩法）";
        }
        "信封可解析但没有模块认领"
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
            primary.clone(),
            Some(envelope.clone()),
            tolerant.as_ref().and_then(|text| {
                primary
                    .as_ref()
                    .map(|candidate| candidate.with_command_text(text))
            }),
            tolerant
                .as_ref()
                .map(|text| envelope.with_command_text(text)),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(matched) = self.route_match(&candidate, active_entertainment) {
                let mut routed = RoutedCommand::from_envelope(envelope, matched);
                routed.authority = envelope.authority();
                routed.message_type = envelope.message_type().to_string();
                routed.role = role_for(self.identity, envelope.username());
                return Some(routed);
            }
            if let Some(authority) = alternate_authority {
                let candidate = candidate.with_authority(authority);
                if let Some(matched) = self.route_match(&candidate, active_entertainment) {
                    let mut routed = RoutedCommand::from_envelope(envelope, matched);
                    routed.authority = envelope.authority();
                    routed.message_type = envelope.message_type().to_string();
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
            ChatCommandModule::Tarot => crate::features::tarot::TarotCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::Tarot)),
            ChatCommandModule::Playlist => {
                crate::features::playlist::PlaylistCommand::parse_chat(envelope)
                    .map(|matched| matched.map(ModuleCommand::Playlist))
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
        // Decision replies belong to active readers, not ordinary/custom routing.
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
                (
                    ChatCommandModule::Playlist,
                    crate::features::playlist::PlaylistCommand::claims_chat,
                ),
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
        // 塔罗是一次性回复，不占用或切换其他娱乐会话。
        if crate::features::tarot::TarotCommand::claims_chat(envelope) {
            return Some(ChatCommandModule::Tarot);
        }
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

#[cfg(test)]
mod tarot_routing_tests {
    use super::*;
    use crate::features::command::CommandObservation;

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
    fn playlist_numbers_are_reserved_for_the_exclusive_window() {
        let router = ChatCommandRouter::without_custom_workflow();
        for index in 1..=5 {
            let text = format!("@{index}");
            let envelope =
                CommandEnvelope::new(&text, "用户", "pink", &text, CommandObservation::default())
                    .unwrap();
            assert!(is_reserved_decision_command(&envelope));
            assert!(router.route(&envelope, None).is_none());
        }
    }

    #[test]
    fn turtle_soup_friend_can_end_but_not_query_status() {
        let router = ChatCommandRouter::without_custom_workflow();
        let active = Some(EntertainmentKind::TurtleSoup);

        let friend_end = router
            .route(&envelope("pink", "#结束"), active)
            .expect("好友应可结束海龟汤");
        assert!(matches!(
            friend_end.command,
            ModuleCommand::TurtleSoup(TurtleSoupCommand::End)
        ));
        assert!(router.route(&envelope("pink", "#状态"), active).is_none());

        let hall_end = router
            .route(&envelope("blue", "#结束"), active)
            .expect("大厅结束语法仍路由到服务层拒绝");
        assert!(matches!(
            hall_end.command,
            ModuleCommand::TurtleSoup(TurtleSoupCommand::End)
        ));
    }

    #[test]
    fn hash_entertainment_help_accepts_the_full_name_too() {
        let router = ChatCommandRouter::without_custom_workflow();
        for text in ["#娱乐", "#帮助", "#娱乐帮助"] {
            let envelope =
                CommandEnvelope::new(text, "用户", "blue", text, CommandObservation::default())
                    .unwrap();
            assert!(router.route(&envelope, None).is_some(), "{text} 应被认领");
        }
    }

    #[test]
    fn ampersand_prefix_is_test_only_and_never_routed() {
        let router = ChatCommandRouter::without_custom_workflow();
        let envelope = CommandEnvelope::new(
            "&状态",
            "用户",
            "blue",
            "&状态",
            CommandObservation::default(),
        )
        .unwrap();
        assert_eq!(envelope.prefix(), CommandPrefix::Amp);
        assert!(
            router.route(&envelope, None).is_none(),
            "& 前缀是测试专用，永不路由/执行"
        );
    }

    #[test]
    fn tarot_routes_in_hall_and_private_without_stealing_game_commands() {
        let router = ChatCommandRouter::without_custom_workflow();
        for source in ["blue", "pink"] {
            for active in [
                None,
                Some(EntertainmentKind::IdiomChain),
                Some(EntertainmentKind::Landlord),
                Some(EntertainmentKind::TurtleSoup),
                Some(EntertainmentKind::Undercover),
            ] {
                let envelope = CommandEnvelope::new(
                    "#塔罗牌AI 学习",
                    "用户",
                    source,
                    "#塔罗牌AI 学习",
                    CommandObservation::default(),
                )
                .unwrap();
                assert_eq!(
                    router.select_module(&envelope, active),
                    Some(ChatCommandModule::Tarot)
                );
                let routed = router.route(&envelope, active).unwrap();
                assert!(matches!(routed.command, ModuleCommand::Tarot(_)));
                assert!(routed.command.requires_hall_sender());
                assert!(routed.command.scopes_lock_to_actor());
            }
        }
        let help = CommandEnvelope::new(
            "#塔罗牌帮助",
            "用户",
            "blue",
            "#塔罗牌帮助",
            CommandObservation::default(),
        )
        .unwrap();
        assert!(
            !router
                .route(&help, None)
                .unwrap()
                .command
                .requires_hall_sender()
        );
        let active = CommandEnvelope::new(
            "#提示",
            "用户",
            "blue",
            "#提示",
            CommandObservation::default(),
        )
        .unwrap();
        assert_eq!(
            router.select_module(&active, Some(EntertainmentKind::IdiomChain)),
            Some(ChatCommandModule::IdiomChain)
        );
    }
}

fn is_reserved_decision_command(envelope: &CommandEnvelope) -> bool {
    // Window inputs are consumed by their active readers, never by ordinary/custom routing.
    if crate::features::playlist::PlaylistCommand::is_window_decision(envelope) {
        return true;
    }
    // 决策语法只属于 `@`：`#` 的娱乐“进行中命令”（如 #确认/#状态）由各玩法的
    // claims_active_chat 判定，不能被当成保留的决策回复而排除。
    if envelope.prefix() != CommandPrefix::At {
        return false;
    }
    let command = envelope.command_text();
    [
        "确认",
        "跳过",
        "换源",
        "AI",
        "选择",
        "本地",
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

/// 决策窗口保留语法（仅用于“未执行”日志归类）。`@选择`/`@本地` 当前并未纳入
/// `is_reserved_decision_command`（会落到“无模块认领”），这里只做日志分类，
/// 不改变任何路由/认领行为。
fn looks_like_reserved_decision_syntax(envelope: &CommandEnvelope) -> bool {
    if is_reserved_decision_command(envelope) {
        return true;
    }
    let command = envelope.command_text();
    ["选择", "本地"]
        .iter()
        .any(|prefix| command.strip_prefix(prefix).is_some_and(decision_boundary))
}

/// 娱乐 `#` 命令存在，但与当前进行中的玩法不匹配：命令本身属于某个娱乐玩法的
/// 进行中操作，却没有对应的 active_entertainment，因此路由层不认领。
fn hash_entertainment_session_mismatch(
    envelope: &CommandEnvelope,
    active_entertainment: Option<EntertainmentKind>,
) -> bool {
    if envelope.prefix() != CommandPrefix::Hash
        || envelope.authority() != CommandAuthority::HallMember
    {
        return false;
    }
    let matches_active = match active_entertainment {
        Some(EntertainmentKind::IdiomChain) => IdiomChainCommand::claims_active_chat(envelope),
        Some(EntertainmentKind::Landlord | EntertainmentKind::RunFast) => {
            LandlordCommand::claims_active_chat(envelope)
        }
        Some(EntertainmentKind::TurtleSoup) => TurtleSoupCommand::claims_active_chat(envelope),
        Some(EntertainmentKind::Undercover) => UndercoverCommand::claims_active_chat(envelope),
        _ => false,
    };
    !matches_active
        && (IdiomChainCommand::claims_active_chat(envelope)
            || LandlordCommand::claims_active_chat(envelope)
            || TurtleSoupCommand::claims_active_chat(envelope)
            || UndercoverCommand::claims_active_chat(envelope))
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
use crate::text::OCR_TRAILING_NOISE;

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
            ("blue", "@选择"),
            ("blue", "@本地"),
            ("pink", "@选择"),
            ("pink", "@本地"),
            ("blue", "@邀请确认"),
            ("blue", "@邀请拒绝"),
            ("blue", "@同意邀请"),
            ("blue", "@拒绝邀请"),
            ("blue", "@同意"),
            ("blue", "@不同意"),
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

        for (message_type, command) in [
            ("blue", "@确认其他"),
            ("blue", "@AI点歌晴天"),
            ("blue", "#确认"),
        ] {
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
    fn moderation_routes_keep_source_and_trusted_role_separate() {
        for role in [
            None,
            Some(IdentityRole::Friend),
            Some(IdentityRole::Admin),
            Some(IdentityRole::Owner),
        ] {
            let identity = IdentityAccess::new(IdentityConfig {
                mappings: role
                    .into_iter()
                    .map(|role| IdentityMapping {
                        nickname: "用户".to_string(),
                        id: uuid::Uuid::from_u128(1),
                        role,
                        note: "展示备注".to_string(),
                    })
                    .collect(),
            });
            let router = ChatCommandRouter {
                custom_workflow: None,
                identity: Some(&identity),
            };
            for source in ["pink", "blue"] {
                for (text, action) in [
                    (
                        "@拉黑UID123456789",
                        crate::features::moderation::ModerationAction::Blacklist,
                    ),
                    (
                        "@屏蔽123456789",
                        crate::features::moderation::ModerationAction::BlockChat,
                    ),
                ] {
                    let routed = router.route(&envelope(source, text), None).unwrap();
                    assert_eq!(routed.role, role);
                    assert_eq!(
                        routed.authority,
                        if source == "pink" {
                            CommandAuthority::Friend
                        } else {
                            CommandAuthority::HallMember
                        }
                    );
                    assert_eq!(
                        routed.permission_required.is_some(),
                        source == "blue" && role.is_none()
                    );
                    let ModuleCommand::Moderation(command) = routed.command else {
                        panic!("moderation command expected");
                    };
                    assert_eq!(command.action, action);
                    assert_eq!(command.uid, "123456789");
                    assert_eq!(command.requester, "用户");
                }
            }
        }
    }

    #[test]
    fn moderation_admin_routing_keeps_uid_validation() {
        let identity = IdentityAccess::new(IdentityConfig {
            mappings: vec![IdentityMapping {
                nickname: "用户".to_string(),
                id: uuid::Uuid::from_u128(1),
                role: IdentityRole::Admin,
                note: String::new(),
            }],
        });
        let router = ChatCommandRouter {
            custom_workflow: None,
            identity: Some(&identity),
        };
        for source in ["pink", "blue"] {
            for text in [
                "@拉黑12345678",
                "@拉黑1234567890",
                "@拉黑UID１２３４５６７８９",
                "@拉黑123456789abc",
            ] {
                assert!(router.route(&envelope(source, text), None).is_none());
            }
        }
    }

    #[test]
    fn unmapped_friend_keeps_friend_source_permissions() {
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
            .route(&envelope("pink", "@删除"), None)
            .expect("无备注好友私聊应保留好友来源命令");
        assert_eq!(routed.authority, CommandAuthority::Friend);
        assert_eq!(routed.role, None);
        assert_eq!(routed.permission_required, None);
        assert_eq!(
            routed.command,
            ModuleCommand::Playback(PlaybackCommand::DeleteCurrentPoolTrack)
        );
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
                IdentityMapping {
                    nickname: "好友".to_string(),
                    id: uuid::Uuid::from_u128(3),
                    role: IdentityRole::Friend,
                    note: "好友备注".to_string(),
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
        assert_eq!(routed.authority, CommandAuthority::HallMember);
        assert_eq!(routed.message_type, "blue");
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
        assert_eq!(routed.authority, CommandAuthority::HallMember);
        assert_eq!(routed.message_type, "blue");
        assert_eq!(routed.role, Some(IdentityRole::Admin));

        let friend = CommandEnvelope::new(
            "好友：@删除",
            "好友",
            "blue",
            "@删除",
            CommandObservation::default(),
        )
        .unwrap();
        let routed = router.route(&friend, None).unwrap();
        assert_eq!(routed.authority, CommandAuthority::HallMember);
        assert_eq!(routed.message_type, "blue");
        assert_eq!(routed.role, Some(IdentityRole::Friend));
        assert_eq!(routed.permission_required, None);

        for command in ["@大厅检测", "@大厅时间", "#斗地主", "#接龙画蛇添足"] {
            let hall = CommandEnvelope::new(
                format!("好友：{command}"),
                "好友",
                "blue",
                command,
                CommandObservation::default(),
            )
            .unwrap();
            let routed = router
                .route(&hall, None)
                .unwrap_or_else(|| panic!("映射好友大厅命令未路由: {command}"));
            assert_eq!(
                routed.authority,
                CommandAuthority::HallMember,
                "command={command}"
            );
            assert_eq!(routed.message_type, "blue", "command={command}");
            assert_eq!(routed.role, Some(IdentityRole::Friend), "command={command}");
            if command == "#接龙画蛇添足" {
                assert_eq!(
                    routed.command,
                    ModuleCommand::IdiomChain(IdiomChainCommand::Start {
                        idiom: "画蛇添足".to_string(),
                        mode: IdiomChainMode::Exact,
                    })
                );
            }
        }
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
                    commands: vec![
                        "确认".to_string(),
                        "选择".to_string(),
                        "本地".to_string(),
                        "邀请确认".to_string(),
                        "邀请拒绝".to_string(),
                        "确认邀请".to_string(),
                        "拒绝邀请".to_string(),
                        "同意邀请".to_string(),
                        "同意".to_string(),
                        "不同意".to_string(),
                    ],
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
        let router = ChatCommandRouter::new(&service);
        for message_type in ["blue", "pink"] {
            for command in [
                "@确认",
                "@选择",
                "@本地",
                "@邀请确认",
                "@邀请拒绝",
                "@确认邀请",
                "@拒绝邀请",
                "@同意邀请",
                "@同意",
                "@不同意",
            ] {
                let envelope = envelope(message_type, command);
                assert!(service.claims_chat(&envelope));
                assert!(
                    router.route(&envelope, None).is_none(),
                    "reserved decision reached custom workflow: {message_type} {command}"
                );
            }
        }
    }

    #[test]
    fn unclaimed_command_reason_separates_decisions_sessions_and_unknown() {
        assert_eq!(
            ChatCommandRouter::unclaimed_command_reason(&envelope("blue", "@确认"), None),
            "保留的决策语法（由独占决策窗口消费，正常分发排除）"
        );
        for text in ["@选择", "@本地"] {
            assert_eq!(
                ChatCommandRouter::unclaimed_command_reason(&envelope("blue", text), None),
                "保留的决策语法（由独占决策窗口消费，正常分发排除）",
                "{text}"
            );
        }
        // #提示 是成语接龙的进行中操作；没有进行中的接龙 → 会话不匹配。
        assert_eq!(
            ChatCommandRouter::unclaimed_command_reason(&envelope("blue", "#提示"), None),
            "娱乐 # 命令与当前会话不匹配（没有进行中的对应玩法）"
        );
        assert_eq!(
            ChatCommandRouter::unclaimed_command_reason(
                &envelope("blue", "#提示"),
                Some(EntertainmentKind::IdiomChain)
            ),
            "信封可解析但没有模块认领"
        );
        // 大厅 # 命令在没有进行中玩法时都会落到“会话不匹配”；
        // 真正的“没有模块认领”用 @ 命令举例。
        assert_eq!(
            ChatCommandRouter::unclaimed_command_reason(&envelope("blue", "@不存在xyz"), None),
            "信封可解析但没有模块认领"
        );
    }
}
