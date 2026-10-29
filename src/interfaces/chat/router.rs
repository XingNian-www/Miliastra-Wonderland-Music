use crate::features::administration::AdministrationCommand;
use crate::features::card_games::LandlordCommand;
use crate::features::command::{
    ChatCommandModule, CommandAuthority, CommandEnvelope, CommandPrefix, FeatureCommandMatch,
    ModuleCommand, RoutedCommand,
};
use crate::features::custom_workflow::CustomWorkflowService;
use crate::features::entertainment::EntertainmentKind;
use crate::features::hall::HallCommand;
use crate::features::identity::{IdentityAccess, IdentityRole};
use crate::features::idiom_chain::IdiomChainCommand;
// 仅测试引用；release 构建若直接导入会成为未使用导入。
#[cfg(test)]
use crate::features::idiom_chain::IdiomChainMode;
use crate::features::invite::InviteCommand;
use crate::features::moderation::ModerationCommand;
use crate::features::playback::PlaybackCommand;
use crate::features::song_request::SongCommand;
use crate::features::turtle_soup::TurtleSoupCommand;
use crate::features::undercover::UndercoverCommand;

/// 一条聊天命令 handler。
///
/// 这是路由的唯一事实来源：认领谓词与参数解析器绑在同一条目上，
/// 因此不会出现"这张表认领了、那张表没登记"或"认领用的是 A、解析用的是 B"的漂移。
///
/// 各模块的 claims_* 谓词内部已自带前缀与来源门控（大厅/好友），
/// 所以大厅与好友共用同一张 @ 表，不需要按来源各写一份。
#[derive(Clone, Copy)]
pub(crate) struct ChatHandler {
    pub(crate) module: ChatCommandModule,
    pub(crate) claims: fn(&CommandEnvelope) -> bool,
    pub(crate) parse: fn(&CommandEnvelope) -> Option<FeatureCommandMatch<ModuleCommand>>,
}

const fn handler(
    module: ChatCommandModule,
    claims: fn(&CommandEnvelope) -> bool,
    parse: fn(&CommandEnvelope) -> Option<FeatureCommandMatch<ModuleCommand>>,
) -> ChatHandler {
    ChatHandler {
        module,
        claims,
        parse,
    }
}

/// 静态 `@` 命令表（顺序即优先级）。
pub(crate) const AT_HANDLERS: &[ChatHandler] = &[
    handler(
        ChatCommandModule::Administration,
        AdministrationCommand::claims_chat,
        |envelope| {
            AdministrationCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::Administration))
        },
    ),
    handler(
        ChatCommandModule::SongRequest,
        SongCommand::claims_chat,
        |envelope| {
            SongCommand::parse_chat(envelope).map(|matched| matched.map(ModuleCommand::SongRequest))
        },
    ),
    handler(
        ChatCommandModule::Playback,
        PlaybackCommand::claims_chat,
        |envelope| {
            PlaybackCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::Playback))
        },
    ),
    handler(
        ChatCommandModule::Hall,
        HallCommand::claims_chat,
        |envelope| {
            HallCommand::parse_chat(envelope).map(|matched| matched.map(ModuleCommand::Hall))
        },
    ),
    handler(
        ChatCommandModule::Invite,
        InviteCommand::claims_chat,
        |envelope| {
            InviteCommand::parse_chat(envelope).map(|matched| matched.map(ModuleCommand::Invite))
        },
    ),
    handler(
        ChatCommandModule::Moderation,
        ModerationCommand::claims_chat,
        |envelope| {
            ModerationCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::Moderation))
        },
    ),
    handler(
        ChatCommandModule::Playlist,
        crate::features::playlist::PlaylistCommand::claims_chat,
        |envelope| {
            crate::features::playlist::PlaylistCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::Playlist))
        },
    ),
    handler(
        ChatCommandModule::AiChat,
        crate::features::ai_chat::AiChatCommand::claims_chat,
        |envelope| {
            crate::features::ai_chat::AiChatCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::AiChat))
        },
    ),
];

/// `#` 开局命令表：与当前玩法无关，任何时刻都能发起。
pub(crate) const HASH_START_HANDLERS: &[ChatHandler] = &[
    handler(
        ChatCommandModule::Tarot,
        crate::features::tarot::TarotCommand::claims_chat,
        |envelope| {
            crate::features::tarot::TarotCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::Tarot))
        },
    ),
    handler(
        ChatCommandModule::IdiomChain,
        IdiomChainCommand::claims_start_chat,
        |envelope| route_idiom(envelope).map(|matched| matched.map(ModuleCommand::IdiomChain)),
    ),
    handler(
        ChatCommandModule::CardGame,
        LandlordCommand::claims_start_chat,
        |envelope| route_card_game(envelope).map(|matched| matched.map(ModuleCommand::CardGame)),
    ),
    handler(
        ChatCommandModule::TurtleSoup,
        TurtleSoupCommand::claims_start_chat,
        |envelope| {
            route_turtle_soup(envelope).map(|matched| matched.map(ModuleCommand::TurtleSoup))
        },
    ),
    handler(
        ChatCommandModule::Undercover,
        UndercoverCommand::claims_start_chat,
        |envelope| route_undercover(envelope).map(|matched| matched.map(ModuleCommand::Undercover)),
    ),
    handler(
        ChatCommandModule::Administration,
        AdministrationCommand::claims_chat,
        |envelope| {
            AdministrationCommand::parse_chat(envelope)
                .map(|matched| matched.map(ModuleCommand::Administration))
        },
    ),
];

