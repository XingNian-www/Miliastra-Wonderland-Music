//! 大厅类回复文案。
//!
//! 本文件由 hall 模块负责；新增模板请在 `HALL` 数组追加。
//!
//! 默认文案逐字沿用改造前 `src/features/hall.rs` 里的硬编码字符串：
//! 标点、全角/半角、空格都保持原样。改默认文案等于改玩家可见行为，
//! 不属于"可调整回复"的范畴——要改请走 `replies.overrides` 配置。

use super::ReplyTemplate;

/// 大厅检测失败（读取链路报错时回复）。
pub(crate) const DETECT_FAILED: ReplyTemplate = ReplyTemplate {
    key: "hall.detect.failed",
    default: "大厅检测失败",
    summary: "大厅检测失败",
    group: "大厅",
};

/// 当前身处公共大厅。
pub(crate) const PUBLIC_HALL_CURRENT: ReplyTemplate = ReplyTemplate {
    key: "hall.public.current",
    default: "当前为公共大厅",
    summary: "@大厅检测的结果：公共大厅",
    group: "大厅",
};

/// 当前大厅名 + 可选的剩余时间后缀。
///
/// `后缀` 由调用方组织：有剩余时间时为"，剩余{}分钟"，没有时为空串，
/// 这样"无时间限制的大厅"不会多出一个逗号。
pub(crate) const CURRENT: ReplyTemplate = ReplyTemplate {
    key: "hall.current",
    default: "当前为{大厅名}{后缀}",
    summary: "@大厅检测的结果：大厅名与剩余时间",
    group: "大厅",
};

/// 大厅到期倒计时。
pub(crate) const TIME_REMAINING: ReplyTemplate = ReplyTemplate {
    key: "hall.time.remaining",
    default: "大厅到期时间，剩余{剩余分钟}分钟",
    summary: "@大厅时间的结果：剩余分钟",
    group: "大厅",
};

/// 大厅时间未知（读不到 OCR 结果或没有倒计时）。
pub(crate) const TIME_UNKNOWN: ReplyTemplate = ReplyTemplate {
    key: "hall.time.unknown",
    default: "大厅时间未知",
    summary: "@大厅时间的结果：时间未知",
    group: "大厅",
};

/// 公共大厅没有倒计时。
pub(crate) const PUBLIC_HALL_NO_LIMIT: ReplyTemplate = ReplyTemplate {
    key: "hall.public.no_limit",
    default: "公共大厅无时间限制",
    summary: "@大厅时间的结果：公共大厅无时间限制",
    group: "大厅",
};

/// 好友执行麦克风切换后的通告。
pub(crate) const MICROPHONE_TOGGLED: ReplyTemplate = ReplyTemplate {
    key: "hall.microphone.toggled",
    default: "{玩家} 执行了切换麦克风状态！",
    summary: "麦克风切换成功通告",
    group: "大厅",
};

/// 大厅即将到期（倒计时归零）。
pub(crate) const EXPIRING_WARNING: ReplyTemplate = ReplyTemplate {
    key: "hall.expiring.warning",
    default: "大厅即将到期",
    summary: "大厅到期提醒：已到期",
    group: "大厅",
};

/// 自动重启游戏前的预告：必须**先**发出去再关窗口，否则玩家只会看到突然掉线。
///
/// 文案里带原因，让玩家知道是程序在维护而不是自己被踢了。
pub(crate) const GAME_RESTART_NOTICE: ReplyTemplate = ReplyTemplate {
    key: "hall.game_restart.notice",
    default: "游戏因{原因}需要重启，稍后自动恢复",
    summary: "自动重启游戏前的预告",
    group: "大厅",
};

/// 游戏重新拉起成功后的播报。
///
/// 游戏重启后语音（麦克风）与音乐播放都是关闭状态，不提醒玩家会以为功能坏了。
pub(crate) const GAME_RESTARTED_RESUME_HINT: ReplyTemplate = ReplyTemplate {
    key: "hall.game_restart.restored",
    default: "游戏已重启，请重新开启麦克风与播放",
    summary: "游戏重启成功后的提醒",
    group: "大厅",
};

/// 大厅即将到期（带剩余分钟）。
pub(crate) const EXPIRING_WARNING_MINUTES: ReplyTemplate = ReplyTemplate {
    key: "hall.expiring.warning_minutes",
    default: "大厅即将到期，剩余{剩余分钟}分钟",
    summary: "大厅到期提醒：剩余分钟",
    group: "大厅",
};

