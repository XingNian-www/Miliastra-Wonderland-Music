//! 个人歌单（收藏 / 随机 / 导入 / 清空）回复文案。
//!
//! key 一旦发布不要改名：改名等同于删除用户已保存的覆盖。
//! 动态数据（歌名、数量）以变量注入，不作为模板的一部分。

use super::ReplyTemplate;

const fn spec(key: &'static str, default: &'static str, summary: &'static str) -> ReplyTemplate {
    ReplyTemplate {
        key,
        default,
        summary,
        group: "歌单",
    }
}

/// 无法识别发言者。
pub(crate) const ACTOR_MISSING: ReplyTemplate = spec(
    "playlist.actor.missing",
    "歌单需要先识别发言者,请稍后重试.",
    "无法识别发言者",
);

/// 打开窗口时收藏为空。
pub(crate) const EMPTY_NO_TRACK_HINT: ReplyTemplate = spec(
    "playlist.empty.hint",
    "你的歌单还是空的,先播一首再发@收藏.",
    "收藏为空（提示先收藏）",
);

/// 收藏为空（简短）。
pub(crate) const EMPTY_PLAIN: ReplyTemplate = spec(
    "playlist.empty.plain",
    "你的歌单还是空的.",
    "收藏为空",
);

/// 窗口推荐的首行。
pub(crate) const WINDOW_HEADER: ReplyTemplate = spec(
    "playlist.window.header",
    "为你推荐{数量}首",
    "歌单窗口-推荐数量",
);

/// 窗口里的一首候选。
pub(crate) const WINDOW_ITEM: ReplyTemplate = spec(
    "playlist.window.item",
    "{序号}. {歌名} - {歌手}",
    "歌单窗口-候选行",
);

/// 窗口底部操作提示。
pub(crate) const WINDOW_FOOTER: ReplyTemplate = spec(
    "playlist.window.footer",
    "20秒内回复@1-@{上限}加入下一曲；@换一批重抽；@关闭退出",
    "歌单窗口-操作说明",
);

/// 选择后排队成功。
pub(crate) const SELECT_QUEUED: ReplyTemplate = spec(
    "playlist.select.queued",
    "下一曲: {歌名} - {歌手}",
    "选曲已加入队列",
);

/// 播放队列已满。
pub(crate) const SELECT_QUEUE_FULL: ReplyTemplate = spec(
    "playlist.select.queue_full",
    "播放队列已满,歌曲未加入.",
    "队列已满，选曲未加入",
);

/// 序号越界时的提示。
pub(crate) const SELECT_OUT_OF_RANGE: ReplyTemplate = spec(
    "playlist.select.out_of_range",
    "请回复@1~@{上限}选择,@关闭退出.",
    "选择序号越界",
);

/// 没有更多可换的歌。
pub(crate) const REROLL_EXHAUSTED: ReplyTemplate = spec(
    "playlist.reroll.exhausted",
    "没有更多可以换的歌了,回复@关闭退出.",
    "重抽已无未展示曲目",
);

/// 关闭窗口。
pub(crate) const WINDOW_CLOSED: ReplyTemplate = spec(
    "playlist.window.closed",
    "已关闭歌单窗口.",
    "关闭歌单窗口",
);

/// 收藏：当前没有播放中的歌曲。
pub(crate) const FAVORITE_NO_TRACK: ReplyTemplate = spec(
    "playlist.favorite.no_track",
    "当前没有正在播放的歌曲,无法收藏.",
    "收藏失败-无播放",
);

/// 收藏成功。
pub(crate) const FAVORITE_ADDED: ReplyTemplate = spec(
    "playlist.favorite.added",
    "已收藏: {歌名} (共{数量}首)",
    "收藏成功",
);

/// 收藏已存在。
pub(crate) const FAVORITE_EXISTS: ReplyTemplate = spec(
    "playlist.favorite.exists",
    "这首歌已在你的歌单中: {歌名}",
    "收藏已存在",
);

/// 取消收藏：当前没有播放中的歌曲。
pub(crate) const UNFAVORITE_NO_TRACK: ReplyTemplate = spec(
    "playlist.unfavorite.no_track",
    "当前没有正在播放的歌曲,无法取消收藏.",
    "取消收藏失败-无播放",
);

/// 取消收藏成功。
pub(crate) const UNFAVORITE_REMOVED: ReplyTemplate = spec(
    "playlist.unfavorite.removed",
    "已取消收藏: {歌名} (剩余{数量}首)",
    "取消收藏成功",
);

/// 收藏夹里没有这首歌。
pub(crate) const UNFAVORITE_MISSING: ReplyTemplate = spec(
    "playlist.unfavorite.missing",
    "收藏夹中没有: {歌名}",
    "取消收藏-未找到",
);

/// 歌单模式：收藏为空。
pub(crate) const PLAY_EMPTY: ReplyTemplate = spec(
    "playlist.play.empty",
    "收藏夹为空,先收藏或导入一些歌曲.",
    "歌单模式-收藏为空",
);

/// 进入歌单模式。
pub(crate) const PLAY_STARTED: ReplyTemplate = spec(
    "playlist.play.started",
    "已进入歌单模式,{数量}首收藏已按随机顺序加入自动队列.",
    "进入歌单模式",
);

/// 已有他人的歌单模式在运行。
pub(crate) const PLAY_OCCUPIED: ReplyTemplate = spec(
    "playlist.play.occupied",
    "当前为{歌手}的歌单;如需切换请先发@大厅模式再发@歌单模式",
    "歌单模式被他人占用",
);

