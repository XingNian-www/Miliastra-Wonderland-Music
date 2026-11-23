//! W6a：`custom.<namespace>.<key>` 自定义回复命名空间（**零编译扩展**）。
//!
//! # 解决什么问题
//!
//! 内置回复只有代码里登记过的那些 key：用户能改文案，但不能**新增**一条回复。
//! 自定义流程想发一句自己的文案，只能把文字硬编码进流程步骤里，既不能复用变量
//! 渲染，也不能在面板里集中修改。
//!
//! 这里给用户一个显式命名空间：`custom.<namespace>.<name>` 形式的 key 不需要
//! 重编译、不需要登记 `REPLY_GROUPS`，写进命令回复表就能用。
//!
//! # 查找顺序（唯一权威定义）
//!
//! 请求 key `K` 时：
//!
//! 1. `K` 不带 `custom.` 前缀（**内置 key**）：
//!    a. 库里存在 `custom.K` 行且文本非空 → **用它**（显式自定义覆盖赢了）；
//!    b. 否则 → 该 key 自己的面板行 → 内置默认（与今天逐字节一致）。
//! 2. `K` 带 `custom.` 前缀（**自定义 key**），先校验形态；形态非法 → 该条无效（`None`）：
//!    a. 前缀之后的部分（tail）恰好是一条内置 key → 按 1 的规则解析同一条 key。
//!       `custom.playback.volume.set` 就是 `playback.volume.set` 的覆盖槽，两者等价。
//!    b. tail 不是内置 key → 只认 `K` 自己的行；没有行/行内留空 → **不发**
//!       （新 key 在代码里没有默认文案可回落，绝不拿别处的文本顶上）。
//!
//! ## 冲突规则与理由
//!
//! **同名时自定义行赢内置行。** 理由：`custom.` 是用户亲手敲下的显式前缀，
//! 而内置行是首次启动时整表写库的产物、用户从没写过它；两者同时存在时，只有
//! 显式那条表达意图。反向的规则（内置赢）会让 `custom.K` 永远无效，等于把
//! "覆盖槽"这个能力直接废掉。
//!
//! 由此得到两条可预测的性质：
//! - `stored_override(stored, "K")` 与 `stored_override(stored, "custom.K")`
//!   在 `K` 是内置 key 时**结果相同**（同一个槽位的两个名字）；
//! - 内置 key 空间与 `custom.` 命名空间不相交：没有任何内置 key 以 `custom.`
//!   开头（`builtin_catalog_never_uses_the_reserved_prefix` 锁定），所以规则不会自指。
//!
//! # 静音语义（与内置一致）
//!
//! - 行的**存在**才表示"这条回复有文案"；行内留空 = 没有文案。
//! - 内置 key：留空 → 回落内置默认（今天的行为）。
//! - 自定义新 key：留空 → 不发（没有默认可回落，`blank_custom_row_sends_nothing`）。
//! - 命令级静音（该命令存了空列表）优先于一切：它会把该命令下的**所有**行删掉，
//!   自定义行连同其覆盖槽一起消失，因此不可能"用自定义行解静音一条命令"。
//! - 未登记归属的 key 永远不被 `is_muted` 判为静音（与今天一致），自定义 key
//!   因此也不会被任何命令的快照误伤。
//!
//! # 变量替换
//!
//! 自定义行与内置行走**同一个**渲染器（[`super::render`]）：允许
//! [`super::REPLY_VARIABLES`] 的全部变量、`{AI回复}`、`{甲|乙}` 随机文案与表情
//! 标签。未定义变量按空串渲染并记入 `missing`（调用方照常记 warn），**不会**把
//! `{xxx}` 原样发到聊天框，也不会 panic。
//!
//! # 形态校验（坏条目只让该条失效）
//!
//! key 必须是 `custom.<ns>.<name>`：3–5 段（`custom` + 命名空间 + 1–3 段名字，
//! 5 段对应覆盖最深的内置 key）、每段非空、整条 ≤96 字符、命名空间 ≤32 字符
//! 且只允许 `[a-z0-9_]`、名字 ≤64 字符且只允许 `[a-z0-9_.]`。
//!
//! 校验发生在**读取单条时**：库里有一条坏 key，只有它自己解析不出来（等于禁用
//! 该条），其余自定义行与整个命令回复段照常加载。不新增 quarantine 表项——
//! 回复行本来就是可逐条降级的容器，不需要段级回滚。

