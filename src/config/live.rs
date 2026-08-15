//! 运行时可热更新配置的共享句柄集合（阶段 7）。
//!
//! Web 配置中心保存/回滚成功后，HTTP 层调用 [`LiveConfigs::apply`] 用最新
//! 完整配置覆盖全部共享值；各消费方在运行态读取共享值（而非启动时一次性
//! clone 的 AppConfig），使 schema 中标 Live 的字段保存后真正作用于运行态，
//! 不虚报「已生效」。
//!
//! 与 schema 的对应关系由测试 [`live_fields_match_schema_effect`] 强制：
//! schema 中 effect==Live 的字段路径集合必须与本结构覆盖的路径集合完全一致，
//! 防止新增 Live 字段但未接入消费方（页面虚报已生效）。

use std::sync::{Arc, RwLock};

use crate::features::playback::SongDedupConfig;

use super::AppConfig;

/// LiveConfigs 覆盖的配置点路径集合（与 schema effect==Live 的字段集合
/// 严格一一对应，由测试 [`live_fields_match_schema_effect`] 强制；新增 Live
/// 字段必须同时登记本常量、本结构共享句柄与消费方读取点）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const LIVE_CONFIG_PATHS: &[&str] = &[
    "queue.protect_current_song_until_finished",
    "queue.external_playback_protect_after_seconds",
    "timing.playback.status_poll_ms",
    "timing.playback.monitor_status_ms",
    "timing.playback.monitor_tick_ms",
    "song_dedup.enabled",
    "song_dedup.window_seconds",
    "song_dedup.max_count",
    "friend_delivery.auto_retry_count",
];

/// 运行时可热更新配置的共享句柄集合（保存成功后由 HTTP 层 apply）。
#[derive(Clone, Default)]
pub struct LiveConfigs {
    /// queue.protect_current_song_until_finished
    pub queue_protect_current_song: Arc<RwLock<bool>>,
    /// queue.external_playback_protect_after_seconds
    pub queue_external_protect_seconds: Arc<RwLock<u64>>,
    /// timing.playback.status_poll_ms
    pub status_poll_ms: Arc<RwLock<u64>>,
    /// timing.playback.monitor_status_ms
    pub monitor_status_ms: Arc<RwLock<u64>>,
    /// timing.playback.monitor_tick_ms
    pub monitor_tick_ms: Arc<RwLock<u64>>,
    /// song_dedup.enabled / window_seconds / max_count（整段共享，
    /// history_path/console_bypass 不热更新）
    pub song_dedup: Arc<RwLock<SongDedupConfig>>,
    /// friend_delivery.auto_retry_count
    pub friend_delivery_auto_retry_count: Arc<RwLock<u32>>,
}