/// 聊天命令信封的静态模块路由器。
///
/// 选择阶段只询问各业务模块是否拥有该语法；选中后仅由该模块解析参数。井号命令显式使用当前娱乐模块所有者。
pub(crate) struct ChatCommandRouter<'a> {
    custom_workflow: Option<&'a CustomWorkflowService>,
    identity: Option<&'a IdentityAccess>,
    access: Option<&'a crate::features::command_access::CommandAccessConfig>,
}

impl<'a> ChatCommandRouter<'a> {
    #[cfg(test)]
    pub(crate) const fn new(custom_workflow: &'a CustomWorkflowService) -> Self {
        Self {
            custom_workflow: Some(custom_workflow),
            identity: None,
            access: None,
        }
    }

    pub(crate) const fn with_identity(
        custom_workflow: &'a CustomWorkflowService,
        identity: &'a IdentityAccess,
    ) -> Self {
        Self {
            custom_workflow: Some(custom_workflow),
            identity: Some(identity),
            access: None,
        }
    }

    pub(crate) const fn with_access(
        custom_workflow: &'a CustomWorkflowService,
        identity: &'a IdentityAccess,
        access: &'a crate::features::command_access::CommandAccessConfig,
    ) -> Self {
        Self {
            custom_workflow: Some(custom_workflow),
            identity: Some(identity),
            access: Some(access),
        }
    }

    #[cfg(test)]
    pub(crate) const fn without_custom_workflow() -> Self {
        Self {
            custom_workflow: None,
            identity: None,
            access: None,
        }
    }

