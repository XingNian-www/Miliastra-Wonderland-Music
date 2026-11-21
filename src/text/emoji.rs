//! 二级聊天表情标签：纯文本数据模型、解析、归一化与白名单校验。
//!
//! 语法（用户 2026-11-09 定稿）：
//!
//! ```text
//! {表情包:<页签序号>:<表情文字>}
//! {表情包:<页签序号>:<文字A>|<文字B>|<文字C>}
//! ```
//!
//! - 页签序号 1-based：1 = 收藏页，2..=`EMOJI_TAB_TOTAL` = 角色页签。
//! - 一个标签多个候选时，发送阶段只在“当前可见且实际存在”的候选里随机选一个。
//! - 标签在最终发送层解析并剥离，不会作为文字发给游戏。
//! - 标签不嵌套；标签内不能出现 `}`；候选不能包含 `|`。
//!
//! **分层**：本模块只做纯文本解析、数据模型与配置校验，不接触 UI/OCR、
//! 不读截图、不发消息，因此可以住在 `text` 层被 `ui`/`features`/`composition`
//! 共用。真正的界面点击编排在 `ui::routines::friend_delivery`，AI 白名单的
//! 业务侧调用方（提示词、投递前过滤）见 `features::emoji`（纯重导出）。
//!
//! `EmojiConfig` 是**纯数据定义**（含自校验），故住在这一层：
//! `config` 段需要它做反序列化与校验，而 `config` 层不允许认识 `features`。

use serde::{Deserialize, Serialize};

/// 页签总数固定为 6：1 收藏 + 5 角色。
///
/// 用户 2026-11-15 明确：**不要点第 7 个页签**——点击第 7 个表情包会让收藏页签
/// 被整体向左挤动并隐藏，红心锚点随即失效。因此上限按 6 校验，扫描也只扫 1..=6。
pub const EMOJI_TAB_TOTAL: u32 = 6;
/// 同一条回复里最多允许的表情标签数。
pub const MAX_EMOJI_TAGS: usize = 4;
/// 单个标签最多允许的候选数。
pub const MAX_EMOJI_CANDIDATES: usize = 8;
/// 单个候选文字的最大字符数（中文按字符算）。
pub const MAX_EMOJI_TEXT_CHARS: usize = 32;

/// 标签前缀。只有完整匹配该前缀的 `{` 才按表情标签处理，其余 `{` 原样保留。
const TAG_PREFIX: &str = "{表情包:";
/// 花括号内的标签正文前缀（不含花括号）。
const TAG_BODY_PREFIX: &str = "表情包:";

/// 判断花括号内的正文是否属于表情标签，供回复模板渲染/校验时放行。
pub fn is_emoji_tag_body(body: &str) -> bool {
    body.starts_with(TAG_BODY_PREFIX)
}

/// 表情段配置。面板可编辑项只有这四项；区域/坐标等技术参数不进配置。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmojiConfig {
    /// 总开关：关闭时整条表情能力不执行（文字不受影响）。
    pub enabled: bool,
    /// 是否允许 AI 回复使用表情；关闭时 AI 回复里的表情标签一律剥离。
    pub ai_enabled: bool,
    /// AI 可用表情白名单。每项格式 `页签序号|表情文字`，例如 `2|交给我吧`。
    pub ai_allowed: Vec<String>,
    /// 是否把表情流程中的完整截图落盘（默认关，避免玩家内容落盘）。
    pub capture_debug: bool,
}

impl Default for EmojiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ai_enabled: false,
            ai_allowed: Vec::new(),
            capture_debug: false,
        }
    }
}

impl EmojiConfig {
    /// 配置校验：AI 白名单必须能被解析；非法项在保存时直接拒绝。
    pub fn validate(&self) -> anyhow::Result<()> {
        parse_ai_allowed(&self.ai_allowed)
            .map_err(|error| anyhow::anyhow!("emoji.ai_allowed: {error}"))?;
        Ok(())
    }
}

/// 解析后的一个表情标签。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmojiTag {
    pub pack: u32,
    pub candidates: Vec<String>,
}

/// 解析后的有序发送段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmojiSegment {
    Text(String),
    Emoji(EmojiTag),
}

/// 一条回复解析后的脚本。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedReply {
    pub segments: Vec<EmojiSegment>,
}

