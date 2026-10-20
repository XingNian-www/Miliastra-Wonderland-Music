//! 点歌类回复文案（点歌主链路）。
//!
//! 这里的 key 高频且面向全体大厅成员，改名会让用户已保存的覆盖失效。
//! 注意：`{标签}` 是命令回显前缀（如 "@点歌"），由调用方注入。

use super::ReplyTemplate;

/// 点歌回复。
pub(crate) const SONG: &[ReplyTemplate] = &[
    ReplyTemplate {
        key: "song.queue.duplicate",
        default: "队列已有: {歌名}",
        summary: "同一首歌已在队列中",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.playing.already",
        default: "当前正在播放: {歌名}",
        summary: "这首歌正在播放",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.ai.disabled",
        default: "{标签}AI点歌未启用",
        summary: "AI 点歌未启用",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.ai.disabled_with_hint",
        default: "{标签}AI点歌未启用，请先配置 ai.api_key",
        summary: "AI 点歌未启用（附配置提示）",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.ai.matching",
        default: "{标签}AI匹配中",
        summary: "开始 AI 匹配",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.ai.rewritten",
        default: "{标签}AI理解搜索词:{搜索词}，再次搜索",
        summary: "AI 改写搜索词后重搜",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.ai.second_search_failed",
        default: "{标签}二次搜索失败，保留首次搜索结果",
        summary: "二次搜索失败保留首次结果",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.no_source",
        default: "{标签}平台无对应歌曲音源",
        summary: "没有可用音源",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.ai.recognize_failed",
        default: "{标签}AI点歌识别失败",
        summary: "AI 识别失败",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.search_failed",
        default: "{标签}{原因}",
        summary: "搜索失败（原因由错误类型给出）",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.queue.added",
        default: "{前缀}({当前}/{上限}): {歌名}",
        summary: "入队成功及队列占用",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.queue.full",
        default: "队列已满，请稍后再试",
        summary: "队列已满",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.queue.full_unknown_state",
        default: "状态未知且队列已满，请稍后再试",
        summary: "状态未知且队列已满",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.dedup.recently_played",
        default: "{歌名}近期已播放过,请稍后再点",
        summary: "重复点播被去重拦截",
        group: "点歌",
    },
    ReplyTemplate {
        key: "song.review.rejected",
        default: "点歌未通过审核: {原因}",
        summary: "审核未通过",
        group: "点歌",
    },
];
