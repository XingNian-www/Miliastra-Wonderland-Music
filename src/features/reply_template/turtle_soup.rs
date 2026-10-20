//! 海龟汤回复文案（玩家可见的消息）。
//!
//! 本文件由 turtle_soup 模块负责；新增模板请在 TURTLE_SOUP 数组追加。
//!
//! **不包含** AI 提示词（system_prompt / review_prompt / custom_prompt 以及
//! JUDGMENT_PROTOCOL 等）：那些属于 turtle_soup.* 配置段，不是玩家可见回复，
//! 模板化会把两套配置混在一起，故本目录只收"发到聊天里的句子"。
//!
//! 默认文案逐字沿用改造前的硬编码字符串：标点、全角/半角、空格都保持原样。
//! 改默认文案等于改玩家可见行为，要改请走 replies.overrides 配置。
//! key 一旦发布不要改名，否则用户已保存的覆盖会失效。

use super::ReplyTemplate;

// ---------------------------------------------------------------------------
// 裁决回复
// ---------------------------------------------------------------------------

/// 裁决回复的后缀。单独成键是因为发送前要按它的显示宽度截断昵称。
pub(crate) const JUDGMENT_SUFFIX: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.judgment.suffix",
    default: "]的{问题}回复：{结果}",
    summary: "裁决回复的后缀（用于计算昵称可用宽度）",
    group: "娱乐",
};

/// 裁决回复正文。
pub(crate) const JUDGMENT_REPLY: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.judgment.reply",
    default: "[{玩家}{后缀}",
    summary: "裁决回复：昵称+问题摘要+裁决结果",
    group: "娱乐",
};

/// 裁决结果：是。
pub(crate) const RESULT_YES: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.result.yes",
    default: "是",
    summary: "裁决结果：是",
    group: "娱乐",
};

/// 裁决结果：否。
pub(crate) const RESULT_NO: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.result.no",
    default: "否",
    summary: "裁决结果：否",
    group: "娱乐",
};

/// 裁决结果：无关。
pub(crate) const RESULT_IRRELEVANT: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.result.irrelevant",
    default: "无关",
    summary: "裁决结果：无关",
    group: "娱乐",
};

/// 裁决结果：部分正确。
pub(crate) const RESULT_PARTIAL: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.result.partial",
    default: "部分正确",
    summary: "裁决结果：部分正确",
    group: "娱乐",
};

/// 裁决结果：完全正确。
pub(crate) const RESULT_COMPLETE: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.result.complete",
    default: "完全正确",
    summary: "裁决结果：完全正确（触发结算）",
    group: "娱乐",
};

/// 裁决结果：拒绝作答。
pub(crate) const RESULT_REFUSED: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.result.refused",
    default: "该问题无法裁决",
    summary: "裁决结果：拒绝",
    group: "娱乐",
};

/// 裁决结果：AI 失败。
pub(crate) const RESULT_REVIEW_FAILED: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.result.review_failed",
    default: "AI裁决失败，请稍后再问",
    summary: "裁决结果：AI 裁决失败",
    group: "娱乐",
};

// ---------------------------------------------------------------------------
// 长答案（##编号）分段提示
// ---------------------------------------------------------------------------

/// 分段提示模板：[前缀][昵称]:后缀。
pub(crate) const BATCH_NOTICE: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.notice",
    default: "{前缀}[{玩家}]:{后缀}",
    summary: "长答案分段提示（前缀+昵称+段号）",
    group: "娱乐",
};

/// 分段前缀：已暂存。
pub(crate) const BATCH_SAVED: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.saved",
    default: "暂存",
    summary: "长答案：该段已暂存",
    group: "娱乐",
};

/// 分段前缀：缺少某段。
pub(crate) const BATCH_MISSING: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.missing",
    default: "缺少",
    summary: "长答案：缺少某一编号段",
    group: "娱乐",
};