impl ParsedReply {
    pub fn has_emoji(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| matches!(segment, EmojiSegment::Emoji(_)))
    }

    pub fn emoji_count(&self) -> usize {
        self.segments
            .iter()
            .filter(|segment| matches!(segment, EmojiSegment::Emoji(_)))
            .count()
    }

    /// 去掉表情段后的纯文本（相邻文本段自然拼接）。
    pub fn plain_text(&self) -> String {
        let mut text = String::new();
        for segment in &self.segments {
            if let EmojiSegment::Text(part) = segment {
                text.push_str(part);
            }
        }
        text
    }

    /// AI 过滤/配置校验后是否还有可发送内容。
    pub fn is_empty(&self) -> bool {
        self.segments.iter().all(|segment| match segment {
            EmojiSegment::Text(text) => text.trim().is_empty(),
            EmojiSegment::Emoji(_) => false,
        })
    }
}

/// 标签语法错误。错误信息直接面向配置面板与日志，必须能说清“哪里不对”。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmojiTagError {
    UnclosedTag,
    EmptyTag,
    MissingPack,
    InvalidPack { value: String },
    PackOutOfRange { pack: u32 },
    MissingText,
    EmptyCandidate,
    TooManyCandidates { count: usize },
    TooManyTags { count: usize },
    TextTooLong { chars: usize },
    NestedTag,
    AiEntryMissingSeparator { entry: String },
    AiEntryInvalidPack { entry: String },
    AiEntryEmptyText { entry: String },
    AiEntryTextTooLong { entry: String },
}

impl std::fmt::Display for EmojiTagError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnclosedTag => write!(formatter, "表情标签缺少右侧花括号"),
            Self::EmptyTag => write!(formatter, "表情标签内容为空"),
            Self::MissingPack => write!(formatter, "表情标签缺少页签序号"),
            Self::InvalidPack { value } => {
                write!(formatter, "表情标签页签序号不是数字：{value}")
            }
            Self::PackOutOfRange { pack } => write!(
                formatter,
                "表情标签页签序号超出范围：{pack}（只能是 1..={EMOJI_TAB_TOTAL}）"
            ),
            Self::MissingText => write!(formatter, "表情标签缺少表情文字"),
            Self::EmptyCandidate => write!(formatter, "表情标签存在空候选"),
            Self::TooManyCandidates { count } => write!(
                formatter,
                "表情标签候选过多：{count}（最多 {MAX_EMOJI_CANDIDATES}）"
            ),
            Self::TooManyTags { count } => write!(
                formatter,
                "同一条回复表情标签过多：{count}（最多 {MAX_EMOJI_TAGS}）"
            ),
            Self::TextTooLong { chars } => write!(
                formatter,
                "表情文字过长：{chars} 字（最多 {MAX_EMOJI_TEXT_CHARS}）"
            ),
            Self::NestedTag => write!(formatter, "表情标签不允许嵌套或包含花括号"),
            Self::AiEntryMissingSeparator { entry } => write!(
                formatter,
                "AI 可用表情配置项缺少分隔符「|」：{entry}（格式：页签序号|表情文字）"
            ),
            Self::AiEntryInvalidPack { entry } => write!(
                formatter,
                "AI 可用表情配置项页签序号非法：{entry}（只能是 1..={EMOJI_TAB_TOTAL}）"
            ),
            Self::AiEntryEmptyText { entry } => {
                write!(formatter, "AI 可用表情配置项表情文字为空：{entry}")
            }
            Self::AiEntryTextTooLong { entry } => write!(
                formatter,
                "AI 可用表情配置项表情文字过长：{entry}（最多 {MAX_EMOJI_TEXT_CHARS} 字）"
            ),
        }
    }
}

impl std::error::Error for EmojiTagError {}

