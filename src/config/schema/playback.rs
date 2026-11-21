//! 播放与点歌段 schema：播放器、点歌队列、同歌去重、歌名匹配。
//!
//! 这一组回答的是"歌从哪来、怎么排队、怎么认定是同一首、歌名怎么对上"。
//! 歌曲审核与点歌 AI 属于 AI 域，见 [`super::ai`]。
//! 字段构造器（`db_live` 等）与段内小节推导都在父模块，这里只声明字段。

use super::{ConfigFieldSchema, FieldKind, MAX_STABLE_COUNT, MAX_TIMEOUT_MS, float, int};

/// playback 段：播放器凭据、程序路径与启动时加载的数值配置。
pub(super) fn playback_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_playback_idle_reload(
            "credential_directory",
            "凭据目录",
            FieldKind::Path,
            "账号凭据目录，敏感；三个平台凭据的版本化存储目录（相对 EXE 根目录）",
        ),
        ConfigFieldSchema::db_idle_reload(
            "login_helper_executable",
            "登录助手程序",
            FieldKind::Path,
            "受控登录使用的短生命周期助手",
        ),
        ConfigFieldSchema::db_live(
            "login_timeout_ms",
            "登录超时",
            int(1, MAX_TIMEOUT_MS),
            "单次交互登录的最长时间，单位毫秒",
        ),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache",
            "音频缓存",
            FieldKind::Object,
            "音频数据缓存整体配置；为 null 时不启用（默认）。启用后编辑下方子字段",
        )
        .nullable(),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache.enabled",
            "启用音频缓存",
            FieldKind::Bool,
            "音频数据缓存（本地代理 + 磁盘缓存）；播放时音源先落本地缓存，重复播放与断网时直接从磁盘服务",
        )
        .optional_parent(),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache.directory",
            "缓存目录",
            FieldKind::Path,
            "音频文件目录；相对路径基于程序 exe 所在目录，留空默认 deps/cache/audio",
        )
        .optional_parent(),
        ConfigFieldSchema::db_live(
            "audio_cache.max_bytes_mb",
            "磁盘占用上限",
            int(1, 1_048_576),
            "磁盘占用上限，单位 MiB",
        )
        .optional_parent(),
        ConfigFieldSchema::db_idle_reload(
            "audio_cache.max_concurrent_downloads",
            "并发下载上限",
            int(1, 1024),
            "同时进行的源站下载任务上限",
        )
        .optional_parent(),
        ConfigFieldSchema::db_live(
            "audio_cache.request_timeout_ms",
            "源站请求超时",
            int(1, MAX_TIMEOUT_MS),
            "源站连接/响应超时，单位毫秒",
        )
        .optional_parent(),
        ConfigFieldSchema::db_live(
            "audio_cache.seek_wait_timeout_ms",
            "跳转等待超时",
            int(1, MAX_TIMEOUT_MS),
            "请求尚未下载完成的位置时，等待下载推进的最长时间，单位毫秒",
        )
        .optional_parent(),
    ]
}

/// queue 段：点歌队列。
pub(super) fn queue_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_idle_reload(
            "max_size",
            "队列最大长度",
            int(1, 100_000),
            "队列最大长度",
        ),
        ConfigFieldSchema::db_idle_reload(
            "pool_max_size",
            "播放池容量",
            int(0, 1_000_000),
            "播放池最大容量；已确认播放过的点歌会进入播放池，队列播完后从播放池随机播放；0 表示禁用播放池",
        ),
        ConfigFieldSchema::db_live(
            "protect_current_song_until_finished",
            "保护当前歌曲",
            FieldKind::Bool,
            "机器人确认播放的歌曲是否保护；true 时新点歌会排队，未结束只能用 @下一首 提前切换；歌曲播完由自然结束自动出队；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "external_playback_protect_after_seconds",
            "外部播放保护时间",
            int(0, 86_400),
            "非点歌歌曲连续正常播放多少秒后加入当前歌曲保护；0 表示外部播放永不保护，单位秒；保存后立即生效",
        ),
    ]
}

/// song_dedup 段：长时间同歌去重。
pub(super) fn song_dedup_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "enabled",
            "启用去重",
            FieldKind::Bool,
            "长时间同歌去重；播放/入队前检查，确认播放成功后记录；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "window_seconds",
            "统计窗口",
            int(1, 86_400),
            "最近多少秒内统计同一首歌播放次数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "max_count",
            "最多播放次数",
            int(1, MAX_STABLE_COUNT),
            "最近窗口内同一首歌最多允许播放次数；达到该值后拒绝或跳过；保存后立即生效",
        ),
        ConfigFieldSchema::db_idle_reload(
            "console_bypass",
            "控制台豁免",
            FieldKind::Bool,
            "控制台来源是否默认豁免长时间同歌去重",
        ),
        ConfigFieldSchema::db_idle_reload(
            "history_path",
            "历史记录路径",
            FieldKind::Path,
            "长时间同歌去重历史持久化路径（相对 EXE 根目录）",
        ),
    ]
}

/// matching 段：歌名/歌手匹配。
pub(super) fn matching_section() -> Vec<ConfigFieldSchema> {
    vec![
        ConfigFieldSchema::db_live(
            "min_song_name_score",
            "歌名最低分数",
            float(0.0, 1.0),
            "歌名最低匹配分数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "short_chinese_song_max_miss",
            "短中文歌名漏字上限",
            int(0, 100),
            "4 字以内中文歌名最多允许漏字数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "long_chinese_song_min_score",
            "长中文歌名最低比例",
            float(0.0, 1.0),
            "长中文歌名最低命中比例；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "max_ocr_noise_chars",
            "OCR 噪声字符上限",
            int(0, 100),
            "完整歌名后最多忽略的 OCR 噪声字符数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "enable_fuzzy_singer",
            "歌手模糊匹配",
            FieldKind::Bool,
            "是否启用中文歌手模糊匹配；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "short_chinese_singer_max_miss",
            "短中文歌手漏字上限",
            int(0, 100),
            "4 字以内中文歌手最多允许漏字数；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "long_chinese_singer_min_score",
            "长中文歌手最低比例",
            float(0.0, 1.0),
            "长中文歌手最低命中比例；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "en_max_edit_fraction",
            "英文歌名编辑距离上限",
            float(0.0, 1.0),
            "英文歌名编辑距离占比上限；保存后立即生效",
        ),
        ConfigFieldSchema::db_live(
            "en_singer_max_edit_fraction",
            "英文歌手编辑距离上限",
            float(0.0, 1.0),
            "英文歌手编辑距离占比上限；保存后立即生效",
        ),
    ]
}