/// 已退出歌单模式。
pub(crate) const MODE_EXITED: ReplyTemplate = spec(
    "playlist.mode.exited",
    "已退出歌单模式,大厅点歌与默认播放池已恢复.",
    "退出歌单模式",
);

/// 本来就是大厅模式。
pub(crate) const MODE_ALREADY_HALL: ReplyTemplate = spec(
    "playlist.mode.already_hall",
    "当前已是大厅模式,大厅点歌与默认播放池正常工作.",
    "已在大厅模式",
);

/// 随机推荐。
pub(crate) const RANDOM_PICKED: ReplyTemplate = spec(
    "playlist.random.picked",
    "为你随机推荐: {歌名} (收藏夹共{数量}首)",
    "随机推荐一首",
);

/// 导入：已有待处理任务。
pub(crate) const IMPORT_PENDING: ReplyTemplate = spec(
    "playlist.import.pending",
    "你已有一个待处理的歌单导入,请先确认或取消.",
    "导入-已有待处理",
);

/// 导入：并发任务过多。
pub(crate) const IMPORT_BUSY: ReplyTemplate = spec(
    "playlist.import.busy",
    "当前歌单导入任务较多,请稍后再试.",
    "导入-任务过多",
);

/// 导入：开始读取。
pub(crate) const IMPORT_LOADING: ReplyTemplate = spec(
    "playlist.import.loading",
    "正在读取歌单,完成后会发送导入预览.",
    "导入-开始读取",
);

/// 导入：仍在读取。
pub(crate) const IMPORT_STILL_LOADING: ReplyTemplate = spec(
    "playlist.import.still_loading",
    "歌单仍在读取,请等候预览后再确认.",
    "导入-仍在读取",
);

/// 导入：已加入收藏。
pub(crate) const IMPORT_DONE: ReplyTemplate = spec(
    "playlist.import.done",
    "已导入 \"{歌名}\",新增{新增}首,重复{重复}首.",
    "导入-已完成",
);

/// 导入：超时。
pub(crate) const IMPORT_TIMEOUT: ReplyTemplate = spec(
    "playlist.import.timeout",
    "歌单导入已超时,请重新发起.",
    "导入-超时",
);

/// 导入：失败。
pub(crate) const IMPORT_FAILED: ReplyTemplate = spec(
    "playlist.import.failed",
    "读取歌单失败或平台暂不可用,请检查分享链接后重试.",
    "导入-失败",
);

/// 导入：超过上限。
pub(crate) const IMPORT_TOO_LARGE: ReplyTemplate = spec(
    "playlist.import.too_large",
    "歌单超过单次导入上限{上限}首,未导入内容.",
    "导入-超过单次上限",
);

/// 导入：已取消。
pub(crate) const IMPORT_CANCELLED: ReplyTemplate = spec(
    "playlist.import.cancelled",
    "已取消歌单导入,收藏未修改.",
    "导入-已取消",
);

/// 清空：本来就是空的。
pub(crate) const CLEAR_ALREADY_EMPTY: ReplyTemplate = spec(
    "playlist.clear.already_empty",
    "你的歌单本来就是空的.",
    "清空-已经为空",
);

/// 清空：二次确认提示。
pub(crate) const CLEAR_CONFIRM: ReplyTemplate = spec(
    "playlist.clear.confirm",
    "将清空你收藏的{数量}首歌曲;回复@确认清空执行,回复@放弃清空取消.",
    "清空-二次确认",
);

/// 清空完成（含退出歌单模式）。
pub(crate) const CLEAR_DONE_WITH_EXIT: ReplyTemplate = spec(
    "playlist.clear.done_with_exit",
    "已清空歌单,共移除{数量}首歌曲;当前歌单模式已退出.",
    "清空完成并退出歌单模式",
);

/// 清空完成。
pub(crate) const CLEAR_DONE: ReplyTemplate = spec(
    "playlist.clear.done",
    "已清空歌单,共移除{数量}首歌曲.",
    "清空完成",
);

/// 清空已取消。
pub(crate) const CLEAR_ABORTED: ReplyTemplate = spec(
    "playlist.clear.aborted",
    "已取消清空.",
    "清空-已取消",
);

/// 全部歌单回复模板。
pub(crate) const PLAYLIST: &[ReplyTemplate] = &[
    ACTOR_MISSING,
    EMPTY_NO_TRACK_HINT,
    EMPTY_PLAIN,
    WINDOW_HEADER,
    WINDOW_ITEM,
    WINDOW_FOOTER,
    SELECT_QUEUED,
    SELECT_QUEUE_FULL,
    SELECT_OUT_OF_RANGE,
    REROLL_EXHAUSTED,
    WINDOW_CLOSED,
    FAVORITE_NO_TRACK,
    FAVORITE_ADDED,
    FAVORITE_EXISTS,
    UNFAVORITE_NO_TRACK,
    UNFAVORITE_REMOVED,
    UNFAVORITE_MISSING,
    PLAY_EMPTY,
    PLAY_STARTED,
    PLAY_OCCUPIED,
    MODE_EXITED,
    MODE_ALREADY_HALL,
    RANDOM_PICKED,
    IMPORT_PENDING,
    IMPORT_BUSY,
    IMPORT_LOADING,
    IMPORT_STILL_LOADING,
    IMPORT_DONE,
    IMPORT_TIMEOUT,
    IMPORT_FAILED,
    IMPORT_TOO_LARGE,
    IMPORT_CANCELLED,
    CLEAR_ALREADY_EMPTY,
    CLEAR_CONFIRM,
    CLEAR_DONE_WITH_EXIT,
    CLEAR_DONE,
    CLEAR_ABORTED,
];