/// 解析一条回复里的表情标签，返回有序 Text/Emoji 段。
pub fn parse_reply(input: &str) -> Result<ParsedReply, EmojiTagError> {
    let mut segments = Vec::new();
    let mut cursor = 0_usize;
    let mut tag_count = 0_usize;

    while let Some(relative) = input[cursor..].find(TAG_PREFIX) {
        let start = cursor + relative;
        let body_start = start + TAG_PREFIX.len();
        let Some(close_relative) = input[body_start..].find('}') else {
            return Err(EmojiTagError::UnclosedTag);
        };
        let close = body_start + close_relative;
        let body = &input[body_start..close];
        if body.contains('{') {
            return Err(EmojiTagError::NestedTag);
        }
        if body.trim().is_empty() {
            return Err(EmojiTagError::EmptyTag);
        }
        let tag = parse_tag_body(body)?;
        tag_count += 1;
        if tag_count > MAX_EMOJI_TAGS {
            return Err(EmojiTagError::TooManyTags { count: tag_count });
        }
        push_text(&mut segments, &input[cursor..start]);
        segments.push(EmojiSegment::Emoji(tag));
        cursor = close + 1;
    }

    push_text(&mut segments, &input[cursor..]);
    Ok(ParsedReply { segments })
}

fn parse_tag_body(body: &str) -> Result<EmojiTag, EmojiTagError> {
    let mut parts = body.split(':');
    let Some(pack_text) = parts.next() else {
        return Err(EmojiTagError::MissingPack);
    };
    let Some(text_part) = parts.next() else {
        return Err(EmojiTagError::MissingText);
    };
    if parts.next().is_some() {
        // 多出来的冒号只可能出现在表情文字里，而图片标签不允许冒号，
        // 这里按“格式错误”处理，避免把半截文字发出去。
        return Err(EmojiTagError::MissingText);
    }
    let pack_text = pack_text.trim();
    if pack_text.is_empty() {
        return Err(EmojiTagError::MissingPack);
    }
    let pack = pack_text
        .parse::<u32>()
        .map_err(|_| EmojiTagError::InvalidPack {
            value: pack_text.to_string(),
        })?;
    if pack == 0 || pack > EMOJI_TAB_TOTAL {
        return Err(EmojiTagError::PackOutOfRange { pack });
    }

    let mut candidates = Vec::new();
    for candidate in text_part.split('|') {
        let candidate = candidate.trim();
        if candidate.is_empty() {
            return Err(EmojiTagError::EmptyCandidate);
        }
        let chars = candidate.chars().count();
        if chars > MAX_EMOJI_TEXT_CHARS {
            return Err(EmojiTagError::TextTooLong { chars });
        }
        if !candidates.iter().any(|existing| existing == candidate) {
            candidates.push(candidate.to_string());
        }
    }
    if candidates.is_empty() {
        return Err(EmojiTagError::MissingText);
    }
    if candidates.len() > MAX_EMOJI_CANDIDATES {
        return Err(EmojiTagError::TooManyCandidates {
            count: candidates.len(),
        });
    }
    Ok(EmojiTag { pack, candidates })
}

fn push_text(segments: &mut Vec<EmojiSegment>, text: &str) {
    if text.is_empty() {
        return;
    }
    match segments.last_mut() {
        Some(EmojiSegment::Text(existing)) => existing.push_str(text),
        _ => segments.push(EmojiSegment::Text(text.to_string())),
    }
}

/// 表情文字归一化：先做全角/半角与空白归一，再**只保留字母、数字、汉字**。
///
/// OCR 经常吃掉或误识别标点（`哼哼～`→`哼哼`、`唔·`→`唔`、`出货吧！`→`出货吧`），
/// 因此表情标签比对不看符号。两边同时归一后比较，多命中仍按歧义拒绝。
pub fn normalize_emoji_label(text: &str) -> String {
    super::normalize_comparison_text(text)
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect()
}

/// 校验面板里的 AI 白名单配置项，返回结构化列表（保持配置顺序）。
pub fn parse_ai_allowed(entries: &[String]) -> Result<Vec<(u32, String)>, EmojiTagError> {
    let mut allowed = Vec::new();
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((pack_text, text)) = entry.split_once('|') else {
            return Err(EmojiTagError::AiEntryMissingSeparator {
                entry: entry.to_string(),
            });
        };
        let pack_text = pack_text.trim();
        let text = text.trim();
        let pack = pack_text
            .parse::<u32>()
            .ok()
            .filter(|pack| *pack >= 1 && *pack <= EMOJI_TAB_TOTAL)
            .ok_or_else(|| EmojiTagError::AiEntryInvalidPack {
                entry: entry.to_string(),
            })?;
        if text.is_empty() || text.contains('|') {
            return Err(EmojiTagError::AiEntryEmptyText {
                entry: entry.to_string(),
            });
        }
        if text.chars().count() > MAX_EMOJI_TEXT_CHARS {
            return Err(EmojiTagError::AiEntryTextTooLong {
                entry: entry.to_string(),
            });
        }
        if !allowed
            .iter()
            .any(|(existing_pack, existing_text): &(u32, String)| {
                *existing_pack == pack && existing_text == text
            })
        {
            allowed.push((pack, text.to_string()));
        }
    }
    Ok(allowed)
}

