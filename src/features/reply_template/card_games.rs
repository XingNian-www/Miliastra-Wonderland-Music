//! 牌类玩法（斗地主/跑得快）回复文案。key 一旦发布不要改名。
//!
//! 默认文案逐字沿用改造前 `src/features/card_games.rs` 里的硬编码字符串：
//! 标点（全角/半角）、空格都保持原样。改默认文案等于改玩家可见行为，
//! 不属于"可调整回复"的范畴——要改请走 `replies.overrides` 配置。
//!
//! 角色名（地主/农民）、动作词（出/托管出/超时出/自动出）、牌型名、牌面本身
//! 都是数据（来自牌局状态或牌库），以变量注入，不作为模板的一部分。
//!
//! 注意：本模块的回复在 effect 入队前就渲染成 String（`CardGameEffect::HallDelivery`
//! 携带成品文案），组装层拿不到模板键，因此这些 key 目前**覆盖不生效**；
//! 升级路径见任务汇报。

use super::ReplyTemplate;

const fn spec(key: &'static str, default: &'static str, summary: &'static str) -> ReplyTemplate {
    ReplyTemplate {
        key,
        default,
        summary,
        group: "娱乐",
    }
}

/// 牌类玩法未启用。
pub(crate) const DISABLED: ReplyTemplate =
    spec("card_games.disabled", "{玩法}未启用", "玩法未启用");

/// 已有牌局或房间进行中。
pub(crate) const ALREADY_ACTIVE: ReplyTemplate = spec(
    "card_games.already_active",
    "已有牌局或房间进行中",
    "已有牌局进行中",
);

/// 创建房间成功。
pub(crate) const LOBBY_CREATED: ReplyTemplate = spec(
    "card_games.lobby.created",
    "{玩家}创建了{玩法}房间，还需2人，发送 #加入 参加",
    "创建牌局房间",
);

/// 没有可加入的房间。
pub(crate) const LOBBY_NONE: ReplyTemplate = spec(
    "card_games.lobby.none",
    "当前没有等待加入的牌局房间",
    "没有等待加入的房间",
);

/// 已经加入本局。
pub(crate) const LOBBY_ALREADY_JOINED: ReplyTemplate = spec(
    "card_games.lobby.already_joined",
    "你已经加入本局{玩法}",
    "已经加入本局",
);

/// 加入房间后还差一人。
pub(crate) const LOBBY_JOINED: ReplyTemplate = spec(
    "card_games.lobby.joined",
    "{玩家}加入{玩法}，还需1人",
    "加入房间，还差一人",
);

/// 跑得快开局（发牌并指定黑桃3先出）。
pub(crate) const RUN_FAST_STARTED: ReplyTemplate = spec(
    "card_games.run_fast.started",
    "跑得快已发手牌，{玩家}持有黑桃3并先出；第一手必须包含黑桃3",
    "跑得快开局",
);

/// 斗地主发牌进入抢地主。
pub(crate) const BIDDING_STARTED: ReplyTemplate = spec(
    "card_games.bidding.started",
    "斗地主已发手牌，随机由{玩家}开始抢地主；发送 #抢 或 #不抢",
    "斗地主开局抢地主",
);

/// 没有待重试的私聊投递。
pub(crate) const RETRY_NONE: ReplyTemplate = spec(
    "card_games.retry.none",
    "当前没有待重试的私聊投递",
    "没有待重试投递",
);

/// 当前不在抢地主阶段。
pub(crate) const BIDDING_NOT_BIDDING: ReplyTemplate = spec(
    "card_games.bidding.not_bidding",
    "当前不在抢地主阶段",
    "不在抢地主阶段",
);

/// 不在本局斗地主中（bidding 阶段固定文案）。
pub(crate) const NOT_IN_LANDLORD_GAME: ReplyTemplate = spec(
    "card_games.game.not_in_landlord",
    "你不在本局斗地主中",
    "不在本局斗地主中",
);

/// 轮到某人抢地主。
pub(crate) const BIDDING_TURN: ReplyTemplate = spec(
    "card_games.bidding.turn",
    "现在轮到{玩家}抢地主",
    "轮到抢地主",
);

/// 一次抢/不抢之后的推进。
///
/// `{决定}` 是"抢地主"或"不抢"（数据），由调用方注入。
pub(crate) const BIDDING_DECIDED: ReplyTemplate = spec(
    "card_games.bidding.decided",
    "{玩家}{决定}；轮到{下一位}抢地主",
    "抢地主阶段推进",
);

