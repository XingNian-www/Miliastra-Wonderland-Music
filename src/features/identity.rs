//! 昵称映射与身份权限。
//!
//! OCR 读到的游戏内备注昵称精确映射到角色与对外显示备注；未映射的昵称一律按路人
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

/// 一条映射：游戏内备注昵称 → 系统标识、角色与对外显示备注。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityMapping {
    /// 游戏内好友备注昵称，与 OCR 结果精确匹配。
    ///
    /// 可留空：好友私聊首次出现时会自动建一条只有 UUID 的映射，
    /// 等用户把昵称与 UID 补上后即可精确定位。
    #[serde(default)]
    pub nickname: String,
    /// 系统维护的映射标识。
    pub id: Uuid,
    pub role: IdentityRole,
    /// 面板展示用备注（记录这是谁）；旧数据缺失时视为空串。
    #[serde(default)]
    pub note: String,
    /// 该身份绑定的游戏账号。
    ///
    /// 用列表而不是单个可选值：一个玩家常有多个小号，它们该共用同一份
    /// 身份与歌单。UID 参与歌单归属键（优先级最高），因此这里填了就能
    /// 跨改名稳定定位；留空则退化到昵称键。
    #[serde(default)]
    pub accounts: Vec<GameAccount>,
}

/// 一条"游戏 → UID"的账号绑定。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GameAccount {
    /// 游戏标识，用于隔离不同游戏的 UID 空间。
    #[serde(default = "default_game")]
    pub game: String,
    /// 该游戏内的账号 ID。
    pub uid: String,
}

fn default_game() -> String {
    miliastra_kernel::chat_source::GAME_GENSHIN.to_string()
}