/// AI 回复只允许发送白名单内的表情；不在白名单的标签剥离，文字保留。
///
/// 返回过滤后的脚本与被剥离的标签数（只用于日志计数，不记录正文）。
pub fn filter_ai_reply(parsed: ParsedReply, allowed: &[(u32, String)]) -> (ParsedReply, usize) {
    let mut segments = Vec::new();
    let mut stripped = 0_usize;
    for segment in parsed.segments {
        match segment {
            EmojiSegment::Text(text) => {
                if !text.is_empty() {
                    match segments.last_mut() {
                        Some(EmojiSegment::Text(existing)) => existing.push_str(&text),
                        _ => segments.push(EmojiSegment::Text(text)),
                    }
                }
            }
            EmojiSegment::Emoji(tag) => {
                let permitted = tag.candidates.iter().all(|candidate| {
                    allowed
                        .iter()
                        .any(|(pack, text)| *pack == tag.pack && text == candidate)
                });
                if permitted {
                    segments.push(EmojiSegment::Emoji(tag));
                } else {
                    stripped += 1;
                }
            }
        }
    }
    (ParsedReply { segments }, stripped)
}

/// 把解析脚本重新序列化成带标签的文本（AI 过滤后回写用）。
pub fn serialize_parsed(parsed: &ParsedReply) -> String {
    let mut output = String::new();
    for segment in &parsed.segments {
        match segment {
            EmojiSegment::Text(text) => output.push_str(text),
            EmojiSegment::Emoji(tag) => {
                output.push_str("{表情包:");
                output.push_str(&tag.pack.to_string());
                output.push(':');
                output.push_str(&tag.candidates.join("|"));
                output.push('}');
            }
        }
    }
    output
}

/// 发送前展开：先解析标签，再交给换行/截断逻辑。
///
/// 返回顺序 = 合并后的文字（可空）+ 按出现顺序排列的表情标签消息。
/// 标签消息本身很短，后续按宽度切分/截断时不会被切开或截掉。
pub fn expand_reply_for_sending(input: &str) -> Vec<String> {
    let Ok(parsed) = parse_reply(input) else {
        return vec![input.to_string()];
    };
    if !parsed.has_emoji() {
        return vec![input.to_string()];
    }
    let mut output = Vec::new();
    let text = parsed.plain_text();
    if !text.trim().is_empty() {
        output.push(text);
    }
    for segment in &parsed.segments {
        if let EmojiSegment::Emoji(tag) = segment {
            output.push(serialize_parsed(&ParsedReply {
                segments: vec![EmojiSegment::Emoji(tag.clone())],
            }));
        }
    }
    output
}

/// 判断一条消息是否“只由表情标签组成”（展开后的标签消息）。
pub fn is_tag_only_message(text: &str) -> bool {
    parse_reply(text)
        .map(|parsed| {
            parsed.has_emoji()
                && parsed
                    .segments
                    .iter()
                    .all(|segment| matches!(segment, EmojiSegment::Emoji(_)))
        })
        .unwrap_or(false)
}

/// AI 回复的表情过滤：`ai_enabled=false` 剥离全部标签；开启时只保留白名单内标签。
///
/// 返回（过滤后的文本，被剥离的标签数）。解析失败时原样返回，不猜测。
pub fn filter_ai_text(text: &str, config: &EmojiConfig) -> (String, usize) {
    let Ok(parsed) = parse_reply(text) else {
        return (text.to_string(), 0);
    };
    if !parsed.has_emoji() {
        return (text.to_string(), 0);
    }
    if !config.ai_enabled {
        let stripped = parsed.emoji_count();
        let mut segments = Vec::new();
        for segment in parsed.segments {
            if let EmojiSegment::Text(value) = segment {
                if !value.is_empty() {
                    match segments.last_mut() {
                        Some(EmojiSegment::Text(existing)) => existing.push_str(&value),
                        _ => segments.push(EmojiSegment::Text(value)),
                    }
                }
            }
        }
        return (serialize_parsed(&ParsedReply { segments }), stripped);
    }
    let allowed = parse_ai_allowed(&config.ai_allowed).unwrap_or_default();
    let (filtered, stripped) = filter_ai_reply(parsed, &allowed);
    (serialize_parsed(&filtered), stripped)
}

