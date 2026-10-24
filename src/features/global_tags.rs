//! 全局标签：不属于任何命令、可在任意回复里使用的占位符。
//!
//! 与普通变量的区别：普通变量由调用点在渲染前注入（静态值），
//! 全局标签在渲染时动态生成——随机池每次取一条不同的候选，
//! 这样同一条回复反复出现也不会完全一样。

use std::collections::BTreeMap;

/// 一个全局标签的定义。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GlobalTag {
    /// 占位符名，渲染时写作 `{name}`。
    pub name: &'static str,
    /// 面板展示名。
    pub label: &'static str,
    /// 候选文案；渲染时随机取一条。
    pub choices: &'static [&'static str],
}

/// 内置的随机池标签。
///
/// 只放「不需要任何业务上下文就能生成」的标签；需要 AI 或外部数据的
/// 不在这里（见 [`crate::features::reply_template`] 的 AI 注入路径）。
pub(crate) const GLOBAL_TAGS: &[GlobalTag] = &[
    GlobalTag {
        name: "卖萌",
        label: "卖萌语气词（随机）",
        choices: &["喵~", "诶嘿", "呜…", "嘻嘻", "喵呜"],
    },
    GlobalTag {
        name: "表情",
        label: "颜文字（随机）",
        choices: &["(*/ω＼*)", "(๑•̀ㅂ•́)و", "(´・ω・`)", "ヽ(￣▽￣)ﾉ", "(￣▽￣)~*"],
    },
    GlobalTag {
        name: "语气",
        label: "句末语气（随机）",
        choices: &["呢", "哦", "呀", "啦", ""],
    },
];

/// 面板要展示的 AI 标签元数据：`{AI回复}` 不能放进 `GLOBAL_TAGS`
/// （那条路径同步随机取值），但用户必须能看到并插入它。
pub(crate) const AI_TAG_LABEL: &str = "让 AI 补一句自然的回复（需启用 AI 聊天）";
pub(crate) const AI_TAG_EXAMPLE: &str = "今天也要开心哦";

/// 需要 AI 生成的标签名。
///
/// 它不能像随机池那样在 `render` 里同步求值：AI 调用有网络延迟，
/// 而回复是执行后立即发送的。所以由调用方在渲染前预生成并注入
/// `ReplyVars`；预生成失败时留空，不阻塞回复。
pub(crate) const AI_TAG: &str = "AI回复";

impl GlobalTag {
    /// 从候选里随机取一条。
    pub(crate) fn pick(&self) -> &'static str {
        if self.choices.is_empty() {
            return "";
        }
        // 用纳秒做种子：不需要密码学强度，只要每次调用不同即可。
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.subsec_nanos())
            .unwrap_or_default() as usize;
        self.choices[seed % self.choices.len()]
    }
}

/// 按名字查全局标签。
pub(crate) fn find(name: &str) -> Option<&'static GlobalTag> {
    GLOBAL_TAGS.iter().find(|tag| tag.name == name)
}

/// 渲染时解析一个全局标签；未知名字返回 `None`，由调用方决定是否记为缺失。
pub(crate) fn resolve(name: &str) -> Option<&'static str> {
    find(name).map(GlobalTag::pick)
}

/// 面板用：名字 → 展示名。供变量列表合并展示。
pub(crate) fn labels() -> BTreeMap<&'static str, &'static str> {
    GLOBAL_TAGS
        .iter()
        .map(|tag| (tag.name, tag.label))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_tags_are_unique_and_non_empty() {
        let mut seen = std::collections::HashSet::new();
        for tag in GLOBAL_TAGS {
            assert!(!tag.name.trim().is_empty());
            assert!(seen.insert(tag.name), "全局标签重名: {}", tag.name);
            assert!(!tag.choices.is_empty(), "{} 没有候选", tag.name);
            // 允许空串候选（如"句末语气"里的"不加语气词"），
            // 但不能全是空串——那样这个标签就没有任何可见效果。
            assert!(
                tag.choices.iter().any(|choice| !choice.is_empty()),
                "{} 的候选全为空串",
                tag.name
            );
        }
    }

    #[test]
    fn pick_always_returns_a_known_choice() {
        for tag in GLOBAL_TAGS {
            let picked = tag.pick();
            assert!(tag.choices.contains(&picked));
        }
    }

    #[test]
    fn unknown_tag_is_not_resolved() {
        assert!(resolve("不存在的标签").is_none());
        assert!(find("卖萌").is_some());
    }
}