/// 三人均未抢地主，重新发牌。
pub(crate) const BIDDING_REDEALT: ReplyTemplate = spec(
    "card_games.bidding.redealt",
    "三人均未抢地主，已重新发牌；随机由{玩家}开始",
    "流局重新发牌",
);

/// 每位玩家的初始手牌（私聊，逐人发送）。
pub(crate) const HAND_INITIAL: ReplyTemplate = spec(
    "card_games.hand.initial",
    "{玩法}初始手牌({张数}张): {手牌}",
    "发牌时的初始手牌",
);

/// 状态里"某人还剩几张"的单项。
pub(crate) const PLAYER_CARD_COUNT: ReplyTemplate = spec(
    "card_games.status.player_count",
    "{玩家} {张数}张",
    "状态里的剩余张数条目",
);

/// 地主初始手牌（私聊）。
pub(crate) const HAND_LANDLORD_INITIAL: ReplyTemplate = spec(
    "card_games.hand.landlord_initial",
    "地主手牌({张数}张): {手牌}",
    "地主初始手牌",
);

/// 地主产生。
pub(crate) const LANDLORD_SELECTED: ReplyTemplate = spec(
    "card_games.landlord.selected",
    "{玩家}成为地主并先出牌；底牌: {底牌}",
    "地主确定",
);

/// 组局等待超时。
pub(crate) const LOBBY_TIMEOUT: ReplyTemplate = spec(
    "card_games.lobby.timeout",
    "{玩法}组局等待超时，房间已取消",
    "组局等待超时",
);

/// 抢地主剩余 30 秒。
pub(crate) const BIDDING_WARNING: ReplyTemplate = spec(
    "card_games.bidding.warning",
    "{玩家}抢地主剩余30秒",
    "抢地主倒计时提醒",
);

/// 抢地主超时，按不抢处理。
pub(crate) const BIDDING_TIMEOUT: ReplyTemplate = spec(
    "card_games.bidding.timeout",
    "{玩家}抢地主超时，按不抢处理；{后续}",
    "抢地主超时",
);

/// 出牌剩余 30 秒。
pub(crate) const TURN_WARNING: ReplyTemplate = spec(
    "card_games.turn.warning",
    "{玩家}出牌剩余30秒",
    "出牌倒计时提醒",
);

/// 跑得快超时且无牌可压，自动过牌。
pub(crate) const PASS_TIMEOUT_NO_BEATING: ReplyTemplate = spec(
    "card_games.pass.timeout_no_beating",
    "{玩家}超时且无牌可压，自动过牌；轮到{下一位}",
    "超时无牌可压自动过",
);

/// 斗地主超时自动过牌。
pub(crate) const PASS_TIMEOUT: ReplyTemplate = spec(
    "card_games.pass.timeout",
    "{玩家}超时，自动过牌；轮到{下一位}",
    "超时自动过牌",
);

/// 托管/超时自动过牌（带"超时{托管后缀}"）。
pub(crate) const PASS_TIMEOUT_WITH_SUFFIX: ReplyTemplate = spec(
    "card_games.pass.timeout_with_suffix",
    "{玩家}超时{托管后缀}，自动过牌；轮到{下一位}",
    "超时（含托管）自动过牌",
);

/// 当前没有进行中的牌局（出牌/过牌共用）。
pub(crate) const NO_ACTIVE_GAME: ReplyTemplate = spec(
    "card_games.game.no_active",
    "当前没有进行中的牌局",
    "没有进行中的牌局",
);

/// 不在本局（带玩法名）。
pub(crate) const NOT_IN_GAME: ReplyTemplate =
    spec("card_games.game.not_in", "你不在本局{玩法}中", "不在本局中");

/// 还没轮到这位玩家。
pub(crate) const WRONG_TURN: ReplyTemplate = spec(
    "card_games.play.wrong_turn",
    "现在轮到{玩家}出牌",
    "还没轮到出牌",
);

/// 没有输入要出的牌。
pub(crate) const PLAY_EMPTY: ReplyTemplate =
    spec("card_games.play.empty", "请输入要出的牌", "未输入要出的牌");