/// 生成给 AI 的可用表情提示词清单。
pub fn ai_prompt_catalog(entries: &[String]) -> String {
    let allowed = parse_ai_allowed(entries).unwrap_or_default();
    if allowed.is_empty() {
        return "当前没有可用表情。".to_string();
    }
    let tags = allowed
        .iter()
        .map(|(pack, text)| format!("{{表情包:{pack}:{text}}}"))
        .collect::<Vec<_>>()
        .join("、");
    format!("可用表情（只能使用这些标签，且不要改写）：{tags}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emoji(pack: u32, candidates: &[&str]) -> EmojiSegment {
        EmojiSegment::Emoji(EmojiTag {
            pack,
            candidates: candidates.iter().map(|value| value.to_string()).collect(),
        })
    }

    fn text(value: &str) -> EmojiSegment {
        EmojiSegment::Text(value.to_string())
    }

    #[test]
    fn parses_single_tag_only() {
        let parsed = parse_reply("{表情包:1:哈哈}").unwrap();
        assert_eq!(parsed.segments, vec![emoji(1, &["哈哈"])]);
        assert!(parsed.has_emoji());
        assert_eq!(parsed.plain_text(), "");
    }

    #[test]
    fn parses_text_around_tag() {
        let parsed = parse_reply("你好{表情包:2:生气}再见").unwrap();
        assert_eq!(
            parsed.segments,
            vec![text("你好"), emoji(2, &["生气"]), text("再见")]
        );
        assert_eq!(parsed.plain_text(), "你好再见");
    }

    #[test]
    fn merges_adjacent_text() {
        let parsed = parse_reply("a{表情包:1:哈哈}b{表情包:1:哈哈}c").unwrap();
        assert_eq!(
            parsed.segments,
            vec![
                text("a"),
                emoji(1, &["哈哈"]),
                text("b"),
                emoji(1, &["哈哈"]),
                text("c"),
            ]
        );
    }

    #[test]
    fn trims_and_dedupes_candidates() {
        let parsed = parse_reply("{表情包:3: 哈哈 | 哈哈 |生气 }").unwrap();
        assert_eq!(parsed.segments, vec![emoji(3, &["哈哈", "生气"])]);
    }

    #[test]
    fn keeps_non_tag_braces_as_text() {
        let parsed = parse_reply("{表情:1:哈哈}").unwrap();
        assert_eq!(parsed.segments, vec![text("{表情:1:哈哈}")]);
        assert!(!parsed.has_emoji());
    }

    #[test]
    fn rejects_unclosed_tag() {
        assert_eq!(
            parse_reply("{表情包:1:哈哈").unwrap_err(),
            EmojiTagError::UnclosedTag
        );
    }

    #[test]
    fn rejects_nested_tag() {
        assert_eq!(
            parse_reply("{表情包:1:{表情包:1:哈哈}}").unwrap_err(),
            EmojiTagError::NestedTag
        );
    }

    #[test]
    fn rejects_pack_out_of_range() {
        assert!(matches!(
            parse_reply("{表情包:0:哈哈}").unwrap_err(),
            EmojiTagError::PackOutOfRange { pack: 0 }
        ));
        assert!(matches!(
            parse_reply("{表情包:8:哈哈}").unwrap_err(),
            EmojiTagError::PackOutOfRange { pack: 8 }
        ));
    }

    #[test]
    fn rejects_invalid_pack() {
        assert!(matches!(
            parse_reply("{表情包:abc:哈哈}").unwrap_err(),
            EmojiTagError::InvalidPack { .. }
        ));
    }

    #[test]
    fn rejects_empty_candidate() {
        assert_eq!(
            parse_reply("{表情包:1:哈哈||生气}").unwrap_err(),
            EmojiTagError::EmptyCandidate
        );
    }

    #[test]
    fn rejects_seventh_pack() {
        assert!(matches!(
            parse_reply("{表情包:7:哈哈}").unwrap_err(),
            EmojiTagError::PackOutOfRange { pack: 7 }
        ));
    }

    #[test]
    fn rejects_too_many_candidates() {
        let candidates = (0..=MAX_EMOJI_CANDIDATES)
            .map(|index| format!("c{index}"))
            .collect::<Vec<_>>()
            .join("|");
        let input = format!("{{表情包:1:{candidates}}}");
        assert!(matches!(
            parse_reply(&input).unwrap_err(),
            EmojiTagError::TooManyCandidates { .. }
        ));
    }

    #[test]
    fn rejects_too_many_tags() {
        let input = "{表情包:1:哈哈}".repeat(MAX_EMOJI_TAGS + 1);
        assert!(matches!(
            parse_reply(&input).unwrap_err(),
            EmojiTagError::TooManyTags { .. }
        ));
    }

    #[test]
    fn rejects_long_candidate() {
        let long = "哈".repeat(MAX_EMOJI_TEXT_CHARS + 1);
        let input = format!("{{表情包:1:{long}}}");
        assert!(matches!(
            parse_reply(&input).unwrap_err(),
            EmojiTagError::TextTooLong { .. }
        ));
    }

    #[test]
    fn accepts_all_supported_packs() {
        for pack in 1..=EMOJI_TAB_TOTAL {
            let input = format!("{{表情包:{pack}:哈哈}}");
            assert!(parse_reply(&input).is_ok(), "pack {pack} 应合法");
        }
    }

    #[test]
    fn parses_ai_allowlist() {
        let entries = vec![
            "2|交给我吧".to_string(),
            " 1 | 哈哈 ".to_string(),
            "2|交给我吧".to_string(),
        ];
        let allowed = parse_ai_allowed(&entries).unwrap();
        assert_eq!(
            allowed,
            vec![(2, "交给我吧".to_string()), (1, "哈哈".to_string())]
        );
    }

    #[test]
    fn rejects_bad_ai_entries() {
        assert!(matches!(
            parse_ai_allowed(&["2交给我吧".to_string()]).unwrap_err(),
            EmojiTagError::AiEntryMissingSeparator { .. }
        ));
        assert!(matches!(
            parse_ai_allowed(&["8|哈哈".to_string()]).unwrap_err(),
            EmojiTagError::AiEntryInvalidPack { .. }
        ));
        assert!(matches!(
            parse_ai_allowed(&["2|".to_string()]).unwrap_err(),
            EmojiTagError::AiEntryEmptyText { .. }
        ));
    }

    #[test]
    fn filters_ai_reply_by_allowlist() {
        let parsed = parse_reply("你好{表情包:2:交给我吧}{表情包:2:生气}{表情包:1:哈哈}").unwrap();
        let allowed = parse_ai_allowed(&["2|交给我吧".to_string(), "1|哈哈".to_string()]).unwrap();
        let (filtered, stripped) = filter_ai_reply(parsed, &allowed);
        assert_eq!(stripped, 1);
        assert_eq!(
            filtered.segments,
            vec![text("你好"), emoji(2, &["交给我吧"]), emoji(1, &["哈哈"]),]
        );
    }

    #[test]
    fn filter_merges_text_after_stripped_tag() {
        let parsed = parse_reply("前{表情包:2:生气}后").unwrap();
        let (filtered, stripped) = filter_ai_reply(parsed, &[]);
        assert_eq!(stripped, 1);
        assert_eq!(filtered.segments, vec![text("前后")]);
    }

    #[test]
    fn serialize_roundtrip() {
        let parsed = parse_reply("你好{表情包:2:交给我吧|吃惊}再见").unwrap();
        let text = serialize_parsed(&parsed);
        assert_eq!(text, "你好{表情包:2:交给我吧|吃惊}再见");
        assert_eq!(parse_reply(&text).unwrap(), parsed);
    }

    #[test]
    fn ai_disabled_strips_all_tags() {
        let config = EmojiConfig::default();
        let (text, stripped) = filter_ai_text("你好{表情包:2:交给我吧}再见", &config);
        assert_eq!(text, "你好再见");
        assert_eq!(stripped, 1);
    }

    #[test]
    fn ai_enabled_keeps_allowlisted_only() {
        let config = EmojiConfig {
            ai_enabled: true,
            ai_allowed: vec!["2|交给我吧".to_string()],
            ..EmojiConfig::default()
        };
        let (text, stripped) = filter_ai_text(
            "你好{表情包:2:交给我吧}{表情包:2:生气}{表情包:1:哈哈}再见",
            &config,
        );
        assert_eq!(text, "你好{表情包:2:交给我吧}再见");
        assert_eq!(stripped, 2);
    }

    #[test]
    fn malformed_ai_reply_is_returned_unchanged() {
        let config = EmojiConfig {
            ai_enabled: true,
            ..EmojiConfig::default()
        };
        let raw = "你好{表情包:9:哈哈}";
        let (text, stripped) = filter_ai_text(raw, &config);
        assert_eq!(text, raw);
        assert_eq!(stripped, 0);
    }

    #[test]
    fn config_validation_rejects_bad_allowlist() {
        let config = EmojiConfig {
            ai_allowed: vec!["2交给我吧".to_string()],
            ..EmojiConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn config_validation_accepts_an_empty_allowlist() {
        assert!(EmojiConfig::default().validate().is_ok());
    }

    #[test]
    fn normalize_emoji_label_ignores_symbols() {
        for (raw, expected) in [
            ("哼哼～", "哼哼"),
            ("唔·", "唔"),
            ("出货吧！", "出货吧"),
            ("好耶……", "好耶"),
            (" 交给我吧 ", "交给我吧"),
            ("ＯＫ", "ok"),
        ] {
            assert_eq!(normalize_emoji_label(raw), expected, "raw={raw}");
        }
        assert_eq!(
            normalize_emoji_label("好耶！"),
            normalize_emoji_label("好耶")
        );
    }

    #[test]
    fn expand_puts_text_first_and_tags_after() {
        let expanded =
            expand_reply_for_sending("哼，声音也放好了，{表情包:2:交给我吧}示例玩家A听完快去睡。");
        assert_eq!(
            expanded,
            vec![
                "哼，声音也放好了，示例玩家A听完快去睡。".to_string(),
                "{表情包:2:交给我吧}".to_string(),
            ]
        );
    }

    #[test]
    fn expand_keeps_tags_away_from_wrapping() {
        // 文字很长 + 标签在边界：标签必须保持完整、单独一条。
        let long = "测".repeat(200);
        let expanded = expand_reply_for_sending(&format!("{long}{{表情包:2:交给我吧}}"));
        assert_eq!(expanded.len(), 2);
        assert_eq!(expanded[0], long);
        assert_eq!(expanded[1], "{表情包:2:交给我吧}");
    }

    #[test]
    fn expand_is_idempotent_for_tag_only_messages() {
        let expanded = expand_reply_for_sending("{表情包:2:交给我吧}");
        assert_eq!(expanded, vec!["{表情包:2:交给我吧}".to_string()]);
        let again: Vec<String> = expanded
            .iter()
            .flat_map(|piece| expand_reply_for_sending(piece))
            .collect();
        assert_eq!(again, expanded);
    }

    #[test]
    fn expand_preserves_multiple_tags_order() {
        let expanded = expand_reply_for_sending("前{表情包:1:哈哈}中{表情包:3:生气|吃惊}后");
        assert_eq!(
            expanded,
            vec![
                "前中后".to_string(),
                "{表情包:1:哈哈}".to_string(),
                "{表情包:3:生气|吃惊}".to_string(),
            ]
        );
    }

    #[test]
    fn tag_only_detection() {
        assert!(is_tag_only_message("{表情包:2:交给我吧}"));
        assert!(!is_tag_only_message("你好{表情包:2:交给我吧}"));
        assert!(!is_tag_only_message("普通文字"));
    }

    #[test]
    fn tag_body_prefix_detection() {
        // 回复模板用它把 `{表情包:…}` 原样放行，不当成变量或 `{a|b}` 随机文案。
        assert!(is_emoji_tag_body("表情包:2:交给我吧"));
        assert!(!is_emoji_tag_body("name"));
        assert!(!is_emoji_tag_body("表情:1:哈哈"));
    }

    #[test]
    fn catalog_lists_tags() {
        let catalog = ai_prompt_catalog(&["2|交给我吧".to_string(), "1|哈哈".to_string()]);
        assert!(catalog.contains("{表情包:2:交给我吧}"));
        assert!(catalog.contains("{表情包:1:哈哈}"));
        assert_eq!(ai_prompt_catalog(&[]), "当前没有可用表情。");
    }
}
