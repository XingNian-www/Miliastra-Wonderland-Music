//! 谁是卧底回复文案。
//!
//! 本文件由 undercover 模块负责；新增模板请在 UNDERCOVER 数组追加。
//!
//! 默认文案逐字沿用改造前 undercover/mod.rs 与 undercover/service.rs 里的硬编码
//! 字符串：标点、全角/半角、空格（含"谁是卧底结束"行里的两个半角空格、结算里
//! 的 = 与 | 分隔符）都保持原样。改默认文案等于改玩家可见行为，
//! 要改请走 replies.overrides 配置。
//!
//! key 一旦发布不要改名，否则用户已保存的覆盖会失效。

use super::ReplyTemplate;

/// 投票说明。它是多条投票提示里的公共片段，作为变量 {投票说明} 注入。
pub(crate) const VOTE_INSTRUCTION: ReplyTemplate = ReplyTemplate {
    key: "undercover.vote.instruction",
    default: "请好友私聊#怀疑对象编号，例如#A，或发送#弃权",
    summary: "投票说明（拼接进多条投票提示）",
    group: "娱乐",
};

/// 开局私聊发词：位置与词语。
pub(crate) const SECRET: ReplyTemplate = ReplyTemplate {
    key: "undercover.start.secret",
    default: "你的位置：{位置}；你的词语：{词语}",
    summary: "开局私聊发给玩家的位置与词语",
    group: "娱乐",
};

/// 开局公屏首行。
pub(crate) const OPENING: ReplyTemplate = ReplyTemplate {
    key: "undercover.start.header",
    default: "谁是卧底开始：{模式}，共{人数}人",
    summary: "开局公屏首行",
    group: "娱乐",
};

/// 开局公屏：请玩家描述。
pub(crate) const DESCRIBE_PROMPT: ReplyTemplate = ReplyTemplate {
    key: "undercover.start.prompt",
    default: "请存活玩家在公屏发送 #内容",
    summary: "开局公屏：开始描述提示",
    group: "娱乐",
};

/// 发词阶段失败，牌局取消。
pub(crate) const DELIVERY_CANCELED: ReplyTemplate = ReplyTemplate {
    key: "undercover.delivery.canceled",
    default: "谁是卧底发词失败，牌局已取消",
    summary: "发词失败导致牌局取消",
    group: "娱乐",
};

/// 加赛阶段无人可投票。
pub(crate) const RUNOFF_NO_VOTERS: ReplyTemplate = ReplyTemplate {
    key: "undercover.runoff.no_voters",
    default: "并列玩家无人可投票，本轮无人淘汰；第{轮次}轮开始，请在公屏发送 #内容",
    summary: "加赛无人可投票，进入下一轮",
    group: "娱乐",
};

/// 加赛描述完成，进入加赛投票。
pub(crate) const RUNOFF_DESCRIBED: ReplyTemplate = ReplyTemplate {
    key: "undercover.runoff.described",
    default: "并列玩家已完成公屏描述，其他存活玩家{投票说明}",
    summary: "加赛描述完成，提示投票",
    group: "娱乐",
};

/// 所有存活玩家描述完成，进入投票。
pub(crate) const DESCRIBE_COMPLETED: ReplyTemplate = ReplyTemplate {
    key: "undercover.describe.completed",
    default: "所有存活玩家已描述，{投票说明}",
    summary: "描述完成，提示投票",
    group: "娱乐",
};

/// 空闲状态。
pub(crate) const STATUS_IDLE: ReplyTemplate = ReplyTemplate {
    key: "undercover.status.idle",
    default: "当前没有谁是卧底房间或牌局",
    summary: "#状态：空闲",
    group: "娱乐",
};

/// 报名大厅状态。
pub(crate) const STATUS_LOBBY: ReplyTemplate = ReplyTemplate {
    key: "undercover.status.lobby",
    default: "谁是卧底等待开局：{模式}，{人数}/{上限}人，玩家：{玩家列表}",
    summary: "#状态：报名大厅",
    group: "娱乐",
};

/// 进行中状态（紧凑格式）。
pub(crate) const STATUS_PLAYING: ReplyTemplate = ReplyTemplate {
    key: "undercover.status.playing",
    default: "{模式},第{轮次}轮,{阶段},{位置}",
    summary: "#状态：进行中（紧凑格式）",
    group: "娱乐",
};