/// 跑得快首手不含黑桃3。
pub(crate) const SPADE_THREE_REQUIRED: ReplyTemplate = spec(
    "card_games.play.spade_three_required",
    "首手必须包含黑桃3",
    "首手必须含黑桃3",
);

/// 牌型无效。
pub(crate) const INVALID_PATTERN: ReplyTemplate = spec(
    "card_games.play.invalid_pattern",
    "这组牌不是当前玩法的有效牌型",
    "无效牌型",
);

/// 下家报单时的单张限制。
pub(crate) const MUST_PLAY_LARGEST_SINGLE: ReplyTemplate = spec(
    "card_games.play.must_play_largest_single",
    "下家已报单，出单张时必须出手中最大牌",
    "报单时必须出最大单张",
);

/// 压不过上一手。
pub(crate) const CANNOT_BEAT: ReplyTemplate = spec(
    "card_games.play.cannot_beat",
    "{牌型}压不过上一手{上一手牌型}",
    "牌型压不过上一手",
);

/// 领出者不能过牌。
pub(crate) const LEADER_CANNOT_PASS: ReplyTemplate = spec(
    "card_games.pass.leader_cannot_pass",
    "你是本轮领出者，不能过牌",
    "领出者不能过牌",
);

/// 跑得快有牌能压时不能过牌。
pub(crate) const MUST_PLAY_RUN_FAST: ReplyTemplate = spec(
    "card_games.pass.must_play",
    "跑得快有牌能压时不能过牌",
    "跑得快必须压牌",
);

/// 没有已发牌的牌局。
pub(crate) const HAND_NO_DEALT_GAME: ReplyTemplate = spec(
    "card_games.hand.no_dealt_game",
    "当前没有已发牌的牌局",
    "没有已发牌的牌局",
);

/// 当前手牌（私聊）。
pub(crate) const HAND_CURRENT: ReplyTemplate = spec(
    "card_games.hand.current",
    "当前手牌({张数}张): {手牌}",
    "查询当前手牌",
);

/// 空闲状态。
pub(crate) const STATUS_IDLE: ReplyTemplate = spec(
    "card_games.status.idle",
    "当前没有牌局房间或进行中的牌局",
    "空闲状态",
);

/// 房间等待加入状态。
pub(crate) const STATUS_LOBBY: ReplyTemplate = spec(
    "card_games.status.lobby",
    "{玩法}等待加入: {玩家列表}，还需{人数}人",
    "房间状态",
);

/// 抢地主阶段状态。
pub(crate) const STATUS_BIDDING: ReplyTemplate = spec(
    "card_games.status.bidding",
    "斗地主抢地主中；轮到:{玩家}；当前抢地主:{当前抢地主}",
    "抢地主阶段状态",
);

/// 跑得快进行中状态。
pub(crate) const STATUS_PLAYING_RUN_FAST: ReplyTemplate = spec(
    "card_games.status.playing_run_fast",
    "跑得快；轮到:{玩家}；剩余:{剩余}；上一手:{上一手}",
    "跑得快进行中状态",
);

/// 斗地主进行中状态。
pub(crate) const STATUS_PLAYING_LANDLORD: ReplyTemplate = spec(
    "card_games.status.playing_landlord",
    "地主:{地主}；轮到:{玩家}；剩余:{剩余}；上一手:{上一手}",
    "斗地主进行中状态",
);

/// 不在当前房间中。
pub(crate) const NOT_IN_LOBBY: ReplyTemplate = spec(
    "card_games.exit.not_in_lobby",
    "你不在当前{玩法}房间中",
    "不在当前房间中",
);

/// 房主取消房间。
pub(crate) const LOBBY_CANCELED: ReplyTemplate = spec(
    "card_games.lobby.canceled",
    "{玩家}取消了{玩法}房间",
    "取消房间",
);

/// 退出房间。
pub(crate) const LOBBY_LEFT: ReplyTemplate = spec(
    "card_games.lobby.left",
    "{玩家}退出了{玩法}房间",
    "退出房间",
);

/// 抢地主阶段退出，牌局结束（固定斗地主）。
pub(crate) const EXIT_LANDLORD_ENDED: ReplyTemplate = spec(
    "card_games.exit.landlord_ended",
    "{玩家}退出，斗地主牌局已结束",
    "退出并结束斗地主",
);

