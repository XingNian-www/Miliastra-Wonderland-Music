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

/// 一条身份映射。
///
/// 五个值各司其职，**不要互相替代**（见 identity-name-display-and-permission.md）：
///
/// | 字段 | 是什么 | 用在哪 |
/// | --- | --- | --- |
/// | `id` | 系统维护的 UUID | 归属键（记忆/歌单/牌局） |
/// | `remark` | **游戏内好友备注串** = 屏幕显示的名字 | OCR 精确匹配、好友列表搜索 |
/// | `real_nickname` | 从 ChatBridge 拿到的玩家自设昵称 | 仅展示/搜索兜底 |
/// | `display_name` | **软件里显示的昵称**（可空） | 玩家可见文案（「XXX 的歌单」「XXX 出牌」） |
/// | `accounts` | 绑定的游戏 UID | 授权与归属的优先依据 |
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityMapping {
    /// **备注串**：游戏内好友备注，也是屏幕显示的名字与 OCR 精确匹配键。
    ///
    /// 自动注册时随机生成，需要用户把它设成游戏内好友备注才会出现在屏幕上；
    /// ChatBridge 的 `remarks` 补到时会覆盖它（`remark_from_chatbridge=true`）。
    /// 可留空：好友私聊首次出现时可能只建了一条只有 UUID 的映射。
    ///
    /// **JSON 键名仍保留旧名 `nickname`**（`rename`）：配置库里的存量数据
    /// 与 `validate_persisted_shape` 都按旧键校验，改键名会让旧库启动失败。
    #[serde(default, rename = "nickname")]
    pub remark: String,
    /// 系统维护的映射标识。
    pub id: Uuid,
    pub role: IdentityRole,
    /// **软件中显示昵称**：玩家可见文案优先用它，**可以为空**。
    ///
    /// 只用于展示（「XXX 的歌单」「XXX 出牌」），不参与授权、不参与 OCR 匹配、
    /// 不用于搜索好友。空时回退 `real_nickname`，再空则用占位「玩家」。
    /// JSON 键名保留旧名 `note`，理由同上。
    #[serde(default, rename = "note")]
    pub display_name: String,
    /// **当前获取的真实昵称**：ChatBridge `/names.names` 的最近值，可以为空。
    ///
    /// 这是玩家自己随时能改的昵称，**只能做展示与搜索兜底**；
    /// 一旦进入授权或 OCR 匹配键，就等于把"改名冒充"的漏洞放回来。
    #[serde(default)]
    pub real_nickname: String,
    /// `remark` 是否来自 ChatBridge 的好友备注。
    ///
    /// 只表示来源，**不能**用它判断 remark 是不是随机串——手填的真名同样是 false。
    /// 用于让 `replace_display_names` 跳过玩家真实备注。
    /// JSON 键名保留旧名 `remark_sourced`，理由同上。
    #[serde(default, rename = "remark_sourced")]
    pub remark_from_chatbridge: bool,
    /// 该身份绑定的游戏账号。
    ///
    /// 用列表而不是单个可选值：一个玩家常有多个小号，它们该共用同一份
    /// 身份与歌单。UID 参与歌单归属键（优先级最高），因此这里填了就能
    /// 跨改名稳定定位；留空则退化到备注串键。
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
        let mut remarks = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        let mut accounts = std::collections::HashSet::new();
        let mut owners = 0usize;
        for mapping in &self.mappings {
            // 空备注串允许存在（自建身份还没绑定备注），但只能有一条：
            // 多条空备注串会让"按备注串查找"永远落空，也失去可读性。
            //
            // 只校验 remark：real_nickname 是玩家可改的昵称，允许重复；
            // display_name 是软件内标签，也允许重复。
            if !mapping.remark.trim().is_empty() && !remarks.insert(mapping.remark.as_str()) {
                bail!("identity.mappings 备注串重复: {}", mapping.remark);
            }
            if !ids.insert(mapping.id) {
                bail!("identity.mappings UUID 重复: {}", mapping.id);
            }
            for account in &mapping.accounts {
                if account.uid.trim().is_empty() {
                    bail!(
                        "identity.mappings 账号 UID 不能为空（备注串: {}）",
                        mapping.remark
                    );
                }
                if !accounts.insert((
                    account.game.trim().to_ascii_lowercase(),
                    account.uid.trim().to_string(),
                )) {
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

/// 玩家可见文案在三个来源都为空时的占位。
///
/// **绝不能用 UUID 前缀兜底**：那是内部键，出现在玩家可见文本里既难读也违反隐私约定。
pub(crate) const PLAYER_NAME_PLACEHOLDER: &str = "玩家";

/// 解析结果：稳定 UUID、角色与三个名字。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub id: Uuid,
    pub role: IdentityRole,
    /// 备注串（屏幕显示名 / OCR 匹配键）。
    pub remark: String,
    /// 软件中显示昵称（可空）。
    pub display_name: String,
    /// 当前获取的真实昵称（可空）。
    pub real_nickname: String,
}

impl ResolvedIdentity {
    /// **玩家可见文案的展示名**：软件中显示昵称 → 真实昵称 → 占位「玩家」。
    ///
    /// 不包含备注串：它是玩家特意设的随机串（防被认出），打到玩家面前既无意义又难看。
    pub fn player_name(&self) -> &str {
        let display = self.display_name.trim();
        if !display.is_empty() {
            return display;
        }
        let real = self.real_nickname.trim();
        if !real.is_empty() {
            return real;
        }
        PLAYER_NAME_PLACEHOLDER
    }
}

/// 生成随机的"游戏内昵称"。
///
/// 与面板的 `randomRemarkNickname` 用同一套字符集：自动创建的身份需要一个
/// 唯一且不与真实玩家冲突的 OCR 匹配键。用固定字符集而不是 UUID 片段，
/// 是为了在日志里读起来仍像一个昵称。
fn random_nickname() -> String {
    // 与 Web 面板 `REMARK_CHARSET` 完全一致：35 个易辨识的英数字母，
    // 去掉了 I/l/0/O 等易混淆字符。
    //
    // 长度取 6：游戏内好友备注有长度上限，8 位在部分昵称上会超。
    // 6 位 → 35^6 ≈ 1.8e9，对「随机占位、不与他人冲突」这个用途足够；
    // 真名由 ChatBridge 的 remarks 补上，占位串只是等待期的临时键。
    // 与面板 `randomRemarkNickname` 的字节数保持一致。
    const CHARSET: &[u8] = b"347ACDEFGHKNPQRTUVWXYacdefhknprtuwxy";
    const LEN: usize = 6;
    let bytes = uuid::Uuid::new_v4().as_u128().to_le_bytes();
    let mut out = String::with_capacity(LEN);
    for index in 0..LEN {
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

    /// 调试用：幂等登记一个模拟身份。
    ///
    /// 只操作 `__sim_` 保留昵称与 990_xxx 段 UID：真实映射不受影响，
    /// 用户也不会在面板里看到这些条目（面板读的是配置库，这里只改内存）。
    ///
    /// 每次调用都要重登是因为配置重载会用库里的值整体覆盖 `IdentityAccess`。
    pub fn ensure_simulation_mapping(&self, nickname: &str, uid: u32, role: IdentityRole) {
        let uid = uid.to_string();
        let mut config = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let existing = config.mappings.iter_mut().find(|mapping| {
            mapping
                .accounts
                .iter()
                .any(|account| account.uid.trim() == uid)
        });
        match existing {
            Some(mapping) => mapping.role = role,
            None => config.mappings.push(IdentityMapping {
                remark: nickname.to_string(),
                id: Uuid::new_v4(),
                role,
                display_name: "模拟命令（调试）".to_string(),
                real_nickname: String::new(),
                remark_from_chatbridge: false,
                accounts: vec![GameAccount {
                    game: miliastra_kernel::chat_source::GAME_GENSHIN.to_string(),
                    uid,
                }],
            }),
        }
    }
    /// 当前映射表的快照。
    ///
    /// 自动为好友 UID 建身份后需要把它写回配置库；不取快照就没法持久化，
    /// 那条映射只能活到下一次配置重载。
    pub fn snapshot(&self) -> IdentityConfig {
        self.inner
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        // 只归一化存下来的那一侧：面板粘贴/手改可能带首尾空白，
        // 而 OCR 读出来是干净的，不 trim 就永远匹配不上。
        // 输入侧仍精确匹配（尾部空白不模糊匹配是既有语义）。
        config
            .mappings
            .iter()
            .find(|mapping| mapping.remark.trim() == nickname)
            .map(|mapping| ResolvedIdentity {
                id: mapping.id,
                role: mapping.role,
                remark: mapping.remark.clone(),
                display_name: mapping.display_name.clone(),
                real_nickname: mapping.real_nickname.clone(),
            })
    }

    /// 按线索解析身份：两条链路共用的唯一入口。
    ///
    /// **有 UID 时只认 UID，绝不回退到昵称。**
    ///
    /// 账号 ID 改不了、不会重名；昵称可以被玩家改成任意名字。若 UID 未登记就
    /// 退回按昵称解析，攻击者只要把昵称改成与某个已登记 Owner/Admin 相同、
    /// 而自己的 UID 从未登记，就能在带 UID 的消息路径（外部聊天源）拿到那个角色。
    /// 这违反 MUST_READ §2「未确认发送者不能用于授权」。
    ///
    /// 昵称只在**完全没有 UID** 时使用：OCR 路径拿不到 UID，只能靠昵称。
    ///
    /// 代价：外部源里「UID 换了、昵称没变」的已登记玩家会被判为未登记。
    /// 这是刻意的方向选择——宁可让旧配置失效，也不让改昵称就能提权。
    pub fn resolve_hint(&self, hint: IdentityHint<'_>) -> Option<ResolvedIdentity> {
        match hint {
            // **只匹配 `nickname`（好友备注），绝不回退到 `note`（游戏真名）。**
            //
            // `nickname` 是"你给对方起的备注"，玩家自己改不了；`note` 是游戏
            // 真名，任何人都能把自己的昵称改成一样来冒充。所以名字只能用于
            // 归属（记忆、歌单），授权一律靠 UID。
            //
            // 曾为了让"大厅 OCR 读到真名也能认出管理员"加过 note 兜底，
            // 结果 moderation 的防冒充测试立刻变红：伪造的 requester 拿到了
            // Admin。这正是本节开头那段注释说的方向，不能破。
            IdentityHint::Nickname(name) => self.resolve(name),
            // **有 UID 时只认 UID**：UID 是最可靠的账号标识，
            // 回退到名字会让"改个昵称就顶替别人身份"成为可能。
            IdentityHint::Account { game, uid, .. } => self.resolve_account(game, uid),
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
                    account.game.trim().to_ascii_lowercase() == game && account.uid.trim() == uid
                })
                .then(|| ResolvedIdentity {
                    id: mapping.id,
                    role: mapping.role,
                    remark: mapping.remark.clone(),
                    display_name: mapping.display_name.clone(),
                    real_nickname: mapping.real_nickname.clone(),
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
        let mut config = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        // 双检：拿到写锁后可能已被别的线程插入同一 UID。
        if let Some(existing) = config.mappings.iter().find(|mapping| {
            mapping
                .accounts
                .iter()
                .any(|account| account.game.eq_ignore_ascii_case(game) && account.uid.trim() == uid)
        }) {
            return Some(existing.id);
        }
        let id = Uuid::new_v4();
        // 备注串（OCR 匹配用）随机生成：API 模式拿不到游戏内备注，留空会让
        // "按备注串查找"永远落空；随机串保证唯一且不与他人冲突。
        //
        // **这个随机串需要用户去游戏里设成好友备注**，否则它不会出现在屏幕上，
        // OCR 也就永远匹配不到（只有 ChatBridge 的 UID 路径能用）。
        // 聊天 API 给的昵称写进 real_nickname（展示/搜索兜底），不污染匹配键。
        let mut rng_nickname = random_nickname();
        let mut attempts = 0;
        while config
            .mappings
            .iter()
            .any(|mapping| mapping.remark == rng_nickname)
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
            remark: rng_nickname,
            id,
            role: IdentityRole::Friend,
            // 软件中显示昵称留空：等用户填一个自己认得出的名字；
            // 空时展示回退到 real_nickname，不会外泄内部键。
            display_name: String::new(),
            // 聊天 API 给的玩家自设昵称：只做展示与搜索兜底。
            real_nickname: nickname
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or_default()
                .to_string(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount {
                game: game.to_string(),
                uid: uid.to_string(),
            }],
        });
        // 不打印 UID：它是真实玩家数据（MUST_READ §9）。
        log::info!(
            "已为一位好友自动创建身份（好友权限）；请把面板里该身份的备注串设为游戏内好友备注，OCR 才能识别，否则只有聊天桥可用"
        );
        Some(id)
    }

    /// 记录 ChatBridge 补查到的**真实昵称**。
    ///
    /// 写进 `real_nickname`（玩家自设昵称），**不动 `display_name`**：
    /// 后者是用户在面板里填的软件显示名，不该被自动值覆盖。
    /// 玩家随时能改昵称，所以这里每次都更新为最新值。
    pub fn record_real_nickname(&self, game: &str, uid: &str, nickname: &str) -> bool {
        let nickname = nickname.trim();
        let uid = uid.trim();
        if nickname.is_empty() || uid.is_empty() {
            return false;
        }
        let mut config = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        let Some(mapping) = config.mappings.iter_mut().find(|mapping| {
            mapping
                .accounts
                .iter()
                .any(|account| account.game.eq_ignore_ascii_case(game) && account.uid.trim() == uid)
        }) else {
            return false;
        };
        if mapping.real_nickname == nickname {
            return false;
        }
        mapping.real_nickname = nickname.to_string();
        true
    }

    /// 用 ChatBridge 的备注覆盖**备注串**（OCR 匹配键）。
    ///
    /// `remarks` 是玩家在游戏里设的好友备注，也正是屏幕显示的名字，
    /// 所以它才是 `remark` 的正主：有就用，没有则不动。
    ///
    /// 唯一性由这里保证——备注串重复会让 `validate` 直接拒绝整份配置，
    /// 所以撞名时放弃写入并返回 false，交给调用方保留原值。
    pub fn set_account_remark(&self, game: &str, uid: &str, remark: &str) -> bool {
        let remark = remark.trim();
        let uid = uid.trim();
        if remark.is_empty() || uid.is_empty() {
            return false;
        }
        let mut config = self.inner.write().unwrap_or_else(PoisonError::into_inner);
        // 昵称必须全局唯一：已被**别人**占用就不写，避免把配置搞成非法。
        let taken_by_other = config.mappings.iter().any(|mapping| {
            mapping.remark.trim() == remark
                && !mapping.accounts.iter().any(|account| {
                    account.game.eq_ignore_ascii_case(game) && account.uid.trim() == uid
                })
        });
        if taken_by_other {
            log::warn!(
                "外部聊天源给出的备注「{}」与已登记的备注串重复，该好友仍保留原匹配键，不覆盖",
                remark
            );
            return false;
        }
        let Some(mapping) = config.mappings.iter_mut().find(|mapping| {
            mapping
                .accounts
                .iter()
                .any(|account| account.game.eq_ignore_ascii_case(game) && account.uid.trim() == uid)
        }) else {
            return false;
        };
        if mapping.remark == remark {
            return false;
        }
        mapping.remark = remark.to_string();
        mapping.remark_from_chatbridge = true;
        // **不写 display_name**：用户明确要求「remarks 只能覆盖备注串」。
        // display_name 是软件内展示标签，与匹配键绑死会让两者悄悄分叉
        // （以后备注改名，展示名仍停在旧值）。
        true
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

    /// **玩家可见文案的展示名**：软件中显示昵称 → 真实昵称 → 占位「玩家」。
    ///
    /// 有账号时先按 UID 解析：外部聊天源的 `from.name` 是资料包昵称，
    /// 与屏幕名无关，只按名字查会落空。
    /// 解析不到身份时退回传入的名字（OCR 文本 / `from.name`）。
    pub fn player_name(&self, name: &str, account: Option<(&str, &str)>) -> String {
        // 反查顺序与归属链一致（备注串 → 真实昵称 → 软件显示名），
        // 避免"同一个人归属能命中、展示却命中不了"。
        let identity = account
            .and_then(|(game, uid)| self.resolve_account(game, uid))
            .or_else(|| self.resolve(name))
            .or_else(|| self.resolve_by_real_nickname(name))
            .or_else(|| self.resolve_by_display_name(name));
        match identity {
            Some(identity) => identity.player_name().to_owned(),
            None => name.to_owned(),
        }
    }

    /// 名字对应的玩家可见展示名（面板、日志、BOT 称呼用它）。
    pub fn player_name_for_name(&self, name: &str) -> String {
        self.player_name(name, None)
    }

    /// 好友搜索名候选：**备注串 → 真实昵称**，去重后返回。
    ///
    /// 屏幕显示的是备注串（无备注时是玩家自设昵称），所以只试这两个名字；
    /// 软件显示名与 UUID 都不该拿去搜好友列表。
    fn search_names_of(identity: &ResolvedIdentity) -> Vec<String> {
        let mut names = Vec::with_capacity(2);
        for candidate in [identity.remark.trim(), identity.real_nickname.trim()] {
            if candidate.is_empty() || names.iter().any(|existing| existing == candidate) {
                continue;
            }
            names.push(candidate.to_owned());
        }
        names
    }

    /// 按三档名字解析身份：命中唯一返回 Ok(Some)，某一档命中多条返回 Err（歧义）。
    ///
    /// 显示名与真实昵称都允许重名，猜错就是把私聊发给另一个人。
    #[allow(clippy::result_unit_err)]
    fn resolve_by_any_name(&self, name: &str) -> std::result::Result<Option<ResolvedIdentity>, ()> {
        fn count(mappings: &[IdentityMapping], predicate: impl Fn(&IdentityMapping) -> bool) -> usize {
            mappings.iter().filter(|mapping| predicate(mapping)).count()
        }
        let config = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        let mappings = config.mappings.as_slice();
        let remark = count(mappings, |mapping| mapping.remark.trim() == name);
        if remark > 1 {
            return Err(());
        }
        if remark == 1 {
            return Ok(mappings
                .iter()
                .find(|mapping| mapping.remark.trim() == name)
                .map(Self::resolved_from_mapping));
        }
        let display = count(mappings, |mapping| mapping.display_name.trim() == name);
        if display > 1 {
            return Err(());
        }
        if display == 1 {
            return Ok(mappings
                .iter()
                .find(|mapping| mapping.display_name.trim() == name)
                .map(Self::resolved_from_mapping));
        }
        let real = count(mappings, |mapping| mapping.real_nickname.trim() == name);
        if real > 1 {
            return Err(());
        }
        if real == 1 {
            return Ok(mappings
                .iter()
                .find(|mapping| mapping.real_nickname.trim() == name)
                .map(Self::resolved_from_mapping));
        }
        Ok(None)
    }

    /// **好友列表搜索名的唯一入口**：所有 UI 投递都只认这一份候选。
    ///
    /// `who` 是「要找谁」，可以是任意已知标识：内部身份键（UUID）、备注串、
    /// 软件显示名或真实昵称；`account` 是外部聊天源给的真实账号。
    ///
    /// 输出按屏幕显示优先级去重：备注串 → 真实昵称。三条边界：
    /// - 传了账号但没登记：返回空表，让调用方告警——`who` 这时可能是 UID，
    ///   拿去搜好友列表既搜不到也不该写进日志；
    /// - 名字命中多条不同身份：返回空表，宁可报找不到人也不发给另一个人；
    /// - 完全解析不到且没有账号：退回 `who` 本身，UI 仍有一次机会（但 UUID 除外）。
    pub fn friend_search_names(&self, who: &str, account: Option<(&str, &str)>) -> Vec<String> {
        let who = who.trim();
        if let Some((game, uid)) = account {
            return self
                .resolve_account(game, uid)
                .map(|identity| Self::search_names_of(&identity))
                .unwrap_or_default();
        }
        if let Ok(uuid) = Uuid::parse_str(who) {
            let config = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            return config
                .mappings
                .iter()
                .find(|mapping| mapping.id == uuid)
                .map(Self::resolved_from_mapping)
                .map(|identity| Self::search_names_of(&identity))
                .unwrap_or_default();
        }
        match self.resolve_by_any_name(who) {
            Ok(Some(identity)) => Self::search_names_of(&identity),
            Ok(None) if who.is_empty() => Vec::new(),
            Ok(None) => vec![who.to_owned()],
            Err(()) => Vec::new(),
        }
    }
    /// 记忆归属键：**UUID 优先，未登记才退回展示名**。
    ///
    /// UUID 是唯一身份码，跨改名、跨显示名变化都稳定。
    /// 外部源给的昵称依次按「备注串 → 真实昵称 → 软件显示名」试：
    /// ChatBridge 的 `from.name` 是真实昵称，OCR 文本是备注串，面板回传的是显示名。
    /// 未登记的玩家没有 UUID，只能用展示名当键（既有局限）。
    pub fn memory_key(&self, name: &str) -> String {
        self.resolve(name)
            .or_else(|| self.resolve_by_real_nickname(name))
            .or_else(|| self.resolve_by_display_name(name))
            .map(|identity| identity.id.to_string())
            .unwrap_or_else(|| self.player_name_for_name(name))
    }

    /// **归属专用**：按「备注串 → 真实昵称 → 软件显示名」解析出身份 UUID。
    ///
    /// 给「歌单归属、记忆、队列」这类**归属**用途准备；**绝不用于授权**。
    /// 所以这里**只返回 UUID，不返回 role**：拿不到角色就不可能被误用成授权判据。
    ///
    /// # 为什么不让 `resolve_hint` 支持 display_name 兜底
    ///
    /// `resolve_hint` 是**授权**路径（`router.rs` 的 `role_for_envelope`、
    /// `listener` 的 `role_of_sender` 都走它）。软件显示名是手填/自动值，
    /// 任何人都能把自己的昵称改成一样来冒充；一旦让 `resolve_hint` 回退，
    /// 「改名冒充管理员」的漏洞就回来了——`moderation` 的防冒充测试会立刻变红。
    pub fn ownership_uuid_by_name(&self, name: &str) -> Option<Uuid> {
        // 顺序：备注串（对方改不了）→ 真实昵称（外部源的 from.name）→
        // 软件显示名（面板里手填的标签，只做最后的兜底）。
        self.resolve(name)
            .or_else(|| self.resolve_by_real_nickname(name))
            .or_else(|| self.resolve_by_display_name(name))
            .map(|identity| identity.id)
    }

    /// 由记忆键反查展示名：键是 UUID 时找对应身份，否则原样返回。
    pub fn player_name_for_key(&self, key: &str) -> String {
        if let Ok(uuid) = Uuid::parse_str(key) {
            let config = self.inner.read().unwrap_or_else(PoisonError::into_inner);
            if let Some(mapping) = config.mappings.iter().find(|mapping| mapping.id == uuid) {
                return ResolvedIdentity {
                    id: mapping.id,
                    role: mapping.role,
                    remark: mapping.remark.clone(),
                    display_name: mapping.display_name.clone(),
                    real_nickname: mapping.real_nickname.clone(),
                }
                .player_name()
                .to_owned();
            }
        }
        key.to_owned()
    }

    /// 按**软件显示名**反查身份（归属用途；不参与授权）。
    fn resolve_by_display_name(&self, name: &str) -> Option<ResolvedIdentity> {
        if name.trim().is_empty() {
            return None;
        }
        let config = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        config
            .mappings
            .iter()
            .find(|mapping| mapping.display_name.trim() == name.trim())
            .map(Self::resolved_from_mapping)
    }

    /// 按**真实昵称**反查身份。
    ///
    /// 只用于投递层把展示名换回屏幕名；真实昵称可被玩家随意改，**不参与授权**。
    fn resolve_by_real_nickname(&self, name: &str) -> Option<ResolvedIdentity> {
        if name.trim().is_empty() {
            return None;
        }
        let config = self.inner.read().unwrap_or_else(PoisonError::into_inner);
        config
            .mappings
            .iter()
            .find(|mapping| mapping.real_nickname.trim() == name.trim())
            .map(Self::resolved_from_mapping)
    }

    fn resolved_from_mapping(mapping: &IdentityMapping) -> ResolvedIdentity {
        ResolvedIdentity {
            id: mapping.id,
            role: mapping.role,
            remark: mapping.remark.clone(),
            display_name: mapping.display_name.clone(),
            real_nickname: mapping.real_nickname.clone(),
        }
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
                // 备注串来自 ChatBridge 时不要改写：那是屏幕上真实显示的名字，
                // 出站文本里出现它就该原样保留。
                if mapping.remark_from_chatbridge {
                    return None;
                }
                // 展示名走统一展示链：显示名 → 真实昵称。
                // 原来只认 display_name，它一为空（自动注册的常见形态）就换不掉，
                // 随机备注串会被原样念给全大厅。两者都为空时保持原文，
                // 不能用占位「玩家」去替换——那会把句子改得莫名其妙。
                let display = mapping.display_name.trim();
                let display = if display.is_empty() {
                    mapping.real_nickname.trim()
                } else {
                    display
                };
                (!mapping.remark.trim().is_empty() && !display.is_empty())
                    .then(|| (mapping.remark.clone(), display.to_owned()))
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

    /// 发送者的角色：有账号时按 UID 解析，否则按昵称。
    ///
    /// **必须与路由期用同一个判定**。路由期按 UID 认出了 Owner/Admin，
    /// 执行期若退回只按昵称（外部源的昵称是随机 OCR 匹配键），就会
    /// 「路由说有权限、执行说没有」，命令被静默丢弃。
    pub fn role_of_sender(
        &self,
        nickname: &str,
        account: Option<&miliastra_kernel::chat_source::CommandAccount>,
    ) -> Option<IdentityRole> {
        let hint = match account {
            Some(account) => IdentityHint::Account {
                game: &account.game,
                uid: &account.uid,
                nickname: Some(nickname),
            },
            None => IdentityHint::Nickname(nickname),
        };
        self.resolve_hint(hint).map(|resolved| resolved.role)
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

    fn mapping(remark: &str, id: Uuid, role: IdentityRole) -> IdentityMapping {
        IdentityMapping {
            remark: remark.to_owned(),
            id,
            role,
            real_nickname: String::new(),
            display_name: String::new(),
            remark_from_chatbridge: false,
            accounts: Vec::new(),
        }
    }

    fn sample_config() -> IdentityConfig {
        let mut owner = mapping("派蒙本蒙", Uuid::from_u128(1), IdentityRole::Owner);
        owner.display_name = "主人备注".to_string();
        IdentityConfig {
            mappings: vec![
                owner,
                mapping("管理小助手", Uuid::from_u128(2), IdentityRole::Admin),
                mapping("好友甲", Uuid::from_u128(3), IdentityRole::Friend),
            ],
        }
    }

    /// 回归：外部源的发起人只有 UID 时，邀请必须能搜到好友列表里的名字。
    ///
    /// 实机故障：`execute_invite_intent` 把信封主语（= UID）当昵称传下去，
    /// 好友列表上显示的是昵称/备注，`recognized == "900000001"` 永假，
    /// 报 `no unique stable friend row`。
    ///
    /// UID 反查得到的候选是「备注串 → 真实昵称」：备注串是屏幕显示名，
    /// 没有备注时屏幕上显示的就是玩家自设昵称。
    #[test]
    fn invite_searches_the_remark_and_real_nickname_for_a_uid_initiator() {
        use crate::features::identity::{
            GameAccount, IdentityAccess, IdentityConfig, IdentityMapping, IdentityRole,
        };

        let identity = IdentityAccess::new(IdentityConfig {
            mappings: vec![IdentityMapping {
                // 备注串是屏幕显示名；显示名只是软件内标签，不参与搜索
                remark: "玩家丙".to_string(),
                id: uuid::Uuid::new_v4(),
                role: IdentityRole::Friend,
                real_nickname: String::new(),
                display_name: "厅主".to_string(),
                remark_from_chatbridge: false,
                accounts: vec![GameAccount {
                    game: miliastra_kernel::chat_source::GAME_GENSHIN.to_string(),
                    uid: "900000001".to_string(),
                }],
            }],
        });

        let names = identity.friend_search_names(
            "900000001",
            Some((miliastra_kernel::chat_source::GAME_GENSHIN, "900000001")),
        );
        // 好友列表显示的是备注串，所以搜索名必须有它。
        assert_eq!(names, vec!["玩家丙".to_string()], "搜索名应只含备注串");
        // 软件显示名只做文案，**不能**拿去搜好友列表。
        assert!(!names.iter().any(|name| name == "厅主"), "显示名不参与搜索");
        assert!(!names.iter().any(|name| name == "900000001"), "UID 不参与搜索");
    }

    /// 未登记的 UID 拿不到任何候选名：调用方应回退到信封主语并在进 UI 前告警。
    #[test]
    fn unknown_uid_yields_no_search_names() {
        let identity = crate::features::identity::IdentityAccess::new(Default::default());
        assert!(
            identity
                .friend_search_names(
                    "999999999",
                    Some((miliastra_kernel::chat_source::GAME_GENSHIN, "999999999")),
                )
                .is_empty()
        );
    }
    #[test]
    fn stored_remark_with_padding_still_matches_the_clean_ocr_name() {
        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            // 面板粘贴/手改可能带首尾空白；OCR 读出来是干净的。
            remark: "  abcd1234  ".to_owned(),
            id: Uuid::from_u128(21),
            role: IdentityRole::Friend,
            real_nickname: String::new(),
            display_name: String::new(),
            remark_from_chatbridge: false,
            accounts: Vec::new(),
        });
        let access = IdentityAccess::new(config);
        assert!(access.resolve("abcd1234").is_some(), "存下来的空白不该影响匹配");
        // 输入侧仍精确：多一个尾空格就不匹配（既有语义）。
        assert!(access.resolve("abcd1234 ").is_none());
    }

    #[test]
    fn resolve_matches_exactly_and_never_maps_unknown() {
        let access = IdentityAccess::new(sample_config());
        let owner = access.resolve("派蒙本蒙").expect("主人应被解析");
        assert_eq!(owner.id, Uuid::from_u128(1));
        assert_eq!(owner.role, IdentityRole::Owner);
        assert_eq!(owner.player_name(), "主人备注");
        // 没有软件显示名、也没有真实昵称时用固定占位：
        // **不**回退到备注串（随机串对玩家无意义），更不回退到 UUID。
        assert_eq!(
            access.player_name("管理小助手", None),
            PLAYER_NAME_PLACEHOLDER
        );
        assert_eq!(access.player_name("主人备注", None), "主人备注");
        assert_eq!(access.role_of("主人备注"), None);
        assert_eq!(access.player_name("陌生人", None), "陌生人");
        assert!(access.resolve("派蒙本蒙 ").is_none(), "尾部空白不模糊匹配");
        assert!(access.resolve("陌生人").is_none(), "未知昵称不映射");
    }

    #[test]
    fn replace_display_names_maps_longest_names_and_preserves_unknown_text() {
        let mut config = sample_config();
        config.mappings.push(IdentityMapping {
            remark: "好友".to_string(),
            id: Uuid::from_u128(4),
            role: IdentityRole::Friend,
            real_nickname: String::new(),
            display_name: "短备注".to_string(),
            remark_from_chatbridge: false,
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
    fn replace_display_names_falls_back_to_the_real_nickname() {
        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            remark: "abcd1234".to_owned(),
            id: Uuid::from_u128(9),
            role: IdentityRole::Friend,
            real_nickname: "真名甲".to_owned(),
            // 自动注册的常见形态：显示名留空，只有聊天桥给的真实昵称。
            display_name: String::new(),
            remark_from_chatbridge: false,
            accounts: Vec::new(),
        });
        let access = IdentityAccess::new(config);
        // 备注串不能出现在出站文本里；显示名为空时回退真实昵称，而不是原样放过。
        assert_eq!(
            access.replace_display_names("abcd1234 点了一首歌"),
            "真名甲 点了一首歌"
        );
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

        // 旧配置（键名 nickname / note / remark_sourced）必须能读进新字段：
        // 面板与数据库里都是旧键，改字段名不能把配置读挂。
        let legacy: IdentityMapping = serde_json::from_str(
            r#"{"nickname":"甲","note":"显示名甲","remark_sourced":true,"id":"00000000-0000-0000-0000-000000000001","role":"friend"}"#,
        )
        .unwrap();
        assert_eq!(legacy.remark, "甲", "旧 nickname 读进 remark");
        assert_eq!(legacy.display_name, "显示名甲", "旧 note 读进 display_name");
        assert!(legacy.remark_from_chatbridge, "旧 remark_sourced 读进新标记");
        assert!(legacy.real_nickname.is_empty(), "旧配置没有真实昵称");

        let without_display: IdentityMapping = serde_json::from_str(
            r#"{"nickname":"甲","id":"00000000-0000-0000-0000-000000000001","role":"friend"}"#,
        )
        .unwrap();
        assert!(without_display.display_name.is_empty(), "缺 display_name 按空串处理");
        assert!(without_display.real_nickname.is_empty(), "缺 real_nickname 按空串处理");

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

    /// 安全边界：有 UID 时绝不因 UID 未登记而回退到昵称授权。
    ///
    /// 否则攻击者把游戏昵称改成与已登记 Owner 相同、自己 UID 不登记，
    /// 就能在带 UID 的消息路径（外部聊天源）拿到 Owner 角色。

    /// 执行期权限复查必须与路由期同一判定：有账号按 UID，否则按昵称。
    ///
    /// 外部源的昵称是随机 OCR 匹配键，与 UID 绑定无关。旧实现只按昵称查，
    /// 于是「路由按 UID 判有权限、执行按昵称判没权限」，管理员命令被静默丢弃。

    /// 回归：歌单归属必须支持按 UID 解析。
    ///
    /// 外部聊天源的私聊 peer 是数字 UID，原来 playlist_owner_candidates 直接
    /// 调 resolve()（只按 nickname 精确匹配），UID 传进去必然落空，
    /// 已登记的玩家被判成未登记——线上表现为大厅能导入歌单、
    /// 好友私聊提示「该玩家未在身份映射登记」。

    /// 回归：记忆归属键是 UUID，展示名是显示昵称。
    ///
    /// 外部聊天源给的昵称是玩家当前的游戏内显示名（随时能改）。
    /// 用 UUID 当键，玩家怎么改名记忆都不会分裂；展示时再换回显示昵称。

    /// 回归：记忆键解析必须对 UUID 幂等，且能从显示名换回 UUID。
    ///
    /// 面板列表下发显示名，查询/删除/重建要的是记忆键（UUID）。
    /// 原来列表换成正向显示名后，入口没做反向解析——已登记玩家点开全是空，
    /// 删除返回 removed:false 却弹「已删除」。
    #[test]
    fn memory_key_round_trips_between_uuid_and_display_name() {
        let mut config = IdentityConfig::default();
        let id = Uuid::new_v4();
        config.mappings.push(IdentityMapping {
            remark: "nick_alpha".to_string(),
            id,
            role: IdentityRole::Friend,
            real_nickname: String::new(),
            display_name: "显示名甲".to_string(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount::new("10001")],
        });
        let access = IdentityAccess::new(config);
        let key = id.to_string();

        // 正向：键 → 显示名。
        assert_eq!(access.player_name_for_key(&key), "显示名甲");
        // 反向：显示名 → 键。
        assert_eq!(access.memory_key("显示名甲"), key);
        // 幂等：UUID 本身再过一遍仍是 UUID（面板回传 key 时走这条）。
        assert_eq!(access.memory_key(&key), key);
        // 游戏内昵称也能回到同一个键。
        assert_eq!(access.memory_key("nick_alpha"), key);
    }

    /// 未登记的名字原样返回：老库里有按昵称直接存的孤儿行。

    /// 回归：自动注册的身份必须对命令执行侧立即可见。
    ///
    /// 原来外部聊天源用 `IdentityAccess::new(config.identity.clone())` 建了一份
    /// **独立副本**，自动建的身份只进了那份副本，命令执行时查的却是
    /// `live_configs.identity`——同一个 UID 永远 role=None。
    /// 写库是对的，但写库不会反向刷新另一个内存实例。
    ///
    /// 这条测试钉住「两者是同一个句柄」这个前提：clone 出来的一侧写入，
    /// 另一侧必须立刻读到。
    #[test]
    fn identity_access_clone_shares_the_same_mappings() {
        let access = IdentityAccess::new(IdentityConfig::default());
        // 模拟 application.rs 的接线：外部源与 live_configs 共享同一句柄。
        let external_side = access.clone();

        let created =
            external_side.ensure_identity_for_account("genshin", "10001", Some("自动注册"));
        assert!(created.is_some(), "应当自动创建身份");

        // 命令执行侧（同一个句柄）必须立刻查得到。
        let resolved = access.resolve_account("genshin", "10001");
        assert!(
            resolved.is_some(),
            "clone 出来的 IdentityAccess 必须共享同一份映射，否则自动注册对命令侧不可见"
        );
        assert_eq!(resolved.unwrap().role, IdentityRole::Friend);
    }

    #[test]
    fn memory_key_leaves_unknown_names_untouched() {
        let access = IdentityAccess::new(IdentityConfig::default());
        assert_eq!(access.memory_key("路人甲"), "路人甲");
        // 已知 UUID 但映射里没有，同样原样返回（不会误改成别的键）。
        let orphan = "00000000-0000-4000-8000-000000000001";
        assert_eq!(access.memory_key(orphan), orphan);
    }

    #[test]
    fn memory_key_is_uuid_and_display_is_the_note() {
        let mut config = IdentityConfig::default();
        let id = Uuid::new_v4();
        config.mappings.push(IdentityMapping {
            remark: "nick_alpha".to_string(),
            id,
            role: IdentityRole::Admin,
            real_nickname: String::new(),
            display_name: "显示名甲".to_string(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount::new("10001")],
        });
        let access = IdentityAccess::new(config);

        // 命中 nickname 或 note，记忆键都是同一个 UUID。
        assert_eq!(access.memory_key("nick_alpha"), id.to_string());
        assert_eq!(access.memory_key("显示名甲"), id.to_string());
        // 展示名统一走展示链（显示名优先）。
        assert_eq!(access.player_name_for_name("nick_alpha"), "显示名甲");
        // 由 UUID 键反查展示名。
        assert_eq!(access.player_name_for_key(&id.to_string()), "显示名甲");
        // 未登记的玩家：键退回显示昵称本身。
        assert_eq!(access.memory_key("陌生人"), "陌生人");
    }


    /// 归属专用解析：note（游戏真名）能解析出 UUID，但**不返回角色**。
    ///
    /// 实机故障：`@歌单` 窗口里 `@1`~`@5` 选歌失败，日志显示
    /// 「发送者『示例玩家』（note）不是属主『id:00000000-…』」。
    /// `playlist_owner_candidates` 走 `resolve_hint`（只认 nickname），
    /// note 形式的发送者解析失败 → 候选为空 → 决策被丢弃。
    #[test]
    fn ownership_uuid_resolves_both_nickname_and_note() {
        let mut config = IdentityConfig::default();
        let id = Uuid::new_v4();
        config.mappings.push(IdentityMapping {
            remark: "abcd1234".to_string(),
            id,
            role: IdentityRole::Admin,
            real_nickname: String::new(),
            display_name: "显示名甲".to_string(),
            remark_from_chatbridge: true,
            accounts: vec![GameAccount::new("10001")],
        });
        let access = IdentityAccess::new(config);

        // 两个名字都指向同一个 UUID——归属需要这个性质。
        assert_eq!(access.ownership_uuid_by_name("abcd1234"), Some(id));
        assert_eq!(access.ownership_uuid_by_name("显示名甲"), Some(id));
        assert_eq!(access.ownership_uuid_by_name("陌生人"), None);
    }

    /// **安全不变量**：归属能用 note，授权不能。
    ///
    /// `ownership_uuid_by_name` 只返回 UUID、不返回 role；而授权路径走的
    /// `resolve_hint` 必须继续**只认 nickname / UID**。这条测试把两者钉在一起：
    /// 同一个 note 名字，归属能命中，授权必须落空。
    ///
    /// 若有人为了「让大厅认得出管理员」把 note 兜底上移到 `resolve_hint`，
    /// 这条会立刻变红。
    #[test]
    fn note_resolves_for_ownership_but_never_for_authorization() {
        let mut config = IdentityConfig::default();
        let id = Uuid::new_v4();
        config.mappings.push(IdentityMapping {
            remark: "abcd1234".to_string(),
            id,
            role: IdentityRole::Admin,
            real_nickname: String::new(),
            display_name: "显示名甲".to_string(),
            remark_from_chatbridge: true,
            accounts: vec![GameAccount::new("10001")],
        });
        let access = IdentityAccess::new(config);

        // 归属：note 命中。
        assert_eq!(access.ownership_uuid_by_name("显示名甲"), Some(id));

        // 授权：note 命中不了。nickname 分支只查 nickname，
        // Account 分支只查 UID——两条都不碰 note。
        assert_eq!(
            access.role_of("显示名甲"),
            None,
            "note 绝不能被当作授权依据：任何人都能把游戏昵称改成一样来冒充"
        );
        assert_eq!(
            access
                .resolve_hint(IdentityHint::Nickname("显示名甲"))
                .map(|resolved| resolved.role),
            None,
            "resolve_hint 的昵称分支必须只认 nickname"
        );
        assert_eq!(
            access
                .resolve_hint(IdentityHint::Account {
                    game: "genshin",
                    uid: "999999999",
                    nickname: Some("显示名甲"),
                })
                .map(|resolved| resolved.role),
            None,
            "UID 未登记时不得回退昵称或 note"
        );
    }
    /// 好友列表候选名以显示昵称为先。
    #[test]
    #[test]
    fn search_names_resolve_by_identity_key_not_just_names() {
        let mut config = IdentityConfig::default();
        let id = Uuid::new_v4();
        config.mappings.push(IdentityMapping {
            remark: "pGAxP".to_string(),
            id,
            role: IdentityRole::Friend,
            real_nickname: "真名甲".to_string(),
            display_name: "显示名甲".to_string(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount::new("10001")],
        });
        let access = IdentityAccess::new(config);

        // 收件人是内部键时必须认得出身份，不能拿 UUID 去搜好友列表。
        let by_key = access.friend_search_names(&id.to_string(), None);
        assert_eq!(by_key, vec!["pGAxP".to_owned(), "真名甲".to_owned()]);
        assert!(access.friend_search_names("", None).is_empty());
        assert!(access
            .friend_search_names("00000000-0000-4000-8000-0000000000ff", None)
            .is_empty());
    }

    #[test]
    fn ambiguous_display_names_are_refused_instead_of_guessed() {
        let mut config = IdentityConfig::default();
        for (remark, id) in [("aa11", Uuid::from_u128(11)), ("bb22", Uuid::from_u128(12))] {
            config.mappings.push(IdentityMapping {
                remark: remark.to_string(),
                id,
                role: IdentityRole::Friend,
                real_nickname: String::new(),
                display_name: "同名".to_string(),
                remark_from_chatbridge: false,
                accounts: Vec::new(),
            });
        }
        let access = IdentityAccess::new(config);

        // 显示名允许重名：命中两条不同身份时放弃，宁可报找不到人，
        // 也不能把私聊发给另一个同名玩家。
        assert!(access.friend_search_names("同名", None).is_empty());
        // 备注串唯一，仍能正常解析。
        assert_eq!(
            access.friend_search_names("aa11", None),
            vec!["aa11".to_owned()]
        );
    }

    #[test]
    fn search_names_use_the_remark_then_the_real_nickname() {
        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            remark: "nick_beta".to_string(),
            id: Uuid::new_v4(),
            role: IdentityRole::Friend,
            real_nickname: "真名乙".to_string(),
            display_name: "显示名乙".to_string(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount::new("10002")],
        });
        let access = IdentityAccess::new(config);
        // 备注串优先（屏幕显示的就是它），真实昵称兜底；显示名不进搜索。
        assert_eq!(
            access.friend_search_names("10002", Some(("genshin", "10002"))),
            vec!["nick_beta".to_owned(), "真名乙".to_owned()]
        );
    }

    /// 好友搜索候选：**备注串 → 真实昵称**，解析不到才退回传入的名字。
    ///
    /// 依据 identity-name-display-and-permission.md：屏幕显示的是备注串（remark），
    /// 软件显示名（display_name）只是面板标签、不会出现在好友列表上。
    /// 外部聊天源的 from.name 是资料包昵称，必须先用账号 UID 解析身份。
    #[test]
    fn friend_search_names_prefer_the_remark_then_fall_back_to_the_raw_name() {
        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            remark: "abcd1234".to_string(),
            id: Uuid::new_v4(),
            role: IdentityRole::Friend,
            real_nickname: String::new(),
            display_name: "显示名乙".to_string(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount::new("10002")],
        });
        let access = IdentityAccess::new(config);
        // 1) 外部源只有 UID：from.name 是资料包昵称，必须靠 UID 解析出备注名。
        assert_eq!(
            access.friend_search_names("资料包昵称", Some(("genshin", "10002"))),
            vec!["abcd1234".to_owned()]
        );
        // 2) 名字本身就是备注（OCR 路径）。
        assert_eq!(
            access.friend_search_names("abcd1234", None),
            vec!["abcd1234".to_owned()]
        );
        // 3) 名字命中的是显示名：仍返回屏幕候选，不是显示名本身。
        assert_eq!(
            access.friend_search_names("显示名乙", None),
            vec!["abcd1234".to_owned()]
        );
        // 4) 都解析不到：退回传入的名字，UI 仍有一次机会。
        assert_eq!(
            access.friend_search_names("未登记玩家", None),
            vec!["未登记玩家".to_owned()]
        );
    }

    #[test]
    fn identity_resolves_by_uid_and_by_nickname() {
        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            remark: "abcd1234".to_string(),
            id: Uuid::new_v4(),
            role: IdentityRole::Friend,
            real_nickname: String::new(),
            display_name: "玩家甲".to_string(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount::new("900000002")],
        });
        let access = IdentityAccess::new(config);

        // OCR 路径：只有游戏内昵称。
        let by_name = access
            .resolve_hint(IdentityHint::Nickname("abcd1234"))
            .expect("按昵称应命中");
        // API 路径：只有 UID。
        let by_uid = access
            .resolve_hint(IdentityHint::Account {
                game: "genshin",
                uid: "900000002",
                nickname: None,
            })
            .expect("按 UID 应命中");

        assert_eq!(by_name.id, by_uid.id, "两条路径必须指向同一身份");
        assert_eq!(by_uid.role, IdentityRole::Friend);
    }

    /// 未登记的 UID 不能靠昵称兜底（安全要求：有 UID 时只认 UID）。
    #[test]
    fn unknown_uid_does_not_fall_back_to_nickname() {
        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            remark: "橘子".to_string(),
            id: Uuid::new_v4(),
            role: IdentityRole::Owner,
            real_nickname: String::new(),
            display_name: String::new(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount::new("111111")],
        });
        let access = IdentityAccess::new(config);
        // 昵称对得上、UID 对不上：必须拒绝，否则改昵称就能提权。
        assert!(
            access
                .resolve_hint(IdentityHint::Account {
                    game: "genshin",
                    uid: "999999",
                    nickname: Some("橘子"),
                })
                .is_none(),
            "UID 未登记时不得回退到昵称"
        );
    }

    #[test]
    fn role_of_sender_prefers_account_over_nickname() {
        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            remark: "随机匹配键".to_string(),
            id: Uuid::from_u128(11),
            role: IdentityRole::Admin,
            real_nickname: String::new(),
            display_name: String::new(),
            remark_from_chatbridge: false,
            accounts: vec![GameAccount {
                game: "genshin".to_string(),
                uid: "100000001".to_string(),
            }],
        });
        let access = IdentityAccess::new(config);

        // 有账号：即使昵称对不上，也应凭 UID 拿到 Admin。
        let account = miliastra_kernel::chat_source::CommandAccount {
            game: "genshin".to_string(),
            uid: "100000001".to_string(),
        };
        assert_eq!(
            access.role_of_sender("API昵称不一致", Some(&account)),
            Some(IdentityRole::Admin),
            "有账号时必须按 UID 解析，不能被昵称不一致挡住"
        );

        // 无账号：退回昵称解析（OCR 路径）。
        assert_eq!(
            access.role_of_sender("随机匹配键", None),
            Some(IdentityRole::Admin),
            "无账号时按昵称解析"
        );

        // 有账号但 UID 未登记：不回退昵称，判为未登记。
        let unknown = miliastra_kernel::chat_source::CommandAccount {
            game: "genshin".to_string(),
            uid: "999999999".to_string(),
        };
        assert_eq!(
            access.role_of_sender("随机匹配键", Some(&unknown)),
            None,
            "UID 未登记时不得回退昵称，否则改昵称即可提权"
        );
    }
    #[test]
    fn uid_miss_never_falls_back_to_nickname_authority() {
        let mut config = IdentityConfig::default();
        config.mappings.push(IdentityMapping {
            remark: "可信昵称".to_string(),
            id: Uuid::from_u128(7),
            role: IdentityRole::Owner,
            real_nickname: String::new(),
            display_name: String::new(),
            remark_from_chatbridge: false,
            accounts: Vec::new(),
        });
        let access = IdentityAccess::new(config);

        // 昵称路径照常命中（OCR 没有 UID，只能靠昵称）。
        let by_name = access.resolve_hint(IdentityHint::Nickname("可信昵称"));
        assert_eq!(
            by_name.map(|resolved| resolved.role),
            Some(IdentityRole::Owner)
        );

        // 带 UID 但 UID 未登记：即使昵称撞上已登记 Owner，也必须解析失败。
        let spoofed = access.resolve_hint(IdentityHint::Account {
            game: "genshin",
            uid: "999999999",
            nickname: Some("可信昵称"),
        });
        assert!(
            spoofed.is_none(),
            "UID 未登记时不得回退昵称，否则改昵称即可提权"
        );
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
        // 自动注册：备注串是随机占位（等用户设成游戏内好友备注），
        // 资料包昵称进 real_nickname，软件显示昵称留空等用户填。
        assert!(resolved.display_name.is_empty());
        assert_eq!(resolved.real_nickname, "荧");
        assert_ne!(resolved.remark, "荧");
        assert_eq!(resolved.remark.chars().count(), 6);
        assert!(resolved.remark.is_ascii(), "备注串用英数");
        // 展示链：显示名为空 → 回退真实昵称。
        assert_eq!(resolved.player_name(), "荧");
        // ChatBridge 的 remarks 是游戏内好友备注，覆盖备注串。
        assert!(access.set_account_remark("genshin", "900000001", "显示名甲"));
        assert_eq!(
            access
                .resolve_account("genshin", "900000001")
                .unwrap()
                .remark,
            "显示名甲"
        );
        // 覆盖后 OCR 读到这个名字就能直接定位到身份。
        assert!(access.resolve("显示名甲").is_some());
        // 空备注不能把已有匹配键擦掉。
        assert!(!access.set_account_remark("genshin", "900000001", "  "));
        assert_eq!(
            access
                .resolve_account("genshin", "900000001")
                .unwrap()
                .remark,
            "显示名甲"
        );
    }

    #[test]
    fn auto_created_identity_without_api_nickname_still_has_display_name() {
        let access = IdentityAccess::new(IdentityConfig::default());
        access
            .ensure_identity_for_account("genshin", "999", None)
            .expect("应创建身份");
        let resolved = access.resolve_account("genshin", "999").unwrap();
        // 三个展示来源都为空时用固定占位，**绝不**回退到内部键或备注串。
        assert!(resolved.display_name.is_empty());
        assert!(resolved.real_nickname.is_empty());
        assert_eq!(resolved.player_name(), PLAYER_NAME_PLACEHOLDER);
        assert!(!resolved.player_name().contains('-'), "展示名不得像 UUID");
        // 备注串仍是随机英数（6 位），只用于匹配。
        assert_eq!(resolved.remark.chars().count(), 6);
        assert!(resolved.remark.is_ascii());
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
