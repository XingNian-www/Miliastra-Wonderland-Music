//! AI 标签：回复里由 AI 现场补一句的占位符。
//!
//! 它不能像普通变量那样同步求值：AI 调用有网络延迟，而回复是执行后立即发送的。
//! 所以由调用方在渲染前预生成并注入 [`ReplyVars`]，预生成失败时留空、不阻塞回复。
//!
//! 原来的「全局标签」（{卖萌}/{表情}/{语气} 这类内置随机池）已移除：
//! 回复模板现在支持 `{a|b|c}` 随机选项，用户直接在回复里写自己的候选更灵活。

/// 面板要展示的 AI 标签元数据。
pub(crate) const AI_TAG_LABEL: &str = "让 AI 补一句自然的回复（需启用 AI 聊天）";
pub(crate) const AI_TAG_EXAMPLE: &str = "今天也要开心哦";

/// 需要 AI 生成的标签名。
pub(crate) const AI_TAG: &str = "AI回复";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_tag_name_is_not_empty_and_has_no_braces() {
        assert!(!AI_TAG.trim().is_empty());
        // 名字会被包进单花括号，自身不能带花括号或竖线（后者是随机选项语法）。
        assert!(!AI_TAG.contains(['{', '}', '|']));
        assert!(!AI_TAG_LABEL.trim().is_empty());
    }
}