/// 进行中退出，牌局结束。
pub(crate) const EXIT_GAME_ENDED: ReplyTemplate = spec(
    "card_games.exit.game_ended",
    "{玩家}退出，{玩法}牌局已结束",
    "退出并结束牌局",
);

/// 跑得快获胜。
pub(crate) const WON_RUN_FAST: ReplyTemplate = spec(
    "card_games.won.run_fast",
    "{玩家}{动作}:{牌面}[{牌型}]，获胜；其余:{剩余}",
    "跑得快获胜",
);

/// 斗地主获胜。
pub(crate) const WON_LANDLORD: ReplyTemplate = spec(
    "card_games.won.landlord",
    "{玩家}({角色})出完{牌面}[{牌型}]，{阵营}获胜。剩余:{剩余}；炸弹{炸弹}次；共{手数}手",
    "斗地主获胜",
);

/// 跑得快出牌。
pub(crate) const PLAY_RUN_FAST: ReplyTemplate = spec(
    "card_games.play.run_fast",
    "{玩家}{动作}:{牌面}[{牌型}] 余{剩余}，轮到{下一位}",
    "跑得快出牌",
);

/// 斗地主出牌动作：手动。
pub(crate) const LANDLORD_ACTION_PLAY: ReplyTemplate =
    spec("card_games.action.play", "出牌", "斗地主手动出牌的动作词");

/// 斗地主出牌动作：超时自动出。
pub(crate) const LANDLORD_ACTION_TIMEOUT: ReplyTemplate = spec(
    "card_games.action.timeout",
    "超时{托管后缀}，自动出",
    "斗地主超时自动出牌的动作词",
);

/// 斗地出牌。
pub(crate) const PLAY_LANDLORD: ReplyTemplate = spec(
    "card_games.play.landlord",
    "{玩家}({角色}){动作} {牌面}[{牌型}]，剩余{剩余}张；轮到{下一位}",
    "斗地主出牌",
);

/// 跑得快手动过牌，下一家继续。
pub(crate) const PASS_MANUAL_NEXT: ReplyTemplate = spec(
    "card_games.pass.manual_next",
    "{玩家}过，轮到{下一位}",
    "手动过牌",
);

/// 跑得快手动过牌，下一家领出。
pub(crate) const PASS_MANUAL_LEAD: ReplyTemplate = spec(
    "card_games.pass.manual_lead",
    "{玩家}过，{下一位}领出",
    "手动过牌后领出",
);

/// 跑得快超时过牌，下一家继续。
pub(crate) const PASS_TIMEOUT_NEXT: ReplyTemplate = spec(
    "card_games.pass.timeout_next",
    "{玩家}超时过，轮到{下一位}",
    "超时过牌",
);

/// 跑得快超时过牌，下一家领出。
pub(crate) const PASS_TIMEOUT_LEAD: ReplyTemplate = spec(
    "card_games.pass.timeout_lead",
    "{玩家}超时过，{下一位}领出",
    "超时过牌后领出",
);

/// 跑得快无牌可压。
pub(crate) const PASS_FORCED_NEXT: ReplyTemplate = spec(
    "card_games.pass.forced_next",
    "{玩家}无牌可压，轮到{下一位}",
    "无牌可压",
);

/// 双方都无牌可压。
pub(crate) const PASS_FORCED_LEAD: ReplyTemplate = spec(
    "card_games.pass.forced_lead",
    "双方无牌可压，{下一位}领出",
    "双方无牌可压后领出",
);

/// 斗地主过牌后重新领出。
pub(crate) const PASS_NEW_TRICK: ReplyTemplate = spec(
    "card_games.pass.new_trick",
    "{玩家}过牌，重新由{下一位}领出",
    "过牌后新轮次",
);

/// 斗地主过牌，下一家继续。
pub(crate) const PASS_NEXT: ReplyTemplate =
    spec("card_games.pass.next", "{玩家}过牌，轮到{下一位}", "过牌");

/// 出牌文本里有无法识别的牌面。
pub(crate) const PARSE_UNKNOWN_CARD: ReplyTemplate = spec(
    "card_games.parse.unknown_card",
    "无法识别牌面: {牌面}",
    "无法识别牌面",
);

/// 手牌里缺少要出的牌。
pub(crate) const PARSE_MISSING_CARD: ReplyTemplate = spec(
    "card_games.parse.missing_card",
    "手牌中缺少 {牌面}",
    "手牌缺少指定牌",
);