/// 状态里的阶段词：描述阶段。
pub(crate) const PHASE_DESCRIBING: ReplyTemplate = ReplyTemplate {
    key: "undercover.phase.describing",
    default: "发言",
    summary: "#状态里的描述阶段词",
    group: "娱乐",
};

/// 状态里的阶段词：投票阶段。
pub(crate) const PHASE_VOTING: ReplyTemplate = ReplyTemplate {
    key: "undercover.phase.voting",
    default: "投票",
    summary: "#状态里的投票阶段词",
    group: "娱乐",
};

/// 局外好友私聊 #谜底 的私有答案。
pub(crate) const REVEAL: ReplyTemplate = ReplyTemplate {
    key: "undercover.reveal.answer",
    default: "平民词:{平民词},卧底词:{卧底词},卧底:{卧底位置}",
    summary: "#谜底：词语与卧底位置",
    group: "娱乐",
};

/// 创建者退出导致房间取消。
pub(crate) const EXIT_CREATOR: ReplyTemplate = ReplyTemplate {
    key: "undercover.exit.creator",
    default: "{玩家}退出，谁是卧底房间已取消",
    summary: "创建者退出，房间取消",
    group: "娱乐",
};

/// 普通玩家退出报名大厅。
pub(crate) const EXIT_LOBBY: ReplyTemplate = ReplyTemplate {
    key: "undercover.exit.lobby",
    default: "{玩家}退出谁是卧底房间，当前{人数}人",
    summary: "玩家退出报名大厅",
    group: "娱乐",
};

/// 对局中玩家主动退出。
pub(crate) const EXIT_PLAYING: ReplyTemplate = ReplyTemplate {
    key: "undercover.exit.playing",
    default: "{位置}主动退出：{身份}",
    summary: "对局中玩家主动退出",
    group: "娱乐",
};

/// 玩家退出后重新开始一轮。
pub(crate) const ROUND_RESTART: ReplyTemplate = ReplyTemplate {
    key: "undercover.round.restart",
    default: "第{轮次}轮重新开始，请存活玩家在公屏发送 #内容",
    summary: "退出后重新开始本轮",
    group: "娱乐",
};

/// 大厅被 #结束 取消。
pub(crate) const LOBBY_CANCELED: ReplyTemplate = ReplyTemplate {
    key: "undercover.lobby.canceled",
    default: "谁是卧底房间已取消",
    summary: "#结束：报名大厅取消",
    group: "娱乐",
};

/// 报名等待超时。
pub(crate) const LOBBY_TIMEOUT: ReplyTemplate = ReplyTemplate {
    key: "undercover.lobby.timeout",
    default: "谁是卧底报名等待超时，房间已取消",
    summary: "报名等待超时",
    group: "娱乐",
};

/// 发词超时。
pub(crate) const DELIVERY_TIMEOUT: ReplyTemplate = ReplyTemplate {
    key: "undercover.delivery.timeout",
    default: "谁是卧底发词超时，牌局已取消",
    summary: "发词超时",
    group: "娱乐",
};

/// 未投票提醒。
pub(crate) const VOTE_MISSING: ReplyTemplate = ReplyTemplate {
    key: "undercover.vote.missing",
    default: "未投票：{位置列表}",
    summary: "投票进度提醒",
    group: "娱乐",
};

/// 描述超时视为退出。
pub(crate) const DESCRIBE_TIMEOUT: ReplyTemplate = ReplyTemplate {
    key: "undercover.describe.timeout",
    default: "{位置}描述超时，视为退出：{身份}",
    summary: "描述超时视为退出",
    group: "娱乐",
};

/// 加赛描述超时且无法继续重投。
pub(crate) const RUNOFF_DESCRIBE_TIMEOUT: ReplyTemplate = ReplyTemplate {
    key: "undercover.runoff.describe_timeout",
    default: "加赛描述超时，无法继续重投；第{轮次}轮开始，请在公屏发送 #内容",
    summary: "加赛描述超时，进入下一轮",
    group: "娱乐",
};

/// 加赛描述阶段截止。
pub(crate) const RUNOFF_DESCRIBE_CUTOFF: ReplyTemplate = ReplyTemplate {
    key: "undercover.runoff.describe_cutoff",
    default: "加赛描述阶段已截止，其他存活玩家{投票说明}",
    summary: "加赛描述截止，提示投票",
    group: "娱乐",
};