/// 分段前缀：超出上限。
pub(crate) const BATCH_LIMIT: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.limit",
    default: "上限",
    summary: "长答案：编号超出上限",
    group: "娱乐",
};

/// 分段前缀：空段。
pub(crate) const BATCH_EMPTY_PART: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.empty_part",
    default: "空段",
    summary: "长答案：该段内容为空",
    group: "娱乐",
};

/// 分段前缀：没有暂存。
pub(crate) const BATCH_NO_DRAFT: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.no_draft",
    default: "无暂存",
    summary: "长答案：提交时没有暂存内容",
    group: "娱乐",
};

/// 分段前缀：格式不正确。
pub(crate) const BATCH_FORMAT: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.format",
    default: "格式",
    summary: "长答案：编号格式不正确",
    group: "娱乐",
};

/// 分段前缀：已清空。
pub(crate) const BATCH_CLEARED: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.cleared",
    default: "清空",
    summary: "长答案：取消时已清空暂存",
    group: "娱乐",
};

/// 分段后缀：提交完成。
pub(crate) const BATCH_DONE: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.done",
    default: "完成",
    summary: "长答案：取消操作已完成",
    group: "娱乐",
};

/// 分段后缀：提交命令本身。
pub(crate) const BATCH_SUBMIT: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.submit",
    default: "##提交",
    summary: "长答案：提交指令名",
    group: "娱乐",
};

/// 分段后缀：格式示例。
pub(crate) const BATCH_FORMAT_HINT: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.batch.format_hint",
    default: "##编号内容",
    summary: "长答案：编号用法示例",
    group: "娱乐",
};

// ---------------------------------------------------------------------------
// 提问被拒绝的提示
// ---------------------------------------------------------------------------

/// 上一条问题还在裁决中。
pub(crate) const QUESTION_PENDING: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.question.pending",
    default: "{玩家}，你上一条问题还在裁决中",
    summary: "同一玩家重复提问的提示",
    group: "娱乐",
};

/// 并发提问过多。
pub(crate) const QUESTION_TOO_MANY: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.question.too_many",
    default: "{玩家}，当前提问过多，请稍后再试",
    summary: "提问队列已满的提示",
    group: "娱乐",
};

/// AI Worker 不可用。
pub(crate) const QUESTION_WORKER_UNAVAILABLE: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.question.worker_unavailable",
    default: "海龟汤 AI Worker 不可用，请稍后再试",
    summary: "AI Worker 断开时的提示",
    group: "娱乐",
};

// ---------------------------------------------------------------------------
// 命令回复
// ---------------------------------------------------------------------------

/// 功能未启用。
pub(crate) const DISABLED: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.disabled",
    default: "海龟汤功能未启用",
    summary: "功能未启用",
    group: "娱乐",
};

/// 当前没有进行中的海龟汤（#状态 与好友 #结束 共用）。
pub(crate) const NO_SESSION: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.no_session",
    default: "当前没有进行中的海龟汤",
    summary: "没有进行中的对局",
    group: "娱乐",
};

/// #状态 的正常回复。
pub(crate) const STATUS: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.status",
    default: "海龟汤{阶段}，已进行{秒数}秒，{人数}人参与，{提问数}个有效提问",
    summary: "#状态：阶段、时长、参与者与提问数",
    group: "娱乐",
};

/// 公屏不能主动结束。
pub(crate) const END_HALL_DENIED: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.end.hall_denied",
    default: "只有好友私聊或Web控制台可以主动结束海龟汤",
    summary: "公屏 #结束 被拒的提示",
    group: "娱乐",
};

/// 好友结束失败。
pub(crate) const END_FAILED: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.end.failed",
    default: "海龟汤结束失败：{原因}",
    summary: "好友 #结束 失败",
    group: "娱乐",
};

/// 开局失败。
pub(crate) const START_FAILED: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.start.failed",
    default: "海龟汤开局失败：{原因}",
    summary: "#海龟汤 开局失败",
    group: "娱乐",
};

