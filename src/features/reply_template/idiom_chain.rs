//! 成语接龙回复文案。key 一旦发布不要改名。
//!
//! 默认文案逐字沿用改造前的硬编码字符串：标点、全角/半角、空格都保持原样。
//! "开头描述"是动态片段（如"“足”字开头"、"与“足”（zu）同音的字开头"），
//! 由业务层拼好后作为变量注入，不参与模板解析。

use super::ReplyTemplate;

/// 海龟汤占用了娱乐会话。
pub(crate) const OCCUPIED_TURTLE_SOUP: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.occupied.turtle_soup",
    default: "海龟汤正在进行，请结束后再开始成语接龙",
    summary: "海龟汤进行中，拒绝开新接龙",
    group: "娱乐",
};

/// 其他娱乐玩法占用了娱乐会话。
pub(crate) const OCCUPIED_OTHER: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.occupied.other",
    default: "{其他玩法}正在进行，请结束后再开始成语接龙",
    summary: "其他玩法进行中，拒绝开新接龙",
    group: "娱乐",
};

/// 功能未启用。
pub(crate) const DISABLED: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.disabled",
    default: "成语接龙未启用",
    summary: "成语接龙未启用",
    group: "娱乐",
};

/// 本局空闲超时。
pub(crate) const EXPIRED: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.expired",
    default: "本局成语接龙已超时，请用 #接龙 或 #同音接龙 重新开局",
    summary: "接龙超时",
    group: "娱乐",
};

/// 已有进行中的接龙。
pub(crate) const ALREADY_ACTIVE: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.already_active",
    default: "已有成语接龙正在进行，请先 #状态",
    summary: "已有接龙进行中",
    group: "娱乐",
};

/// 词库里没有可随机开局的成语。
pub(crate) const NO_START_CANDIDATE: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.no_start_candidate",
    default: "词库中没有可用于随机开局的成语",
    summary: "无随机开局候选",
    group: "娱乐",
};

/// 开局成语没有可接词。
pub(crate) const DEAD_END_START: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.dead_end_start",
    default: "这个成语没有可接词，请换一个开局成语",
    summary: "开局成语死路",
    group: "娱乐",
};

/// 开头描述：同音接龙（带拼音）。
pub(crate) const EXPECTED_HOMOPHONE: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.expected.homophone",
    default: "与“{字}”（{拼音}）同音的字开头",
    summary: "同音接龙的开头描述",
    group: "娱乐",
};

/// 开头描述：普通接龙。
pub(crate) const EXPECTED_EXACT: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.expected.exact",
    default: "“{字}”字开头",
    summary: "普通接龙的开头描述",
    group: "娱乐",
};

/// 开局成功。
pub(crate) const STARTED: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.started",
    default: "{模式}开始：{成语}。请接{开头描述}的成语。",
    summary: "接龙开局成功",
    group: "娱乐",
};

/// 未开局就提交成语。
pub(crate) const NO_SESSION_START: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.no_session.start",
    default: "还没有开局，请用 #接龙 或 #同音接龙 随机开局",
    summary: "未开局时提交成语",
    group: "娱乐",
};

/// 提交成语的首字不符合要求。
pub(crate) const WRONG_FIRST_CHAR: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.wrong_first_char",
    default: "接龙失败：请用{开头描述}。",
    summary: "接龙首字不符",
    group: "娱乐",
};

/// 重复提交已出现过的成语。
pub(crate) const DUPLICATE: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.duplicate",
    default: "这个成语已经出现过了",
    summary: "成语重复",
    group: "娱乐",
};

/// 接龙封龙（无后续可接词）。
pub(crate) const COMPLETED: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.completed",
    default: "接龙封龙：{成语}。共接出 {数量} 个成语。",
    summary: "接龙封龙结束",
    group: "娱乐",
};

/// 接龙成功。
pub(crate) const ACCEPTED: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.accepted",
    default: "接龙成功：{上一个} -> {成语}。请接{开头描述}。",
    summary: "接龙成功",
    group: "娱乐",
};

/// 当前没有进行中的接龙（状态/提示/结束/解释共用）。
pub(crate) const NO_SESSION: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.no_session",
    default: "当前没有进行中的成语接龙",
    summary: "没有进行中的接龙",
    group: "娱乐",
};

/// 接龙状态。
pub(crate) const STATUS: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.status",
    default: "{模式}当前第 {数量} 个：{成语}。请接{开头描述}。",
    summary: "接龙状态",
    group: "娱乐",
};