/// 描述阶段截止。
pub(crate) const DESCRIBE_CUTOFF: ReplyTemplate = ReplyTemplate {
    key: "undercover.describe.cutoff",
    default: "描述阶段已截止，存活玩家{投票说明}",
    summary: "描述截止，提示投票",
    group: "娱乐",
};

/// 加赛仍然并列。
pub(crate) const RUNOFF_TIED: ReplyTemplate = ReplyTemplate {
    key: "undercover.runoff.tied",
    default: "加赛仍并列，本轮无人淘汰；第{轮次}轮开始，请在公屏发送 #内容",
    summary: "加赛仍并列，进入下一轮",
    group: "娱乐",
};

/// 首次出现最高票并列，进入加赛。
pub(crate) const RUNOFF_ENTER: ReplyTemplate = ReplyTemplate {
    key: "undercover.runoff.enter",
    default: "{位置列表}最高票并列，进入并列加赛，请并列玩家在公屏发送 #内容",
    summary: "最高票并列，进入加赛",
    group: "娱乐",
};

/// 本轮无人得票。
pub(crate) const VOTE_NONE: ReplyTemplate = ReplyTemplate {
    key: "undercover.vote.none",
    default: "本轮无人获得票数；第{轮次}轮开始，请存活玩家在公屏发送 #内容",
    summary: "无人得票，进入下一轮",
    group: "娱乐",
};

/// 玩家被淘汰。
pub(crate) const ELIMINATED: ReplyTemplate = ReplyTemplate {
    key: "undercover.vote.eliminated",
    default: "{位置}已淘汰：{身份}",
    summary: "投票淘汰公告",
    group: "娱乐",
};

/// 新一轮开始。
pub(crate) const ROUND_START: ReplyTemplate = ReplyTemplate {
    key: "undercover.round.start",
    default: "第{轮次}轮开始，请存活玩家在公屏发送 #内容",
    summary: "新一轮开始提示",
    group: "娱乐",
};

/// 结算首行（注意"结束："后是两个半角空格）。
pub(crate) const SETTLEMENT_HEADER: ReplyTemplate = ReplyTemplate {
    key: "undercover.settlement.header",
    default: "谁是卧底结束：{胜方}  平民词：{平民词}  卧底词：{卧底词}",
    summary: "结算首行：胜方与词语",
    group: "娱乐",
};

/// 结算逐人一行。
pub(crate) const SETTLEMENT_PLAYER: ReplyTemplate = ReplyTemplate {
    key: "undercover.settlement.player",
    default: "{位置}：{玩家}（{身份}）",
    summary: "结算中的单个玩家",
    group: "娱乐",
};

/// 结算末行：总轮数。
pub(crate) const SETTLEMENT_ROUNDS: ReplyTemplate = ReplyTemplate {
    key: "undercover.settlement.rounds",
    default: "共进行{轮次}轮",
    summary: "结算末行：总轮数",
    group: "娱乐",
};

/// 位置映射片段（每行由 | 连接多个片段）。
pub(crate) const POSITION_MAPPING: ReplyTemplate = ReplyTemplate {
    key: "undercover.start.position_mapping",
    default: "{位置}={玩家}",
    summary: "开局位置与昵称映射",
    group: "娱乐",
};

/// 胜方：平民。
pub(crate) const WINNER_CIVILIAN: ReplyTemplate = ReplyTemplate {
    key: "undercover.winner.civilian",
    default: "平民胜利",
    summary: "结算胜方：平民",
    group: "娱乐",
};

/// 胜方：卧底。
pub(crate) const WINNER_UNDERCOVER: ReplyTemplate = ReplyTemplate {
    key: "undercover.winner.undercover",
    default: "卧底胜利",
    summary: "结算胜方：卧底",
    group: "娱乐",
};

/// 胜方：无。
pub(crate) const WINNER_NONE: ReplyTemplate = ReplyTemplate {
    key: "undercover.winner.none",
    default: "无胜方",
    summary: "结算胜方：无",
    group: "娱乐",
};

/// 身份词：平民。
pub(crate) const ROLE_CIVILIAN: ReplyTemplate = ReplyTemplate {
    key: "undercover.role.civilian",
    default: "平民",
    summary: "玩家身份词：平民",
    group: "娱乐",
};

/// 身份词：卧底。
pub(crate) const ROLE_UNDERCOVER: ReplyTemplate = ReplyTemplate {
    key: "undercover.role.undercover",
    default: "卧底",
    summary: "玩家身份词：卧底",
    group: "娱乐",
};

