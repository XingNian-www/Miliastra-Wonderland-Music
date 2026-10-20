//! 播放控制类回复文案。
//!
//! 本文件由 playback 模块负责；新增模板请在 `PLAYBACK` 数组追加，
//! 不要改动其它模块的文件（并行开发按文件隔离，避免写冲突）。

use super::ReplyTemplate;

/// 一条播放回复模板。
///
/// 用命名常量而不是数组下标引用，是为了让调用点写 `playback::PAUSE_OK`：
/// 下标会随插入顺序漂移，改一条就会静默串到别的文案上。
/// 每个常量都必须同时出现在下面的 `PLAYBACK` 数组里（有测试守着），
/// 否则面板看不到、配置也无法覆盖。
pub(crate) const PAUSE_OK: ReplyTemplate = ReplyTemplate {
    key: "playback.pause.ok",
    default: "已暂停",
    summary: "暂停成功",
    group: "播放",
};

pub(crate) const RESUME_OK: ReplyTemplate = ReplyTemplate {
    key: "playback.resume.ok",
    default: "已恢复播放",
    summary: "恢复播放成功",
    group: "播放",
};

pub(crate) const NEXT_POOL_FALLBACK: ReplyTemplate = ReplyTemplate {
    key: "playback.next.pool_fallback",
    default: "播放列表为空，随机续播: {关键词}",
    summary: "队列空时下一首从播放池续播",
    group: "播放",
};

pub(crate) const NEXT_EMPTY: ReplyTemplate = ReplyTemplate {
    key: "playback.next.empty",
    default: "队列和播放池都为空，无可播放歌曲",
    summary: "队列与播放池都没有可播歌曲",
    group: "播放",
};

pub(crate) const DELETE_TRACK_UNAVAILABLE: ReplyTemplate = ReplyTemplate {
    key: "playback.delete.track_unavailable",
    default: "当前歌曲信息不可用，未删除播放池歌曲",
    summary: "读不到当前歌曲，未执行播放池删除",
    group: "播放",
};

pub(crate) const DELETE_REMOVED_POOL_FALLBACK: ReplyTemplate = ReplyTemplate {
    key: "playback.delete.removed.pool_fallback",
    default: "已从播放池删除当前歌曲，随机续播: {关键词}",
    summary: "删除成功并从播放池续播",
    group: "播放",
};

pub(crate) const DELETE_NOT_IN_POOL_POOL_FALLBACK: ReplyTemplate = ReplyTemplate {
    key: "playback.delete.not_in_pool.pool_fallback",
    default: "当前歌曲不在播放池中，随机续播: {关键词}",
    summary: "当前歌曲不在池中但仍从池续播",
    group: "播放",
};

pub(crate) const DELETE_REMOVED_QUEUE_EMPTY: ReplyTemplate = ReplyTemplate {
    key: "playback.delete.removed.queue_empty",
    default: "已从播放池删除当前歌曲；队列和播放池都为空",
    summary: "删除成功但无歌可续",
    group: "播放",
};

pub(crate) const DELETE_NOT_IN_POOL_QUEUE_EMPTY: ReplyTemplate = ReplyTemplate {
    key: "playback.delete.not_in_pool.queue_empty",
    default: "当前歌曲不在播放池中；队列和播放池都为空",
    summary: "当前歌曲不在池中且无歌可续",
    group: "播放",
};

pub(crate) const PREVIOUS_MISSING: ReplyTemplate = ReplyTemplate {
    key: "playback.previous.missing",
    default: "没有上一曲记录",
    summary: "没有可回退的上一曲",
    group: "播放",
};

pub(crate) const VOLUME_SET: ReplyTemplate = ReplyTemplate {
    key: "playback.volume.set",
    default: "音量已设置为 {音量}",
    summary: "音量设置成功",
    group: "播放",
};

pub(crate) const LYRICS_TOGGLE_TRANSLATION: ReplyTemplate = ReplyTemplate {
    key: "playback.lyrics.toggle.translation",
    default: "歌词已切换为翻译",
    summary: "歌词切到翻译",
    group: "播放",
};

pub(crate) const LYRICS_TOGGLE_ORIGINAL: ReplyTemplate = ReplyTemplate {
    key: "playback.lyrics.toggle.original",
    default: "歌词已切换为原文",
    summary: "歌词切到原文",
    group: "播放",
};

