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
fn default_policy(id: &str) -> Option<CommandPolicy> {
    // 注意不要再加回 @搜索：该命令已移除，command_access 之外也无人认识它，
    // 留着只会让 validate 接受一份指向不存在命令的配置。
    if id == "@点歌"
        || super::song_request::command_specs()
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
        minimum_role: minimum.max(minimum_floor(id)),
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
            if policy.minimum_role < minimum_floor(id) {
                bail!("command_access.overrides {id} 不能低于主人权限");
            }
        }
        Ok(())
    }
}
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
    let id = match &command.command {
        ModuleCommand::IdiomChain(_) => "#接龙",
        ModuleCommand::CardGame(_) => "#斗地主",
        ModuleCommand::Undercover(_) => "#卧底",
        ModuleCommand::TurtleSoup(_) => "#海龟汤",
        ModuleCommand::AiChat(_) => "@忘记我",
        // 帮助类：必须给真实 id。
        //
        // `administration::claims_chat` 把 matched 固定写成 "#"，而上面按 matched
        // 反查 spec 必然失败（没有任何 spec 的名字是空串），于是旧实现落进 `_ => ""`，
        // 再用兜底的 Admin 策略——大厅普通成员发 #帮助 会被判「权限不足」，
        // 而面板显示的是 user 且可编辑，改了完全不生效。
        ModuleCommand::Administration(AdministrationCommand::Help) => "@帮助",
        ModuleCommand::Administration(AdministrationCommand::EntertainmentHelp) => "@娱乐帮助",
        _ => "",
    };
    // 空 id 说明这条命令没有可配置的策略：按最宽松处理，绝不默认 Admin。
    // 默认 Admin 会让任何未登记的路径变成「静默拒绝」，而拒绝原因还查不到。
    let policy = if id.is_empty() {
        CommandPolicy {
            enabled: true,
            minimum_role: CommandPermission::User,
        }
    } else {
        catalog_policy(config, id).unwrap_or(CommandPolicy {
            enabled: true,
            minimum_role: CommandPermission::User,
        })
    };
    (id.into(), policy)
}
pub(crate) fn resolve_with_active(
    config: &CommandAccessConfig,
    command: &RoutedCommand,
    active: Option<super::entertainment::EntertainmentKind>,
) -> (String, CommandPolicy) {
    if matches!(
        active,
        Some(super::entertainment::EntertainmentKind::RunFast)
    ) && matches!(&command.command,ModuleCommand::CardGame(c) if !matches!(c,super::card_games::LandlordCommand::Start | super::card_games::LandlordCommand::RunFastStart))
    {
        return ("#跑得快".into(), catalog_policy(config, "#跑得快").unwrap());
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
        c.overrides.insert(
            "@点歌".into(),
            CommandPolicy {
                enabled: false,
                minimum_role: CommandPermission::Friend,
            },
        );
        c.validate().unwrap();
        assert!(!catalog_policy(&c, "@点歌").unwrap().enabled);
        c.overrides.insert(
            "@不存在".into(),
            CommandPolicy {
                enabled: true,
                minimum_role: CommandPermission::User,
            },
        );
        assert!(c.validate().is_err());
    }
}