/// 斗地主/跑得快全部模板。键一旦发布不要改名。
pub(crate) const CARD_GAMES: &[ReplyTemplate] = &[
    DISABLED,
    ALREADY_ACTIVE,
    LOBBY_CREATED,
    LOBBY_NONE,
    LOBBY_ALREADY_JOINED,
    LOBBY_JOINED,
    RUN_FAST_STARTED,
    BIDDING_STARTED,
    RETRY_NONE,
    BIDDING_NOT_BIDDING,
    NOT_IN_LANDLORD_GAME,
    BIDDING_TURN,
    BIDDING_DECIDED,
    BIDDING_REDEALT,
    HAND_INITIAL,
    HAND_LANDLORD_INITIAL,
    PLAYER_CARD_COUNT,
    LANDLORD_SELECTED,
    LOBBY_TIMEOUT,
    BIDDING_WARNING,
    BIDDING_TIMEOUT,
    TURN_WARNING,
    PASS_TIMEOUT_NO_BEATING,
    PASS_TIMEOUT,
    PASS_TIMEOUT_WITH_SUFFIX,
    NO_ACTIVE_GAME,
    NOT_IN_GAME,
    WRONG_TURN,
    PLAY_EMPTY,
    SPADE_THREE_REQUIRED,
    INVALID_PATTERN,
    MUST_PLAY_LARGEST_SINGLE,
    CANNOT_BEAT,
    LEADER_CANNOT_PASS,
    MUST_PLAY_RUN_FAST,
    HAND_NO_DEALT_GAME,
    HAND_CURRENT,
    STATUS_IDLE,
    STATUS_LOBBY,
    STATUS_BIDDING,
    STATUS_PLAYING_RUN_FAST,
    STATUS_PLAYING_LANDLORD,
    NOT_IN_LOBBY,
    LOBBY_CANCELED,
    LOBBY_LEFT,
    EXIT_LANDLORD_ENDED,
    EXIT_GAME_ENDED,
    WON_RUN_FAST,
    WON_LANDLORD,
    PLAY_RUN_FAST,
    LANDLORD_ACTION_PLAY,
    LANDLORD_ACTION_TIMEOUT,
    PLAY_LANDLORD,
    PASS_MANUAL_NEXT,
    PASS_MANUAL_LEAD,
    PASS_TIMEOUT_NEXT,
    PASS_TIMEOUT_LEAD,
    PASS_FORCED_NEXT,
    PASS_FORCED_LEAD,
    PASS_NEW_TRICK,
    PASS_NEXT,
    PARSE_UNKNOWN_CARD,
    PARSE_MISSING_CARD,
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
    fn every_card_game_template_is_registered_in_the_catalog() {
        let registered: BTreeSet<&str> = catalog().map(|spec| spec.key).collect();
        for spec in CARD_GAMES {
            assert!(
                registered.contains(spec.key),
                "{} 未登记进 catalog()",
                spec.key
            );
            assert!(!spec.default.trim().is_empty(), "{} 默认文案为空", spec.key);
            assert_eq!(spec.group, "娱乐", "{} 分组应为娱乐", spec.key);
            assert!(!spec.summary.trim().is_empty(), "{} 缺少摘要", spec.key);
        }
        assert!(!CARD_GAMES.is_empty(), "牌类模板目录不应为空");
    }

    #[test]
    fn rendering_card_game_templates_reproduces_the_pre_migration_strings() {
        // 每条渲染结果都必须与改造前 card_games.rs 里的 format! 输出逐字一致。
        assert_eq!(
            text(&DISABLED, ReplyVars::new().with("玩法", "斗地主")),
            "斗地主未启用"
        );
        assert_eq!(
            text(&ALREADY_ACTIVE, ReplyVars::new()),
            "已有牌局或房间进行中"
        );
        assert_eq!(
            text(
                &LOBBY_CREATED,
                ReplyVars::new().with("玩家", "甲").with("玩法", "斗地主")
            ),
            "甲创建了斗地主房间，还需2人，发送 #加入 参加"
        );
        assert_eq!(
            text(&LOBBY_NONE, ReplyVars::new()),
            "当前没有等待加入的牌局房间"
        );
        assert_eq!(
            text(
                &LOBBY_ALREADY_JOINED,
                ReplyVars::new().with("玩法", "跑得快")
            ),
            "你已经加入本局跑得快"
        );
        assert_eq!(
            text(
                &LOBBY_JOINED,
                ReplyVars::new().with("玩家", "乙").with("玩法", "跑得快")
            ),
            "乙加入跑得快，还需1人"
        );
        assert_eq!(
            text(&RUN_FAST_STARTED, ReplyVars::new().with("玩家", "甲")),
            "跑得快已发手牌，甲持有黑桃3并先出；第一手必须包含黑桃3"
        );
        assert_eq!(
            text(&BIDDING_STARTED, ReplyVars::new().with("玩家", "乙")),
            "斗地主已发手牌，随机由乙开始抢地主；发送 #抢 或 #不抢"
        );
        assert_eq!(
            text(&RETRY_NONE, ReplyVars::new()),
            "当前没有待重试的私聊投递"
        );
        assert_eq!(
            text(&BIDDING_NOT_BIDDING, ReplyVars::new()),
            "当前不在抢地主阶段"
        );
        assert_eq!(
            text(&NOT_IN_LANDLORD_GAME, ReplyVars::new()),
            "你不在本局斗地主中"
        );
        assert_eq!(
            text(&BIDDING_TURN, ReplyVars::new().with("玩家", "丙")),
            "现在轮到丙抢地主"
        );
        assert_eq!(
            text(
                &BIDDING_DECIDED,
                ReplyVars::new()
                    .with("玩家", "甲")
                    .with("决定", "抢地主")
                    .with("下一位", "乙")
            ),
            "甲抢地主；轮到乙抢地主"
        );
        assert_eq!(
            text(&BIDDING_REDEALT, ReplyVars::new().with("玩家", "乙")),
            "三人均未抢地主，已重新发牌；随机由乙开始"
        );
        assert_eq!(
            text(
                &HAND_INITIAL,
                ReplyVars::new()
                    .with("玩法", "跑得快")
                    .with("张数", "16")
                    .with("手牌", "3 4 5")
            ),
            "跑得快初始手牌(16张): 3 4 5"
        );
        assert_eq!(
            text(
                &PLAYER_CARD_COUNT,
                ReplyVars::new().with("玩家", "甲").with("张数", "10")
            ),
            "甲 10张"
        );
        assert_eq!(
            text(
                &HAND_LANDLORD_INITIAL,
                ReplyVars::new().with("张数", "20").with("手牌", "3 4 5")
            ),
            "地主手牌(20张): 3 4 5"
        );
        assert_eq!(
            text(
                &LANDLORD_SELECTED,
                ReplyVars::new().with("玩家", "甲").with("底牌", "3 4 5")
            ),
            "甲成为地主并先出牌；底牌: 3 4 5"
        );
        assert_eq!(
            text(&LOBBY_TIMEOUT, ReplyVars::new().with("玩法", "跑得快")),
            "跑得快组局等待超时，房间已取消"
        );
        assert_eq!(
            text(&BIDDING_WARNING, ReplyVars::new().with("玩家", "甲")),
            "甲抢地主剩余30秒"
        );
        assert_eq!(
            text(
                &BIDDING_TIMEOUT,
                ReplyVars::new()
                    .with("玩家", "甲")
                    .with("后续", "丙成为地主并先出牌；底牌: 3 4 5")
            ),
            "甲抢地主超时，按不抢处理；丙成为地主并先出牌；底牌: 3 4 5"
        );
        assert_eq!(
            text(&TURN_WARNING, ReplyVars::new().with("玩家", "乙")),
            "乙出牌剩余30秒"
        );
        assert_eq!(
            text(
                &PASS_TIMEOUT_NO_BEATING,
                ReplyVars::new().with("玩家", "甲").with("下一位", "乙")
            ),
            "甲超时且无牌可压，自动过牌；轮到乙"
        );
        assert_eq!(
            text(
                &PASS_TIMEOUT,
                ReplyVars::new().with("玩家", "甲").with("下一位", "乙")
            ),
            "甲超时，自动过牌；轮到乙"
        );
        assert_eq!(
            text(
                &PASS_TIMEOUT_WITH_SUFFIX,
                ReplyVars::new()
                    .with("玩家", "甲")
                    .with("托管后缀", "并进入托管")
                    .with("下一位", "乙")
            ),
            "甲超时并进入托管，自动过牌；轮到乙"
        );
        assert_eq!(
            text(&NO_ACTIVE_GAME, ReplyVars::new()),
            "当前没有进行中的牌局"
        );
        assert_eq!(
            text(&NOT_IN_GAME, ReplyVars::new().with("玩法", "跑得快")),
            "你不在本局跑得快中"
        );
        assert_eq!(
            text(&WRONG_TURN, ReplyVars::new().with("玩家", "乙")),
            "现在轮到乙出牌"
        );
        assert_eq!(text(&PLAY_EMPTY, ReplyVars::new()), "请输入要出的牌");
        assert_eq!(
            text(&SPADE_THREE_REQUIRED, ReplyVars::new()),
            "首手必须包含黑桃3"
        );
        assert_eq!(
            text(&INVALID_PATTERN, ReplyVars::new()),
            "这组牌不是当前玩法的有效牌型"
        );
        assert_eq!(
            text(&MUST_PLAY_LARGEST_SINGLE, ReplyVars::new()),
            "下家已报单，出单张时必须出手中最大牌"
        );
        assert_eq!(
            text(
                &CANNOT_BEAT,
                ReplyVars::new()
                    .with("牌型", "单张")
                    .with("上一手牌型", "对子")
            ),
            "单张压不过上一手对子"
        );
        assert_eq!(
            text(&LEADER_CANNOT_PASS, ReplyVars::new()),
            "你是本轮领出者，不能过牌"
        );
        assert_eq!(
            text(&MUST_PLAY_RUN_FAST, ReplyVars::new()),
            "跑得快有牌能压时不能过牌"
        );
        assert_eq!(
            text(&HAND_NO_DEALT_GAME, ReplyVars::new()),
            "当前没有已发牌的牌局"
        );
        assert_eq!(
            text(
                &HAND_CURRENT,
                ReplyVars::new().with("张数", "17").with("手牌", "3 4 5")
            ),
            "当前手牌(17张): 3 4 5"
        );
        assert_eq!(
            text(&STATUS_IDLE, ReplyVars::new()),
            "当前没有牌局房间或进行中的牌局"
        );
        assert_eq!(
            text(
                &STATUS_LOBBY,
                ReplyVars::new()
                    .with("玩法", "斗地主")
                    .with("玩家列表", "甲、乙")
                    .with("人数", "1")
            ),
            "斗地主等待加入: 甲、乙，还需1人"
        );
        assert_eq!(
            text(
                &STATUS_BIDDING,
                ReplyVars::new()
                    .with("玩家", "甲")
                    .with("当前抢地主", "暂无")
            ),
            "斗地主抢地主中；轮到:甲；当前抢地主:暂无"
        );
        assert_eq!(
            text(
                &STATUS_PLAYING_RUN_FAST,
                ReplyVars::new()
                    .with("玩家", "甲")
                    .with("剩余", "甲 10张")
                    .with("上一手", "无")
            ),
            "跑得快；轮到:甲；剩余:甲 10张；上一手:无"
        );
        assert_eq!(
            text(
                &STATUS_PLAYING_LANDLORD,
                ReplyVars::new()
                    .with("地主", "甲")
                    .with("玩家", "乙")
                    .with("剩余", "甲 10张")
                    .with("上一手", "无")
            ),
            "地主:甲；轮到:乙；剩余:甲 10张；上一手:无"
        );
        assert_eq!(
            text(&NOT_IN_LOBBY, ReplyVars::new().with("玩法", "跑得快")),
            "你不在当前跑得快房间中"
        );
        assert_eq!(
            text(
                &LOBBY_CANCELED,
                ReplyVars::new().with("玩家", "甲").with("玩法", "斗地主")
            ),
            "甲取消了斗地主房间"
        );
        assert_eq!(
            text(
                &LOBBY_LEFT,
                ReplyVars::new().with("玩家", "乙").with("玩法", "斗地主")
            ),
            "乙退出了斗地主房间"
        );
        assert_eq!(
            text(&EXIT_LANDLORD_ENDED, ReplyVars::new().with("玩家", "甲")),
            "甲退出，斗地主牌局已结束"
        );
        assert_eq!(
            text(
                &EXIT_GAME_ENDED,
                ReplyVars::new().with("玩家", "甲").with("玩法", "跑得快")
            ),
            "甲退出，跑得快牌局已结束"
        );
        assert_eq!(
            text(
                &WON_RUN_FAST,
                ReplyVars::new()
                    .with("玩家", "甲")
                    .with("动作", "出")
                    .with("牌面", "44")
                    .with("牌型", "对子")
                    .with("剩余", "2张/2张")
            ),
            "甲出:44[对子]，获胜；其余:2张/2张"
        );
        assert_eq!(
            text(
                &WON_LANDLORD,
                ReplyVars::new()
                    .with("玩家", "甲")
                    .with("角色", "地主")
                    .with("牌面", "3 4 5")
                    .with("牌型", "顺子")
                    .with("阵营", "地主方")
                    .with("剩余", "甲 0张、乙 3张、丙 5张")
                    .with("炸弹", "0")
                    .with("手数", "12")
            ),
            "甲(地主)出完3 4 5[顺子]，地主方获胜。剩余:甲 0张、乙 3张、丙 5张；炸弹0次；共12手"
        );
        assert_eq!(
            text(
                &PLAY_RUN_FAST,
                ReplyVars::new()
                    .with("玩家", "甲")
                    .with("动作", "出")
                    .with("牌面", "44")
                    .with("牌型", "对子")
                    .with("剩余", "1")
                    .with("下一位", "乙")
            ),
            "甲出:44[对子] 余1，轮到乙"
        );
        assert_eq!(text(&LANDLORD_ACTION_PLAY, ReplyVars::new()), "出牌");
        assert_eq!(
            text(
                &LANDLORD_ACTION_TIMEOUT,
                ReplyVars::new().with("托管后缀", "并进入托管")
            ),
            "超时并进入托管，自动出"
        );
        assert_eq!(
            text(
                &LANDLORD_ACTION_TIMEOUT,
                ReplyVars::new().with("托管后缀", "")
            ),
            "超时，自动出"
        );
        assert_eq!(
            text(
                &PLAY_LANDLORD,
                ReplyVars::new()
                    .with("玩家", "甲")
                    .with("角色", "地主")
                    .with("动作", "出牌")
                    .with("牌面", "3 4 5")
                    .with("牌型", "顺子")
                    .with("剩余", "7")
                    .with("下一位", "乙")
            ),
            "甲(地主)出牌 3 4 5[顺子]，剩余7张；轮到乙"
        );
        assert_eq!(
            text(
                &PASS_MANUAL_NEXT,
                ReplyVars::new().with("玩家", "甲").with("下一位", "乙")
            ),
            "甲过，轮到乙"
        );
        assert_eq!(
            text(
                &PASS_MANUAL_LEAD,
                ReplyVars::new().with("玩家", "甲").with("下一位", "乙")
            ),
            "甲过，乙领出"
        );
        assert_eq!(
            text(
                &PASS_TIMEOUT_NEXT,
                ReplyVars::new().with("玩家", "甲").with("下一位", "乙")
            ),
            "甲超时过，轮到乙"
        );
        assert_eq!(
            text(
                &PASS_TIMEOUT_LEAD,
                ReplyVars::new().with("玩家", "甲").with("下一位", "乙")
            ),
            "甲超时过，乙领出"
        );
        assert_eq!(
            text(
                &PASS_FORCED_NEXT,
                ReplyVars::new().with("玩家", "甲").with("下一位", "乙")
            ),
            "甲无牌可压，轮到乙"
        );
        assert_eq!(
            text(&PASS_FORCED_LEAD, ReplyVars::new().with("下一位", "甲")),
            "双方无牌可压，甲领出"
        );
        assert_eq!(
            text(
                &PASS_NEW_TRICK,
                ReplyVars::new().with("玩家", "甲").with("下一位", "乙")
            ),
            "甲过牌，重新由乙领出"
        );
        assert_eq!(
            text(
                &PASS_NEXT,
                ReplyVars::new().with("玩家", "甲").with("下一位", "乙")
            ),
            "甲过牌，轮到乙"
        );
        assert_eq!(
            text(&PARSE_UNKNOWN_CARD, ReplyVars::new().with("牌面", "X")),
            "无法识别牌面: X"
        );
        assert_eq!(
            text(&PARSE_MISSING_CARD, ReplyVars::new().with("牌面", "3")),
            "手牌中缺少 3"
        );
    }
}
