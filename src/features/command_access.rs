//! 命令权限与消息来源独立；目录、聊天和 AI 共享同一策略。
use super::{
    command::{CommandAuthority, CommandPrefix, ModuleCommand, RoutedCommand},
    identity::IdentityRole,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandPermission {
    User,
    Friend,
    Admin,
    Owner,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandPolicy {
    pub enabled: bool,
    pub minimum_role: CommandPermission,
}
impl CommandPermission {
    /// 面板与错误提示里使用的档位名。
    ///
    /// 校验文案不要硬编码「主人权限」：除 `@值守模式` 外所有命令的下限都是
    /// User，硬编码会给出一条与真实下限不符的报错。
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::User => "普通成员",
            Self::Friend => "好友",
            Self::Admin => "管理员",
            Self::Owner => "主人",
        }
    }
}
impl CommandPolicy {
    pub(crate) fn allows(&self, actor: Option<CommandPermission>) -> bool {
        self.enabled && actor.is_some_and(|a| a >= self.minimum_role)
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandAccessConfig {
    pub overrides: BTreeMap<String, CommandPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_song_source: Option<String>,
}
/// 角色 → 权限档位。
///
/// 曾经有一个 `known_actor: bool` 参数，四个调用点都传
/// `!username.trim().is_empty()`；而用户名不可能为空（`CommandEnvelope::new`
/// 拒绝空用户名，控制台固定「控制台」，二级占位符固定「二级大厅」），
/// 那个守卫恒真，已删除。身份是否 Unknown 由调用方在上游判定。
pub(crate) fn actor_permission(role: Option<IdentityRole>) -> Option<CommandPermission> {
    Some(match role {
        None => CommandPermission::User,
        Some(IdentityRole::Friend) => CommandPermission::Friend,
        Some(IdentityRole::Admin) => CommandPermission::Admin,
        Some(IdentityRole::Owner) => CommandPermission::Owner,
    })
}

/// 消息来源能给出的**最低权限**。
///
/// 用户原话：大厅里好友发言带显式粉色标记、二级聊天里好友发言进左侧列表（小红点），
/// 这两个信号「用于最低权限推断」——即便身份没登记，也至少是好友。
/// 蓝色大厅（普通成员发言）只给 User。
pub(crate) fn authority_floor(authority: CommandAuthority) -> CommandPermission {
    match authority {
        CommandAuthority::Friend => CommandPermission::Friend,
        CommandAuthority::HallMember => CommandPermission::User,
    }
}

/// 判定用的权限档位：**身份命中时覆盖，未命中时退回来源的最低推断。**
///
/// 用户原话：「nickname 是用来推断权限的，如果读取到 nickname 就默认用这个的权限
/// （是覆盖而不是最低）」。
///
/// - 身份命中（UID 或 nickname 解析出 role）→ **直接采用**该 role 的档位，
///   不与来源下界取 min/max，也不"至少好友"地兜底——精确身份优先。
/// - 身份未命中 → 用来源下界：粉色好友消息 / 大厅好友标记 → Friend；蓝色大厅 → User。
///
/// 这样"大厅里未登记的好友"仍能用好友级命令（粉色标记兜底），
/// 而"登记成 Admin 的人"在大厅不会因为来源是 HallMember 被降级。
pub(crate) fn actor_permission_with_floor(
    role: Option<IdentityRole>,
    authority: CommandAuthority,
) -> CommandPermission {
    match role {
        // Some 分支里 actor_permission 恒返回 Some，兜底只为消除 Option。
        Some(_) => actor_permission(role).unwrap_or(CommandPermission::User),
        None => authority_floor(authority),
    }
}
fn prefix(p: CommandPrefix) -> char {
    match p {
        CommandPrefix::At => '@',
        CommandPrefix::Hash => '#',
        CommandPrefix::Amp => '&',
    }
}
/// 需要管理员及以上才能使用的命令。
///
/// 这个列表是**唯一**的 Admin 判定来源。以前靠 `authorities` 隐式推导
/// （FRIEND_ONLY → Admin），既与声明反向，又让大厅 Admin 能执行「仅好友」命令。
/// 现在只把真正有破坏性、且大厅里不该随便用的命令列在这里。
const ADMIN_COMMANDS: &[&str] = &[
    // 破坏性且无二次确认，或会持久化到游戏账号。
    "@队列清空",
    "@拉黑",
    "@屏蔽",
    "@邀请",
    // 改变 BOT 全局运行行为：关掉识别、设定闲置退出都会影响所有人。
    // 用户明确要求这三条按 Admin 处理（不必 Owner）。
    "@启用",
    "@禁用",
    "@闲置退出",
];

/// 不可再降低的权限下限。
///
/// 只有 `@值守模式` 保留 Owner 下限——它切换的是聊天值守目标界面，
/// 属于 BOT 的核心运行姿态。`@启用` / `@禁用` / `@闲置退出` 由
/// [ADMIN_COMMANDS] 按 Admin 处理，不再设 Owner 下限。
///
/// 其余命令下限是 User（枚举最低就是 User，等价于「不额外抬高」）。
pub(crate) fn minimum_floor(id: &str) -> CommandPermission {
    if id == "@值守模式" {
        CommandPermission::Owner
    } else {
        CommandPermission::User
    }
}

/// 已废弃的命令面板条目。
///
/// `@点歌` 曾经是唯一入口，现已拆成五个平台命令。运行期
/// `SongCommand::permission_id()` 只会返回平台 id，所以 `overrides["@点歌"]`
/// 既不会生效、也不会出现在面板里——留着只会让用户以为改它有用。
///
/// 加载链路按本清单丢弃（[CommandAccessConfig::sanitize_deprecated_overrides]），
/// 丢弃发生在 `validate()` **之前**，所以老配置不会加载失败。
pub(crate) const DEPRECATED_POLICY_IDS: &[&str] = &["@点歌"];

/// 点歌类命令的默认权限下限。
///
/// **不再从 `authorities` 推断。** 渠道（HallMember/Friend）现在只回答
/// 「这条命令在哪些界面能被识别」，不代表「仅好友可用」——原来靠它推断，
/// 于是 `@B站点歌` 在大厅连匹配都匹配不上，管理员也触发不了。
///
/// 权限的唯一判据是 `minimum_role` 的 `>=` 比较：设成 Friend，则
/// Friend/Admin/Owner 都能用，大厅与私聊都行；设成 User 就是所有人都能用。
/// 用户可以在面板逐条改，这才是「独立控制」。
///
/// B站音源默认仍是 Friend（它在好友私聊里最可靠），但只作为**默认值**，
/// 不再是一道绕不过去的渠道墙。
fn song_minimum_role(id: &str) -> CommandPermission {
    match id {
        "@B站点歌" => CommandPermission::Friend,
        _ => CommandPermission::User,
    }
}
/// 某个 id 的**默认**策略（不含用户在面板里的 override）。
///
/// 权限下限 [minimum_floor] 在这里**统一**套一次，不再分散在各分支里：
/// 以前只有末尾那个分支 `.max(minimum_floor(id))`，song 与 custom_workflow
/// 两个提前 return 的分支漏掉了。合并成单一出口后，新加分支不会再漏。
fn default_policy(id: &str) -> Option<CommandPolicy> {
    default_policy_ungated(id).map(|mut policy| {
        policy.minimum_role = policy.minimum_role.max(minimum_floor(id));
        policy
    })
}

/// [default_policy] 的分支部分：只推导默认档位，不套 [minimum_floor]。
fn default_policy_ungated(id: &str) -> Option<CommandPolicy> {
    // 注意不要再加回 @搜索：该命令已移除，command_access 之外也无人认识它，
    // 留着只会让 validate 接受一份指向不存在命令的配置。
    //
    // `@点歌` 同理（A1）：它已拆成五个平台命令，运行期
    // `SongCommand::permission_id()` 永远不会返回它，所以不能再给它默认策略。
    // 否则配置里的 `overrides["@点歌"]` 会被 validate 接受，却永远不生效、
    // 也永远不出现在面板里——用户改了以为生效，实际是死配置。
    if super::song_request::command_specs()
        .iter()
        .any(|spec| super::song_request::permission_id_for_command_name(spec.name) == Some(id))
    {
        return Some(CommandPolicy {
            enabled: true,
            minimum_role: song_minimum_role(id),
        });
    }
    if id == "custom_workflow" {
        return Some(CommandPolicy {
            enabled: true,
            minimum_role: CommandPermission::Admin,
        });
    }
    let specs: Vec<_> = super::command_catalog::all_specs()
        .into_iter()
        .filter(|s| format!("{}{}", prefix(s.prefix), s.name) == id)
        .collect();
    // specs 为空说明这个 id 没有任何注册命令：没有策略可给，直接返回 None，
    // 由调用方决定兜底。以前这里还有一段 `matches!(id, "#接龙" | ...)` 的兜底，
    // 但那四条命令都在 command_catalog 里注册了，specs 必然非空，恒不可达。
    if specs.is_empty() {
        return None;
    }
    // 默认角色只分两档，与「谁能在大厅里发」这一件事绑定：
    //
    // - 大厅可见（HALL_ONLY / BOTH / # 会话态）→ User：大厅成员即可用；
    // - 仅好友（FRIEND_ONLY）→ Friend：好友即可用。
    //
    // 旧实现把 FRIEND_ONLY 落到 `else => Admin`，是**语义反向**：声明写「仅好友」，
    // 实际要求「好友且管理员」，于是纯好友角色一条都用不了；而大厅里的 Admin
    // 反而因为来源翻转能执行这些「仅好友」命令。这就是约束对齐要修的核心问题。
    //
    // 需要更严的命令用下面的 ADMIN_COMMANDS 显式列出，不再靠 authorities 隐式推导。
    let minimum = if ADMIN_COMMANDS.contains(&id) {
        CommandPermission::Admin
    } else if specs.iter().any(|s| {
        s.authorities.is_empty()
            || s.prefix == CommandPrefix::Hash
            || s.authorities.contains(&CommandAuthority::HallMember)
    }) {
        CommandPermission::User
    } else {
        // 仅好友可发的命令：默认就该是 Friend，而不是 Admin。
        CommandPermission::Friend
    };
    Some(CommandPolicy {
        enabled: true,
        minimum_role: minimum,
    })
}
pub(crate) fn catalog_policy(config: &CommandAccessConfig, id: &str) -> Option<CommandPolicy> {
    let default = default_policy(id)?;
    // **不能回退到 "@点歌"**：那会让五个点歌命令共用同一份配置，
    // 用户在面板改了 "@QQ点歌" 的权限，其他平台跟着一起变，
    // 表现为「这几个点歌没法独立控制」。每条命令各用各的 override，
    // 没配就用它自己的默认值。
    let mut policy = config.overrides.get(id).cloned().unwrap_or(default);
    policy.minimum_role = policy.minimum_role.max(minimum_floor(id));
    Some(policy)
}
impl CommandAccessConfig {
    /// 丢弃已废弃的面板条目（见 [DEPRECATED_POLICY_IDS]）。
    ///
    /// **必须在 `validate()` 之前调用**：老配置库里可能还留着 `@点歌` 的
    /// override，而 `@点歌` 已经不再是合法面板 id（见 [default_policy_ungated]）。
    /// 先 sanitize 再 validate，老用户才不会因为一条死配置启动失败。
    ///
    /// 只改内存里的配置，不回写数据库：用户库里那份原样保留，
    /// 这样回滚到旧版本仍能读到自己当初设的值。
    ///
    /// 返回被丢弃的条目数。
    pub(crate) fn sanitize_deprecated_overrides(&mut self) -> usize {
        let mut dropped = 0;
        for id in DEPRECATED_POLICY_IDS {
            if self.overrides.remove(*id).is_some() {
                dropped += 1;
                log::warn!(
                    "command_access.overrides 中的 {} 已废弃并忽略：点歌已拆分为五个平台命令（@QQ点歌/@网易点歌/@酷狗点歌/@B站点歌/@AI点歌），请改用对应的平台条目",
                    id
                );
            }
        }
        dropped
    }

    pub(crate) fn apply_default_song_source(&self, song: &mut super::song_request::SongCommand) {
        use super::song_request::SongSource;
        if song.prefix != "点歌" || song.source == SongSource::Bilibili {
            return;
        }
        song.source =
            SongSource::from_str(self.default_song_source.as_deref().unwrap_or("qqmusic"))
                .unwrap_or(SongSource::QqMusic);
    }
    pub fn validate(&self) -> Result<()> {
        if self
            .default_song_source
            .as_deref()
            .is_some_and(|source| super::song_request::SongSource::from_str(source).is_none())
        {
            bail!("默认点歌平台无效");
        }
        for (id, policy) in &self.overrides {
            if default_policy(id).is_none() {
                bail!("command_access.overrides 未知命令ID: {id}");
            }
            let floor = minimum_floor(id);
            if policy.minimum_role < floor {
                bail!(
                    "command_access.overrides {id} 不能低于该命令的最低权限「{}」（{:?}）",
                    floor.label(),
                    floor
                );
            }
        }
        Ok(())
    }
}
/// 没有面板条目的**内部来源**命令使用的策略。
///
/// 这类命令（歌单窗口的 `@1`~`@5` 选择、AI 聊天关键词触发）不是用户可在
/// 面板里开关的条目，因此没有 override 可读。语义要显式且唯一：
/// **启用、门槛 User**——不默认 Admin（默认 Admin 会让未登记路径变成查不到
/// 原因的静默拒绝）。
///
/// 刻意写成具名常量而不是内联字面量，就是为了让「这里放宽是有意为之」
/// 在调用点一眼可见；以及让 [resolve_for_routed] 的空 id 白名单可被测试断言。
pub(crate) const INTERNAL_SOURCE_POLICY: CommandPolicy = CommandPolicy {
    enabled: true,
    minimum_role: CommandPermission::User,
};

pub(crate) fn resolve_for_routed(
    config: &CommandAccessConfig,
    command: &RoutedCommand,
) -> (String, CommandPolicy) {
    use super::administration::AdministrationCommand;
    let forced = match &command.command {
        ModuleCommand::SongRequest(song) => Some(song.permission_id()),
        ModuleCommand::Administration(AdministrationCommand::SetCommandsEnabled {
            enabled,
            ..
        }) => Some(if *enabled { "@启用" } else { "@禁用" }),
        ModuleCommand::Administration(AdministrationCommand::IdleExit { .. }) => Some("@闲置退出"),
        ModuleCommand::Administration(AdministrationCommand::ChatListenerMode(_)) => {
            Some("@值守模式")
        }
        ModuleCommand::CustomWorkflow(_) => Some("custom_workflow"),
        ModuleCommand::IdiomChain(super::idiom_chain::IdiomChainCommand::Start {
            mode: super::idiom_chain::IdiomChainMode::Homophone,
            ..
        }) => Some("#同音接龙"),
        ModuleCommand::CardGame(super::card_games::LandlordCommand::RunFastStart) => {
            Some("#跑得快")
        }
        ModuleCommand::IdiomChain(_) => Some("#接龙"),
        ModuleCommand::CardGame(_) => Some("#斗地主"),
        ModuleCommand::Undercover(_) => Some("#卧底"),
        ModuleCommand::TurtleSoup(_) => Some("#海龟汤"),
        _ => None,
    };
    if let Some(id) = forced {
        return (id.into(), catalog_policy(config, id).unwrap());
    }
    let text = crate::text::normalize_chat_text(&command.matched);
    let spec = super::command_catalog::all_specs()
        .into_iter()
        .filter(|s| s.matches_text(text.strip_prefix(prefix(s.prefix)).unwrap_or(&text)))
        .max_by_key(|s| s.name.chars().count());
    if let Some(spec) = spec {
        let id = format!("{}{}", prefix(spec.prefix), spec.name);
        if let Some(policy) = catalog_policy(config, &id) {
            return (id, policy);
        }
    }
    // **不要再用 `_ => ""` 兜底**：那会让任何未映射的变体变成「面板禁用无效 +
    // 人人可用」，而且新增 `ModuleCommand` 变体时编译器一声不吭。
    // 这里逐变体穷尽枚举，新加变体必须显式回答「它的策略 id 是什么」；
    // 只有「确实没有面板条目」的内部来源（歌单窗口选择、AI 关键词提问）才允许返回空 id。
    let id = match &command.command {
        ModuleCommand::IdiomChain(_) => "#接龙",
        ModuleCommand::CardGame(_) => "#斗地主",
        ModuleCommand::Undercover(_) => "#卧底",
        ModuleCommand::TurtleSoup(_) => "#海龟汤",
        // 帮助类：必须给真实 id。
        //
        // `administration::claims_chat` 把 matched 固定写成 "#"，而上面按 matched
        // 反查 spec 必然失败（没有任何 spec 的名字是空串），于是旧实现落进 `_ => ""`，
        // 再用兜底的 Admin 策略——大厅普通成员发 #帮助 会被判「权限不足」，
        // 而面板显示的是 user 且可编辑，改了完全不生效。
        ModuleCommand::Administration(AdministrationCommand::Help) => "@帮助",
        ModuleCommand::Administration(AdministrationCommand::EntertainmentHelp) => "@娱乐帮助",
        // 歌单窗口里的 `@1`~`@5` 是决策语法，不是面板条目；它跟着歌单功能走，
        // 没有独立开关，所以显式归入内部来源（下方 warn 分支不会命中它们）。
        ModuleCommand::Playlist(super::playlist::PlaylistCommand::Select(_)) => "",
        // AI 聊天的关键词触发（`Ask`）由 ai_chat 自己的配置决定，同样没有面板条目。
        ModuleCommand::AiChat(super::ai_chat::AiChatCommand::Ask { .. }) => "",
        // 以下变体在 `forced` 或「按 matched 反查面板 spec」里就该拿到 id。
        // 走到这里说明命令目录与权限策略表漂移了（**程序错误**，不是用户配置问题）：
        // 仍然给最宽松策略，绝不默认 Admin（会造成查不到原因的静默拒绝），
        // 但必须留一条可检索的告警。
        ModuleCommand::SongRequest(_)
        | ModuleCommand::Playback(_)
        | ModuleCommand::Hall(_)
        | ModuleCommand::Administration(_)
        | ModuleCommand::Tarot(_)
        | ModuleCommand::Playlist(_)
        | ModuleCommand::Invite(_)
        | ModuleCommand::Moderation(_)
        | ModuleCommand::CustomWorkflow(_)
        | ModuleCommand::AiChat(_) => {
            log::warn!(
                "命令没有对应的面板策略，按内部来源处理（命令目录与权限表漂移）: matched={} text={}",
                command.matched,
                command.raw
            );
            ""
        }
    };
    let policy = if id.is_empty() {
        INTERNAL_SOURCE_POLICY
    } else {
        catalog_policy(config, id).unwrap_or_else(|| {
            log::warn!("命令 {} 查不到权限策略，按内部来源处理", id);
            INTERNAL_SOURCE_POLICY
        })
    };
    (id.into(), policy)
}
/// 跑得快对局中，**跟着对局走**的动作才改判到 `#跑得快` 策略。
///
/// 用户禁用 `#跑得快` 的本意是「别开这局 / 别玩这个玩法」，
/// 不该把 `#状态` / `#重试` / `#手牌` 这类**纯查询**也一起拒掉——
/// 旧实现只排除 Start/RunFastStart，其余 CardGame 一律改判，
/// 于是关掉 `#跑得快` 后连看手牌都提示权限不足。
///
/// 归类原则：会改变对局状态（出牌/过/退出/加入/抢地主）→ 跟对局走；
/// 只读地报告状态（状态/重试/手牌）→ 跟它自己的面板条目走。
fn runfast_session_action(command: &super::card_games::LandlordCommand) -> bool {
    use super::card_games::LandlordCommand;
    match command {
        // 开局类：不改判，交给它自己的面板条目（#跑得快 / #斗地主）。
        LandlordCommand::Start | LandlordCommand::RunFastStart => false,
        // 纯查询：只报告状态，不影响对局，禁用玩法不该连查询一起关掉。
        LandlordCommand::Status | LandlordCommand::Retry | LandlordCommand::Hand => false,
        // 其余都改变对局状态：出牌、过、加入、抢、不抢、退出。
        LandlordCommand::Play(_)
        | LandlordCommand::Pass
        | LandlordCommand::Join
        | LandlordCommand::Rob
        | LandlordCommand::Decline
        | LandlordCommand::Exit => true,
    }
}

pub(crate) fn resolve_with_active(
    config: &CommandAccessConfig,
    command: &RoutedCommand,
    active: Option<super::entertainment::EntertainmentKind>,
) -> (String, CommandPolicy) {
    if matches!(
        active,
        Some(super::entertainment::EntertainmentKind::RunFast)
    ) && matches!(
        &command.command,
        ModuleCommand::CardGame(c) if runfast_session_action(c)
    ) {
        return (
            "#跑得快".into(),
            catalog_policy(config, "#跑得快").unwrap_or(INTERNAL_SOURCE_POLICY),
        );
    }
    resolve_for_routed(config, command)
}

#[cfg(test)]
mod tests {

    /// 用户规则：**身份命中就覆盖来源下界，未命中才用下界。**
    ///
    /// 原话：「nickname 是用来推断权限的，如果读取到 nickname 就默认用这个的
    /// 权限（是覆盖而不是最低）」。
    ///
    /// 覆盖的含义：Admin 在大厅（来源下界 User）仍是 Admin，不会被来源降级；
    /// Friend 在好友来源（下界 Friend）也不会被抬高成别的。
    #[test]
    fn named_identity_overrides_the_source_floor() {
        use crate::features::command::CommandAuthority;
        use crate::features::identity::IdentityRole;

        assert_eq!(
            actor_permission_with_floor(Some(IdentityRole::Admin), CommandAuthority::HallMember),
            CommandPermission::Admin,
            "Admin 在大厅不能被来源降级",
        );
        assert_eq!(
            actor_permission_with_floor(Some(IdentityRole::Owner), CommandAuthority::HallMember),
            CommandPermission::Owner,
        );
        assert_eq!(
            actor_permission_with_floor(Some(IdentityRole::Friend), CommandAuthority::Friend),
            CommandPermission::Friend,
        );
        // 身份是 Friend、来源是蓝色大厅：仍按身份算 Friend，不看来源下界。
        assert_eq!(
            actor_permission_with_floor(Some(IdentityRole::Friend), CommandAuthority::HallMember),
            CommandPermission::Friend,
        );
    }

    /// 未登记时退回**来源下界**：粉色好友消息/大厅好友标记 → Friend，蓝色大厅 → User。
    ///
    /// 这就是用户说的「大厅里好友发言有粉色标记、二级聊天里好友发言进左侧列表，
    /// 用于最低权限推断」——没登记也至少是好友。
    #[test]
    fn unnamed_sender_falls_back_to_the_source_floor() {
        use crate::features::command::CommandAuthority;

        assert_eq!(
            actor_permission_with_floor(None, CommandAuthority::Friend),
            CommandPermission::Friend,
            "粉色好友消息：未登记也至少是好友",
        );
        assert_eq!(
            actor_permission_with_floor(None, CommandAuthority::HallMember),
            CommandPermission::User,
            "蓝色大厅普通成员：只给 User",
        );
    }

    use super::*;
    #[test]
    fn source_is_not_identity_and_higher_roles_inherit() {
        let p = CommandPolicy {
            enabled: true,
            minimum_role: CommandPermission::Admin,
        };
        assert!(!p.allows(actor_permission(None)));
        assert!(!p.allows(actor_permission(Some(IdentityRole::Friend))));
        assert!(p.allows(actor_permission(Some(IdentityRole::Owner))));
        // Unknown 身份（既无角色也无昵称）必须在调用方就被挡住，
        // 不该靠 actor_permission 里的 known_actor 参数兜底——
        // 那个参数恒为 true，已删除。这里断言 Unknown 得到 User 档位，
        // 由 Admin 门槛拒绝。
        assert!(!p.allows(actor_permission(None)));
    }
    #[test]
    fn panel_policy_validates_and_overrides_defaults() {
        let mut c = CommandAccessConfig::default();
        // 有效面板条目：五个点歌平台各自独立。
        c.overrides.insert(
            "@QQ点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::Friend,
            },
        );
        c.validate().unwrap();
        assert!(!catalog_policy(&c, "@QQ点歌").unwrap().enabled);
        assert_eq!(
            catalog_policy(&c, "@QQ点歌").unwrap().minimum_role,
            CommandPermission::Friend,
        );
        // 未登记 id 必须被拒绝。
        c.overrides.insert(
            "@不存在".into(),
            CommandPolicy {
                enabled: true,
                minimum_role: CommandPermission::User,
            },
        );
        assert!(c.validate().is_err());
    }

    /// A1：加载链路必须先 sanitize 再 validate——否则老配置库里那条早已
    /// 不再生效的 `@点歌` override 会让整个程序启动失败。
    #[test]
    fn sanitize_drops_deprecated_overrides_before_validate() {
        let mut c = CommandAccessConfig::default();
        c.overrides.insert(
            "@点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::Admin,
            },
        );
        // 丢弃之前 validate 必然失败（这正是必须 sanitize 的理由）。
        assert!(c.validate().is_err(), "sanitize 之前应校验失败");

        assert_eq!(c.sanitize_deprecated_overrides(), 1, "应丢弃一条废弃条目");
        assert!(
            !c.overrides.contains_key("@点歌"),
            "sanitize 之后不应再有 @点歌"
        );
        // sanitize 之后同一份配置必须能通过校验（老用户不会启动失败）。
        c.validate().unwrap();

        // 幂等：没有废弃条目时返回 0，且不影响正常条目。
        c.overrides.insert(
            "@QQ点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::Friend,
            },
        );
        assert_eq!(c.sanitize_deprecated_overrides(), 0);
        assert!(!c.overrides["@QQ点歌"].enabled);
        c.validate().unwrap();

