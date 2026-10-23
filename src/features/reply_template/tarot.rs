//! 塔罗玩法回复文案。key 一旦发布不要改名。
//!
//! 只收录**叙述性文案**（帮助、冷却、AI 不可用等）。牌面数据行
//! （"位置：牌名·正逆位，牌义"）由牌库数据拼装，不属于玩家可调整的措辞，
//! 模板化会把数据结构和展示耦合在一起，因此保留原样。
//!
//! 默认文案逐字沿用改造前的硬编码字符串：标点、全角/半角、空格都保持原样。

use super::ReplyTemplate;

/// 塔罗帮助。
///
/// 单条模板、多行文案：一行发一条消息。
pub(crate) const HELP: ReplyTemplate = ReplyTemplate {
    key: "tarot.help",
    default: "#塔罗牌 [问题] 抽当天三张：现状、提醒、建议；带问题自动AI解读。\n#塔罗牌重抽 [问题] 提前换一组当天牌。\n#塔罗牌AI [问题] 解读当前三张，不重新抽牌。",
    summary: "塔罗帮助",
    group: "娱乐",
};

/// 问题超长或含控制字符。
pub(crate) const QUESTION_TOO_LONG: ReplyTemplate = ReplyTemplate {
    key: "tarot.question.too_long",
    default: "塔罗问题请使用不超过80字的单行文字。",
    summary: "塔罗问题超长",
    group: "娱乐",
};

/// 没有识别到发言者。
pub(crate) const ACTOR_MISSING: ReplyTemplate = ReplyTemplate {
    key: "tarot.actor.missing",
    default: "塔罗需要先识别发言者，请稍后重试。",
    summary: "塔罗缺少发言者",
    group: "娱乐",
};

/// 冷却中（重抽与 AI 解读共用）。
pub(crate) const COOLDOWN: ReplyTemplate = ReplyTemplate {
    key: "tarot.cooldown",
    default: "塔罗冷却中，请{秒数}秒后重抽或AI解读。",
    summary: "塔罗冷却中",
    group: "娱乐",
};

/// 同时在用的人数达到上限。
pub(crate) const BUSY: ReplyTemplate = ReplyTemplate {
    key: "tarot.busy",
    default: "塔罗暂忙，请稍后再试。",
    summary: "塔罗并发已满",
    group: "娱乐",
};

/// 当日牌阵标题（"昵称的今日塔罗牌"）。
pub(crate) const SPREAD_TITLE: ReplyTemplate = ReplyTemplate {
    key: "tarot.spread.title",
    default: "{昵称}的今日塔罗牌",
    summary: "塔罗牌阵标题",
    group: "娱乐",
};

/// AI 未启用但保留了基础牌义。
pub(crate) const AI_DISABLED: ReplyTemplate = ReplyTemplate {
    key: "tarot.ai.disabled",
    default: "塔罗AI未启用，请先配置AI；已保留基础牌义。",
    summary: "塔罗 AI 未启用",
    group: "娱乐",
};

/// AI 解读失败但牌面不变。
pub(crate) const AI_UNAVAILABLE: ReplyTemplate = ReplyTemplate {
    key: "tarot.ai.unavailable",
    default: "塔罗AI解读暂不可用，牌面不变，请参考基础牌义。",
    summary: "塔罗 AI 解读失败",
    group: "娱乐",
};

/// 塔罗全部模板。键一旦发布不要改名。
pub(crate) const TAROT: &[ReplyTemplate] = &[
    HELP,
    QUESTION_TOO_LONG,
    ACTOR_MISSING,
    COOLDOWN,
    BUSY,
    SPREAD_TITLE,
    AI_DISABLED,
    AI_UNAVAILABLE,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::reply_template::{ReplyVars, catalog, render};
    use std::collections::BTreeSet;

    fn text(spec: &ReplyTemplate, vars: ReplyVars) -> String {
        render(spec, None, &vars).text
    }

    #[test]
    fn every_tarot_template_is_registered_in_the_catalog() {
        // 登记遗漏会让面板看不到、配置也覆盖不到。
        let registered: BTreeSet<&str> = catalog().map(|spec| spec.key).collect();
        for spec in TAROT {
            assert!(
                registered.contains(spec.key),
                "{} 未登记进 catalog()",
                spec.key
            );
            assert!(!spec.default.trim().is_empty(), "{} 默认文案为空", spec.key);
            assert_eq!(spec.group, "娱乐", "{} 分组应为娱乐", spec.key);
            assert!(!spec.summary.trim().is_empty(), "{} 缺少摘要", spec.key);
        }
        assert!(!TAROT.is_empty(), "塔罗模板目录不应为空");
    }

    #[test]
    fn rendering_tarot_templates_reproduces_the_pre_migration_strings() {
        // 改造前这些文案写死在 execute_for_day 里；渲染结果必须逐字一致。
        // 帮助合并成单条多行模板：三行内容与合并前逐字一致。
        assert_eq!(
            text(&HELP, ReplyVars::new()),
            "#塔罗牌 [问题] 抽当天三张：现状、提醒、建议；带问题自动AI解读。\n\
             #塔罗牌重抽 [问题] 提前换一组当天牌。\n\
             #塔罗牌AI [问题] 解读当前三张，不重新抽牌。"
        );
        assert_eq!(
            text(&QUESTION_TOO_LONG, ReplyVars::new()),
            "塔罗问题请使用不超过80字的单行文字。"
        );
        assert_eq!(
            text(&ACTOR_MISSING, ReplyVars::new()),
            "塔罗需要先识别发言者，请稍后重试。"
        );
        assert_eq!(text(&BUSY, ReplyVars::new()), "塔罗暂忙，请稍后再试。");
        assert_eq!(
            text(&AI_DISABLED, ReplyVars::new()),
            "塔罗AI未启用，请先配置AI；已保留基础牌义。"
        );
        assert_eq!(
            text(&AI_UNAVAILABLE, ReplyVars::new()),
            "塔罗AI解读暂不可用，牌面不变，请参考基础牌义。"
        );
        assert_eq!(
            text(&COOLDOWN, ReplyVars::new().with("秒数", "12")),
            "塔罗冷却中，请12秒后重抽或AI解读。"
        );
        assert_eq!(
            text(&SPREAD_TITLE, ReplyVars::new().with("昵称", "小明")),
            "小明的今日塔罗牌"
        );
    }
}