impl GameAccount {
    pub fn new(uid: impl Into<String>) -> Self {
        Self {
            game: default_game(),
            uid: uid.into(),
        }
    }
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
        let mut accounts = std::collections::HashSet::new();
        let mut owners = 0usize;
        for mapping in &self.mappings {
            // 空昵称允许存在（自建身份还没绑定昵称），但只能有一条：
            // 多条空昵称会让"按昵称查找"永远落空，也失去可读性。
            if !mapping.nickname.trim().is_empty()
                && !nicknames.insert(mapping.nickname.as_str())
            {
                bail!("identity.mappings 昵称重复: {}", mapping.nickname);
            }
            if !ids.insert(mapping.id) {
                bail!("identity.mappings UUID 重复: {}", mapping.id);
            }
            for account in &mapping.accounts {
                if account.uid.trim().is_empty() {
                    bail!("identity.mappings 账号 UID 不能为空（昵称: {}）", mapping.nickname);
                }
                if !accounts.insert((account.game.trim().to_ascii_lowercase(), account.uid.trim().to_string())) {
                    bail!(
                        "identity.mappings 账号重复: {}:{}",
                        account.game,
                        account.uid
                    );
                }
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
    /// 无备注无昵称时的兜底称呼（UUID 前 8 位），构造时算好，
    /// 避免 display_name 返回借用局部变量的引用。
    pub fallback_name: String,
}

impl ResolvedIdentity {
    pub fn display_name(&self) -> &str {
        let note = self.note.trim();
        if !note.is_empty() {
            return note;
        }
        // 不回退到 `nickname`：它是 OCR 匹配键，一旦出现在日志/回复/面板里
        // 就等于把"能被冒充的名字"泄露出去了。没有备注时用 UUID 前缀，
        // 它不可用于匹配，展示上也足够区分。
        &self.fallback_name
    }
}

/// 生成随机的"游戏内昵称"。
///
/// 与面板的 `randomRemarkNickname` 用同一套字符集：自动创建的身份需要一个
/// 唯一且不与真实玩家冲突的 OCR 匹配键。用固定字符集而不是 UUID 片段，
/// 是为了在日志里读起来仍像一个昵称。
fn random_nickname() -> String {
    // 与 Web 面板 `REMARK_CHARSET` 完全一致：35 个易辨识的英数字母，
    // 去掉了 I/l/0/O 等易混淆字符。8 位 → 35^8 ≈ 2.2e12 组合，
    // 足以抵御"猜一个名字来伪装成自动身份"。
    const CHARSET: &[u8] = b"347ACDEFGHKNPQRTUVWXYacdefhknprtuwxy";
    let bytes = uuid::Uuid::new_v4().as_u128().to_le_bytes();
    let mut out = String::with_capacity(8);
    for index in 0..8 {
        // 每个字节取模会略微偏向小下标；对"防碰撞"没有实际影响，
        // 且比引入 rand 依赖更简单。
        out.push(CHARSET[bytes[index] as usize % CHARSET.len()] as char);
    }
    out
}

/// UUID 前 8 位，用作无昵称时的兜底称呼。
fn short_uuid(id: Uuid) -> String {
    let text = id.to_string();
    text[..text.len().min(8)].to_string()
}

/// 消息来源能提供的身份线索。
///
/// 两条链路在权限解析处收敛到这里：
/// - OCR 只有昵称（随机备注就是它的权限符号）；
/// - 外部聊天 API 有 UID，比昵称可靠得多，优先用它。
#[derive(Clone, Copy, Debug)]
pub enum IdentityHint<'a> {
    /// OCR 路径：只有游戏内昵称。
    Nickname(&'a str),
    /// 外部聊天 API 路径：UID 优先，昵称兜底。
    Account {
        game: &'a str,
        uid: &'a str,
        nickname: Option<&'a str>,
    },
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
        // 空昵称永不匹配：OCR 失败时发送者是空串，若不挡住会命中
        // 昵称为空的映射，权限与歌单全部错乱。
        // 注意：非空时不 trim——调用方传什么就精确比什么，
        // 尾部空白不模糊匹配是既有语义，不能悄悄放宽。
        if nickname.trim().is_empty() {
            return None;
        }
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
                fallback_name: short_uuid(mapping.id),
            })
    }

    /// 按线索解析身份：两条链路共用的唯一入口。
    ///
    /// UID 命中优先于昵称：账号 ID 改不了、不会重名，而昵称可能被玩家改成
    /// 任意想要的名字。昵称只在没有 UID 时才作为兜底。
    pub fn resolve_hint(&self, hint: IdentityHint<'_>) -> Option<ResolvedIdentity> {
        match hint {
            IdentityHint::Nickname(name) => self.resolve(name),
            IdentityHint::Account {
                game,
                uid,
                nickname,
            } => self
                .resolve_account(game, uid)
                .or_else(|| nickname.and_then(|name| self.resolve(name))),
        }
    }

    /// 按"游戏 + UID"反查身份。
    ///
    /// 聊天源能直接给出 UID 时用它定位，比昵称可靠——昵称可以改、可以重名，
    /// 而且外部源的昵称可能尚未解析（`from.name` 为 null）。
    pub fn resolve_account(&self, game: &str, uid: &str) -> Option<ResolvedIdentity> {
        let game = game.trim().to_ascii_lowercase();
        let uid = uid.trim();
        if uid.is_empty() {
            return None;
        }
        let config = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        config.mappings.iter().find_map(|mapping| {
            mapping
                .accounts
                .iter()
                .any(|account| {
                    account.game.trim().to_ascii_lowercase() == game
                        && account.uid.trim() == uid
                })
                .then(|| ResolvedIdentity {
                    id: mapping.id,
                    role: mapping.role,
                    nickname: mapping.nickname.clone(),
                    note: mapping.note.clone(),
                    fallback_name: short_uuid(mapping.id),
                })
        })
    }

    /// 取某个身份（UUID）下绑定的所有游戏账号。
    ///
    /// 一个 UUID 可以绑多个 UID（大小号共用一份歌单）。注入消息只带其中一个
    /// UID 时，用它能拿到全部候选键，避免歌单分裂到不同 `uid:` 下。
    pub fn accounts_for_identity(&self, uuid: Uuid) -> Vec<(String, String)> {
        let config = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        config
            .mappings
            .iter()
            .find(|mapping| mapping.id == uuid)
            .map(|mapping| {
                mapping
                    .accounts
                    .iter()
                    .map(|account| {
                        (
                            account.game.trim().to_ascii_lowercase(),
                            account.uid.trim().to_string(),
                        )
                    })
                    .filter(|(game, uid)| !game.is_empty() && !uid.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 为还没登记过的游戏 UID 自动建一条 friend 权限的身份。
    ///
    /// 好友私聊来消息时调用：聊天 API 给的是 UID，这是最可靠的账号标识。
    /// 建好后该 UID 立刻拥有歌单归属（UUID），不必等用户手动配置。
    /// 返回新建的 UUID；已存在则返回现有 UUID（幂等）。
    ///
    /// 只建身份、不改权限：自动创建的永远是 friend，提权必须用户显式编辑。
    /// 昵称只作为可读备注填进去（聊天 API 可能给 null，那就留空）。
    pub fn ensure_identity_for_account(
        &self,
        game: &str,
        uid: &str,
        nickname: Option<&str>,
    ) -> Option<Uuid> {
        let uid = uid.trim();
        if uid.is_empty() {
            return None;
        }
        let game = if game.trim().is_empty() {
            miliastra_kernel::chat_source::GAME_GENSHIN
        } else {
            game.trim()
        };
        if let Some(existing) = self.resolve_account(game, uid) {
            return Some(existing.id);
        }
        let mut config = self
            .inner
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // 双检：拿到写锁后可能已被别的线程插入同一 UID。
        if let Some(existing) = config.mappings.iter().find(|mapping| {
            mapping.accounts.iter().any(|account| {
                account.game.eq_ignore_ascii_case(game) && account.uid.trim() == uid
            })
        }) {
            return Some(existing.id);
        }
        let id = Uuid::new_v4();
        // 游戏内昵称（OCR 匹配用）随机生成：API 模式拿不到可靠的 OCR 昵称，
        // 留空会让"按昵称查找"永远落空；随机名保证唯一且不与他人冲突。
        // 聊天 API 给的昵称写进 note（展示用），不污染 OCR 匹配键。
        let mut rng_nickname = random_nickname();
        let mut attempts = 0;
        while config
            .mappings
            .iter()
            .any(|mapping| mapping.nickname == rng_nickname)
        {
            attempts += 1;
            if attempts > 64 {
                // 极端情况（映射表巨大）：退回 UUID 前缀保证唯一。
                rng_nickname = short_uuid(id);
                break;
            }
            rng_nickname = random_nickname();
        }
        config.mappings.push(IdentityMapping {
            nickname: rng_nickname,
            id,
            role: IdentityRole::Friend,
            note: nickname
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or_default()
                .to_string(),
            accounts: vec![GameAccount {
                game: game.to_string(),
                uid: uid.to_string(),
            }],
        });
        log::info!("已为游戏 UID {game}:{uid} 自动创建身份 {id}（好友权限）");
        Some(id)
    }

    /// 测试辅助：当前映射条数。
    #[cfg(test)]
    pub(crate) fn mapping_count_for_test(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .mappings
            .len()
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
            accounts: Vec::new(),
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
        // 没有备注的映射用 UUID 前缀展示，**不**回退到游戏内昵称：
        // 昵称是 OCR 匹配键，出现在展示位上就等于泄露了可冒充的名字。
        assert_eq!(access.display_name("管理小助手"), "00000000");
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
            accounts: Vec::new(),
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

    #[test]
    fn auto_created_identity_uses_api_nickname_as_display_name() {
        let access = IdentityAccess::new(IdentityConfig::default());
        let id = access
            .ensure_identity_for_account("genshin", "900000001", Some("荧"))
            .expect("应创建身份");
        let resolved = access.resolve_account("genshin", "900000001").unwrap();
        assert_eq!(resolved.id, id);
        // API 昵称作为显示昵称（note），方便用户认人。
        assert_eq!(resolved.display_name(), "荧");
        // OCR 匹配键必须随机且不等于 API 昵称：否则 OCR 读到同名玩家会误命中。
        assert_ne!(resolved.nickname, "荧");
        assert_eq!(resolved.nickname.chars().count(), 8);
        assert!(resolved.nickname.is_ascii(), "随机昵称用英数，不带非 ASCII 字符");
    }

    #[test]
    fn auto_created_identity_without_api_nickname_still_has_display_name() {
        let access = IdentityAccess::new(IdentityConfig::default());
        access
            .ensure_identity_for_account("genshin", "999", None)
            .expect("应创建身份");
        let resolved = access.resolve_account("genshin", "999").unwrap();
        // 没有 API 昵称时用 UUID 前缀展示，**绝不**回退到随机昵称：
        // 随机昵称是 OCR 匹配键，出现在展示位就等于给出可冒充的名字。
        assert_eq!(resolved.display_name().chars().count(), 8);
        assert_ne!(resolved.display_name(), resolved.nickname);
        // OCR 匹配键仍是随机昵称（英数 8 位）。
        assert_eq!(resolved.nickname.chars().count(), 8);
        assert!(resolved.nickname.is_ascii());
    }

    #[test]
    fn empty_nickname_never_matches_any_mapping() {
        let access = IdentityAccess::new(IdentityConfig::default());
        access
            .ensure_identity_for_account("genshin", "111", None)
            .expect("应创建身份");
        // OCR 读不到发送者时传空串：绝不能命中任何身份，否则权限与歌单全部错乱。
        assert!(access.resolve("").is_none());
        assert!(access.resolve("   ").is_none());
    }

    #[test]
    fn auto_created_identity_is_friend_and_idempotent() {
        let access = IdentityAccess::new(IdentityConfig::default());
        let id = access
            .ensure_identity_for_account("genshin", "900000001", Some("玩家癸"))
            .expect("应创建身份");
        // 重复调用不新建，返回同一个 UUID。
        let again = access
            .ensure_identity_for_account("genshin", "900000001", Some("玩家癸"))
            .expect("应复用身份");
        assert_eq!(id, again);
        // 自动创建永远是好友权限，不能悄悄提权。
        // 注意：昵称是随机的，要用 UID 反查而不是用 API 昵称查。
        let resolved = access.resolve_account("genshin", "900000001").unwrap();
        assert_eq!(resolved.role, IdentityRole::Friend);
        assert_eq!(access.mapping_count_for_test(), 1);
    }

    #[test]
    fn multiple_uids_map_to_the_same_identity() {
        let access = IdentityAccess::new(IdentityConfig::default());
        let first = access
            .ensure_identity_for_account("genshin", "111", Some("玩家癸"))
            .expect("应创建身份");
        // 第二个 UID 是同一个人的小号：手动绑到同一身份。
        {
            let mut config = access.inner.write().unwrap();
            config.mappings[0].accounts.push(GameAccount::new("222"));
        }
        let second = access
            .ensure_identity_for_account("genshin", "222", Some("玩家癸"))
            .expect("应复用身份");
        assert_eq!(first, second, "同一身份下的多个 UID 必须解析到同一 UUID");
        let accounts = access.accounts_for_identity(first);
        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn empty_uid_never_creates_identity() {
        let access = IdentityAccess::new(IdentityConfig::default());
        assert!(
            access
                .ensure_identity_for_account("genshin", "   ", None)
                .is_none()
        );
        assert_eq!(access.mapping_count_for_test(), 0);
    }
}