/// 正在公布汤面。
pub(crate) const ANNOUNCING: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.announcing",
    default: "海龟汤正在公布汤面，请稍候",
    summary: "公布汤面阶段的重复开局提示",
    group: "娱乐",
};

/// 正在结算。
pub(crate) const SETTLING: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.settling",
    default: "海龟汤正在结算，请稍候",
    summary: "结算阶段的重复开局提示",
    group: "娱乐",
};

/// 汤面重发失败。
pub(crate) const SURFACE_REPEAT_FAILED: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.surface.repeat_failed",
    default: "汤面重发失败：{原因}",
    summary: "重发汤面失败",
    group: "娱乐",
};

// ---------------------------------------------------------------------------
// 结算与分段标签
// ---------------------------------------------------------------------------

/// 汤面分段标签（生成"汤面1/2：…"）。
pub(crate) const LABEL_SURFACE: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.label.surface",
    default: "汤面",
    summary: "汤面消息的分段标签",
    group: "娱乐",
};

/// 结算分段标签。
pub(crate) const LABEL_SETTLEMENT: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.label.settlement",
    default: "结算",
    summary: "结算消息的分段标签",
    group: "娱乐",
};

/// 汤底分段标签。
pub(crate) const LABEL_BOTTOM: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.label.bottom",
    default: "汤底",
    summary: "汤底消息的分段标签",
    group: "娱乐",
};

/// 结算原因：玩家还原真相。
pub(crate) const REASON_WINNER: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.reason.winner",
    default: "玩家[{玩家}]已还原真相",
    summary: "结算原因：玩家还原真相",
    group: "娱乐",
};

/// 结算原因：好友主动结束。
pub(crate) const REASON_FRIEND: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.reason.friend",
    default: "好友[{玩家}]已主动结束当前海龟汤",
    summary: "结算原因：好友主动结束",
    group: "娱乐",
};

/// 结算原因：Web 控制台结束。
pub(crate) const REASON_WEB: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.reason.web",
    default: "Web控制台已主动结束当前海龟汤",
    summary: "结算原因：Web 控制台结束",
    group: "娱乐",
};

/// 结算原因：长时间无人提问。
pub(crate) const REASON_IDLE: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.reason.idle_timeout",
    default: "海龟汤因长时间无人提问自动结束",
    summary: "结算原因：空闲超时",
    group: "娱乐",
};

/// 结算原因：达到最长时长。
pub(crate) const REASON_MAX_DURATION: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.reason.max_duration",
    default: "海龟汤达到最长时长，已自动结束",
    summary: "结算原因：达到最长时长",
    group: "娱乐",
};

/// 结算摘要行。
pub(crate) const SETTLEMENT_SUMMARY: ReplyTemplate = ReplyTemplate {
    key: "turtle_soup.settlement.summary",
    default: "{原因}。本局共有{人数}名参与者，收到{提问数}个有效提问",
    summary: "结算首行：原因+参与人数+提问数",
    group: "娱乐",
};