/// 没有可解释的目标。
pub(crate) const NO_EXPLANATION_TARGET: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.no_explanation_target",
    default: "请使用 #解释 成语，或先开始一局接龙",
    summary: "无可解释目标",
    group: "娱乐",
};

/// 词库缺该成语的来源/解释。
pub(crate) const MISSING_EXPLANATION: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.missing_explanation",
    default: "这个成语暂无来源和解释",
    summary: "成语缺少来源与解释",
    group: "娱乐",
};

/// 解释的首条消息（来源与解释随后分批发送）。
pub(crate) const EXPLANATION_TITLE: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.explanation.title",
    default: "成语：{成语}",
    summary: "解释的成语名",
    group: "娱乐",
};

/// 没有安全提示。
pub(crate) const NO_HINT: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.no_hint",
    default: "当前没有不会立即封龙的安全提示",
    summary: "无安全提示",
    group: "娱乐",
};

/// 提示。
pub(crate) const HINT: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.hint",
    default: "提示：{成语}",
    summary: "接龙提示",
    group: "娱乐",
};

/// 非开局玩家尝试结束。
pub(crate) const STOP_DENIED: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.stop_denied",
    default: "只有开局玩家可以结束本局接龙",
    summary: "无权结束接龙",
    group: "娱乐",
};

/// 主动结束接龙。
pub(crate) const STOPPED: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.stopped",
    default: "成语接龙已结束，共接出 {数量} 个成语。",
    summary: "接龙被结束",
    group: "娱乐",
};

/// 提交内容不是纯汉字。
pub(crate) const INVALID_IDIOM: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.invalid_idiom",
    default: "请输入只包含汉字的成语",
    summary: "输入不是纯汉字成语",
    group: "娱乐",
};

/// 词库中没有该成语。
pub(crate) const IDIOM_NOT_IN_LEXICON: ReplyTemplate = ReplyTemplate {
    key: "idiom_chain.not_in_lexicon",
    default: "这个词不在当前成语词库中",
    summary: "词库无该成语",
    group: "娱乐",
};