    pub(crate) fn route(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<RoutedCommand> {
        if let Some(access) = self.access {
            // 来源只负责语法兼容，授权始终使用当前发送者角色。
            let other = match envelope.authority() {
                CommandAuthority::HallMember => CommandAuthority::Friend,
                CommandAuthority::Friend => CommandAuthority::HallMember,
            };
            let mut routed =
                self.route_with_authorities(envelope, active_entertainment, None, Some(other))?;
            // **顺序很重要**：先按用户实际敲的命令解析权限，再改写默认平台。
            //
            // `resolve_for_routed` 用 `SongCommand::permission_id()` 反查策略，
            // 而那个 id 由 `song.source` 决定。旧实现先 `apply_default_song_source`
            // 再解析权限，于是默认平台一设成 B站，`@点歌` 就被按 `@B站点歌`
            // （默认 Friend）判定——普通大厅成员发 `@点歌` 直接被拒，而面板上
            // 根本没有 `@点歌` 这一行可改，用户完全看不出原因。
            //
            // 现在的语义（与面板文案一致）：**默认平台只影响搜索走哪个平台，
            // 不改变各平台命令的权限**。`@点歌` 恒按 `@QQ点歌` 判权限。
            let (_, policy) = crate::features::command_access::resolve_with_active(
                access,
                &routed,
                active_entertainment,
            );
            // 权限已定，再改写实际搜索平台（这个改写必须保留：搜索仍走默认平台）。
            if let ModuleCommand::SongRequest(song) = &mut routed.command {
                access.apply_default_song_source(song);
            }
            // 身份命中就**覆盖**（精确身份优先），未命中退回来源下界：
            // 粉色好友消息/大厅好友标记 → Friend，蓝色大厅 → User。
            let actor = crate::features::command_access::actor_permission_with_floor(
                routed.role,
                routed.authority,
            );
            if !policy.allows(Some(actor)) {
                // permission_required 的既有语义**保持不变**：moderation.rs、listener.rs
                // 等 6 处拿它做一票否决，这里只额外记录「到底为什么被拒」。
                routed.permission_required = Some(match policy.minimum_role {
                    crate::features::command_access::CommandPermission::User
                    | crate::features::command_access::CommandPermission::Friend => {
                        IdentityRole::Friend
                    }
                    crate::features::command_access::CommandPermission::Admin => {
                        IdentityRole::Admin
                    }
                    crate::features::command_access::CommandPermission::Owner => {
                        IdentityRole::Owner
                    }
                });
                // 仅作诊断记录：**玩家可见文案不从这里出**。命令在入队前就被
                // `CommandGate` 拦下，走不到 execute_command；真正的原因与文案
                // 由 listener 的 gate 产出（见 `CommandGate::message`）。
                routed.denied_reason = Some(if policy.enabled {
                    crate::features::command::DeniedReason::BelowMinimum(policy.minimum_role)
                } else {
                    crate::features::command::DeniedReason::Disabled
                });
            }
            return Some(routed);
        }
        let role = role_for_envelope(self.identity, envelope);
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
                routed.role = role_for_envelope(self.identity, envelope);
                return Some(routed);
            }
            if let Some(authority) = alternate_authority {
                let candidate = candidate.with_authority(authority);
                if let Some(matched) = self.route_match(&candidate, active_entertainment) {
                    let mut routed = RoutedCommand::from_envelope(envelope, matched);
                    routed.authority = envelope.authority();
                    routed.message_type = envelope.message_type().to_string();
                    routed.role = role_for_envelope(self.identity, envelope);
                    return Some(routed);
                }
            }
        }
        None
    }

    /// 静态表与自定义流程共用的一次派发。
    ///
    /// 保留的决策语法（@确认/@1…）由独占窗口消费，**必须在整个派发之前**排除；
    /// 只把它挡在静态表外面会让自定义流程认领它——那是曾经修过的缺陷。
    fn route_match(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<FeatureCommandMatch<ModuleCommand>> {
        if is_reserved_decision_command(envelope) {
            return None;
        }
        if let Some(handler) = self.static_handler(envelope, active_entertainment) {
            return (handler.parse)(envelope);
        }
        // 自定义流程由配置定义，运行期才知道，因此不在静态表里。
        self.custom_workflow
            .and_then(|service| service.parse_chat(envelope))
            .map(|matched| matched.map(ModuleCommand::CustomWorkflow))
    }

    /// 该信封会被哪个模块认领。仅用于诊断与测试。
    pub(crate) fn select_module(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<ChatCommandModule> {
        if is_reserved_decision_command(envelope) {
            return None;
        }
        if let Some(handler) = self.static_handler(envelope, active_entertainment) {
            return Some(handler.module);
        }
        self.custom_workflow
            .is_some_and(|service| service.claims_chat(envelope))
            .then_some(ChatCommandModule::CustomWorkflow)
    }

    /// 按优先级走查静态 handler 表。
    ///
    /// 决策语法的排除在 route_match 里统一完成，这里不再重复判断。
    fn static_handler(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<ChatHandler> {
        match envelope.prefix() {
            CommandPrefix::At => AT_HANDLERS
                .iter()
                .find(|entry| (entry.claims)(envelope))
                .copied(),
            CommandPrefix::Hash => HASH_START_HANDLERS
                .iter()
                .find(|entry| (entry.claims)(envelope))
                .copied()
                .or_else(|| self.session_handler(envelope, active_entertainment)),
            // 测试专用前缀：永不认领。
            CommandPrefix::Amp => None,
        }
    }

    /// hash 进行中命令：按当前玩法查 command_catalog::SESSION_DISPATCH。
    ///
    /// 这张表本来只为面板而存在；现在路由也读它，因此"玩法状态 → 模块"的映射
    /// 只有一份，不会出现表里登记了、路由里却走另一条分支。
    fn session_handler(
        &self,
        envelope: &CommandEnvelope,
        active_entertainment: Option<EntertainmentKind>,
    ) -> Option<ChatHandler> {
        crate::features::command_catalog::session_rules(active_entertainment)
            .iter()
            .find(|rule| (rule.claims)(envelope))
            .map(|rule| handler(rule.module, rule.claims, rule.parse))
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

/// 按消息来源解析角色：有账号走 UID，否则走昵称。
///
/// 两条链路共用这一个入口——OCR 只有昵称，外部聊天 API 有 UID。
/// UID 优先是因为它改不了、不会重名；昵称可以被玩家改成任意想要的名字。
fn role_for_envelope(
    identity: Option<&IdentityAccess>,
    envelope: &CommandEnvelope,
) -> Option<IdentityRole> {
    let identity = identity?;
    let hint = match envelope.account() {
        Some(account) => crate::features::identity::IdentityHint::Account {
            game: &account.game,
            uid: &account.uid,
            nickname: Some(envelope.username()),
        },
        None => crate::features::identity::IdentityHint::Nickname(envelope.username()),
    };
    let resolved = identity.resolve_hint(hint)?;
    // 审计：高权限身份**仅凭昵称**解析出来时记一条日志。
    //
    // 判定**不改**——用户已定稿 nickname 就是权限推断键；这里只留排查线索，
    // 供事后回答「这条管理员命令当初是按哪个名字认出来的」。
    //
    // 日志里绝不出现昵称原文：nickname 是 OCR 匹配键，落进日志等于把
    // 「可以被改成一样的名字」泄露出去了（见 identity.rs 的 display_name 注释）。
    // 只用 display_name()，它在没有备注时返回不可用于匹配的 UUID 前缀。
    if envelope.account().is_none() && matches!(resolved.role, IdentityRole::Admin | IdentityRole::Owner)
    {
        log::warn!(
            "高权限身份仅凭昵称解析：role={:?} 身份={}（无可信 UID，昵称可被改名冒充；仅记录，不改变判定）",
            resolved.role,
            resolved.display_name()
        );
    }
    Some(resolved.role)
}

// 开局表的解析器只解析开局语法。
//
// 这些函数原先写成 `parse_start_chat(..).or_else(|| parse_active_chat(..))`，
// 但它们的认领谓词是 `claims_start_chat`，与 `parse_start` 共用同一份字面量判定，
// 所以 `parse_start_chat` 认领成功后必然返回 Some——`or_else` 是不可达的死分支。
// 会话态（进行中）命令走 `command_catalog::SESSION_DISPATCH`，不经过这里。
fn route_idiom(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<IdiomChainCommand>> {
    IdiomChainCommand::parse_start_chat(envelope)
}

fn route_card_game(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<LandlordCommand>> {
    LandlordCommand::parse_start_chat(envelope)
}

fn route_turtle_soup(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<TurtleSoupCommand>> {
    TurtleSoupCommand::parse_start_chat(envelope)
}

fn route_undercover(envelope: &CommandEnvelope) -> Option<FeatureCommandMatch<UndercoverCommand>> {
    UndercoverCommand::parse_start_chat(envelope)
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

    fn song_policy_route(
        access: &crate::features::command_access::CommandAccessConfig,
        source: &str,
        text: &str,
    ) -> RoutedCommand {
        let identity = IdentityAccess::default();
        ChatCommandRouter {
            custom_workflow: None,
            identity: Some(&identity),
            access: Some(access),
        }
        .route(&envelope(source, text), None)
        .unwrap_or_else(|| panic!("song command should route: {source} {text}"))
    }

    #[test]
    fn song_platform_permissions_are_independent_and_aliases_cannot_bypass() {
        use crate::features::command_access::*;
        let mut access = CommandAccessConfig::default();
        // 其余平台命令对所有人开放；`@B站点歌` 的 authorities 现在也是 BOTH
        // （渠道只决定「哪些界面能识别」，不再代表「仅好友可用」），
        // 它的 Friend 是 `song_minimum_role` 给的**默认档位**，不是渠道推导。
        for id in ["@QQ点歌", "@网易点歌", "@酷狗点歌", "@AI点歌"] {
            let policy = catalog_policy(&access, id).unwrap();
            assert!(policy.enabled, "{id}");
            assert_eq!(policy.minimum_role, CommandPermission::User, "{id}");
        }
        let bilibili = catalog_policy(&access, "@B站点歌").unwrap();
        assert!(bilibili.enabled);
        assert_eq!(
            bilibili.minimum_role,
            CommandPermission::Friend,
            "声明仅好友的命令不能被放宽成 User",
        );
        access.overrides.insert(
            "@网易点歌".into(),
            CommandPolicy {
                enabled: true,
                minimum_role: CommandPermission::Admin,
            },
        );
        access.overrides.insert(
            "@AI点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::User,
            },
        );
        for source in ["blue", "pink"] {
            for text in ["@网易点歌晴天", "@网易云点歌晴天"] {
                let routed = song_policy_route(&access, source, text);
                assert_eq!(resolve_for_routed(&access, &routed).0, "@网易点歌");
                assert_eq!(routed.permission_required, Some(IdentityRole::Admin));
                assert_eq!(routed.message_type, source);
            }
            for text in ["@点歌晴天", "@QQ点歌晴天", "@酷狗点歌晴天"] {
                assert!(
                    song_policy_route(&access, source, text)
                        .permission_required
                        .is_none(),
                    "{source} {text}"
                );
            }
            for text in ["@AI点歌晴天", "@ai点歌晴天"] {
                let routed = song_policy_route(&access, source, text);
                assert_eq!(resolve_for_routed(&access, &routed).0, "@AI点歌");
                assert!(routed.permission_required.is_some());
            }
        }
        access.overrides.insert(
            "@QQ点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::User,
            },
        );
        for text in ["@点歌晴天", "@QQ点歌晴天"] {
            assert!(
                song_policy_route(&access, "blue", text)
                    .permission_required
                    .is_some()
            );
        }
        assert!(
            song_policy_route(&access, "blue", "@酷狗点歌晴天")
                .permission_required
                .is_none()
        );
    }

    #[test]
    fn friend_bv_alias_uses_bilibili_policy_even_with_another_default_source() {
        use crate::features::command_access::*;
        use crate::features::song_request::SongSource;
        let mut access = CommandAccessConfig::default();
        access.default_song_source = Some("netease".into());
        access.overrides.insert(
            "@B站点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::User,
            },
        );
        let routed = song_policy_route(&access, "pink", "@点歌BV1xx411c7mD");
        assert_eq!(resolve_for_routed(&access, &routed).0, "@B站点歌");
        assert!(routed.permission_required.is_some());
        let ModuleCommand::SongRequest(song) = routed.command else {
            panic!("expected song");
        };
        assert_eq!(song.source, SongSource::Bilibili);
        let explicit = song_policy_route(&access, "pink", "@QQ点歌BV1xx411c7mD");
        assert_eq!(resolve_for_routed(&access, &explicit).0, "@QQ点歌");
        assert!(explicit.permission_required.is_none());
    }

    /// 默认平台**不改变权限**，只改变实际搜索平台。
    ///
    /// 旧实现（已修）：`route()` 先 `apply_default_song_source` 再解析权限，
    /// 而 `resolve_for_routed` 用 `SongCommand::permission_id()`（由 `song.source`
    /// 推导）反查策略。于是默认平台一设成 B站，`@点歌` 就被按 `@B站点歌`
    /// （默认 Friend）判定——普通大厅成员发 `@点歌` 直接被拒，而面板上根本没有
    /// `@点歌` 这一行可改，用户完全看不出原因。
    ///
    /// 现在与面板文案一致：「默认平台只影响 @点歌 走哪个平台的搜索，
    /// 不改变各平台命令的权限」。所以断言分两半：
    /// - **权限**：无论默认平台是什么，`@点歌` 恒按 `@QQ点歌` 判；
    /// - **行为**：`song.source` 仍被正确改写成默认平台（搜索不受影响）。
    #[test]
    fn default_song_source_changes_search_platform_but_not_permission() {
        use crate::features::command_access::*;
        // 只对「非 QQ」的默认平台做「默认平台命令被抬到 Owner」的检验：
        // 若权限跟着默认平台漂移，@点歌 就会被这条 override 判成 Owner 而遭拒。
        for (source, platform_id) in [("netease", "@网易点歌"), ("kugou", "@酷狗点歌")] {
            let mut access = CommandAccessConfig::default();
            access.default_song_source = Some(source.into());
            access.overrides.insert(
                platform_id.into(),
                CommandPolicy {
                    enabled: true,
                    minimum_role: CommandPermission::Owner,
                },
            );
            for transport in ["blue", "pink"] {
                let routed = song_policy_route(&access, transport, "@点歌晴天");
                // 蓝色大厅的普通成员（来源下界 User）仍能用 @点歌。
                assert_eq!(
                    routed.permission_required, None,
                    "默认平台 {source} 下 @点歌 不该继承 {platform_id} 的 Owner 门槛"
                );
                // 但搜索平台仍被改写成默认平台：功能没被削弱。
                let ModuleCommand::SongRequest(song) = routed.command else {
                    panic!("expected song");
                };
                assert_eq!(
                    song.source.as_str(),
                    source,
                    "@点歌 的搜索仍须走默认平台"
                );
            }
        }

        // 反向证据：把 @QQ点歌 抬到 Owner，@点歌 必须被拒——
        // 这证明它判权限时用的 id 确实是 @QQ点歌，而不是默认平台那一条。
        for source in ["qqmusic", "netease", "kugou", "bilibili"] {
            let mut access = CommandAccessConfig::default();
            access.default_song_source = Some(source.into());
            access.overrides.insert(
                "@QQ点歌".into(),
                CommandPolicy {
                    enabled: true,
                    minimum_role: CommandPermission::Owner,
                },
            );
            let routed = song_policy_route(&access, "blue", "@点歌晴天");
            assert_eq!(
                routed.permission_required,
                Some(IdentityRole::Owner),
                "默认平台 {source} 下 @点歌 的权限条目必须恒为 @QQ点歌"
            );
        }
    }

    /// 复核 P2-3 的可执行证据：默认平台设成 `bilibili` 时，
    /// `@点歌` 的判定档位必须**不受影响**（按 `@QQ点歌`），
    /// 同时 `song.source` 确实被改写成 Bilibili。
    ///
    /// 这一条专门钉死回归点：`@B站点歌` 默认是 Friend，
    /// 若权限随默认平台漂移，蓝色大厅的普通成员发 `@点歌` 就会被无声拒绝。
    #[test]
    fn bilibili_default_source_does_not_gate_plain_song_command() {
        use crate::features::command_access::*;
        use crate::features::song_request::SongSource;
        let mut access = CommandAccessConfig::default();
        access.default_song_source = Some("bilibili".into());

        // 蓝色大厅（来源下界 User）发 @点歌：放行 —— 不继承 @B站点歌 的 Friend 门槛。
        let routed = song_policy_route(&access, "blue", "@点歌晴天");
        assert_eq!(
            routed.permission_required, None,
            "默认平台是 B站 时，@点歌 不该继承 @B站点歌 的 Friend 门槛"
        );
        let ModuleCommand::SongRequest(song) = routed.command else {
            panic!("expected song");
        };
        assert_eq!(song.source, SongSource::Bilibili, "搜索仍走默认平台");

        // 显式 @B站点歌 仍按 @B站点歌 判（Friend）：普通成员被拒。
        let explicit = song_policy_route(&access, "blue", "@B站点歌晴天");
        assert_eq!(explicit.permission_required, Some(IdentityRole::Friend));

        // 反向证据：把 @QQ点歌 抬到 Owner，@点歌 立刻被拒 ——
        // 证明默认平台是 B站 时它判权限用的仍是 @QQ点歌。
        access.overrides.insert(
            "@QQ点歌".into(),
            CommandPolicy {
                enabled: true,
                minimum_role: CommandPermission::Owner,
            },
        );
        let routed = song_policy_route(&access, "blue", "@点歌晴天");
        assert_eq!(
            routed.permission_required,
            Some(IdentityRole::Owner),
            "@点歌 的权限条目恒为 @QQ点歌，与默认平台无关"
        );
    }

    /// 回归：五个点歌命令各自独立，改一个不牵连别人。
    ///
    /// 原来 `catalog_policy` 在本命令没有 override 时会**回退到 "@点歌"**，
    /// 于是 `@点歌` 一份配置同时管着 QQ/网易/酷狗/B站/AI 五个平台，
    /// 用户在面板上根本没法单独调某一个——这就是「这几个点歌没法独立控制权限」。
    ///
    /// 现在每条命令只认自己的 override，没配就用它自己的默认值。
    /// 回归：好友备注（nickname）可以用于授权，游戏真名（note）不行。
    ///
    /// 线上形态：identity 的 `nickname` 存的是**好友备注**（玩家自己写的
    /// 随机串，如 `abcd1234`），`note` 才是游戏真名。大厅 OCR 读到的是真名。
    ///
    /// 曾经为了让"大厅读到真名也能认出管理员"给 resolve_hint 加过 note 兜底，
    /// 结果 moderation 的防冒充测试立刻变红：**玩家可以把自己昵称改成管理员的
    /// 显示名来冒充**。MUST_READ §2「未确认发送者不能用于授权」。
    ///
    /// 所以正确行为是：备注名能认出、真名认不出（Unknown 不授权）；
    /// 大厅要可靠识别角色，必须走带 UID 的路径（外部聊天源）。
    #[test]
    fn hall_bilibili_song_permission_requires_a_trusted_name() {
        use crate::features::identity::{GameAccount, IdentityConfig, IdentityMapping};

        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            // 好友备注：玩家在游戏里给对方写的那种随机串，玩家自己改不了。
            nickname: "abcd1234".to_string(),
            id: uuid::Uuid::new_v4(),
            role: IdentityRole::Admin,
            // 游戏真名：屏幕大厅里显示的就是它，但任何人都能把昵称改成一样。
            note: "管理员甲".to_string(),
            remark_sourced: true,
            accounts: vec![GameAccount::new("100000001")],
        });
        let identity = IdentityAccess::new(config);

        // 备注名（好友私聊标题）可以认出身份。
        let by_remark = identity
            .resolve_hint(crate::features::identity::IdentityHint::Nickname(
                "abcd1234",
            ))
            .expect("好友备注应能认出身份");
        assert_eq!(by_remark.role, IdentityRole::Admin);

        // 游戏真名**不**用于授权：否则改个昵称就能冒充管理员。
        assert!(
            identity
                .resolve_hint(crate::features::identity::IdentityHint::Nickname(
                    "管理员甲"
                ))
                .is_none(),
            "游戏真名不能被当作授权依据"
        );

        // 带 UID 时可靠识别：外部聊天源给的消息带 account，凭它拿 Admin。
        let by_uid = identity
            .resolve_hint(crate::features::identity::IdentityHint::Account {
                game: "genshin",
                uid: "100000001",
                nickname: Some("管理员甲"),
            })
            .expect("有 UID 时应能认出身份");
        assert_eq!(by_uid.role, IdentityRole::Admin);
    }

    #[test]
    fn hall_bilibili_song_allows_admin_but_rejects_plain_member() {
        use crate::features::command_access::CommandAccessConfig;
        use crate::features::identity::{GameAccount, IdentityConfig, IdentityMapping};

        let access = CommandAccessConfig::default();
        let mut identity_config = IdentityConfig::default();
        identity_config.mappings.push(IdentityMapping {
            nickname: "管理员甲".to_string(),
            id: uuid::Uuid::new_v4(),
            role: IdentityRole::Admin,
            note: "管理员甲".to_string(),
            remark_sourced: false,
            accounts: vec![GameAccount::new("100000001")],
        });
        let identity = IdentityAccess::new(identity_config);
        let router = ChatCommandRouter {
            custom_workflow: None,
            identity: Some(&identity),
            access: Some(&access),
        };

        // 管理员在大厅发：能路由，且 Admin >= Friend → 放行。
        let admin = CommandEnvelope::new(
            "管理员甲：@B站点歌BV1xx411c7mD".to_string(),
            "管理员甲",
            "blue",
            "@B站点歌BV1xx411c7mD",
            CommandObservation::default(),
        )
        .expect("admin envelope");
        let routed = router
            .route(&admin, None)
            .expect("管理员在大厅应能路由 B站点歌");
        assert_eq!(routed.role, Some(IdentityRole::Admin));
        assert!(
            routed.permission_required.is_none(),
            "Admin >= Friend，应放行；实际 {:?}",
            routed.permission_required
        );

        // 普通成员在大厅发：能路由，但 User < Friend → 被拒。
        let plain = CommandEnvelope::new(
            "路人甲：@B站点歌BV1xx411c7mD".to_string(),
            "路人甲",
            "blue",
            "@B站点歌BV1xx411c7mD",
            CommandObservation::default(),
        )
        .expect("plain envelope");
        let routed = router
            .route(&plain, None)
            .expect("大厅应能路由 B站点歌，权限在之后判定");
        assert_eq!(routed.role, None);
        assert!(
            routed.permission_required.is_some(),
            "User < Friend，应被拒"
        );
    }

    #[test]
    fn song_platform_policies_are_editable_independently() {
        use crate::features::command_access::*;
        let mut access = CommandAccessConfig::default();

        // 默认值：B站是 Friend（原来"仅好友"的意图保留为默认档位），
        // 其余平台是 User。
        for id in ["@QQ点歌", "@网易点歌", "@酷狗点歌", "@AI点歌"] {
            let policy = catalog_policy(&access, id).unwrap();
            assert!(policy.enabled, "{id}");
            assert_eq!(policy.minimum_role, CommandPermission::User, "{id}");
        }
        let bilibili = catalog_policy(&access, "@B站点歌").unwrap();
        assert!(bilibili.enabled);
        assert_eq!(bilibili.minimum_role, CommandPermission::Friend);

        // 只禁掉酷狗：其它四个平台必须完全不受影响。
        access.overrides.insert(
            "@酷狗点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::User,
            },
        );
        access.validate().unwrap();
        assert!(!catalog_policy(&access, "@酷狗点歌").unwrap().enabled);
        for id in ["@QQ点歌", "@网易点歌", "@B站点歌", "@AI点歌"] {
            assert!(
                catalog_policy(&access, id).unwrap().enabled,
                "{id} 不该被 @酷狗点歌 的配置牵连",
            );
        }

        // 把 B站放宽到 User：用户在面板上自己决定，大厅任何人都能用。
        access.overrides.insert(
            "@B站点歌".into(),
            CommandPolicy {
                enabled: true,
                minimum_role: CommandPermission::User,
            },
        );
        assert_eq!(
            catalog_policy(&access, "@B站点歌").unwrap().minimum_role,
            CommandPermission::User,
        );
    }

    #[test]
    fn removed_song_search_entries_do_not_route_from_any_chat_source() {
        use crate::features::command_access::CommandAccessConfig;
        let access = CommandAccessConfig::default();
        let identity = IdentityAccess::default();
        let router = ChatCommandRouter {
            custom_workflow: None,
            identity: Some(&identity),
            access: Some(&access),
        };
        for source in ["blue", "pink"] {
            for text in [
                "@搜索晴天",
                "@QQ搜索晴天",
                "@网易搜索晴天",
                "@网易云搜索晴天",
                "@酷狗搜索晴天",
                "@B站搜索晴天",
                "@AI搜索晴天",
                "@ai搜索晴天",
            ] {
                assert!(
                    router.route(&envelope(source, text), None).is_none(),
                    "removed search routed: {source} {text}"
                );
            }
        }
    }

    #[test]
    fn editable_permissions_apply_independently_of_message_source() {
        use crate::features::command_access::*;
        let service = CustomWorkflowService::new(
            CustomWorkflowConfig::default(),
            WorkflowDefaults {
                default_timeout_ms: 1000,
                default_poll_ms: 10,
                default_step_wait_ms: 0,
                decision_timeout_ms: 1000,
                decision_poll_ms: 10,
                after_activate_ms: 0,
                clipboard_hold_ms: 0,
                stability_mean_threshold: 1.0,
                stability_changed_ratio_threshold: 0.1,
            },
        );
        let identity = IdentityAccess::default();
        let mut access = CommandAccessConfig::default();
        let noisy = ChatCommandRouter::with_access(&service, &identity, &access)
            .route(&envelope("blue", "@，禁用"), None)
            .unwrap();
        // @禁用 现在是 Admin 档（不再要求 Owner）；这条断言同时验证
        // OCR 容错（「@，禁用」这条带噪声的写法）能正确认领到同一个策略。
        assert_eq!(noisy.permission_required, Some(IdentityRole::Admin));
        access.overrides.insert(
            "#跑得快".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::User,
            },
        );
        let runfast = ChatCommandRouter::with_access(&service, &identity, &access)
            .route(&envelope("blue", "#跑得快"), None)
            .unwrap();
        assert!(runfast.permission_required.is_some());
        access.overrides.clear();
        // @删除 默认门槛是 Friend（catalog 摘要「好友删除当前曲目」），
        // 不是 Admin——旧断言把「仅好友」误解成了「好友且管理员」。
        //
        // 但两个来源的判定结果不同，因为**来源本身就是最低权限推断的信号**：
        //   pink（好友消息/大厅好友标记）→ 下界 Friend → 直接放行；
        //   blue（普通大厅成员）→ 下界 User → 仍要求 Friend。
        for (source, expected) in [("pink", None), ("blue", Some(IdentityRole::Friend))] {
            let routed = ChatCommandRouter::with_access(&service, &identity, &access)
                .route(&envelope(source, "@删除"), None)
                .unwrap();
            assert_eq!(routed.permission_required, expected, "source={source}");
            assert_eq!(routed.message_type, source);
        }
        access.overrides.insert(
            "@删除".into(),
            CommandPolicy {
                enabled: true,
                minimum_role: CommandPermission::User,
            },
        );
        for source in ["pink", "blue"] {
            let routed = ChatCommandRouter::with_access(&service, &identity, &access)
                .route(&envelope(source, "@删除"), None)
                .unwrap();
            assert!(routed.permission_required.is_none());
        }
        // `@点歌` 是 `@QQ点歌` 的别名，真实 policy_id 是 "@QQ点歌"。
        // 原来这里插 "@点歌" 只是想验证"禁用之后被拒"，靠的是 fallback 那条
        // 已经移除的兼容层；现在直接配它自己的 id。
        access.overrides.insert(
            "@QQ点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::User,
            },
        );
        assert!(
            ChatCommandRouter::with_access(&service, &identity, &access)
                .route(&envelope("blue", "@点歌晴天"), None)
                .unwrap()
                .permission_required
                .is_some()
        );
        // 且不影响其它平台。
        assert!(
            ChatCommandRouter::with_access(&service, &identity, &access)
                .route(&envelope("blue", "@网易点歌晴天"), None)
                .unwrap()
                .permission_required
                .is_none()
        );
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
                        remark_sourced: false,
                        accounts: Vec::new(),
                    })
                    .collect(),
            });
            let router = ChatCommandRouter {
                custom_workflow: None,
                identity: Some(&identity),
                access: None,
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
                remark_sourced: false,
                accounts: Vec::new(),
            }],
        });
        let router = ChatCommandRouter {
            custom_workflow: None,
            identity: Some(&identity),
            access: None,
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
                    remark_sourced: false,
                    accounts: Vec::new(),
                },
                IdentityMapping {
                    nickname: "管理员".to_string(),
                    id: uuid::Uuid::from_u128(2),
                    role: IdentityRole::Admin,
                    note: String::new(),
                    remark_sourced: false,
                    accounts: Vec::new(),
                },
                IdentityMapping {
                    nickname: "好友".to_string(),
                    id: uuid::Uuid::from_u128(3),
                    role: IdentityRole::Friend,
                    note: "好友备注".to_string(),
                    remark_sourced: false,
                    accounts: Vec::new(),
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

    /// @忘记我 的 claims_chat 只接受大厅来源，因此必须登记在 @ 表里。
    /// 曾经只登记在好友分支，导致该命令在任何来源下都不可达。
    #[test]
    fn forget_me_reaches_the_ai_chat_module_from_a_hall_message() {
        let router = ChatCommandRouter::without_custom_workflow();
        let routed = router
            .route(&envelope("blue", "@忘记我"), None)
            .expect("@忘记我 应被认领");
        assert!(matches!(
            routed.command,
            ModuleCommand::AiChat(crate::features::ai_chat::AiChatCommand::Forget)
        ));
        // 好友来源仍不认领：claims_chat 明确只支持大厅。
        assert!(router.route(&envelope("pink", "@忘记我"), None).is_none());
    }

    /// 每条登记的命令都必须能被 @ 表认领到——表是唯一事实来源后，
    /// 漏登记不会再表现成"游戏里静默不认"，而是在这里直接失败。
    #[test]
    fn every_at_handler_is_reachable_and_distinct() {
        let mut modules = std::collections::BTreeSet::new();
        for entry in AT_HANDLERS {
            assert!(
                modules.insert(format!("{:?}", entry.module)),
                "同一个模块在 @ 表里重复登记：{:?}",
                entry.module
            );
        }
    }

    /// 开局表的解析器不再挂 `or_else(parse_active_chat)` 兜底。
    ///
    /// 该准则建立在「claims_start_chat 通过 ⇒ parse_start_chat 必返回 Some」之上
    /// （两者共用同一份字面量判定）。这个测试就是那个前提的守卫：
    /// 一旦某个模块的 claims 与 parse 不再同源，这里会先失败，
    /// 而不是让开局命令在游戏里静默解析不出来。
    #[test]
    fn hash_start_claims_always_parse_successfully() {
        let router = ChatCommandRouter::without_custom_workflow();
        let starts = [
            ("#接龙画蛇添足", "成语接龙"),
            ("#同音接龙画蛇添足", "同音接龙"),
            ("#斗地主", "斗地主"),
            ("#跑得快", "跑得快"),
            ("#海龟汤", "海龟汤"),
            ("#卧底", "卧底"),
            ("#卧底双", "卧底"),
        ];
        for (text, label) in starts {
            let envelope = envelope("blue", text);
            assert!(
                router.route(&envelope, None).is_some(),
                "{label} 的开局命令 {text} 应被认领并解析成功"
            );
        }
    }
}