use std::collections::BTreeMap;

use crate::config::StoredReply;

/// 自定义回复 key 的保留前缀。
pub(crate) const CUSTOM_KEY_PREFIX: &str = "custom.";

/// 整条 key 的字符上限（含前缀）。
pub(crate) const MAX_CUSTOM_KEY_CHARS: usize = 96;
/// 命名空间段的字符上限。
pub(crate) const MAX_CUSTOM_NAMESPACE_CHARS: usize = 32;
/// 名字段的字符上限。
const MAX_CUSTOM_NAME_CHARS: usize = 64;
/// 段数上限：`custom` + 命名空间 + 至多三段名字。
///
/// 3 段是"自己的新 key"（`custom.hall.greeting`）；4–5 段对应覆盖内置 key
/// （内置 key 最深 4 段，如 `playback.delete.removed.pool_fallback`，加前缀即 5 段）。
const MAX_CUSTOM_SEGMENTS: usize = 5;

/// 自定义 key 的形态错误。每条都带足够信息定位到具体哪一段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CustomKeyError {
    /// 没有 `custom.` 前缀（例如直接写 `song.queue.added`）。
    MissingPrefix,
    /// 只有前缀、没有内容。
    Empty,
    /// 整条 key 过长。
    TooLong { chars: usize, limit: usize },
    /// 段数不足（`custom.hall`）或过多（`custom.a.b.c.d.e`）。
    WrongDepth {
        segments: usize,
        min: usize,
        max: usize,
    },
    /// 有空的段（`custom..x`、`custom.a..b`、`custom.a.`）。
    EmptySegment { index: usize },
    /// 命名空间过长。
    NamespaceTooLong { chars: usize, limit: usize },
    /// 名字过长。
    NameTooLong { chars: usize, limit: usize },
    /// 命名空间里出现不允许的字符。
    NamespaceChar { ch: char },
    /// 名字里出现不允许的字符。
    NameChar { ch: char },
}

impl std::fmt::Display for CustomKeyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPrefix => write!(
                formatter,
                "自定义回复 key 必须以 {CUSTOM_KEY_PREFIX} 开头，例如 custom.workflow.greeting"
            ),
            Self::Empty => write!(
                formatter,
                "自定义回复 key 不能只有 {CUSTOM_KEY_PREFIX} 前缀"
            ),
            Self::TooLong { chars, limit } => write!(
                formatter,
                "自定义回复 key 最多 {limit} 个字符（当前 {chars} 个）"
            ),
            Self::WrongDepth { segments, min, max } => write!(
                formatter,
                "自定义回复 key 必须是 {min} 到 {max} 段（当前 {segments} 段），形如 custom.<命名空间>.<名字>"
            ),
            Self::EmptySegment { index } => write!(
                formatter,
                "自定义回复 key 的第 {} 段为空，不能出现连续的点或结尾的点",
                index + 1
            ),
            Self::NamespaceTooLong { chars, limit } => write!(
                formatter,
                "自定义回复 key 的命名空间最多 {limit} 个字符（当前 {chars} 个）"
            ),
            Self::NameTooLong { chars, limit } => write!(
                formatter,
                "自定义回复 key 的名字部分最多 {limit} 个字符（当前 {chars} 个）"
            ),
            Self::NamespaceChar { ch } => write!(
                formatter,
                "自定义回复 key 的命名空间不能含字符「{ch}」，只允许小写字母、数字与下划线"
            ),
            Self::NameChar { ch } => write!(
                formatter,
                "自定义回复 key 的名字部分不能含字符「{ch}」，只允许小写字母、数字、下划线与点"
            ),
        }
    }
}