/// 成语接龙全部模板。键一旦发布不要改名。
pub(crate) const IDIOM_CHAIN: &[ReplyTemplate] = &[
    OCCUPIED_TURTLE_SOUP,
    OCCUPIED_OTHER,
    DISABLED,
    EXPIRED,
    ALREADY_ACTIVE,
    NO_START_CANDIDATE,
    DEAD_END_START,
    EXPECTED_HOMOPHONE,
    EXPECTED_EXACT,
    STARTED,
    NO_SESSION_START,
    WRONG_FIRST_CHAR,
    DUPLICATE,
    COMPLETED,
    ACCEPTED,
    NO_SESSION,
    STATUS,
    NO_EXPLANATION_TARGET,
    MISSING_EXPLANATION,
    EXPLANATION_TITLE,
    NO_HINT,
    HINT,
    STOP_DENIED,
    STOPPED,
    INVALID_IDIOM,
    IDIOM_NOT_IN_LEXICON,
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
    fn every_idiom_chain_template_is_registered_in_the_catalog() {
        let registered: BTreeSet<&str> = catalog().map(|spec| spec.key).collect();
        for spec in IDIOM_CHAIN {
            assert!(
                registered.contains(spec.key),
                "{} 未登记进 catalog()",
                spec.key
            );
            assert!(!spec.default.trim().is_empty(), "{} 默认文案为空", spec.key);
            assert_eq!(spec.group, "娱乐", "{} 分组应为娱乐", spec.key);
            assert!(!spec.summary.trim().is_empty(), "{} 缺少摘要", spec.key);
        }
        assert!(!IDIOM_CHAIN.is_empty(), "成语接龙模板目录不应为空");
    }

    #[test]
    fn rendering_idiom_chain_templates_reproduces_the_pre_migration_strings() {
        // 改造前这些文案是 outcome()/format! 直接拼出来的；渲染结果必须逐字一致。
        assert_eq!(
            text(&OCCUPIED_TURTLE_SOUP, ReplyVars::new()),
            "海龟汤正在进行，请结束后再开始成语接龙"
        );
        assert_eq!(
            text(
                &OCCUPIED_OTHER,
                ReplyVars::new().with("其他玩法", "谁是卧底")
            ),
            "谁是卧底正在进行，请结束后再开始成语接龙"
        );
        assert_eq!(text(&DISABLED, ReplyVars::new()), "成语接龙未启用");
        assert_eq!(
            text(&EXPIRED, ReplyVars::new()),
            "本局成语接龙已超时，请用 #接龙 或 #同音接龙 重新开局"
        );
        assert_eq!(
            text(&ALREADY_ACTIVE, ReplyVars::new()),
            "已有成语接龙正在进行，请先 #状态"
        );
        assert_eq!(
            text(&NO_START_CANDIDATE, ReplyVars::new()),
            "词库中没有可用于随机开局的成语"
        );
        assert_eq!(
            text(&DEAD_END_START, ReplyVars::new()),
            "这个成语没有可接词，请换一个开局成语"
        );
        assert_eq!(
            text(&NO_SESSION_START, ReplyVars::new()),
            "还没有开局，请用 #接龙 或 #同音接龙 随机开局"
        );
        assert_eq!(text(&DUPLICATE, ReplyVars::new()), "这个成语已经出现过了");
        assert_eq!(
            text(&NO_SESSION, ReplyVars::new()),
            "当前没有进行中的成语接龙"
        );
        assert_eq!(
            text(&NO_EXPLANATION_TARGET, ReplyVars::new()),
            "请使用 #解释 成语，或先开始一局接龙"
        );
        assert_eq!(
            text(&MISSING_EXPLANATION, ReplyVars::new()),
            "这个成语暂无来源和解释"
        );
        assert_eq!(
            text(&NO_HINT, ReplyVars::new()),
            "当前没有不会立即封龙的安全提示"
        );
        assert_eq!(
            text(&STOP_DENIED, ReplyVars::new()),
            "只有开局玩家可以结束本局接龙"
        );
        assert_eq!(
            text(&INVALID_IDIOM, ReplyVars::new()),
            "请输入只包含汉字的成语"
        );
        assert_eq!(
            text(&IDIOM_NOT_IN_LEXICON, ReplyVars::new()),
            "这个词不在当前成语词库中"
        );

        // 带变量的文案：变量拼接位置、标点、空格都要与改造前一致。
        let vars = ReplyVars::new()
            .with("模式", "同音接龙")
            .with("成语", "画蛇添足")
            .with("开头描述", "与“足”（zu）同音的字开头");
        assert_eq!(
            text(&STARTED, vars.clone()),
            "同音接龙开始：画蛇添足。请接与“足”（zu）同音的字开头的成语。"
        );
        assert_eq!(
            text(
                &EXPECTED_HOMOPHONE,
                ReplyVars::new().with("字", "足").with("拼音", "zu")
            ),
            "与“足”（zu）同音的字开头"
        );
        assert_eq!(
            text(&EXPECTED_EXACT, ReplyVars::new().with("字", "足")),
            "“足”字开头"
        );
        let vars = ReplyVars::new().with("开头描述", "“足”字开头");
        assert_eq!(
            text(&WRONG_FIRST_CHAR, vars.clone()),
            "接龙失败：请用“足”字开头。"
        );
        let vars = ReplyVars::new().with("成语", "足智多谋").with("数量", "7");
        assert_eq!(
            text(&COMPLETED, vars),
            "接龙封龙：足智多谋。共接出 7 个成语。"
        );
        let vars = ReplyVars::new()
            .with("上一个", "画蛇添足")
            .with("成语", "足智多谋")
            .with("开头描述", "“谋”字开头");
        assert_eq!(
            text(&ACCEPTED, vars),
            "接龙成功：画蛇添足 -> 足智多谋。请接“谋”字开头。"
        );
        let vars = ReplyVars::new()
            .with("模式", "成语接龙")
            .with("数量", "3")
            .with("成语", "人山人海")
            .with("开头描述", "“海”字开头");
        assert_eq!(
            text(&STATUS, vars),
            "成语接龙当前第 3 个：人山人海。请接“海”字开头。"
        );
        assert_eq!(
            text(
                &EXPLANATION_TITLE,
                ReplyVars::new().with("成语", "画蛇添足")
            ),
            "成语：画蛇添足"
        );
        assert_eq!(
            text(&HINT, ReplyVars::new().with("成语", "足不出户")),
            "提示：足不出户"
        );
        assert_eq!(
            text(&STOPPED, ReplyVars::new().with("数量", "5")),
            "成语接龙已结束，共接出 5 个成语。"
        );
    }
}