pub(crate) const LYRICS_TIMED_STARTED: ReplyTemplate = ReplyTemplate {
    key: "playback.lyrics.timed_started",
    default: "限时歌词已开启，将持续{秒数}秒；默认显示翻译，发送@切换可切换原文/翻译",
    summary: "限时歌词开启",
    group: "播放",
};

pub(crate) const LYRICS_ALREADY_RUNNING: ReplyTemplate = ReplyTemplate {
    key: "playback.lyrics.already_running",
    default: "后台歌词已经在运行",
    summary: "后台歌词已在运行",
    group: "播放",
};

pub(crate) const LYRICS_BACKGROUND_STARTED: ReplyTemplate = ReplyTemplate {
    key: "playback.lyrics.background.started",
    default: "后台歌词已开启；默认显示翻译，发送@切换 可切换原文/翻译；",
    summary: "后台歌词开启",
    group: "播放",
};

pub(crate) const LYRICS_SINGLE_SONG_STARTED: ReplyTemplate = ReplyTemplate {
    key: "playback.lyrics.single_song.started",
    default: "单曲后台歌词已开启；默认显示翻译，发送@切换可切换原文/翻译；切歌后自动停止",
    summary: "单曲后台歌词开启",
    group: "播放",
};

pub(crate) const LYRICS_BACKGROUND_STOPPED: ReplyTemplate = ReplyTemplate {
    key: "playback.lyrics.background.stopped",
    default: "后台歌词已停止",
    summary: "后台歌词已停止",
    group: "播放",
};

pub(crate) const LYRICS_BACKGROUND_NOT_RUNNING: ReplyTemplate = ReplyTemplate {
    key: "playback.lyrics.background.not_running",
    default: "后台歌词当前未运行",
    summary: "后台歌词本就没在运行",
    group: "播放",
};

pub(crate) const QUEUE_DELETE_INVALID: ReplyTemplate = ReplyTemplate {
    key: "playback.queue.delete.invalid",
    default: "没有匹配到有效队列序号",
    summary: "删除队列项时没有有效序号",
    group: "播放",
};

pub(crate) const QUEUE_DELETE_MISSING: ReplyTemplate = ReplyTemplate {
    key: "playback.queue.delete.missing",
    default: "队列删除失败或序号不存在",
    summary: "要删的队列项不存在",
    group: "播放",
};

pub(crate) const QUEUE_DELETE_DONE: ReplyTemplate = ReplyTemplate {
    key: "playback.queue.delete.done",
    default: "队列已删除: {条目}",
    summary: "队列删除成功及被删条目",
    group: "播放",
};

pub(crate) const QUEUE_EMPTY: ReplyTemplate = ReplyTemplate {
    key: "playback.queue.empty",
    default: "队列为空",
    summary: "队列为空（列表与清空共用）",
    group: "播放",
};

pub(crate) const QUEUE_CLEARED: ReplyTemplate = ReplyTemplate {
    key: "playback.queue.cleared",
    default: "队列已清空: {数量} 首",
    summary: "队列清空成功及数量",
    group: "播放",
};

pub(crate) const QUEUE_HEADER: ReplyTemplate = ReplyTemplate {
    key: "playback.queue.header",
    default: "完整队列({当前}/{上限}): ",
    summary: "完整队列表头",
    group: "播放",
};

pub(crate) const QUEUE_ENTRY: ReplyTemplate = ReplyTemplate {
    key: "playback.queue.entry",
    default: "{序号}.{歌名}",
    summary: "完整队列单条条目",
    group: "播放",
};

pub(crate) const DEDUP_RECENTLY_PLAYED: ReplyTemplate = ReplyTemplate {
    key: "playback.dedup.recently_played",
    default: "{歌名}近期已播放过,请稍后再点",
    summary: "重复点播被去重拦截（点播路径）",
    group: "播放",
};

pub(crate) const DEDUP_SKIPPED: ReplyTemplate = ReplyTemplate {
    key: "playback.dedup.skipped",
    default: "{歌名}近期已播放过,已跳过",
    summary: "重复曲目被去重跳过（队列路径）",
    group: "播放",
};

pub(crate) const NO_SOURCE: ReplyTemplate = ReplyTemplate {
    key: "playback.no_source",
    default: "平台无对应歌曲音源",
    summary: "搜索无候选音源",
    group: "播放",
};