/// 该 key 是否落在自定义命名空间里（形态未校验，只看前缀）。
pub(crate) fn is_custom_key(key: &str) -> bool {
    key.starts_with(CUSTOM_KEY_PREFIX)
}

/// 内置 key 对应的覆盖槽 key：`playback.volume.set` → `custom.playback.volume.set`。
pub(crate) fn shadow_key(builtin_key: &str) -> String {
    format!("{CUSTOM_KEY_PREFIX}{builtin_key}")
}

/// 解析并校验一条自定义 key，返回 `custom.` 之后的尾部。
///
/// 这是**唯一**校验入口：运行时读取与面板保存必须共用，否则会出现
/// "面板能存、发不出去"或反之。
pub(crate) fn parse_custom_key(key: &str) -> Result<&str, CustomKeyError> {
    let Some(tail) = key.strip_prefix(CUSTOM_KEY_PREFIX) else {
        return Err(CustomKeyError::MissingPrefix);
    };
    let chars = key.chars().count();
    if chars > MAX_CUSTOM_KEY_CHARS {
        return Err(CustomKeyError::TooLong {
            chars,
            limit: MAX_CUSTOM_KEY_CHARS,
        });
    }
    if tail.is_empty() {
        return Err(CustomKeyError::Empty);
    }
    // 段数按 `.` 计；`custom` 自己占一段。
    let segments: Vec<&str> = tail.split('.').collect();
    let total = segments.len() + 1;
    if total > MAX_CUSTOM_SEGMENTS || segments.len() < 2 {
        return Err(CustomKeyError::WrongDepth {
            segments: total,
            min: 3,
            max: MAX_CUSTOM_SEGMENTS,
        });
    }
    if let Some(index) = segments.iter().position(|part| part.is_empty()) {
        return Err(CustomKeyError::EmptySegment { index });
    }
    let namespace = segments[0];
    let namespace_chars = namespace.chars().count();
    if namespace_chars > MAX_CUSTOM_NAMESPACE_CHARS {
        return Err(CustomKeyError::NamespaceTooLong {
            chars: namespace_chars,
            limit: MAX_CUSTOM_NAMESPACE_CHARS,
        });
    }
    if let Some(ch) = namespace
        .chars()
        .find(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '_'))
    {
        return Err(CustomKeyError::NamespaceChar { ch });
    }
    let name = &tail[namespace.len() + 1..];
    let name_chars = name.chars().count();
    if name_chars > MAX_CUSTOM_NAME_CHARS {
        return Err(CustomKeyError::NameTooLong {
            chars: name_chars,
            limit: MAX_CUSTOM_NAME_CHARS,
        });
    }
    if let Some(ch) = name
        .chars()
        .find(|ch| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || *ch == '_' || *ch == '.'))
    {
        return Err(CustomKeyError::NameChar { ch });
    }
    Ok(tail)
}

/// 面板保存路径用的布尔封装。
pub(crate) fn custom_key_is_valid(key: &str) -> bool {
    parse_custom_key(key).is_ok()
}

/// 库里是否出现过任何 `custom.` 行。
///
/// 内置 key 每次解析都要试一次覆盖槽；没有自定义行时这个判断让绝大多数调用
/// 直接跳过字符串拼接，保持"默认零改动"路径的开销可忽略。
fn has_custom_rows(stored: &BTreeMap<String, Vec<StoredReply>>) -> bool {
    stored
        .values()
        .flatten()
        .any(|row| row.key.starts_with(CUSTOM_KEY_PREFIX))
}

