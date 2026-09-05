//! 昵称映射与身份权限。
//!
//! OCR 读到的备注昵称精确映射到稳定 UUID 与角色；未映射的昵称一律按路人
//! 处理，不做任何自动归并。权限三级：主人（Owner）> 管理员（Admin）>
//! 好友（Friend），高级角色包含低级角色的全部权限。

use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 身份角色；声明顺序即权限从低到高，可直接用 Ord 比较。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdentityRole {
    Friend,
    Admin,
    Owner,
}

/// 一条映射：OCR 备注昵称 → 稳定 UUID + 角色。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityMapping {
    /// 游戏内好友备注昵称，与 OCR 结果精确匹配。
    pub nickname: String,
    /// 内部身份 UUID：仅用于数据库内区分与去重，面板不回显。
    pub id: Uuid,
    pub role: IdentityRole,
    /// 面板展示用备注（记录这是谁）；旧数据缺失时视为空串。
    #[serde(default)]
    pub note: String,
}

/// identity 配置段：手动维护的映射表。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityConfig {
    #[serde(default)]
    pub mappings: Vec<IdentityMapping>,
}

impl IdentityConfig {
    pub fn validate(&self) -> Result<()> {
        let mut nicknames = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        let mut owners = 0usize;
        for mapping in &self.mappings {
            if mapping.nickname.trim().is_empty() {
                bail!("identity.mappings 存在空昵称");
            }
            if !nicknames.insert(mapping.nickname.as_str()) {
                bail!("identity.mappings 昵称重复: {}", mapping.nickname);
            }
            if !ids.insert(mapping.id) {
                bail!("identity.mappings UUID 重复: {}", mapping.id);
            }
            if mapping.role == IdentityRole::Owner {
                owners += 1;
            }
        }
        if owners > 1 {
            bail!("identity.mappings 最多允许一位主人（owner），当前 {owners} 位");
        }
        Ok(())
    }
}

/// 解析结果：稳定 UUID、角色与用户备注。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub id: Uuid,
    pub role: IdentityRole,
    pub nickname: String,
    pub note: String,
}

impl ResolvedIdentity {
    pub fn display_name(&self) -> &str {
        let note = self.note.trim();
        if note.is_empty() {
            &self.nickname
        } else {
            note
        }
    }
}

/// 身份查询共享句柄；LiveConfigs 持有并在配置保存后整体替换快照。
#[derive(Clone, Default)]
pub struct IdentityAccess {
    inner: Arc<RwLock<IdentityConfig>>,
}