/// 私聊投递尚未完成。
pub(crate) const DELIVERY_WAITING: ReplyTemplate = ReplyTemplate {
    key: "undercover.delivery.waiting",
    default: "谁是卧底私聊投递尚未完成，请使用#重试或#结束",
    summary: "私聊投递未完成时的提示",
    group: "娱乐",
};

/// 报名成功（创建房间）。
pub(crate) const VERIFY_CREATE_OK: ReplyTemplate = ReplyTemplate {
    key: "undercover.verify.create_success",
    default: "谁是卧底报名成功，请回到大厅等待组局",
    summary: "创建房间的好友验证消息",
    group: "娱乐",
};

/// 报名成功（加入房间）。
pub(crate) const VERIFY_JOIN_OK: ReplyTemplate = ReplyTemplate {
    key: "undercover.verify.join_success",
    default: "谁是卧底报名成功，请回到大厅等待开局",
    summary: "加入房间的好友验证消息",
    group: "娱乐",
};

/// 报名失败：好友列表找不到唯一昵称。
pub(crate) const VERIFY_FAILED: ReplyTemplate = ReplyTemplate {
    key: "undercover.verify.failed",
    default: "谁是卧底报名失败：好友列表未找到唯一昵称",
    summary: "好友验证失败",
    group: "娱乐",
};

/// 加入时没有报名房间。
pub(crate) const ERROR_NO_LOBBY: ReplyTemplate = ReplyTemplate {
    key: "undercover.error.no_lobby",
    default: "当前没有谁是卧底报名房间",
    summary: "加入时没有报名房间",
    group: "娱乐",
};

/// 加入时已经开局。
pub(crate) const ERROR_ALREADY_STARTED: ReplyTemplate = ReplyTemplate {
    key: "undercover.error.already_started",
    default: "谁是卧底已经开局",
    summary: "加入时已经开局",
    group: "娱乐",
};

/// 重复加入。
pub(crate) const ERROR_ALREADY_JOINED: ReplyTemplate = ReplyTemplate {
    key: "undercover.error.already_joined",
    default: "你已经加入本局谁是卧底",
    summary: "重复加入",
    group: "娱乐",
};

/// 功能未启用。
pub(crate) const ERROR_DISABLED: ReplyTemplate = ReplyTemplate {
    key: "undercover.error.disabled",
    default: "谁是卧底未启用",
    summary: "功能未启用",
    group: "娱乐",
};

/// 已有房间或牌局。
pub(crate) const ERROR_ALREADY_ACTIVE: ReplyTemplate = ReplyTemplate {
    key: "undercover.error.already_active",
    default: "已有谁是卧底房间或牌局进行中",
    summary: "已有房间或牌局",
    group: "娱乐",
};

/// 被其他娱乐玩法占用。
pub(crate) const ERROR_OCCUPIED: ReplyTemplate = ReplyTemplate {
    key: "undercover.error.occupied",
    default: "{玩法}正在进行，请结束后再开始谁是卧底",
    summary: "其他娱乐玩法正在进行",
    group: "娱乐",
};

/// #谜底 只能私聊。
pub(crate) const REVEAL_FRIEND_ONLY: ReplyTemplate = ReplyTemplate {
    key: "undercover.reveal.friend_only",
    default: "#谜底只能通过好友私聊使用",
    summary: "#谜底 的非好友使用提示",
    group: "娱乐",
};

/// 没有待重试的投递。
pub(crate) const RETRY_NONE: ReplyTemplate = ReplyTemplate {
    key: "undercover.retry.none",
    default: "当前没有待重试的谁是卧底私聊投递",
    summary: "#重试：没有待重试投递",
    group: "娱乐",
};

/// 待重试投递已失效。
pub(crate) const RETRY_EXPIRED: ReplyTemplate = ReplyTemplate {
    key: "undercover.retry.expired",
    default: "待重试投递已失效",
    summary: "#重试：会话已失效",
    group: "娱乐",
};

/// 结果未知不能重试（带原因）。
pub(crate) const RETRY_UNSAFE: ReplyTemplate = ReplyTemplate {
    key: "undercover.retry.unsafe",
    default: "谁是卧底私聊投递结果未知，不能重试，请#结束：{原因}",
    summary: "#重试：结果未知不能重试",
    group: "娱乐",
};