impl LiveConfigs {
    /// 从完整配置初始化全部值（启动时创建一次）。
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            queue_protect_current_song: Arc::new(RwLock::new(
                config.queue.protect_current_song_until_finished,
            )),
            queue_external_protect_seconds: Arc::new(RwLock::new(
                config.queue.external_playback_protect_after_seconds,
            )),
            status_poll_ms: Arc::new(RwLock::new(config.timing.playback.status_poll_ms)),
            monitor_status_ms: Arc::new(RwLock::new(config.timing.playback.monitor_status_ms)),
            monitor_tick_ms: Arc::new(RwLock::new(config.timing.playback.monitor_tick_ms)),
            song_dedup: Arc::new(RwLock::new(config.song_dedup.clone())),
            friend_delivery_auto_retry_count: Arc::new(RwLock::new(
                config.friend_delivery.auto_retry_count,
            )),
        }
    }

    /// 用新配置覆盖全部值（保存成功后调用）；共享句柄保持不变，
    /// 运行态读取点无需重建即可看到新值。
    pub fn apply(&self, config: &AppConfig) {
        // 锁中毒时恢复继续使用,避免把可恢复的配置热更新升级为线程崩溃。
        use std::sync::PoisonError;
        *self
            .queue_protect_current_song
            .write()
            .unwrap_or_else(PoisonError::into_inner) =
            config.queue.protect_current_song_until_finished;
        *self
            .queue_external_protect_seconds
            .write()
            .unwrap_or_else(PoisonError::into_inner) =
            config.queue.external_playback_protect_after_seconds;
        *self
            .status_poll_ms
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.timing.playback.status_poll_ms;
        *self
            .monitor_status_ms
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.timing.playback.monitor_status_ms;
        *self
            .monitor_tick_ms
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.timing.playback.monitor_tick_ms;
        *self
            .song_dedup
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.song_dedup.clone();
        *self
            .friend_delivery_auto_retry_count
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.friend_delivery.auto_retry_count;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::config::{Effect, config_sections};

    /// 构造一份与默认值不同的完整配置，用于取值/覆盖断言。
    fn changed_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.queue.protect_current_song_until_finished = false;
        config.queue.external_playback_protect_after_seconds = 99;
        config.timing.playback.status_poll_ms = 1111;
        config.timing.playback.monitor_status_ms = 2222;
        config.timing.playback.monitor_tick_ms = 333;
        config.song_dedup.enabled = false;
        config.song_dedup.window_seconds = 123;
        config.song_dedup.max_count = 7;
        config.friend_delivery.auto_retry_count = 5;
        config
    }

    #[test]
    fn from_config_extracts_values() {
        let live = LiveConfigs::from_config(&changed_config());
        assert!(!*live.queue_protect_current_song.read().unwrap());
        assert_eq!(*live.queue_external_protect_seconds.read().unwrap(), 99);
        assert_eq!(*live.status_poll_ms.read().unwrap(), 1111);
        assert_eq!(*live.monitor_status_ms.read().unwrap(), 2222);
        assert_eq!(*live.monitor_tick_ms.read().unwrap(), 333);
        let dedup = live.song_dedup.read().unwrap();
        assert!(!dedup.enabled);
        assert_eq!(dedup.window_seconds, 123);
        assert_eq!(dedup.max_count, 7);
        drop(dedup);
        assert_eq!(*live.friend_delivery_auto_retry_count.read().unwrap(), 5);
    }

    #[test]
    fn apply_overwrites_all_values() {
        let live = LiveConfigs::from_config(&AppConfig::default());
        live.apply(&changed_config());
        assert!(!*live.queue_protect_current_song.read().unwrap());
        assert_eq!(*live.queue_external_protect_seconds.read().unwrap(), 99);
        assert_eq!(*live.status_poll_ms.read().unwrap(), 1111);
        assert_eq!(*live.monitor_status_ms.read().unwrap(), 2222);
        assert_eq!(*live.monitor_tick_ms.read().unwrap(), 333);
        let dedup = live.song_dedup.read().unwrap();
        assert!(!dedup.enabled);
        assert_eq!(dedup.window_seconds, 123);
        assert_eq!(dedup.max_count, 7);
        drop(dedup);
        assert_eq!(*live.friend_delivery_auto_retry_count.read().unwrap(), 5);
        // 再次覆盖回默认值：共享句柄不变，值必须跟着变。
        live.apply(&AppConfig::default());
        assert!(*live.queue_protect_current_song.read().unwrap());
        assert_eq!(live.song_dedup.read().unwrap().max_count, 1);
    }

    /// schema 中 effect==Live 的字段路径集合必须与 LiveConfigs 覆盖的路径
    /// 集合完全一致（防虚报）：新增 Live 字段必须同时接入共享句柄，
    /// 已接入的字段不得被改回 Restart。
    #[test]
    fn live_fields_match_schema_effect() {
        let schema_live: BTreeSet<String> = config_sections()
            .into_iter()
            .flat_map(|section| section.fields)
            .filter(|field| field.effect == Effect::Live)
            .map(|field| field.path)
            .collect();
        let covered: BTreeSet<String> = LIVE_CONFIG_PATHS
            .iter()
            .map(|path| path.to_string())
            .collect();
        assert_eq!(
            schema_live, covered,
            "schema 中 effect==Live 的字段路径集合必须与 LiveConfigs 覆盖的路径集合完全一致"
        );
    }
}