pub(crate) const SEARCH_FAILED: ReplyTemplate = ReplyTemplate {
    key: "playback.search.failed",
    default: "{前缀}{原因}",
    summary: "搜索失败（好友前缀 + 原因）",
    group: "播放",
};

pub(crate) const ITEM_FAILURE: ReplyTemplate = ReplyTemplate {
    key: "playback.item_failure",
    default: "{来源}: {原因}",
    summary: "单个曲目播放失败（来源 + 原因）",
    group: "播放",
};

pub(crate) const FAILURE_REASON: ReplyTemplate = ReplyTemplate {
    key: "playback.failure.reason",
    default: "{原因}",
    summary: "播放失败原因（可加前缀）",
    group: "播放",
};

pub(crate) const SOURCE_SWITCHED: ReplyTemplate = ReplyTemplate {
    key: "playback.source_switched",
    default: "已自动换源:{歌名}",
    summary: "自动换源成功",
    group: "播放",
};

pub(crate) const QUEUE_SKIPPED: ReplyTemplate = ReplyTemplate {
    key: "playback.queue.skipped",
    default: "{原因}，已跳过当前歌曲",
    summary: "队列项失败后跳过当前歌曲",
    group: "播放",
};

/// 播放控制回复。键一旦发布不要改名，否则用户已保存的覆盖会失效。
///
/// 这里必须收齐上面每一个常量；漏登记等于该条文案在面板里消失、也无法被覆盖。
pub(crate) const PLAYBACK: &[ReplyTemplate] = &[
    PAUSE_OK,
    RESUME_OK,
    NEXT_POOL_FALLBACK,
    NEXT_EMPTY,
    DELETE_TRACK_UNAVAILABLE,
    DELETE_REMOVED_POOL_FALLBACK,
    DELETE_NOT_IN_POOL_POOL_FALLBACK,
    DELETE_REMOVED_QUEUE_EMPTY,
    DELETE_NOT_IN_POOL_QUEUE_EMPTY,
    PREVIOUS_MISSING,
    VOLUME_SET,
    LYRICS_TOGGLE_TRANSLATION,
    LYRICS_TOGGLE_ORIGINAL,
    LYRICS_TIMED_STARTED,
    LYRICS_ALREADY_RUNNING,
    LYRICS_BACKGROUND_STARTED,
    LYRICS_SINGLE_SONG_STARTED,
    LYRICS_BACKGROUND_STOPPED,
    LYRICS_BACKGROUND_NOT_RUNNING,
    QUEUE_DELETE_INVALID,
    QUEUE_DELETE_MISSING,
    QUEUE_DELETE_DONE,
    QUEUE_EMPTY,
    QUEUE_CLEARED,
    QUEUE_HEADER,
    QUEUE_ENTRY,
    DEDUP_RECENTLY_PLAYED,
    DEDUP_SKIPPED,
    NO_SOURCE,
    SEARCH_FAILED,
    ITEM_FAILURE,
    FAILURE_REASON,
    SOURCE_SWITCHED,
    QUEUE_SKIPPED,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::reply_template::ReplyVars;
    use crate::features::reply_template::catalog;

    /// 本模块负责的全部模板常量。新增模板时先加常量、再加进 PLAYBACK、最后补到这里。
    const EVERY_PLAYBACK_SPEC: [ReplyTemplate; 34] = [
        PAUSE_OK,
        RESUME_OK,
        NEXT_POOL_FALLBACK,
        NEXT_EMPTY,
        DELETE_TRACK_UNAVAILABLE,
        DELETE_REMOVED_POOL_FALLBACK,
        DELETE_NOT_IN_POOL_POOL_FALLBACK,
        DELETE_REMOVED_QUEUE_EMPTY,
        DELETE_NOT_IN_POOL_QUEUE_EMPTY,
        PREVIOUS_MISSING,
        VOLUME_SET,
        LYRICS_TOGGLE_TRANSLATION,
        LYRICS_TOGGLE_ORIGINAL,
        LYRICS_TIMED_STARTED,
        LYRICS_ALREADY_RUNNING,
        LYRICS_BACKGROUND_STARTED,
        LYRICS_SINGLE_SONG_STARTED,
        LYRICS_BACKGROUND_STOPPED,
        LYRICS_BACKGROUND_NOT_RUNNING,
        QUEUE_DELETE_INVALID,
        QUEUE_DELETE_MISSING,
        QUEUE_DELETE_DONE,
        QUEUE_EMPTY,
        QUEUE_CLEARED,
        QUEUE_HEADER,
        QUEUE_ENTRY,
        DEDUP_RECENTLY_PLAYED,
        DEDUP_SKIPPED,
        NO_SOURCE,
        SEARCH_FAILED,
        ITEM_FAILURE,
        FAILURE_REASON,
        SOURCE_SWITCHED,
        QUEUE_SKIPPED,
    ];

    #[test]
    fn every_playback_key_is_reachable_from_the_catalog_with_a_non_empty_default() {
        let catalog_keys = catalog()
            .map(|spec| spec.key)
            .collect::<std::collections::BTreeSet<_>>();
        for spec in EVERY_PLAYBACK_SPEC {
            assert!(
                catalog_keys.contains(spec.key),
                "{} 未登记进 catalog()，面板看不到也无法覆盖",
                spec.key
            );
            assert!(!spec.default.trim().is_empty(), "{} 默认文案为空", spec.key);
            assert!(!spec.summary.trim().is_empty(), "{} 缺少摘要", spec.key);
            assert_eq!(spec.group, "播放", "{} 分组应为播放", spec.key);
        }
    }

    #[test]
    fn every_playback_constant_is_listed_in_the_published_array() {
        // 定义常量却忘了放进 PLAYBACK，是并行开发里最容易漏的一步。
        let published = PLAYBACK
            .iter()
            .map(|spec| spec.key)
            .collect::<std::collections::BTreeSet<_>>();
        for spec in EVERY_PLAYBACK_SPEC {
            assert!(
                published.contains(spec.key),
                "{} 未加入 PLAYBACK 数组",
                spec.key
            );
        }
        assert_eq!(published.len(), PLAYBACK.len(), "PLAYBACK 里有重复键");
        assert_eq!(
            published.len(),
            EVERY_PLAYBACK_SPEC.len(),
            "PLAYBACK 多出未在测试清单登记的条目"
        );
    }

    /// 迁移前的原始文案（逐字抄自改造前的 `port.reply` 调用点）。
    ///
    /// 这份表故意不复用上面的常量：如果谁顺手改了某个 default（哪怕只是标点），
    /// 这里会立刻红。改文案属于改行为，必须显式改这份表并说明理由。
    const PRE_MIGRATION_TEXTS: &[(&str, &str)] = &[
        ("playback.pause.ok", "已暂停"),
        ("playback.resume.ok", "已恢复播放"),
        (
            "playback.next.pool_fallback",
            "播放列表为空，随机续播: 晴天",
        ),
        ("playback.next.empty", "队列和播放池都为空，无可播放歌曲"),
        (
            "playback.delete.track_unavailable",
            "当前歌曲信息不可用，未删除播放池歌曲",
        ),
        (
            "playback.delete.removed.pool_fallback",
            "已从播放池删除当前歌曲，随机续播: 晴天",
        ),
        (
            "playback.delete.not_in_pool.pool_fallback",
            "当前歌曲不在播放池中，随机续播: 晴天",
        ),
        (
            "playback.delete.removed.queue_empty",
            "已从播放池删除当前歌曲；队列和播放池都为空",
        ),
        (
            "playback.delete.not_in_pool.queue_empty",
            "当前歌曲不在播放池中；队列和播放池都为空",
        ),
        ("playback.previous.missing", "没有上一曲记录"),
        ("playback.volume.set", "音量已设置为 80"),
        ("playback.lyrics.toggle.translation", "歌词已切换为翻译"),
        ("playback.lyrics.toggle.original", "歌词已切换为原文"),
        (
            "playback.lyrics.timed_started",
            "限时歌词已开启，将持续5秒；默认显示翻译，发送@切换可切换原文/翻译",
        ),
        ("playback.lyrics.already_running", "后台歌词已经在运行"),
        (
            "playback.lyrics.background.started",
            "后台歌词已开启；默认显示翻译，发送@切换 可切换原文/翻译；",
        ),
        (
            "playback.lyrics.single_song.started",
            "单曲后台歌词已开启；默认显示翻译，发送@切换可切换原文/翻译；切歌后自动停止",
        ),
        ("playback.lyrics.background.stopped", "后台歌词已停止"),
        (
            "playback.lyrics.background.not_running",
            "后台歌词当前未运行",
        ),
        ("playback.queue.delete.invalid", "没有匹配到有效队列序号"),
        ("playback.queue.delete.missing", "队列删除失败或序号不存在"),
        ("playback.queue.delete.done", "队列已删除: 1.晴天"),
        ("playback.queue.empty", "队列为空"),
        ("playback.queue.cleared", "队列已清空: 3 首"),
        ("playback.queue.header", "完整队列(2/20): "),
        ("playback.queue.entry", "1.晴天"),
        (
            "playback.dedup.recently_played",
            "晴天近期已播放过,请稍后再点",
        ),
        ("playback.dedup.skipped", "晴天近期已播放过,已跳过"),
        ("playback.no_source", "平台无对应歌曲音源"),
        ("playback.search.failed", "好友甲:歌曲搜索繁忙，请稍后再试"),
        ("playback.item_failure", "kugou: 登录凭据已失效，请重新登录"),
        ("playback.failure.reason", "平台无对应歌曲音源"),
        ("playback.source_switched", "已自动换源:Song-Artist"),
        (
            "playback.queue.skipped",
            "平台无对应歌曲音源，已跳过当前歌曲",
        ),
    ];

    /// 每条模板配一份代表性变量，用于复现迁移前的成品文案。
    fn sample_vars(key: &str) -> ReplyVars {
        match key {
            "playback.next.pool_fallback"
            | "playback.delete.removed.pool_fallback"
            | "playback.delete.not_in_pool.pool_fallback" => {
                ReplyVars::new().with("关键词", "晴天")
            }
            "playback.volume.set" => ReplyVars::new().with("音量", "80"),
            "playback.lyrics.timed_started" => ReplyVars::new().with("秒数", "5"),
            "playback.queue.delete.done" => ReplyVars::new().with("条目", "1.晴天"),
            "playback.queue.cleared" => ReplyVars::new().with("数量", "3"),
            "playback.queue.header" => ReplyVars::new().with("当前", "2").with("上限", "20"),
            "playback.queue.entry" => ReplyVars::new().with("序号", "1").with("歌名", "晴天"),
            "playback.dedup.recently_played" | "playback.dedup.skipped" => {
                ReplyVars::new().with("歌名", "晴天")
            }
            "playback.search.failed" => ReplyVars::new()
                .with("前缀", "好友甲:")
                .with("原因", "歌曲搜索繁忙，请稍后再试"),
            "playback.item_failure" => ReplyVars::new()
                .with("来源", "kugou")
                .with("原因", "登录凭据已失效，请重新登录"),
            "playback.failure.reason" => ReplyVars::new().with("原因", "平台无对应歌曲音源"),
            "playback.source_switched" => ReplyVars::new().with("歌名", "Song-Artist"),
            "playback.queue.skipped" => ReplyVars::new().with("原因", "平台无对应歌曲音源"),
            _ => ReplyVars::new(),
        }
    }

    #[test]
    fn rendering_each_playback_template_reproduces_the_pre_migration_text() {
        use crate::features::reply_template::render;
        assert_eq!(
            PRE_MIGRATION_TEXTS.len(),
            PLAYBACK.len(),
            "金标表必须覆盖全部模板"
        );
        for spec in PLAYBACK {
            let expected = PRE_MIGRATION_TEXTS
                .iter()
                .find(|(key, _)| *key == spec.key)
                .map(|(_, text)| *text)
                .unwrap_or_else(|| panic!("{} 缺少迁移前文案金标", spec.key));
            let rendered = render(spec, None, &sample_vars(spec.key));
            assert_eq!(
                rendered.text, expected,
                "{} 默认文案与迁移前不一致",
                spec.key
            );
            assert!(
                rendered.missing.is_empty(),
                "{} 渲染时缺少变量 {:?}",
                spec.key,
                rendered.missing
            );
        }
    }

    #[test]
    fn playback_keys_stay_in_the_playback_namespace() {
        for spec in EVERY_PLAYBACK_SPEC {
            assert!(
                spec.key.starts_with("playback."),
                "{} 不在 playback. 命名空间下",
                spec.key
            );
        }
    }
}