/// 精确查一条自定义行的原文。
///
/// - key 形态非法 → `None`：**只让该条失效**，不影响其它行；
/// - 行文本留空 → `None`：与内置"留空 = 用默认"同一口径，但自定义新 key 没有
///   默认可回落，因此等价于"不发"，绝不发空消息；
/// - 同一 key 出现在多个命令下 → 按命令 ID 顺序取第一条（确定性；面板保存限制
///   不重复，只有手改库才可能出现）。
pub(crate) fn row_text(stored: &BTreeMap<String, Vec<StoredReply>>, key: &str) -> Option<String> {
    if !custom_key_is_valid(key) {
        return None;
    }
    stored
        .values()
        .flatten()
        .find(|row| row.key == key)
        .map(|row| row.text.clone())
        .filter(|text| !text.trim().is_empty())
}

/// 内置 key 的覆盖槽原文（`custom.<builtin_key>`）。
pub(crate) fn shadow_text(
    stored: &BTreeMap<String, Vec<StoredReply>>,
    builtin_key: &str,
) -> Option<String> {
    if !has_custom_rows(stored) {
        return None;
    }
    row_text(stored, &shadow_key(builtin_key))
}

/// 库里全部**可用**的自定义 key（形态合法且文本非空），去重、字典序。
///
/// 面板据此汇报"用户新增了哪些自定义回复"；坏条目与留空行不出现在清单里，
/// 因此也不会被当成可用回复。
pub(crate) fn custom_keys(stored: &BTreeMap<String, Vec<StoredReply>>) -> Vec<&str> {
    let mut keys: Vec<&str> = stored
        .values()
        .flatten()
        .map(|row| row.key.as_str())
        .filter(|key| row_text(stored, key).is_some())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// 面板保存时的 key 校验（`/commands/replies` 的唯一入口）。
///
/// 两种自定义写法都放行：`custom:<任意>`（历史形态）与 `custom.<ns>.<name>`
/// （W6a 命名空间，形态必须合法）。其余 key 仍按"必须属于该命令"拒绝。
pub(crate) fn validate_save_key(key: &str, belongs_to_command: bool) -> Result<(), String> {
    if belongs_to_command {
        return Ok(());
    }
    if key
        .strip_prefix("custom:")
        .is_some_and(|suffix| !suffix.trim().is_empty())
    {
        return Ok(());
    }
    if is_custom_key(key) {
        // 面板保存是用户唯一的写入途径（手改库走 Lenient 容错），因此这里严格
        // 拒绝坏形态，而不是存一条永远发不出去的行。
        return parse_custom_key(key)
            .map(|_| ())
            .map_err(|error| error.to_string());
    }
    Err("静态回复key不属于该命令，自定义key必须使用custom.命名空间或custom:前缀".to_string())
}

/// 覆盖查找：先自定义命名空间，再内置 key 的覆盖槽。
///
/// `super::stored_override` 只在这一步之后才看该 key 自己的面板行，因此
/// **查找顺序的唯一权威定义在本模块**（见模块文档的表格）。
pub(crate) fn override_slot(
    stored: &BTreeMap<String, Vec<StoredReply>>,
    key: &str,
) -> Option<String> {
    if !is_custom_key(key) {
        return shadow_text(stored, key);
    }
    // 自定义 key：先看它自己的行（形态非法时 row_text 早退 → 该条失效）。
    if let Some(text) = row_text(stored, key) {
        return Some(text);
    }
    // 行缺失/留空：只有 tail 是内置 key 时才有默认可回落，
    // 与直接请求内置 key 完全一致（同一个槽位的两个名字）。
    let Ok(tail) = parse_custom_key(key) else {
        return None;
    };
    if super::spec_for_key(tail).is_some() {
        super::stored_override(stored, tail)
    } else {
        None
    }
}

/// 面板投影：用户新增的可用自定义回复清单。
///
/// 由 `interfaces/http` 直接塞进 `/commands/replies` 响应；放在这里而不是路由层，
/// 是因为"哪些 key 算可用回复"的判定必须与发送路径同源。
pub(crate) fn panel_view(stored: &BTreeMap<String, Vec<StoredReply>>) -> serde_json::Value {
    serde_json::Value::Array(
        custom_keys(stored)
            .into_iter()
            .map(|key| {
                let namespace = key
                    .strip_prefix(CUSTOM_KEY_PREFIX)
                    .and_then(|tail| tail.split('.').next())
                    .unwrap_or_default();
                serde_json::json!({
                    "key": key,
                    "namespace": namespace,
                    "text": row_text(stored, key),
                })
            })
            .collect(),
    )
}

/// 面板投影：key 形态规则（前端做即时提示，不必抄一遍常量）。
pub(crate) fn panel_rules() -> serde_json::Value {
    serde_json::json!({
        "prefix": CUSTOM_KEY_PREFIX,
        "maxChars": MAX_CUSTOM_KEY_CHARS,
        "maxNamespaceChars": MAX_CUSTOM_NAMESPACE_CHARS,
        "pattern": "custom.<命名空间>.<名字>：命名空间只允许小写字母/数字/下划线，名字还允许点；3–5 段",
    })
}

#[cfg(test)]
mod tests {
    use super::super::{ReplyTemplate, ReplyVars};
    use super::super::{catalog, render, spec_for_key, stored_override};
    use super::*;

    /// 构造"命令 → 若干行"的存储投影。
    ///
    /// 内置 key 必须挂到**它真正所属的命令**下（`stored_override` 按归属查行）；
    /// `custom.*` 行没有归属，统一挂在 `@测试` 下——它们只能被覆盖槽查询到。
    fn stored(rows: &[(&str, &str)]) -> BTreeMap<String, Vec<StoredReply>> {
        use super::super::command_reply_entry_for_key;
        let mut commands: BTreeMap<String, Vec<StoredReply>> = BTreeMap::new();
        for (key, text) in rows {
            let command = command_reply_entry_for_key(key)
                .map(|entry| entry.id.to_string())
                .unwrap_or_else(|| "@测试".to_string());
            commands.entry(command).or_default().push(StoredReply {
                key: (*key).to_string(),
                text: (*text).to_string(),
            });
        }
        commands
    }

    /// 走**真实发送形状**：查覆盖 → 渲染 → 按发送路径拆条。
    ///
    /// 自定义 key 在代码里没有 `ReplyTemplate`，调用方只能自备一个空的回落
    /// （`default: ""`）；这条辅助函数就是那种调用的最小复现，产物与
    /// `split_configured_lines` 交给聊天层的消息逐字一致。
    fn messages_for(
        commands: &BTreeMap<String, Vec<StoredReply>>,
        key: &str,
        vars: &ReplyVars,
    ) -> Vec<String> {
        let Some(text) = stored_override(commands, key) else {
            return Vec::new();
        };
        let spec = ReplyTemplate {
            key: "",
            default: "",
            summary: "",
            group: "",
        };
        let rendered = render(&spec, Some(&text), vars);
        crate::text::split_configured_lines(&rendered.text)
    }

    #[test]
    fn custom_key_shape_limits_are_enforced() {
        // 合法：3 段（新 key）、4 段、5 段（覆盖最深的内置 key）。
        for key in [
            "custom.workflow.greeting",
            "custom.ns2.name_2",
            "custom.playback.volume.set",
            "custom.playback.delete.removed.pool_fallback",
        ] {
            assert!(custom_key_is_valid(key), "{key} 应被接受");
        }
        assert_eq!(
            parse_custom_key("custom.playback.volume.set"),
            Ok("playback.volume.set")
        );

        // 非法：缺前缀、只有前缀、段数不足/过多、空段、非法字符。
        for (key, expected) in [
            ("song.queue.added", CustomKeyError::MissingPrefix),
            ("custom.", CustomKeyError::Empty),
            (
                "custom.hall",
                CustomKeyError::WrongDepth {
                    segments: 2,
                    min: 3,
                    max: MAX_CUSTOM_SEGMENTS,
                },
            ),
            (
                "custom.a.b.c.d.e",
                CustomKeyError::WrongDepth {
                    segments: 6,
                    min: 3,
                    max: MAX_CUSTOM_SEGMENTS,
                },
            ),
            ("custom..name", CustomKeyError::EmptySegment { index: 0 }),
            ("custom.ns.", CustomKeyError::EmptySegment { index: 1 }),
            ("custom.NS.name", CustomKeyError::NamespaceChar { ch: 'N' }),
            ("custom.ns.名字", CustomKeyError::NameChar { ch: '名' }),
            ("custom.ns.na me", CustomKeyError::NameChar { ch: ' ' }),
        ] {
            assert_eq!(
                parse_custom_key(key),
                Err(expected),
                "{key} 的校验结果与预期不符"
            );
        }
        // 长度上限。
        let long_name = format!("custom.ns.{}", "b".repeat(MAX_CUSTOM_NAME_CHARS + 1));
        assert!(matches!(
            parse_custom_key(&long_name),
            Err(CustomKeyError::NameTooLong { .. })
        ));
        let long_namespace = format!("custom.{}.x", "n".repeat(MAX_CUSTOM_NAMESPACE_CHARS + 1));
        assert!(matches!(
            parse_custom_key(&long_namespace),
            Err(CustomKeyError::NamespaceTooLong { .. })
        ));
        let long_key = format!("custom.ns.{}", "b".repeat(MAX_CUSTOM_KEY_CHARS));
        assert!(matches!(
            parse_custom_key(&long_key),
            Err(CustomKeyError::TooLong { .. })
        ));
    }

    #[test]
    fn custom_reply_overrides_a_builtin_key() {
        // 冲突规则：库里有 custom.<内置 key> 行时，自定义行赢。
        let commands = stored(&[
            ("playback.volume.set", "内置行文案"),
            ("custom.playback.volume.set", "自定义文案"),
        ]);
        assert_eq!(
            stored_override(&commands, "playback.volume.set").as_deref(),
            Some("自定义文案"),
            "custom.<内置 key> 行必须胜过该 key 自己的面板行"
        );
        // 覆盖槽的两个名字等价：直接请求 custom.<内置 key> 得到同一条文本。
        assert_eq!(
            stored_override(&commands, "custom.playback.volume.set").as_deref(),
            Some("自定义文案")
        );

        // 没有自定义行时，内置解析一个字都不变（这是"默认行为等价"的锚点）。
        let builtin_only = stored(&[("playback.volume.set", "内置行文案")]);
        assert_eq!(
            stored_override(&builtin_only, "playback.volume.set").as_deref(),
            Some("内置行文案")
        );
        // 覆盖槽的两个名字等价：没有自定义行时，请求覆盖槽与请求内置 key 一致
        // 地回落内置行（而不是"查不到覆盖"）。
        assert_eq!(
            stored_override(&builtin_only, "custom.playback.volume.set"),
            stored_override(&builtin_only, "playback.volume.set")
        );
        // 内置行缺失时同理：覆盖槽也查不到文本，调用方用自己的默认值。
        let empty = stored(&[]);
        assert_eq!(stored_override(&empty, "custom.playback.volume.set"), None);
    }

    #[test]
    fn custom_reply_is_usable_without_recompiling() {
        // 这个 key 在编译期不存在（`catalog()` 里没有它），只能来自配置。
        let key = "custom.workflow.greeting";
        assert!(
            spec_for_key(key).is_none(),
            "该 key 不能是编译期内置模板，否则测不到“新增”"
        );
        let commands = stored(&[(key, "欢迎{玩家}，本局由我主持")]);
        assert_eq!(
            messages_for(&commands, key, &ReplyVars::new().with("玩家", "小明")),
            vec!["欢迎小明，本局由我主持".to_string()],
            "配置里新增的 key 必须能组装出真实待发送消息"
        );
        // 没有这一行时，新 key 没有任何默认值可回落：不发，而不是发一句代码里的文案。
        assert!(messages_for(&commands, "custom.workflow.other", &ReplyVars::new()).is_empty());
    }

    #[test]
    fn custom_reply_expands_variables_like_builtin() {
        let commands = stored(&[("custom.workflow.greeting", "{玩家} 点播的 {歌名} 已排上")]);
        let rendered = render(
            &ReplyTemplate {
                key: "",
                default: "",
                summary: "",
                group: "",
            },
            stored_override(&commands, "custom.workflow.greeting").as_deref(),
            &ReplyVars::new().with("玩家", "小明").with("歌名", "晴天"),
        );
        assert_eq!(rendered.text, "小明 点播的 晴天 已排上");
        assert!(rendered.missing.is_empty());
        assert!(rendered.overridden, "文本来自配置而不是内置默认");

        // 与内置模板走同一个渲染器：随机文案与表情标签同样生效。
        let with_random = stored(&[("custom.workflow.pick", "好{喵~|诶嘿}")]);
        let text = render(
            &ReplyTemplate {
                key: "",
                default: "",
                summary: "",
                group: "",
            },
            stored_override(&with_random, "custom.workflow.pick").as_deref(),
            &ReplyVars::new(),
        )
        .text;
        assert!(
            !text.contains('{') && !text.contains('|'),
            "随机文案未被展开: {text}"
        );
        assert!(text.starts_with('好'));
    }

    #[test]
    fn custom_reply_with_unknown_variable_is_emptied_not_raw() {
        // 设计：未知变量按空串渲染并记入 missing（与内置同一条路径），既不把
        // `{未知变量}` 原样发进聊天框，也不 panic。
        let commands = stored(&[("custom.workflow.greeting", "欢迎{未知变量}加入")]);
        let rendered = render(
            &ReplyTemplate {
                key: "",
                default: "",
                summary: "",
                group: "",
            },
            stored_override(&commands, "custom.workflow.greeting").as_deref(),
            &ReplyVars::new(),
        );
        assert_eq!(rendered.text, "欢迎加入");
        assert!(!rendered.text.contains('{'), "不得把占位符原样发出去");
        assert_eq!(rendered.missing, vec!["未知变量".to_string()]);

        // 未闭合的花括号原样保留（与内置一致），同样不 panic。
        let unclosed = stored(&[("custom.workflow.broken", "前缀{未闭合")]);
        assert_eq!(
            stored_override(&unclosed, "custom.workflow.broken").as_deref(),
            Some("前缀{未闭合")
        );
    }

    #[test]
    fn malformed_custom_reply_only_disables_that_entry() {
        // 一条坏条目（大写、层级过深、空段）只让自己失效：好条目照常解析，
        // 读取本身不失败，也不影响内置 key。
        let commands = stored(&[
            ("custom.Ns.Bad", "坏条目"),
            ("custom.a.b.c.d.e", "层级过深"),
            ("custom..empty", "空段"),
            ("custom.workflow.good", "好条目"),
            ("playback.volume.set", "内置行文案"),
        ]);
        assert_eq!(
            stored_override(&commands, "custom.workflow.good").as_deref(),
            Some("好条目")
        );
        assert_eq!(
            stored_override(&commands, "playback.volume.set").as_deref(),
            Some("内置行文案")
        );
        for broken in ["custom.Ns.Bad", "custom.a.b.c.d.e", "custom..empty"] {
            assert_eq!(
                stored_override(&commands, broken),
                None,
                "{broken} 应只让自己失效"
            );
        }
    }

    /// 端到端：`custom.<ns>.<key>` 写进**真实配置库**后能走到发送路径。
    ///
    /// 与上面各条互补：它们验证查找/校验/渲染规则，这条验证"库里多一行 →
    /// `stored_override` 能读到 → 组装出真实待发送文本"，也就是用户不重编译
    /// 就能新增一条回复的完整闭环。
    #[test]
    fn custom_reply_is_deliverable_from_the_real_config_store() {
        use crate::config::{BootstrapConfig, ConfigStore, HttpConfig, LoggingConfig};

        let root = std::env::temp_dir().join(format!("custom-reply-{}", uuid::Uuid::new_v4()));
        let database_path = root.join("deps/data/playback.sqlite3");
        std::fs::create_dir_all(database_path.parent().unwrap()).unwrap();
        let bootstrap = BootstrapConfig {
            database_path: database_path.clone(),
            http: HttpConfig::default(),
            logging: LoggingConfig::default(),
        };
        let mut store = ConfigStore::open(&database_path, &root, bootstrap).unwrap();
        let key = "custom.workflow.greeting";
        store
            .save_reply_templates(
                0,
                &BTreeMap::from([(
                    "@测试".to_string(),
                    vec![StoredReply {
                        key: key.to_string(),
                        text: "欢迎{玩家}".to_string(),
                    }],
                )]),
            )
            .unwrap();

        let rows = store.reply_templates().unwrap();
        assert_eq!(
            messages_for(&rows, key, &ReplyVars::new().with("玩家", "小明")),
            vec!["欢迎小明".to_string()],
            "配置库里新增的行必须能组装出真实待发送消息"
        );
        // 面板清单读的是同一份投影。
        assert_eq!(custom_keys(&rows), vec![key]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn custom_reply_count_excludes_blank_and_malformed_rows() {
        // 面板汇报的条数 = 真正可发送的自定义回复数：坏 key 与留空行都不算。
        let commands = stored(&[
            ("custom.workflow.greeting", "欢迎"),
            ("custom.workflow.empty", "  "),
            ("custom.Bad.case", "坏 key"),
            ("playback.volume.set", "内置行"),
        ]);
        assert_eq!(custom_keys(&commands), vec!["custom.workflow.greeting"]);
        // 没有自定义行的库（= 今天所有已部署的库）汇报 0 条，默认行为不变。
        let builtin_only = stored(&[("playback.volume.set", "内置行")]);
        assert!(custom_keys(&builtin_only).is_empty());
    }

    #[test]
    fn blank_custom_row_sends_nothing() {
        // 留空 = 没有文案（与内置"留空 = 用默认"同口径，但自定义新 key 无默认可用）。
        let blank = stored(&[("custom.workflow.greeting", "   ")]);
        assert_eq!(stored_override(&blank, "custom.workflow.greeting"), None);
        assert!(messages_for(&blank, "custom.workflow.greeting", &ReplyVars::new()).is_empty());

        // 内置 key 的留空仍是"回落默认"，不会被自定义规则带偏。
        let builtin_blank = stored(&[("playback.volume.set", "")]);
        assert_eq!(stored_override(&builtin_blank, "playback.volume.set"), None);
        // 留空的覆盖槽同样视为"没覆盖"，回落内置行。
        let shadow_blank = stored(&[
            ("playback.volume.set", "内置行文案"),
            ("custom.playback.volume.set", "  "),
        ]);
        assert_eq!(
            stored_override(&shadow_blank, "playback.volume.set").as_deref(),
            Some("内置行文案")
        );
    }

    #[test]
    fn builtin_catalog_never_uses_the_reserved_prefix() {
        // 冲突规则的前提：内置 key 空间与 `custom.` 命名空间不相交。
        // 一旦有内置 key 叫 `custom.*`，覆盖规则会变成自指。
        for spec in catalog() {
            assert!(
                !is_custom_key(spec.key),
                "内置模板 {} 使用了保留前缀 {}",
                spec.key,
                CUSTOM_KEY_PREFIX
            );
        }
    }
}