/// 大厅回复。键一旦发布不要改名。
pub(crate) const HALL: &[ReplyTemplate] = &[
    DETECT_FAILED,
    PUBLIC_HALL_CURRENT,
    CURRENT,
    TIME_REMAINING,
    TIME_UNKNOWN,
    PUBLIC_HALL_NO_LIMIT,
    MICROPHONE_TOGGLED,
    EXPIRING_WARNING,
    EXPIRING_WARNING_MINUTES,
    GAME_RESTART_NOTICE,
    GAME_RESTARTED_RESUME_HINT,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::reply_template::{ReplyVars, catalog, render};
    use std::collections::BTreeSet;

    #[test]
    fn every_hall_template_is_registered_in_the_catalog_with_a_default() {
        // 登记遗漏会让面板看不到、配置也覆盖不到，等于文案仍然写死在代码里。
        let registered: BTreeSet<&str> = catalog().map(|spec| spec.key).collect();
        for spec in HALL {
            assert!(
                registered.contains(spec.key),
                "{} 未登记进 catalog()",
                spec.key
            );
            assert!(!spec.default.trim().is_empty(), "{} 默认文案为空", spec.key);
            assert_eq!(spec.group, "大厅", "{} 分组应为大厅", spec.key);
            assert!(!spec.summary.trim().is_empty(), "{} 缺少摘要", spec.key);
        }
        assert!(!HALL.is_empty(), "大厅模板目录不应为空");
    }

    #[test]
    fn rendering_hall_templates_reproduces_the_pre_migration_strings() {
        // 改造前这些文案是 format! 直接拼出来的；渲染结果必须逐字一致，
        // 否则"可配置化"就顺手改了玩家看到的话。
        assert_eq!(
            render(&DETECT_FAILED, None, &ReplyVars::new()).text,
            "大厅检测失败"
        );
        assert_eq!(
            render(&PUBLIC_HALL_CURRENT, None, &ReplyVars::new()).text,
            "当前为公共大厅"
        );
        assert_eq!(
            render(&PUBLIC_HALL_NO_LIMIT, None, &ReplyVars::new()).text,
            "公共大厅无时间限制"
        );
        assert_eq!(
            render(&TIME_UNKNOWN, None, &ReplyVars::new()).text,
            "大厅时间未知"
        );

        let vars = ReplyVars::new()
            .with("大厅名", "一级大厅")
            .with("后缀", "，剩余5分钟");
        assert_eq!(
            render(&CURRENT, None, &vars).text,
            "当前为一级大厅，剩余5分钟"
        );
        let vars = ReplyVars::new()
            .with("大厅名", "未识别到大厅名称")
            .with("后缀", "");
        assert_eq!(render(&CURRENT, None, &vars).text, "当前为未识别到大厅名称");

        let vars = ReplyVars::new().with("剩余分钟", "9");
        assert_eq!(
            render(&TIME_REMAINING, None, &vars).text,
            "大厅到期时间，剩余9分钟"
        );
        assert_eq!(
            render(&EXPIRING_WARNING_MINUTES, None, &vars).text,
            "大厅即将到期，剩余9分钟"
        );
        assert_eq!(
            render(&EXPIRING_WARNING, None, &ReplyVars::new()).text,
            "大厅即将到期"
        );
        // 注意默认文案里"执行了"前是一个半角空格。
        let vars = ReplyVars::new().with("玩家", "小明");
        assert_eq!(
            render(&MICROPHONE_TOGGLED, None, &vars).text,
            "小明 执行了切换麦克风状态！"
        );
    }
    /// 重启预告必须把原因渲染进去：只说「需要重启」玩家仍不知道发生了什么，
    /// 而把原因写死的旧做法又改不了（这条要能在配置中心覆盖）。
    #[test]
    fn game_restart_notice_renders_the_reason() {
        let vars = ReplyVars::new().with("原因", "聊天源失联");
        assert_eq!(
            render(&GAME_RESTART_NOTICE, None, &vars).text,
            "游戏因聊天源失联需要重启，稍后自动恢复"
        );
        // 变量缺省时不该 panic 或留下裸占位符以外的破坏性内容；
        // 这里只钉住「有变量时渲染正确」，缺变量由 render 的 missing 机制报出。
        assert!(GAME_RESTART_NOTICE.default.contains("{原因}"));
    }

    /// 重启后的提醒必须同时提到「麦克风」与「播放」：
    /// 只提其中一项，另一项玩家就会以为坏了。
    #[test]
    fn game_restarted_hint_mentions_microphone_and_playback() {
        let text = render(&GAME_RESTARTED_RESUME_HINT, None, &ReplyVars::new()).text;
        assert_eq!(text, "游戏已重启，请重新开启麦克风与播放");
        assert!(text.contains("麦克风"));
        assert!(text.contains("播放"));
    }
}