impl IdentityAccess {
    pub fn new(config: IdentityConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(config)),
        }
    }

    /// 配置保存成功后整体替换映射表。
    pub fn replace(&self, config: IdentityConfig) {
        *self.inner.write().unwrap_or_else(PoisonError::into_inner) = config;
    }

    /// 精确解析昵称；未映射返回 None。
    pub fn resolve(&self, nickname: &str) -> Option<ResolvedIdentity> {
        let config = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        config
            .mappings
            .iter()
            .find(|mapping| mapping.nickname == nickname)
            .map(|mapping| ResolvedIdentity {
                id: mapping.id,
                role: mapping.role,
                nickname: mapping.nickname.clone(),
                note: mapping.note.clone(),
            })
    }

    pub fn display_name(&self, nickname: &str) -> String {
        self.resolve(nickname)
            .map(|identity| identity.display_name().to_owned())
            .unwrap_or_else(|| nickname.to_owned())
    }

    /// 获取好友操作接口使用的原始游戏昵称；备注昵称只用于身份识别和展示。
    pub fn canonical_name(&self, nickname: &str) -> String {
        self.resolve(nickname)
            .map(|identity| identity.nickname)
            .unwrap_or_else(|| nickname.to_owned())
    }

    /// 在聊天发送前替换文本中的 OCR 身份昵称，避免把映射字符串原样发回大厅。
    pub fn replace_display_names(&self, text: &str) -> String {
        let mut mappings = self
            .inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .mappings
            .iter()
            .filter_map(|mapping| {
                let display_name = mapping.note.trim();
                (!mapping.nickname.is_empty() && !display_name.is_empty())
                    .then(|| (mapping.nickname.clone(), display_name.to_owned()))
            })
            .collect::<Vec<_>>();
        mappings.sort_by_key(|mapping| std::cmp::Reverse(mapping.0.len()));

        let mut result = String::with_capacity(text.len());
        let mut remaining = text;
        while !remaining.is_empty() {
            if let Some((nickname, display_name)) = mappings
                .iter()
                .find(|(nickname, _)| remaining.starts_with(nickname))
            {
                result.push_str(display_name);
                remaining = &remaining[nickname.len()..];
            } else {
                let character = remaining.chars().next().expect("非空字符串必定包含字符");
                result.push(character);
                remaining = &remaining[character.len_utf8()..];
            }
        }
        result
    }

    pub fn role_of(&self, nickname: &str) -> Option<IdentityRole> {
        self.resolve(nickname).map(|identity| identity.role)
    }

    #[cfg(test)]
    /// 是否达到指定角色（含更高角色）；未映射昵称一律 false。
    pub fn is_at_least(&self, nickname: &str, required: IdentityRole) -> bool {
        self.role_of(nickname).is_some_and(|role| role >= required)
    }

    #[cfg(test)]
    pub fn is_owner(&self, nickname: &str) -> bool {
        self.is_at_least(nickname, IdentityRole::Owner)
    }

    #[cfg(test)]
    pub fn is_admin_or_above(&self, nickname: &str) -> bool {
        self.is_at_least(nickname, IdentityRole::Admin)
    }

    #[cfg(test)]
    pub fn is_friend_or_above(&self, nickname: &str) -> bool {
        self.is_at_least(nickname, IdentityRole::Friend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(nickname: &str, id: Uuid, role: IdentityRole) -> IdentityMapping {
        IdentityMapping {
            nickname: nickname.to_owned(),
            id,
            role,
            note: String::new(),
        }
    }

    fn sample_config() -> IdentityConfig {
        let mut owner = mapping("派蒙本蒙", Uuid::from_u128(1), IdentityRole::Owner);
        owner.note = "主人备注".to_string();
        IdentityConfig {
            mappings: vec![
                owner,
                mapping("管理小助手", Uuid::from_u128(2), IdentityRole::Admin),
                mapping("好友甲", Uuid::from_u128(3), IdentityRole::Friend),
            ],
        }
    }

    #[test]
    fn resolve_matches_exactly_and_never_maps_unknown() {
        let access = IdentityAccess::new(sample_config());
        let owner = access.resolve("派蒙本蒙").expect("主人应被解析");
        assert_eq!(owner.id, Uuid::from_u128(1));
        assert_eq!(owner.role, IdentityRole::Owner);
        assert_eq!(owner.display_name(), "主人备注");
        assert_eq!(access.display_name("管理小助手"), "管理小助手");
        assert_eq!(access.display_name("主人备注"), "主人备注");
        assert_eq!(access.role_of("主人备注"), None);
        assert_eq!(access.display_name("陌生人"), "陌生人");
        assert!(access.resolve("派蒙本蒙 ").is_none(), "尾部空白不模糊匹配");
        assert!(access.resolve("陌生人").is_none(), "未知昵称不映射");
    }

    #[test]
    fn replace_display_names_maps_longest_names_and_preserves_unknown_text() {
        let mut config = sample_config();
        config.mappings.push(IdentityMapping {
            nickname: "好友".to_string(),
            id: Uuid::from_u128(4),
            role: IdentityRole::Friend,
            note: "短备注".to_string(),
        });
        let access = IdentityAccess::new(config);
        assert_eq!(
            access.replace_display_names("@派蒙本蒙的请求,@好友甲已处理,陌生人"),
            "@主人备注的请求,@短备注甲已处理,陌生人"
        );
        assert_eq!(access.replace_display_names("好友甲好友"), "短备注甲短备注");
    }

    #[test]
    fn replace_display_names_ignores_empty_notes() {
        let access = IdentityAccess::new(sample_config());
        assert_eq!(
            access.replace_display_names("管理小助手和好友甲"),
            "管理小助手和好友甲"
        );
    }

    #[test]
    fn role_hierarchy_includes_lower_levels() {
        let access = IdentityAccess::new(sample_config());
        assert!(access.is_owner("派蒙本蒙"));
        assert!(access.is_admin_or_above("派蒙本蒙"));
        assert!(access.is_friend_or_above("派蒙本蒙"));
        assert!(!access.is_owner("管理小助手"));
        assert!(access.is_admin_or_above("管理小助手"));
        assert!(access.is_friend_or_above("管理小助手"));
        assert!(!access.is_admin_or_above("好友甲"));
        assert!(access.is_friend_or_above("好友甲"));
        assert!(!access.is_friend_or_above("陌生人"));
    }

    #[test]
    fn replace_takes_effect_immediately() {
        let access = IdentityAccess::new(IdentityConfig::default());
        assert!(access.resolve("派蒙本蒙").is_none());
        access.replace(sample_config());
        assert!(access.is_owner("派蒙本蒙"));
    }

    #[test]
    fn validate_rejects_duplicate_nickname_uuid_and_multiple_owners() {
        let duplicate = IdentityConfig {
            mappings: vec![
                mapping("甲", Uuid::from_u128(1), IdentityRole::Friend),
                mapping("甲", Uuid::from_u128(2), IdentityRole::Friend),
            ],
        };
        assert!(duplicate.validate().is_err());

        let duplicate_id = IdentityConfig {
            mappings: vec![
                mapping("甲", Uuid::from_u128(9), IdentityRole::Friend),
                mapping("乙", Uuid::from_u128(9), IdentityRole::Friend),
            ],
        };
        assert!(
            duplicate_id.validate().is_err(),
            "UUID 全局唯一，重复须拒绝"
        );

        let without_note: IdentityMapping = serde_json::from_str(
            r#"{"nickname":"甲","id":"00000000-0000-0000-0000-000000000001","role":"friend"}"#,
        )
        .unwrap();
        assert!(without_note.note.is_empty(), "缺 note 一律按空串处理");

        let invalid_id = serde_json::from_str::<IdentityMapping>(
            r#"{"nickname":"甲","id":"invalid","role":"friend"}"#,
        );
        assert!(invalid_id.is_err(), "id 必须是 UUID");

        let two_owners = IdentityConfig {
            mappings: vec![
                mapping("甲", Uuid::from_u128(1), IdentityRole::Owner),
                mapping("乙", Uuid::from_u128(2), IdentityRole::Owner),
            ],
        };
        assert!(two_owners.validate().is_err());

        assert!(sample_config().validate().is_ok());
        assert!(IdentityConfig::default().validate().is_ok());
    }

    #[test]
    fn role_serialization_is_lowercase() {
        let json = serde_json::to_string(&IdentityRole::Owner).unwrap();
        assert_eq!(json, "\"owner\"");
        let role: IdentityRole = serde_json::from_str("\"admin\"").unwrap();
        assert_eq!(role, IdentityRole::Admin);
    }
}
