//! 运行时可热更新配置的共享句柄集合（阶段 7）。
//!
//! Web 配置中心保存/回滚成功后，HTTP 层调用 [`LiveConfigs::apply`]，只把
//! schema 中标为 Live 的字段合并进有效运行态快照与专用共享句柄。其他字段
//! 虽然已经落库，但在子进程重载或手动重启前仍保留启动值，避免新旧组件混用配置。
//!
//! 与 schema 的对应关系由测试 [`live_fields_match_schema_effect`] 强制：
//! schema 中 effect==Live 的字段路径集合必须与本结构覆盖的路径集合完全一致，
//! 防止新增 Live 字段但未接入消费方（页面虚报已生效）。

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, RwLock},
};

use crate::features::custom_workflow::CustomWorkflowConfig;
use crate::features::playback::{MatchConfig, SongDedupConfig};

use super::AppConfig;

/// LiveConfigs 覆盖的配置点路径集合（与 schema effect==Live 的字段集合
/// 严格一一对应，由测试 [`live_fields_match_schema_effect`] 强制；新增 Live
/// 字段必须同时登记本常量、本结构共享句柄与消费方读取点）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const LIVE_CONFIG_PATHS: &[&str] = &[
    "stability.secondary_hall_count",
    "timing.loop_idle_ms",
    "timing.chat_scan.fallback_ms",
    "timing.chat_scan.change_debounce_ms",
    "timing.chat_scan.change_cooldown_ms",
    "timing.command.post_settle_ms",
    "timing.command.help_batch_ms",
    "timing.invite.confirm_timeout_ms",
    "timing.invite.confirm_poll_ms",
    "queue.protect_current_song_until_finished",
    "queue.external_playback_protect_after_seconds",
    "timing.playback.status_poll_ms",
    "timing.playback.monitor_status_ms",
    "timing.playback.monitor_tick_ms",
    "song_dedup.enabled",
    "song_dedup.window_seconds",
    "song_dedup.max_count",
    "friend_delivery.auto_retry_count",
    "state.executed_commands_log_path",
    "output.send_enabled",
    "matching.min_song_name_score",
    "matching.short_chinese_song_max_miss",
    "matching.long_chinese_song_min_score",
    "matching.max_ocr_noise_chars",
    "matching.enable_fuzzy_singer",
    "matching.short_chinese_singer_max_miss",
    "matching.long_chinese_singer_min_score",
    "matching.en_max_edit_fraction",
    "matching.en_singer_max_edit_fraction",
    "custom_workflows.enabled",
    "custom_workflows.default_threshold",
    "custom_workflows.wait_template_absent_stable_default",
    "custom_workflows.max_hold_key_seconds",
    "custom_workflows.templates",
    "custom_workflows.workflows",
];

/// 当前进程真正采用的配置快照。
///
/// 保存配置时只把 [`LIVE_CONFIG_PATHS`] 中的字段合并进来。这样消费方可以按一次
/// 操作获取一致快照，同时不会误读已经落库、但仍需子进程重载或重启才能生效的字段。
#[derive(Clone)]
struct EffectiveConfig {
    inner: Arc<RwLock<Arc<AppConfig>>>,
}

/// 当前子进程中等待闲置时重载的配置变更。
///
/// 字段集合在整个子进程生命周期内单调累积。开始关停时只设置标记、不清空集合，
/// 这样与关停并发完成的保存仍会留在诊断快照中；新子进程会从数据库读取最终配置。
#[derive(Default)]
struct PendingReload {
    fields: BTreeSet<String>,
    shutdown_started: bool,
}

impl EffectiveConfig {
    fn new(config: &AppConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Arc::new(config.clone()))),
        }
    }

    fn snapshot(&self) -> Arc<AppConfig> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn replace(&self, config: AppConfig) {
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(config);
    }
}