        // 空配置也返回 0。
        let mut empty = CommandAccessConfig::default();
        assert_eq!(empty.sanitize_deprecated_overrides(), 0);
    }

    /// A1：`@点歌` 已拆成五个平台命令，运行期没有路径会返回这个 id。
    ///
    /// 因此它**不能再被接受为面板条目**——接受就意味着「用户改了不生效、
    /// 面板里也看不到」的死配置。（加载链路的 sanitize 负责先把老配置里的它丢掉，
    /// 再交给 validate，所以老用户不会因为这条启动失败。）
    #[test]
    fn deprecated_song_aggregate_id_is_not_a_panel_entry() {
        assert!(default_policy("@点歌").is_none(), "@点歌 不应再有默认策略");
        assert!(catalog_policy(&CommandAccessConfig::default(), "@点歌").is_none());

        let mut c = CommandAccessConfig::default();
        c.overrides.insert(
            "@点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::User,
            },
        );
        assert!(
            c.validate().is_err(),
            "废弃的 @点歌 override 必须被拒绝（先 sanitize 再 validate）"
        );

        // 五个平台仍然各自可用，不受影响。
        for id in ["@QQ点歌", "@网易点歌", "@酷狗点歌", "@B站点歌", "@AI点歌"] {
            assert!(
                catalog_policy(&CommandAccessConfig::default(), id).is_some(),
                "{id} 应有自己的默认策略"
            );
        }
    }

    /// A3：把 floor 收进单一出口后，**任何** `default_policy` 结果都不会低于
    /// `minimum_floor(id)`。
    ///
    /// **诚实说明：这项目前属于「预防性结构重构」，不是已修的实际缺陷。**
    /// `minimum_floor` 除 `@值守模式` 外一律返回 `User`，而 `User` 是枚举最低值，
    /// 所以对现有全部命令而言 `.max(minimum_floor(id))` 是恒等变换——即便回滚 A3，
    /// 也没有任何一条现有命令会因此被放宽或收紧。唯一 floor 是 `Owner` 的
    /// `@值守模式` 走的是末尾分支，旧代码本来就有 `.max()`。
    ///
    /// 复核时做过变异验证：把 floor 精确回滚成「只有末尾分支套 `.max()`」
    /// （即 A3 修改前的形态），本测试**仍然通过** —— 这是复核者论证的直接证据，
    /// 也说明为什么必须在这里如实标注「预防性」，而不是写成「已修 bug」。
    ///
    /// 保留它的价值在**将来**：一旦新增一条 floor 高于 `User` 的命令，而它恰好
    /// 走某个提前 return 分支（song / custom_workflow），漏套 floor 会被这里抓住
    /// （全量遍历 + 末尾那条「floor 表只有 @值守模式 非 User」的断言会同时报警）。
    ///
    /// 之所以从「4 个固定 id」改成遍历 `command_catalog::all_specs()` 全量命令：
    /// 固定取样只能证明那几条，新增命令不会进断言；全量遍历才能覆盖「将来新增的
    /// 分支 / 新增的命令漏套 floor」这个真实风险。
    #[test]
    fn default_policy_applies_the_floor_on_every_branch() {
        // 全量：命令目录里的每一条命令。
        let mut checked = 0;
        for spec in crate::features::command_catalog::all_specs() {
            let id = format!("{}{}", prefix(spec.prefix), spec.name);
            let policy = default_policy(&id).unwrap_or_else(|| panic!("{id} 应有默认策略"));
            assert!(
                policy.minimum_role >= minimum_floor(&id),
                "{id} 的默认档位 {:?} 低于下限 {:?}",
                policy.minimum_role,
                minimum_floor(&id)
            );
            checked += 1;
        }
        assert!(checked > 0, "命令目录为空，全量断言没有实际覆盖率");

        // 不在命令目录里、但同样走 default_policy 的两个特殊 id。
        for id in ["custom_workflow", "@值守模式"] {
            let policy = default_policy(id).unwrap_or_else(|| panic!("{id} 应有默认策略"));
            assert!(
                policy.minimum_role >= minimum_floor(id),
                "{id} 的默认档位 {:?} 低于下限 {:?}",
                policy.minimum_role,
                minimum_floor(id)
            );
        }

        // `@值守模式` 是当前唯一 floor 为 Owner 的命令。这里同时钉住
        // 「现行 floor 表确实只有它」——若将来新增了更高 floor，本断言会提醒
        // 作者回来复核上面那段「预防性」说明是否仍然成立。
        let mut non_user_floors: Vec<String> = crate::features::command_catalog::all_specs()
            .iter()
            .map(|spec| format!("{}{}", prefix(spec.prefix), spec.name))
            .chain(["custom_workflow".to_string()])
            .filter(|id| minimum_floor(id) != CommandPermission::User)
            .collect();
        non_user_floors.sort();
        assert_eq!(
            non_user_floors,
            vec!["@值守模式".to_string()],
            "floor 表变了：请重新评估 A3 是否仍是恒等变换，并同步上面注释"
        );
        assert_eq!(
            default_policy("@值守模式").unwrap().minimum_role,
            CommandPermission::Owner,
        );
    }

    /// A3 的另一半：`catalog_policy` 只是把 override 与默认值合并，
    /// 用户配置也不能把档位压到 floor 以下。
    #[test]
    fn catalog_policy_never_drops_below_the_floor() {
        let mut c = CommandAccessConfig::default();
        // 直接构造越界配置（validate 会拒绝，但运行期读取必须也夹紧）。
        c.overrides.insert(
            "@值守模式".into(),
            CommandPolicy {
                enabled: true,
                minimum_role: CommandPermission::User,
            },
        );
        assert!(c.validate().is_err());
        assert_eq!(
            catalog_policy(&c, "@值守模式").unwrap().minimum_role,
            CommandPermission::Owner,
            "越界 override 在读取时也必须被夹回到下限",
        );
    }

    /// A2：空 id 只允许出现在白名单内的内部来源。
    ///
    /// 这条测试要兑现名字里的「limited to」：不仅验证那两个白名单来源**能**
    /// 返回空 id，还要验证**没有任何真实命令**会落进那个空 id 分支。
    /// 否则名字只是装饰——OR 分支落 `INTERNAL_SOURCE_POLICY`（enabled + User）
    /// 与旧的 `_ => ""` 兜底运行时行为完全一致，面板禁用照样能被绕过。
    #[test]
    fn empty_policy_id_is_limited_to_whitelisted_internal_sources() {
        use crate::features::command::{CommandObservation, CommandPrefix, ModuleCommand};
        let config = CommandAccessConfig::default();
        let routed = |command: ModuleCommand, matched: &str| crate::features::command::RoutedCommand {
            matched: matched.to_string(),
            raw: matched.to_string(),
            user_command: format!("@{matched}"),
            message_type: "blue".to_string(),
            username: "玩家甲".to_string(),
            account: None,
            authority: CommandAuthority::HallMember,
            role: None,
            permission_required: None,
            denied_reason: None,
            command,
            observation: CommandObservation::default(),
        };
        let _ = CommandPrefix::At;

        // 白名单内：歌单窗口选择、AI 关键词提问 → 允许空 id。
        for (command, matched) in [
            (
                ModuleCommand::Playlist(super::super::playlist::PlaylistCommand::Select(3)),
                "3",
            ),
            (
                ModuleCommand::AiChat(super::super::ai_chat::AiChatCommand::Ask {
                    question: "今天天气".to_string(),
                }),
                "随意文本",
            ),
        ] {
            let (id, policy) = resolve_for_routed(&config, &routed(command, matched));
            assert!(id.is_empty(), "{matched} 应走内部来源白名单");
            assert_eq!(policy.enabled, INTERNAL_SOURCE_POLICY.enabled);
            assert_eq!(
                policy.minimum_role,
                INTERNAL_SOURCE_POLICY.minimum_role,
                "内部来源策略必须显式且唯一：不默认 Admin"
            );
        }

        // 白名单外的普通命令必须能拿到**真实** id，不能落进空 id 兜底。
        for (command, matched, expected) in [
            (
                ModuleCommand::Playback(crate::features::playback::PlaybackCommand::Status),
                "状态",
                "@状态",
            ),
            (
                ModuleCommand::CardGame(crate::features::card_games::LandlordCommand::Status),
                "状态",
                "#斗地主",
            ),
        ] {
            let (id, _) = resolve_for_routed(&config, &routed(command, matched));
            assert_eq!(id, expected, "{matched} 必须解析到自己的面板条目");
        }

        // ★ 名不副实的部分在这里补上：**遍历命令目录全量**，验证没有一条真实
        // 命令会落进空 id 分支。
        //
        // 覆盖的是「新增命令后忘了在 resolve_for_routed 里给它 id」这个真实风险：
        // 只要命令按自己的规范名被路由（`matched` == spec.name），就必须能通过
        // 「按 matched 反查面板 spec」那一步拿到非空 id。
        //
        // 已知的覆盖盲区（如实记录，不要因为这条测试变绿就以为没有风险）：
        // 像 `administration::Help` 那样把 `matched` 写成 "#"（而非规范名的）
        // 模块，反查必然失败，要靠 resolve_for_routed 里**显式**的 fallback 分支
        // 兜住。本测试用 spec.name 构造，覆盖不到那种「matched 与规范名不符」的
        // 新增情形——新增那类命令时仍需人工确认 fallback 分支。
        let mut covered = 0;
        for spec in crate::features::command_catalog::all_specs() {
            let (id, policy) = resolve_for_routed(
                &config,
                &routed(
                    ModuleCommand::Playback(crate::features::playback::PlaybackCommand::Status),
                    spec.name,
                ),
            );
            assert!(
                !id.is_empty(),
                "命令 {} 落进了空 id 分支：面板禁用会失效",
                spec.name
            );
            assert!(
                catalog_policy(&config, &id).is_some(),
                "命令 {} 解析出的 id {id} 不是可配置条目",
                spec.name
            );
            assert!(policy.enabled, "命令 {} 的默认策略不应是禁用", spec.name);
            covered += 1;
        }
        assert!(
            covered >= 40,
            "命令目录只有 {covered} 条，全量断言没有实际覆盖率"
        );
    }

    /// A2 的常量约束：白名单与「内部来源」判定必须互相对得上。
    #[test]
    fn internal_source_whitelist_is_explicit_and_not_admin() {
        assert_eq!(
            INTERNAL_SOURCE_POLICY.minimum_role,
            CommandPermission::User,
            "内部来源不能默认 Admin——那会让未登记路径变成查不到原因的静默拒绝"
        );
        assert!(INTERNAL_SOURCE_POLICY.enabled);
    }

    /// A4：跑得快对局中，只有**跟着对局走**的动作改判到 #跑得快。
    #[test]
    fn runfast_session_override_excludes_read_only_queries() {
        use crate::features::card_games::LandlordCommand;
        use crate::features::entertainment::EntertainmentKind;

        // 纯查询 / 开局：不改判。
        for command in [
            LandlordCommand::Status,
            LandlordCommand::Retry,
            LandlordCommand::Hand,
            LandlordCommand::Start,
            LandlordCommand::RunFastStart,
        ] {
            assert!(
                !runfast_session_action(&command),
                "{command:?} 是查询或开局，不该被 #跑得快 特判吃掉"
            );
        }
        // 改变对局状态：跟对局走。
        for command in [
            LandlordCommand::Play("3".to_string()),
            LandlordCommand::Pass,
            LandlordCommand::Join,
            LandlordCommand::Rob,
            LandlordCommand::Decline,
            LandlordCommand::Exit,
        ] {
            assert!(runfast_session_action(&command), "{command:?} 应跟随对局");
        }

        // 端到端：用户禁用 #跑得快 后，查询仍按自己的条目放行，动作被拒。
        let mut config = CommandAccessConfig::default();
        config.overrides.insert(
            "#跑得快".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::User,
            },
        );
        let routed = |command: LandlordCommand| crate::features::command::RoutedCommand {
            matched: "状态".to_string(),
            raw: "状态".to_string(),
            user_command: "#状态".to_string(),
            message_type: "blue".to_string(),
            username: "玩家甲".to_string(),
            account: None,
            authority: CommandAuthority::HallMember,
            role: None,
            permission_required: None,
            denied_reason: None,
            command: ModuleCommand::CardGame(command),
            observation: crate::features::command::CommandObservation::default(),
        };
        let active = Some(EntertainmentKind::RunFast);
        let (id, policy) = resolve_with_active(
            &config,
            &routed(LandlordCommand::Status),
            active,
        );
        assert_eq!(id, "#斗地主", "查询类走它自己的条目，不被 #跑得快 牵连");
        assert!(policy.enabled, "禁用 #跑得快 不该连看状态一起拒掉");

        let (id, policy) = resolve_with_active(
            &config,
            &routed(LandlordCommand::Pass),
            active,
        );
        assert_eq!(id, "#跑得快");
        assert!(!policy.enabled, "跟着对局走的动作仍受 #跑得快 开关约束");
    }
}