/// 全部海龟汤回复模板。
pub(crate) const TURTLE_SOUP: &[ReplyTemplate] = &[
    JUDGMENT_SUFFIX,
    JUDGMENT_REPLY,
    RESULT_YES,
    RESULT_NO,
    RESULT_IRRELEVANT,
    RESULT_PARTIAL,
    RESULT_COMPLETE,
    RESULT_REFUSED,
    RESULT_REVIEW_FAILED,
    BATCH_NOTICE,
    BATCH_SAVED,
    BATCH_MISSING,
    BATCH_LIMIT,
    BATCH_EMPTY_PART,
    BATCH_NO_DRAFT,
    BATCH_FORMAT,
    BATCH_CLEARED,
    BATCH_DONE,
    BATCH_SUBMIT,
    BATCH_FORMAT_HINT,
    QUESTION_PENDING,
    QUESTION_TOO_MANY,
    QUESTION_WORKER_UNAVAILABLE,
    DISABLED,
    NO_SESSION,
    STATUS,
    END_HALL_DENIED,
    END_FAILED,
    START_FAILED,
    ANNOUNCING,
    SETTLING,
    SURFACE_REPEAT_FAILED,
    LABEL_SURFACE,
    LABEL_SETTLEMENT,
    LABEL_BOTTOM,
    REASON_WINNER,
    REASON_FRIEND,
    REASON_WEB,
    REASON_IDLE,
    REASON_MAX_DURATION,
    SETTLEMENT_SUMMARY,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::reply_template::{ReplyVars, catalog, render};
    use std::collections::BTreeSet;

    #[test]
    fn every_turtle_soup_template_is_registered_in_the_catalog() {
        let registered: BTreeSet<&str> = catalog().map(|spec| spec.key).collect();
        for spec in TURTLE_SOUP {
            assert!(
                registered.contains(spec.key),
                "{} 未登记进 catalog()",
                spec.key
            );
            assert!(!spec.default.trim().is_empty(), "{} 默认文案为空", spec.key);
            assert_eq!(spec.group, "娱乐", "{} 分组应为娱乐", spec.key);
            assert!(!spec.summary.trim().is_empty(), "{} 缺少摘要", spec.key);
        }
        assert!(!TURTLE_SOUP.is_empty(), "海龟汤模板目录不应为空");
    }

    #[test]
    fn rendering_turtle_soup_templates_reproduces_the_pre_migration_strings() {
        // 改造前这些文案是 format! 直接拼出来的；渲染结果必须逐字一致。
        let vars = ReplyVars::new().with("问题", "他认识死者吗").with("结果", "是");
        assert_eq!(
            render(&JUDGMENT_SUFFIX, None, &vars).text,
            "]的他认识死者吗回复：是"
        );
        let suffix = render(&JUDGMENT_SUFFIX, None, &vars).text;
        assert_eq!(suffix, "]的他认识死者吗回复：是");
        let vars = vars.with("玩家", "Alice").with("后缀", suffix);
        assert_eq!(
            render(&JUDGMENT_REPLY, None, &vars).text,
            "[Alice]的他认识死者吗回复：是"
        );

        let vars = ReplyVars::new().with("前缀", "暂存").with("玩家", "Alice").with("后缀", "##2");
        assert_eq!(
            render(&BATCH_NOTICE, None, &vars).text,
            "暂存[Alice]:##2"
        );

        let vars = ReplyVars::new()
            .with("阶段", "进行中")
            .with("秒数", "12")
            .with("人数", "2")
            .with("提问数", "3");
        assert_eq!(
            render(&STATUS, None, &vars).text,
            "海龟汤进行中，已进行12秒，2人参与，3个有效提问"
        );

        let vars = ReplyVars::new().with("玩家", "Alice");
        assert_eq!(
            render(&QUESTION_PENDING, None, &vars).text,
            "Alice，你上一条问题还在裁决中"
        );

        let vars = ReplyVars::new().with("原因", "AI 超时");
        assert_eq!(
            render(&END_FAILED, None, &vars).text,
            "海龟汤结束失败：AI 超时"
        );

        let vars = ReplyVars::new().with("玩家", "甲");
        assert_eq!(
            render(&REASON_WINNER, None, &vars).text,
            "玩家[甲]已还原真相"
        );

        let vars = ReplyVars::new()
            .with("原因", "海龟汤因长时间无人提问自动结束")
            .with("人数", "2")
            .with("提问数", "5");
        assert_eq!(
            render(&SETTLEMENT_SUMMARY, None, &vars).text,
            "海龟汤因长时间无人提问自动结束。本局共有2名参与者，收到5个有效提问"
        );

        assert_eq!(render(&LABEL_SURFACE, None, &ReplyVars::new()).text, "汤面");
        assert_eq!(render(&LABEL_SETTLEMENT, None, &ReplyVars::new()).text, "结算");
        assert_eq!(render(&LABEL_BOTTOM, None, &ReplyVars::new()).text, "汤底");
    }
}