/// 投递结果未知。
pub(crate) const DELIVERY_RESULT_UNKNOWN: ReplyTemplate = ReplyTemplate {
    key: "undercover.delivery.result_unknown",
    default: "谁是卧底私聊投递结果未知，不能重试，请#结束",
    summary: "私聊投递结果未知",
    group: "娱乐",
};

/// 投递失败且没有安全剩余项。
pub(crate) const DELIVERY_NO_SAFE: ReplyTemplate = ReplyTemplate {
    key: "undercover.delivery.no_safe_remainder",
    default: "谁是卧底私聊投递失败且没有安全剩余项，请#结束",
    summary: "私聊投递失败且无安全剩余项",
    group: "娱乐",
};

/// 投递未完成（有可重试剩余项）。
pub(crate) const DELIVERY_INCOMPLETE: ReplyTemplate = ReplyTemplate {
    key: "undercover.delivery.incomplete",
    default: "谁是卧底私聊投递未完成，请#重试或#结束",
    summary: "私聊投递未完成",
    group: "娱乐",
};

/// 全部谁是卧底回复模板。
pub(crate) const UNDERCOVER: &[ReplyTemplate] = &[
    VOTE_INSTRUCTION,
    SECRET,
    OPENING,
    DESCRIBE_PROMPT,
    DELIVERY_CANCELED,
    RUNOFF_NO_VOTERS,
    RUNOFF_DESCRIBED,
    DESCRIBE_COMPLETED,
    STATUS_IDLE,
    STATUS_LOBBY,
    STATUS_PLAYING,
    PHASE_DESCRIBING,
    PHASE_VOTING,
    REVEAL,
    EXIT_CREATOR,
    EXIT_LOBBY,
    EXIT_PLAYING,
    ROUND_RESTART,
    LOBBY_CANCELED,
    LOBBY_TIMEOUT,
    DELIVERY_TIMEOUT,
    VOTE_MISSING,
    DESCRIBE_TIMEOUT,
    RUNOFF_DESCRIBE_TIMEOUT,
    RUNOFF_DESCRIBE_CUTOFF,
    DESCRIBE_CUTOFF,
    RUNOFF_TIED,
    RUNOFF_ENTER,
    VOTE_NONE,
    ELIMINATED,
    ROUND_START,
    SETTLEMENT_HEADER,
    SETTLEMENT_PLAYER,
    SETTLEMENT_ROUNDS,
    POSITION_MAPPING,
    WINNER_CIVILIAN,
    WINNER_UNDERCOVER,
    WINNER_NONE,
    ROLE_CIVILIAN,
    ROLE_UNDERCOVER,
    DELIVERY_WAITING,
    VERIFY_CREATE_OK,
    VERIFY_JOIN_OK,
    VERIFY_FAILED,
    ERROR_NO_LOBBY,
    ERROR_ALREADY_STARTED,
    ERROR_ALREADY_JOINED,
    ERROR_DISABLED,
    ERROR_ALREADY_ACTIVE,
    ERROR_OCCUPIED,
    REVEAL_FRIEND_ONLY,
    RETRY_NONE,
    RETRY_EXPIRED,
    RETRY_UNSAFE,
    DELIVERY_RESULT_UNKNOWN,
    DELIVERY_NO_SAFE,
    DELIVERY_INCOMPLETE,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::reply_template::{ReplyVars, catalog, render};
    use std::collections::BTreeSet;

    #[test]
    fn every_undercover_template_is_registered_in_the_catalog() {
        let registered: BTreeSet<&str> = catalog().map(|spec| spec.key).collect();
        for spec in UNDERCOVER {
            assert!(
                registered.contains(spec.key),
                "{} 未登记进 catalog()",
                spec.key
            );
            assert!(!spec.default.trim().is_empty(), "{} 默认文案为空", spec.key);
            assert_eq!(spec.group, "娱乐", "{} 分组应为娱乐", spec.key);
            assert!(!spec.summary.trim().is_empty(), "{} 缺少摘要", spec.key);
        }
        assert!(!UNDERCOVER.is_empty(), "谁是卧底模板目录不应为空");
    }

    #[test]
    fn rendering_undercover_templates_reproduces_the_pre_migration_strings() {
        // 改造前这些文案是 format! 直接拼出来的；渲染结果必须逐字一致。
        let vars = ReplyVars::new().with("位置", "A").with("词语", "苹果");
        assert_eq!(
            render(&SECRET, None, &vars).text,
            "你的位置：A；你的词语：苹果"
        );

        let vars = ReplyVars::new().with("模式", "单卧底").with("人数", "4");
        assert_eq!(
            render(&OPENING, None, &vars).text,
            "谁是卧底开始：单卧底，共4人"
        );
        assert_eq!(
            render(&DESCRIBE_PROMPT, None, &ReplyVars::new()).text,
            "请存活玩家在公屏发送 #内容"
        );

        let vars = ReplyVars::new().with("轮次", "2");
        assert_eq!(
            render(&RUNOFF_NO_VOTERS, None, &vars).text,
            "并列玩家无人可投票，本轮无人淘汰；第2轮开始，请在公屏发送 #内容"
        );
        assert_eq!(
            render(&ROUND_START, None, &vars).text,
            "第2轮开始，请存活玩家在公屏发送 #内容"
        );

        let instruction = render(&VOTE_INSTRUCTION, None, &ReplyVars::new()).text;
        let vars = ReplyVars::new().with("投票说明", instruction);
        assert_eq!(
            render(&DESCRIBE_COMPLETED, None, &vars).text,
            "所有存活玩家已描述，请好友私聊#怀疑对象编号，例如#A，或发送#弃权"
        );

        let vars = ReplyVars::new()
            .with("模式", "双卧底")
            .with("人数", "4")
            .with("上限", "11")
            .with("玩家列表", "甲、乙");
        assert_eq!(
            render(&STATUS_LOBBY, None, &vars).text,
            "谁是卧底等待开局：双卧底，4/11人，玩家：甲、乙"
        );

        let vars = ReplyVars::new()
            .with("模式", "单卧底")
            .with("轮次", "1")
            .with("阶段", "发言")
            .with("位置", "abcd");
        assert_eq!(
            render(&STATUS_PLAYING, None, &vars).text,
            "单卧底,第1轮,发言,abcd"
        );

        let vars = ReplyVars::new()
            .with("平民词", "苹果")
            .with("卧底词", "梨")
            .with("卧底位置", "B");
        assert_eq!(
            render(&REVEAL, None, &vars).text,
            "平民词:苹果,卧底词:梨,卧底:B"
        );

        let vars = ReplyVars::new().with("玩家", "甲");
        assert_eq!(
            render(&EXIT_CREATOR, None, &vars).text,
            "甲退出，谁是卧底房间已取消"
        );
        let vars = ReplyVars::new().with("玩家", "甲").with("人数", "3");
        assert_eq!(
            render(&EXIT_LOBBY, None, &vars).text,
            "甲退出谁是卧底房间，当前3人"
        );
        let vars = ReplyVars::new().with("位置", "A").with("身份", "卧底");
        assert_eq!(render(&EXIT_PLAYING, None, &vars).text, "A主动退出：卧底");

        let vars = ReplyVars::new().with("位置列表", "A、B");
        assert_eq!(render(&VOTE_MISSING, None, &vars).text, "未投票：A、B");
        assert_eq!(
            render(&RUNOFF_ENTER, None, &vars).text,
            "A、B最高票并列，进入并列加赛，请并列玩家在公屏发送 #内容"
        );

        let vars = ReplyVars::new()
            .with("胜方", "平民胜利")
            .with("平民词", "苹果")
            .with("卧底词", "梨");
        assert_eq!(
            render(&SETTLEMENT_HEADER, None, &vars).text,
            "谁是卧底结束：平民胜利  平民词：苹果  卧底词：梨"
        );
        let vars = ReplyVars::new()
            .with("位置", "A")
            .with("玩家", "甲")
            .with("身份", "平民");
        assert_eq!(
            render(&SETTLEMENT_PLAYER, None, &vars).text,
            "A：甲（平民）"
        );
        let vars = ReplyVars::new().with("位置", "A").with("玩家", "甲");
        assert_eq!(render(&POSITION_MAPPING, None, &vars).text, "A=甲");

        let vars = ReplyVars::new().with("玩法", "斗地主");
        assert_eq!(
            render(&ERROR_OCCUPIED, None, &vars).text,
            "斗地主正在进行，请结束后再开始谁是卧底"
        );
        let vars = ReplyVars::new().with("原因", "结果未知");
        assert_eq!(
            render(&RETRY_UNSAFE, None, &vars).text,
            "谁是卧底私聊投递结果未知，不能重试，请#结束：结果未知"
        );
    }
}