/// 运行时可热更新配置的共享句柄集合（保存成功后由 HTTP 层 apply）。
#[derive(Clone)]
pub struct LiveConfigs {
    effective: EffectiveConfig,
    pending_reload: Arc<Mutex<PendingReload>>,
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
    /// timing.chat_scan.change_debounce_ms
    pub change_debounce_ms: Arc<RwLock<u64>>,
    /// song_dedup.enabled / window_seconds / max_count（整段共享，
    /// history_path/console_bypass 不热更新）
    pub song_dedup: Arc<RwLock<SongDedupConfig>>,
    /// friend_delivery.auto_retry_count
    pub friend_delivery_auto_retry_count: Arc<RwLock<u32>>,
    /// output.send_enabled
    pub output_send_enabled: Arc<RwLock<bool>>,
    /// matching.*
    pub matching: Arc<RwLock<MatchConfig>>,
    /// custom_workflows.*
    pub custom_workflows: Arc<RwLock<CustomWorkflowConfig>>,
}

impl Default for LiveConfigs {
    fn default() -> Self {
        Self::from_config(&AppConfig::default())
    }
}

impl LiveConfigs {
    /// 从完整配置初始化全部值（启动时创建一次）。
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            effective: EffectiveConfig::new(config),
            pending_reload: Arc::new(Mutex::new(PendingReload::default())),
            queue_protect_current_song: Arc::new(RwLock::new(
                config.queue.protect_current_song_until_finished,
            )),
            queue_external_protect_seconds: Arc::new(RwLock::new(
                config.queue.external_playback_protect_after_seconds,
            )),
            status_poll_ms: Arc::new(RwLock::new(config.timing.playback.status_poll_ms)),
            monitor_status_ms: Arc::new(RwLock::new(config.timing.playback.monitor_status_ms)),
            monitor_tick_ms: Arc::new(RwLock::new(config.timing.playback.monitor_tick_ms)),
            change_debounce_ms: Arc::new(RwLock::new(config.timing.chat_scan.change_debounce_ms)),
            song_dedup: Arc::new(RwLock::new(config.song_dedup.clone())),
            friend_delivery_auto_retry_count: Arc::new(RwLock::new(
                config.friend_delivery.auto_retry_count,
            )),
            output_send_enabled: Arc::new(RwLock::new(config.output.send_enabled)),
            matching: Arc::new(RwLock::new(config.matching.clone())),
            custom_workflows: Arc::new(RwLock::new(config.custom_workflows.clone())),
        }
    }

    /// 获取一次操作应使用的有效配置快照；内部读锁只用于克隆 `Arc`，不会跨业务调用。
    pub fn snapshot(&self) -> Arc<AppConfig> {
        self.effective.snapshot()
    }

    /// 登记需要在闲置时重载子进程才能生效的字段。
    ///
    /// 保存和回滚都只追加字段，不尝试撤销已有请求：即使配置最终回到启动值，
    /// 多执行一次重载也比错误取消另一次并发保存所需的重载更安全。
    pub(crate) fn schedule_reload(&self, fields: impl IntoIterator<Item = String>) {
        let mut pending = self
            .pending_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending
            .fields
            .extend(fields.into_iter().filter(|field| !field.is_empty()));
    }

    /// 返回当前进程累计等待重载的字段；结果按路径排序并与内部状态解耦。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pending_reload_fields(&self) -> BTreeSet<String> {
        self.pending_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fields
            .clone()
    }

    /// 是否已经有配置变更等待闲置重载。
    pub(crate) fn has_pending_reload(&self) -> bool {
        !self
            .pending_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fields
            .is_empty()
    }

    /// 原子认领一次闲置重载请求。
    ///
    /// 首次调用在存在待处理字段时返回当时的字段快照；之后返回 `None`。
    /// 内部字段不会被清空，关停期间并发登记的变更仍会保留，并由新进程从数据库加载。
    pub(crate) fn begin_reload(&self) -> Option<BTreeSet<String>> {
        let mut pending = self
            .pending_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.fields.is_empty() || pending.shutdown_started {
            return None;
        }
        pending.shutdown_started = true;
        Some(pending.fields.clone())
    }

    /// 保存成功后合并全部 Live 字段；共享句柄保持不变，运行态读取点无需重建。
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
            .change_debounce_ms
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.timing.chat_scan.change_debounce_ms;
        let mut song_dedup = self
            .song_dedup
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        song_dedup.enabled = config.song_dedup.enabled;
        song_dedup.window_seconds = config.song_dedup.window_seconds;
        song_dedup.max_count = config.song_dedup.max_count;
        drop(song_dedup);
        *self
            .friend_delivery_auto_retry_count
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.friend_delivery.auto_retry_count;
        *self
            .output_send_enabled
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.output.send_enabled;
        *self
            .matching
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.matching.clone();
        *self
            .custom_workflows
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.custom_workflows.clone();

        let mut effective = Arc::unwrap_or_clone(self.effective.snapshot());
        effective.stability.secondary_hall_count = config.stability.secondary_hall_count;
        effective.timing.loop_idle_ms = config.timing.loop_idle_ms;
        effective.timing.chat_scan.fallback_ms = config.timing.chat_scan.fallback_ms;
        effective.timing.chat_scan.change_debounce_ms = config.timing.chat_scan.change_debounce_ms;
        effective.timing.chat_scan.change_cooldown_ms = config.timing.chat_scan.change_cooldown_ms;
        effective.timing.command.post_settle_ms = config.timing.command.post_settle_ms;
        effective.timing.command.help_batch_ms = config.timing.command.help_batch_ms;
        effective.timing.invite.confirm_timeout_ms = config.timing.invite.confirm_timeout_ms;
        effective.timing.invite.confirm_poll_ms = config.timing.invite.confirm_poll_ms;
        effective.queue.protect_current_song_until_finished =
            config.queue.protect_current_song_until_finished;
        effective.queue.external_playback_protect_after_seconds =
            config.queue.external_playback_protect_after_seconds;
        effective.timing.playback.status_poll_ms = config.timing.playback.status_poll_ms;
        effective.timing.playback.monitor_status_ms = config.timing.playback.monitor_status_ms;
        effective.timing.playback.monitor_tick_ms = config.timing.playback.monitor_tick_ms;
        effective.song_dedup.enabled = config.song_dedup.enabled;
        effective.song_dedup.window_seconds = config.song_dedup.window_seconds;
        effective.song_dedup.max_count = config.song_dedup.max_count;
        effective.friend_delivery.auto_retry_count = config.friend_delivery.auto_retry_count;
        effective.state.executed_commands_log_path =
            config.state.executed_commands_log_path.clone();
        effective.output.send_enabled = config.output.send_enabled;
        effective.matching = config.matching.clone();
        effective.custom_workflows = config.custom_workflows.clone();
        self.effective.replace(effective);
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
        config.stability.secondary_hall_count = 6;
        config.timing.loop_idle_ms = 77;
        config.timing.chat_scan.fallback_ms = 4444;
        config.timing.chat_scan.change_debounce_ms = 155;
        config.timing.chat_scan.change_cooldown_ms = 666;
        config.timing.command.post_settle_ms = 888;
        config.timing.command.help_batch_ms = 999;
        config.timing.invite.confirm_timeout_ms = 12_345;
        config.timing.invite.confirm_poll_ms = 234;
        config.state.executed_commands_log_path = "changed-commands.log".into();
        config.output.send_enabled = false;
        config.matching.min_song_name_score = 0.75;
        config.matching.enable_fuzzy_singer = false;
        config.custom_workflows.enabled = true;
        config.custom_workflows.default_threshold = 0.73;
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
        assert_eq!(*live.change_debounce_ms.read().unwrap(), 155);
        let dedup = live.song_dedup.read().unwrap();
        assert!(!dedup.enabled);
        assert_eq!(dedup.window_seconds, 123);
        assert_eq!(dedup.max_count, 7);
        drop(dedup);
        assert_eq!(*live.friend_delivery_auto_retry_count.read().unwrap(), 5);
        assert!(!*live.output_send_enabled.read().unwrap());
        assert_eq!(live.matching.read().unwrap().min_song_name_score, 0.75);
        assert!(live.custom_workflows.read().unwrap().enabled);
        let snapshot = live.snapshot();
        assert_eq!(snapshot.stability.secondary_hall_count, 6);
        assert_eq!(snapshot.timing.loop_idle_ms, 77);
        assert_eq!(snapshot.timing.command.help_batch_ms, 999);
        assert_eq!(
            snapshot.state.executed_commands_log_path,
            std::path::PathBuf::from("changed-commands.log")
        );
    }

    #[test]
    fn pending_reload_is_shared_deduplicated_and_sorted() {
        let live = LiveConfigs::default();
        assert!(!live.has_pending_reload());
        assert!(live.pending_reload_fields().is_empty());

        live.schedule_reload(vec![
            "queue.max_size".to_string(),
            "providers.qq".to_string(),
            "queue.max_size".to_string(),
            String::new(),
        ]);

        let cloned = live.clone();
        assert!(cloned.has_pending_reload());
        assert_eq!(
            cloned.pending_reload_fields(),
            BTreeSet::from(["providers.qq".to_string(), "queue.max_size".to_string(),])
        );
    }

    #[test]
    fn begin_reload_claims_once_without_losing_concurrent_saves() {
        let live = LiveConfigs::default();
        live.schedule_reload(vec!["queue.max_size".to_string()]);

        let concurrent = live.clone();
        let save = std::thread::spawn(move || {
            concurrent.schedule_reload(vec!["providers.qq".to_string()]);
        });

        let claimed = live.begin_reload().expect("应认领待处理的重载请求");
        save.join().expect("并发登记线程不应崩溃");

        assert!(claimed.contains("queue.max_size"));
        assert_eq!(
            live.pending_reload_fields(),
            BTreeSet::from(["providers.qq".to_string(), "queue.max_size".to_string(),])
        );
        assert!(live.has_pending_reload());
        assert_eq!(live.begin_reload(), None, "关停请求只能认领一次");
    }

    #[test]
    fn begin_reload_requires_a_pending_field() {
        let live = LiveConfigs::default();
        assert_eq!(live.begin_reload(), None);

        live.schedule_reload(vec!["queue.max_size".to_string()]);
        assert_eq!(
            live.begin_reload(),
            Some(BTreeSet::from(["queue.max_size".to_string()]))
        );
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
        assert_eq!(*live.change_debounce_ms.read().unwrap(), 155);
        let dedup = live.song_dedup.read().unwrap();
        assert!(!dedup.enabled);
        assert_eq!(dedup.window_seconds, 123);
        assert_eq!(dedup.max_count, 7);
        drop(dedup);
        assert_eq!(*live.friend_delivery_auto_retry_count.read().unwrap(), 5);
        assert!(!*live.output_send_enabled.read().unwrap());
        assert!(!live.matching.read().unwrap().enable_fuzzy_singer);
        assert_eq!(
            live.custom_workflows.read().unwrap().default_threshold,
            0.73
        );
        let snapshot = live.snapshot();
        assert_eq!(snapshot.timing.chat_scan.fallback_ms, 4444);
        assert_eq!(snapshot.timing.chat_scan.change_debounce_ms, 155);
        assert_eq!(snapshot.timing.chat_scan.change_cooldown_ms, 666);
        assert_eq!(snapshot.timing.command.post_settle_ms, 888);
        assert_eq!(snapshot.timing.invite.confirm_timeout_ms, 12_345);
        assert_eq!(snapshot.timing.invite.confirm_poll_ms, 234);
        assert!(!snapshot.output.send_enabled);
        assert_eq!(snapshot.matching.min_song_name_score, 0.75);
        assert_eq!(snapshot.custom_workflows.default_threshold, 0.73);
        // 再次覆盖回默认值：共享句柄不变，值必须跟着变。
        live.apply(&AppConfig::default());
        assert!(*live.queue_protect_current_song.read().unwrap());
        assert_eq!(live.song_dedup.read().unwrap().max_count, 1);
    }

    #[test]
    fn apply_keeps_non_live_fields_at_their_effective_values() {
        let initial = AppConfig::default();
        let live = LiveConfigs::from_config(&initial);
        let mut changed = changed_config();
        changed.queue.max_size = initial.queue.max_size + 10;
        changed.window.target_process = "changed.exe".to_string();
        changed.song_dedup.console_bypass = !initial.song_dedup.console_bypass;

        live.apply(&changed);

        let snapshot = live.snapshot();
        assert_eq!(snapshot.queue.max_size, initial.queue.max_size);
        assert_eq!(
            snapshot.window.target_process,
            initial.window.target_process
        );
        assert_eq!(
            snapshot.song_dedup.console_bypass,
            initial.song_dedup.console_bypass
        );
        assert_eq!(snapshot.timing.loop_idle_ms, changed.timing.loop_idle_ms);
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
