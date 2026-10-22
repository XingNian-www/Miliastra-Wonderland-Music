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
pub(crate) fn actor_permission(
    role: Option<IdentityRole>,
    known_actor: bool,
) -> Option<CommandPermission> {
    if !known_actor {
        return None;
    }
    Some(match role {
        None => CommandPermission::User,
        Some(IdentityRole::Friend) => CommandPermission::Friend,
        Some(IdentityRole::Admin) => CommandPermission::Admin,
        Some(IdentityRole::Owner) => CommandPermission::Owner,
    })
}
fn prefix(p: CommandPrefix) -> char {
    match p {
        CommandPrefix::At => '@',
        CommandPrefix::Hash => '#',
        CommandPrefix::Amp => '&',
    }
}
pub(crate) fn minimum_floor(id: &str) -> CommandPermission {
    if matches!(id, "@启用" | "@禁用" | "@值守模式" | "@闲置退出") {
        CommandPermission::Owner
    } else {
        CommandPermission::User
    }
}
fn default_policy(id: &str) -> Option<CommandPolicy> {
    if id == "@点歌"
        || id == "@搜索"
        || super::song_request::command_specs()
            .iter()
            .any(|spec| super::song_request::permission_id_for_command_name(spec.name) == Some(id))
    {
        return Some(CommandPolicy {
            enabled: true,
            minimum_role: CommandPermission::User,
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
    if specs.is_empty() {
        return matches!(id, "#接龙" | "#斗地主" | "#卧底" | "#海龟汤").then_some(CommandPolicy {
            enabled: true,
            minimum_role: CommandPermission::User,
        });
    }
    let minimum = if matches!(id, "@暂停" | "@继续" | "@播放" | "@下一首" | "@上一首") {
        CommandPermission::Friend
    } else if matches!(
        id,
        "@删除"
            | "@队列删除"
            | "@队列清空"
            | "@音量"
            | "@后台歌词"
            | "@持续歌词"
            | "@拉黑"
            | "@屏蔽"
            | "@邀请"
            | "@麦克风"
    ) {
        CommandPermission::Admin
    } else if specs.iter().any(|s| {
        s.authorities.is_empty()
            || s.prefix == CommandPrefix::Hash
            || s.authorities.contains(&CommandAuthority::HallMember)
    }) {
        CommandPermission::User
    } else {
        CommandPermission::Admin
    };
    Some(CommandPolicy {
        enabled: true,
        minimum_role: minimum.max(minimum_floor(id)),
    })
}
pub(crate) fn catalog_policy(config: &CommandAccessConfig, id: &str) -> Option<CommandPolicy> {
    let default = default_policy(id)?;
    let mut policy = config
        .overrides
        .get(id)
        .cloned()
        .or_else(|| {
            super::song_request::command_specs()
                .iter()
                .any(|spec| {
                    super::song_request::permission_id_for_command_name(spec.name) == Some(id)
                })
                .then(|| config.overrides.get("@点歌").cloned())
                .flatten()
        })
        .unwrap_or(default);
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
        _ => "",
    };
    let policy = catalog_policy(config, id).unwrap_or(CommandPolicy {
        enabled: true,
        minimum_role: CommandPermission::Admin,
    });
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
    use super::*;
    #[test]
    fn source_is_not_identity_and_higher_roles_inherit() {
        let p = CommandPolicy {
            enabled: true,
            minimum_role: CommandPermission::Admin,
        };
        assert!(!p.allows(actor_permission(None, true)));
        assert!(!p.allows(actor_permission(Some(IdentityRole::Friend), true)));
        assert!(p.allows(actor_permission(Some(IdentityRole::Owner), true)));
        assert!(!p.allows(actor_permission(Some(IdentityRole::Owner), false)));
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
