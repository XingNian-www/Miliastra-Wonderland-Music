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
use crate::features::identity::IdentityAccess;
use crate::features::playback::{MatchConfig, SongDedupConfig};

use super::{AppConfig, Effect, config_effect_for_path};

/// LiveConfigs 覆盖的配置点路径集合（与 schema effect==Live 的字段集合
/// 严格一一对应，由测试 [`live_fields_match_schema_effect`] 强制；新增 Live
/// 字段必须同时登记本常量、本结构共享句柄与消费方读取点）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const LIVE_CONFIG_PATHS: &[&str] = &[
    "stability.secondary_hall_count",
    "timing.watchdog_restart_ms",
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
    "timing.playback.lyrics_lead_seconds",
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
    // ai_chat：除 cooldown_seconds 外的全部字段（cooldown 由启动期 TriggerGate 使用，
    // 不能实时改；其余消费方每次读 snapshot）。
    "ai_chat.enabled",
    "ai_chat.keyword",
    "ai_chat.trigger_mode",
    "ai_chat.min_access",
    "ai_chat.actor_token_budget",
    "ai_chat.actor_summary_budget",
    "ai_chat.hall_token_budget",
    "ai_chat.hall_summary_budget",
    "ai_chat.bot_token_budget",
    "ai_chat.allow_commands",
    "ai_chat.model",
    "ai_chat.reasoning",
    "ai_chat.max_tokens",
    "ai_chat.system_prompt",
    "ai_chat.answer_prompt",
    "ai_chat.extract_prompt",
    "ai_chat.tool_prompts.look",
    "ai_chat.tool_prompts.speak",
    "ai_chat.tool_prompts.command",
    "ai_chat.vision.enabled",
    "ai_chat.vision.mode",
    "ai_chat.vision.capture_scope",
    "ai_chat.vision.max_edge",
    "ai_chat.vision.jpeg_quality",
    "ai_chat.vision.max_bytes",
    "ai_chat.vision.timeout_ms",
    "ai_chat.vision.model",
    "ai_chat.vision.allow_full_screen",
    "ai_chat.ai_reply.mode",
    "ai_chat.ai_reply.include_template",
    "ai_chat.ai_reply.include_command",
    "ai_chat.ai_reply.system_prompt",
    "ai_chat.ai_reply.user_prompt",
    "ai_chat.ai_reply.max_tokens",
    "ai_chat.ai_reply.timeout_seconds",
    // tts：除 endpoint/api_key（IdleReload）外的运行期字段。
    "tts.enabled",
    "tts.model",
    "tts.reference_id",
    "tts.voice",
    "tts.format",
    "tts.duck_volume",
    "tts.duck_fade_ms",
    "tts.max_chars",
    "tts.timeout_ms",
    "tts.queue_max",
    "tts.trigger_scope",
    // song_ai：本地推荐两项由点歌端口每次读 snapshot。
    "song_ai.local_recommend",
    "song_ai.local_recommend_min_score",
    // 点歌决策窗口（task-29 已把消费方改为每次读 snapshot）。
    "timing.decision.timeout_ms",
    "timing.decision.poll_ms",
    // 启动流程三个开关/入口（listener 每次读 live 快照）。
    "startup.enabled",
    "startup.enter_game",
    "startup.enter_wonderland",
    // 外部聊天源：仅连接参数实时，其余（开关/url/token/自动启动/路径/uid）保留旧值。
    "chat_source.reconnect_ms",
    "chat_source.lease_timeout_ms",
    // task-28：task-27 已完成接线（消费方读 live 快照）。
    "ai_chat.cooldown_seconds",
    "moderation.stable_vote_samples",
    "moderation.required_vote_margin",
    // task-33：播放/缓存 7 项（消费方已由 task-31 接线为读 live 快照；
    // audio_cache 只实时三个数值，启用/目录/并发数仍需重载）。
    "playback.login_timeout_ms",
    "playback.audio_cache.max_bytes_mb",
    "playback.audio_cache.request_timeout_ms",
    "playback.audio_cache.seek_wait_timeout_ms",
    "timing.playback.uri_stable_samples",
    "timing.playback.transport_stable_samples",
    "timing.playback.stale_timeout_ms",
    "custom_workflows.enabled",
    "custom_workflows.default_threshold",
    "custom_workflows.wait_template_absent_stable_default",
    "custom_workflows.max_hold_key_seconds",
    "custom_workflows.templates",
    "custom_workflows.workflows",
    "identity.mappings",
    "command_access.overrides",
    "command_access.default_song_source",
    "visual.name",
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
    /// `queue.protect_current_song_until_finished`
    pub queue_protect_current_song: Arc<RwLock<bool>>,
    /// `queue.external_playback_protect_after_seconds`
    pub queue_external_protect_seconds: Arc<RwLock<u64>>,
    /// `timing.playback.status_poll_ms`
    pub status_poll_ms: Arc<RwLock<u64>>,
    /// `timing.playback.monitor_status_ms`
    pub monitor_status_ms: Arc<RwLock<u64>>,
    /// `timing.playback.monitor_tick_ms`
    pub monitor_tick_ms: Arc<RwLock<u64>>,
    /// `timing.playback.lyrics_lead_seconds`
    pub lyrics_lead_seconds: Arc<RwLock<f64>>,
    /// `timing.chat_scan.change_debounce_ms`
    pub change_debounce_ms: Arc<RwLock<u64>>,
    /// song_dedup.enabled / window_seconds / max_count（整段共享，
    /// history_path/console_bypass 不热更新）
    pub song_dedup: Arc<RwLock<SongDedupConfig>>,
    /// `friend_delivery.auto_retry_count`
    pub friend_delivery_auto_retry_count: Arc<RwLock<u32>>,
    /// `output.send_enabled`
    pub output_send_enabled: Arc<RwLock<bool>>,
    /// `matching.*`
    pub matching: Arc<RwLock<MatchConfig>>,
    /// `custom_workflows.*`
    pub custom_workflows: Arc<RwLock<CustomWorkflowConfig>>,
    /// identity.mappings（整段共享，身份查询直接挂在该句柄上）
    pub identity: IdentityAccess,
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
            lyrics_lead_seconds: Arc::new(RwLock::new(config.timing.playback.lyrics_lead_seconds)),
            change_debounce_ms: Arc::new(RwLock::new(config.timing.chat_scan.change_debounce_ms)),
            song_dedup: Arc::new(RwLock::new(config.song_dedup.clone())),
            friend_delivery_auto_retry_count: Arc::new(RwLock::new(
                config.friend_delivery.auto_retry_count,
            )),
            output_send_enabled: Arc::new(RwLock::new(config.output.send_enabled)),
            matching: Arc::new(RwLock::new(config.matching.clone())),
            custom_workflows: Arc::new(RwLock::new(config.custom_workflows.clone())),
            identity: IdentityAccess::new(config.identity.clone()),
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
    /// 只读快照，Web 状态接口也使用它报告保存值与生效值之间的差异，
    /// 不认领、不清空重载请求。
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

    /// 当前累计变更中是否包含必须等播放器空闲才能重载的字段。
    pub(crate) fn pending_reload_requires_playback_idle(&self) -> bool {
        self.pending_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fields
            .iter()
            .any(|field| config_effect_for_path(field) == Some(Effect::PlaybackIdleReload))
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
    /// 只把身份映射同步进 effective 快照，不动其他字段。
    ///
    /// 自动注册好友身份时（外部聊天源首次见到某个 UID）只更新了
    /// `IdentityAccess` 与配置库，没有回写这里。下一次 `apply` 会用
    /// 旧快照 `identity.replace(...)` 把它抹掉——线上表现为
    /// 「自动注册的身份不生效，去设置里手动保存一次才行」。
    pub fn sync_identity_snapshot(&self, identity: crate::features::identity::IdentityConfig) {
        let current = self.effective.snapshot();
        if current.identity.mappings == identity.mappings {
            return;
        }
        let mut next = (*current).clone();
        next.identity = identity;
        self.effective.replace(next);
    }

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
            .lyrics_lead_seconds
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config.timing.playback.lyrics_lead_seconds;
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
        self.identity.replace(config.identity.clone());
        self.sync_identity_snapshot(config.identity.clone());

        // 与 load_full 同一口径：活配置里也不能留着已废弃的面板条目。
        let mut command_access = config.command_access.clone();
        command_access.sanitize_deprecated_overrides();

        let mut effective = Arc::unwrap_or_clone(self.effective.snapshot());
        effective.stability.secondary_hall_count = config.stability.secondary_hall_count;
        effective.timing.watchdog_restart_ms = config.timing.watchdog_restart_ms;
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
        effective.timing.playback.lyrics_lead_seconds = config.timing.playback.lyrics_lead_seconds;
        effective.song_dedup.enabled = config.song_dedup.enabled;
        effective.song_dedup.window_seconds = config.song_dedup.window_seconds;
        effective.song_dedup.max_count = config.song_dedup.max_count;
        effective.friend_delivery.auto_retry_count = config.friend_delivery.auto_retry_count;
        effective.state.executed_commands_log_path =
            config.state.executed_commands_log_path.clone();
        effective.output.send_enabled = config.output.send_enabled;
        effective.matching = config.matching.clone();
        effective.custom_workflows = config.custom_workflows.clone();
        effective.identity = config.identity.clone();
        effective.command_access = command_access;
        // ai_chat：schema 字段现已全部 Live（含 cooldown_seconds），整体 clone。
        effective.ai_chat = config.ai_chat.clone();
        // 模型/能力配置可能变化，清掉视觉能力缓存，避免旧“不支持”状态粘住。
        crate::config::ai::clear_vision_capability_cache();
        // moderation：只实时两个投票阈值；区域/点位等其它字段保留旧值。
        effective.moderation.stable_vote_samples = config.moderation.stable_vote_samples;
        effective.moderation.required_vote_margin = config.moderation.required_vote_margin;
        // playback：登录超时直接赋值；audio_cache 只实时三个数值，且仅在
        // 新旧两侧都启用（Some）时逐字段合并——启用开关/目录/并发数属于
        // 结构配置，仍走空闲重载。
        effective.playback.login_timeout_ms = config.playback.login_timeout_ms;
        if let (Some(effective_cache), Some(config_cache)) = (
            effective.playback.audio_cache.as_mut(),
            config.playback.audio_cache.as_ref(),
        ) {
            effective_cache.max_bytes_mb = config_cache.max_bytes_mb;
            effective_cache.request_timeout_ms = config_cache.request_timeout_ms;
            effective_cache.seek_wait_timeout_ms = config_cache.seek_wait_timeout_ms;
        }
        effective.timing.playback.uri_stable_samples = config.timing.playback.uri_stable_samples;
        effective.timing.playback.transport_stable_samples =
            config.timing.playback.transport_stable_samples;
        effective.timing.playback.stale_timeout_ms = config.timing.playback.stale_timeout_ms;
        effective.song_ai.local_recommend = config.song_ai.local_recommend;
        effective.song_ai.local_recommend_min_score = config.song_ai.local_recommend_min_score;
        effective.timing.decision.timeout_ms = config.timing.decision.timeout_ms;
        effective.timing.decision.poll_ms = config.timing.decision.poll_ms;
        effective.startup.enabled = config.startup.enabled;
        effective.startup.enter_game = config.startup.enter_game;
        effective.startup.enter_wonderland = config.startup.enter_wonderland;
        // chat_source：只实时 reconnect/lease 两项；其余字段（含 secret token、
        // 自动启动与可执行路径）必须保留旧值，避免未登记字段被悄悄实时。
        let mut chat_source = effective.chat_source.clone();
        chat_source.reconnect_ms = config.chat_source.reconnect_ms;
        chat_source.lease_timeout_ms = config.chat_source.lease_timeout_ms;
        effective.chat_source = chat_source;
        // tts：只实时 Live 字段；endpoint/api_key 保留旧值（IdleReload）。
        let mut tts = config.tts.clone();
        tts.endpoint = effective.tts.endpoint.clone();
        tts.api_key = effective.tts.api_key.clone();
        effective.tts = tts;
        effective.visual = config.visual.clone();
        self.effective.replace(effective);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::config::config_sections;
    use crate::features::identity::IdentityConfig;

    /// 构造一份与默认值不同的完整配置，用于取值/覆盖断言。
    fn changed_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.timing.watchdog_restart_ms = 4321;
        config.queue.protect_current_song_until_finished = false;
        config.queue.external_playback_protect_after_seconds = 99;
        config.timing.playback.status_poll_ms = 1111;
        config.timing.playback.monitor_status_ms = 2222;
        config.timing.playback.monitor_tick_ms = 333;
        config.timing.playback.lyrics_lead_seconds = 1.5;
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
        config.identity = IdentityConfig {
            mappings: vec![crate::features::identity::IdentityMapping {
                remark: "派蒙本蒙".to_string(),
                id: uuid::Uuid::from_u128(1),
                role: crate::features::identity::IdentityRole::Owner,
                real_nickname: String::new(),
                display_name: String::new(),
                remark_from_chatbridge: false,
                accounts: Vec::new(),
            }],
        };
        config
    }

    #[test]
    fn panel_command_policy_and_visual_name_apply_without_restart() {
        let live = LiveConfigs::default();
        let mut changed = AppConfig::default();
        changed.visual.name = "我的档案".into();
        changed.command_access.overrides.insert(
            "@QQ点歌".into(),
            crate::features::command_access::CommandPolicy {
                enabled: false,
                minimum_role: crate::features::command_access::CommandPermission::Admin,
            },
        );
        live.apply(&changed);
        assert_eq!(live.snapshot().visual.name, "我的档案");
        assert!(!live.snapshot().command_access.overrides["@QQ点歌"].enabled);
        live.apply(&AppConfig::default());
        assert!(live.snapshot().command_access.overrides.is_empty());
        assert_eq!(live.snapshot().visual.name, "1080P");
    }

    /// A1：活配置与启动加载同一口径——已废弃的 `@点歌` 条目在 `apply` 时被丢弃，
    /// 不能留在运行态快照里「看起来生效、实际没人读」。
    #[test]
    fn applying_a_deprecated_policy_entry_drops_it_from_the_effective_snapshot() {
        let live = LiveConfigs::default();
        let mut changed = AppConfig::default();
        changed.command_access.overrides.insert(
            "@点歌".into(),
            crate::features::command_access::CommandPolicy {
                enabled: false,
                minimum_role: crate::features::command_access::CommandPermission::Admin,
            },
        );
        changed.command_access.overrides.insert(
            "@QQ点歌".into(),
            crate::features::command_access::CommandPolicy {
                enabled: false,
                minimum_role: crate::features::command_access::CommandPermission::User,
            },
        );
        live.apply(&changed);
        let snapshot = live.snapshot();
        assert!(
            !snapshot.command_access.overrides.contains_key("@点歌"),
            "废弃条目必须被丢弃"
        );
        // 正常条目不受影响。
        assert!(!snapshot.command_access.overrides["@QQ点歌"].enabled);
    }

    #[test]
    fn from_config_extracts_values() {
        let live = LiveConfigs::from_config(&changed_config());
        assert!(!*live.queue_protect_current_song.read().unwrap());
        assert_eq!(*live.queue_external_protect_seconds.read().unwrap(), 99);
        assert_eq!(*live.status_poll_ms.read().unwrap(), 1111);
        assert_eq!(*live.monitor_status_ms.read().unwrap(), 2222);
        assert_eq!(*live.monitor_tick_ms.read().unwrap(), 333);
        assert_eq!(*live.lyrics_lead_seconds.read().unwrap(), 1.5);
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
        assert_eq!(snapshot.timing.watchdog_restart_ms, 4321);
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
    fn pending_reload_uses_the_strictest_playback_barrier() {
        let live = LiveConfigs::default();
        live.schedule_reload(["queue.max_size".to_string()]);
        assert!(!live.pending_reload_requires_playback_idle());

        live.schedule_reload(["playback.credential_directory".to_string()]);
        assert!(live.pending_reload_requires_playback_idle());
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

    /// 回归：自动注册的身份必须能立刻用，不需要去设置里手动保存。
    ///
    /// 原来只更新 `IdentityAccess` 与配置库，没有回写 effective 快照；
    /// 下一次 `apply` 会用旧快照 `identity.replace(...)` 把它抹掉。
    /// 线上表现为「新好友第一次说话时没有好友权限，保存一次设置才好」。
    #[test]
    fn auto_registered_identity_survives_the_next_apply() {
        let mut config = AppConfig::default();
        let live = LiveConfigs::from_config(&config);

        // 模拟自动注册：直接改 IdentityAccess 再同步快照（生产里由
        // external_chat 调 persist_identity 完成）。
        let auto = crate::features::identity::IdentityMapping {
            remark: "auto_nick".to_string(),
            id: uuid::Uuid::new_v4(),
            role: crate::features::identity::IdentityRole::Friend,
            real_nickname: String::new(),
            display_name: "自动注册".to_string(),
            remark_from_chatbridge: false,
            accounts: vec![crate::features::identity::GameAccount::new("900000099")],
        };
        let mut updated = config.identity.clone();
        updated.mappings.push(auto.clone());
        live.identity.replace(updated.clone());
        live.sync_identity_snapshot(updated.clone());

        // 下一次 apply 用的是旧 AppConfig：快照若不带上新映射，它就会消失。
        config.identity = updated.clone();
        live.apply(&config);

        assert!(
            live.identity
                .resolve_hint(crate::features::identity::IdentityHint::Account {
                    game: "genshin",
                    uid: "900000099",
                    nickname: None,
                })
                .is_some(),
            "自动注册的身份在 apply 之后必须仍然可用"
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
        assert_eq!(*live.lyrics_lead_seconds.read().unwrap(), 1.5);
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
        assert!(live.identity.is_owner("派蒙本蒙"));
        assert!(!live.identity.is_friend_or_above("陌生人"));
        let snapshot = live.snapshot();
        assert_eq!(snapshot.timing.watchdog_restart_ms, 4321);
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
        assert_eq!(*live.lyrics_lead_seconds.read().unwrap(), 0.0);
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

    fn set_json_path(root: &mut serde_json::Value, path: &str, value: serde_json::Value) {
        let mut current = root;
        let segments: Vec<&str> = path.split('.').collect();
        for segment in &segments[..segments.len() - 1] {
            current = current
                .get_mut(*segment)
                .expect("测试路径的中间节点必须存在");
        }
        current[segments[segments.len() - 1]] = value;
    }

    fn json_at<'a>(root: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
        path.split('.').fold(root, |current, segment| {
            current.get(segment).expect("测试路径必须存在")
        })
    }

    /// P2-0 缺口补丁：新增 Live 字段必须逐条证明 `apply()` 真的把值写进快照。
    /// 表驱动：每个路径先证明「改动值 ≠ 默认值」，apply 后快照 == 改动值；
    /// 未登记 Live 的 cooldown_seconds / song_ai.system_prompt / song_ai.model
    /// 即使被改动，也必须保留旧值。
    #[test]
    fn newly_live_ai_chat_and_song_ai_fields_reach_the_snapshot() {
        use serde_json::json;

        let live = LiveConfigs::default();
        let baseline = serde_json::to_value(AppConfig::default()).expect("默认配置序列化");
        let mut changed_value = serde_json::to_value(AppConfig::default()).expect("序列化默认配置");
        let live_values: Vec<(&str, serde_json::Value)> = vec![
            ("ai_chat.enabled", json!(true)),
            ("ai_chat.keyword", json!("测试关键词")),
            ("ai_chat.tool_prompts.look", json!("自定义 look")),
            ("ai_chat.tool_prompts.speak", json!("自定义 speak")),
            ("ai_chat.tool_prompts.command", json!("自定义 command")),
            ("ai_chat.trigger_mode", json!("contains")),
            ("ai_chat.min_access", json!("owner")),
            ("ai_chat.actor_token_budget", json!(123_456)),
            ("ai_chat.actor_summary_budget", json!(23_456)),
            ("ai_chat.hall_token_budget", json!(234_567)),
            ("ai_chat.hall_summary_budget", json!(34_567)),
            ("ai_chat.bot_token_budget", json!(45_678)),
            ("ai_chat.allow_commands", json!(false)),
            ("ai_chat.model", json!("test-model")),
            ("ai_chat.reasoning", json!("high")),
            ("ai_chat.max_tokens", json!(54_321)),
            ("ai_chat.system_prompt", json!("changed-system-prompt")),
            ("ai_chat.answer_prompt", json!("changed-answer-prompt")),
            ("ai_chat.extract_prompt", json!("changed-extract-prompt")),
            ("ai_chat.ai_reply.mode", json!("rewrite")),
            ("ai_chat.ai_reply.include_template", json!(false)),
            ("ai_chat.ai_reply.include_command", json!(false)),
            (
                "ai_chat.ai_reply.system_prompt",
                json!("changed-reply-system-prompt"),
            ),
            (
                "ai_chat.ai_reply.user_prompt",
                json!("changed-reply-user-prompt"),
            ),
            ("ai_chat.ai_reply.max_tokens", json!(321)),
            ("ai_chat.ai_reply.timeout_seconds", json!(77)),
            ("ai_chat.vision.enabled", json!(false)),
            ("ai_chat.vision.mode", json!("tool")),
            ("ai_chat.vision.max_edge", json!(1_024)),
            ("ai_chat.vision.jpeg_quality", json!(60)),
            ("ai_chat.vision.max_bytes", json!(2_000_000)),
            ("ai_chat.vision.timeout_ms", json!(1_200)),
            ("ai_chat.vision.model", json!("vision-test-model")),
            ("ai_chat.vision.allow_full_screen", json!(false)),
            ("song_ai.local_recommend", json!(false)),
            ("song_ai.local_recommend_min_score", json!(0.42)),
            ("ai_chat.cooldown_seconds", json!(9_999)),
        ];
        for (path, value) in &live_values {
            assert_ne!(
                json_at(&baseline, path),
                value,
                "测试值必须与默认值不同（证明 apply 真的覆盖）: {path}"
            );
            set_json_path(&mut changed_value, path, value.clone());
        }
        // 未登记 Live 的字段：改动后必须被 apply 丢弃（保留旧值）。
        set_json_path(
            &mut changed_value,
            "song_ai.system_prompt",
            json!("should-not-apply"),
        );
        set_json_path(
            &mut changed_value,
            "song_ai.model",
            json!("should-not-apply-model"),
        );
        let changed: AppConfig =
            serde_json::from_value(changed_value.clone()).expect("改动配置反序列化");

        live.apply(&changed);
        let snapshot = serde_json::to_value(&*live.snapshot()).expect("快照序列化");

        for (path, _) in &live_values {
            assert_eq!(
                json_at(&snapshot, path),
                json_at(&changed_value, path),
                "新增 Live 字段 apply 后必须等于改动值: {path}"
            );
        }
        for path in ["song_ai.system_prompt", "song_ai.model"] {
            assert_eq!(
                json_at(&snapshot, path),
                json_at(&baseline, path),
                "未登记 Live 的字段必须保留旧值: {path}"
            );
        }
    }
    /// tts：Live 字段逐项进快照，endpoint/api_key 保留旧值（IdleReload）。
    #[test]
    fn newly_live_tts_fields_reach_the_snapshot() {
        use serde_json::json;

        let live = LiveConfigs::default();
        let baseline = serde_json::to_value(AppConfig::default()).expect("默认配置序列化");
        let mut changed_value = serde_json::to_value(AppConfig::default()).expect("序列化默认配置");
        let live_values: Vec<(&str, serde_json::Value)> = vec![
            ("tts.enabled", json!(true)),
            ("tts.model", json!("tts-test-model")),
            ("tts.reference_id", json!("tts-test-reference")),
            ("tts.voice", json!("tts-test-voice")),
            ("tts.duck_volume", json!(45)),
            ("tts.duck_fade_ms", json!(300)),
            ("tts.max_chars", json!(120)),
            ("tts.timeout_ms", json!(9_000)),
            ("tts.queue_max", json!(4)),
        ];
        for (path, value) in &live_values {
            assert_ne!(
                json_at(&baseline, path),
                value,
                "测试值必须与默认值不同（证明 apply 真的覆盖）: {path}"
            );
            set_json_path(&mut changed_value, path, value.clone());
        }
        set_json_path(
            &mut changed_value,
            "tts.endpoint",
            json!("https://should-not-apply.example"),
        );
        set_json_path(&mut changed_value, "tts.api_key", json!("should-not-apply-key"));
        let changed: AppConfig =
            serde_json::from_value(changed_value.clone()).expect("改动配置反序列化");

        live.apply(&changed);
        let snapshot = serde_json::to_value(&*live.snapshot()).expect("快照序列化");
        for (path, _) in &live_values {
            assert_eq!(
                json_at(&snapshot, path),
                json_at(&changed_value, path),
                "tts Live 字段 apply 后必须等于改动值: {path}"
            );
        }
        for path in ["tts.endpoint", "tts.api_key"] {
            assert_eq!(
                json_at(&snapshot, path),
                json_at(&baseline, path),
                "tts 的 IdleReload 字段必须保留旧值: {path}"
            );
        }
    }
    /// task-30：startup 3 + decision 2 + chat_source 2 的表驱动 apply 断言。
    /// 未登记 Live 的 startup.launch_game / chat_source.enabled / url 必须保留旧值。
    #[test]
    fn newly_live_startup_decision_chat_source_fields_reach_the_snapshot() {
        use serde_json::json;

        let live = LiveConfigs::default();
        let baseline = serde_json::to_value(AppConfig::default()).expect("默认配置序列化");
        let mut changed_value = serde_json::to_value(AppConfig::default()).expect("序列化默认配置");
        let live_values: Vec<(&str, serde_json::Value)> = vec![
            ("startup.enabled", json!(true)),
            ("startup.enter_game", json!(true)),
            ("startup.enter_wonderland", json!(true)),
            ("timing.decision.timeout_ms", json!(21_000)),
            ("timing.decision.poll_ms", json!(2_100)),
            ("chat_source.reconnect_ms", json!(4_321)),
            ("chat_source.lease_timeout_ms", json!(54_321)),
        ];
        for (path, value) in &live_values {
            assert_ne!(
                json_at(&baseline, path),
                value,
                "测试值必须与默认值不同（证明 apply 真的覆盖）: {path}"
            );
            set_json_path(&mut changed_value, path, value.clone());
        }
        // 未登记 Live 的字段：改动后必须被 apply 丢弃（保留旧值）。
        set_json_path(&mut changed_value, "startup.launch_game", json!(true));
        set_json_path(&mut changed_value, "chat_source.enabled", json!(true));
        set_json_path(
            &mut changed_value,
            "chat_source.url",
            json!("ws://127.0.0.1:19999/stream"),
        );
        let changed: AppConfig =
            serde_json::from_value(changed_value.clone()).expect("改动配置反序列化");

        live.apply(&changed);
        let snapshot = serde_json::to_value(&*live.snapshot()).expect("快照序列化");

        for (path, _) in &live_values {
            assert_eq!(
                json_at(&snapshot, path),
                json_at(&changed_value, path),
                "新增 Live 字段 apply 后必须等于改动值: {path}"
            );
        }
        for path in [
            "startup.launch_game",
            "chat_source.enabled",
            "chat_source.url",
        ] {
            assert_eq!(
                json_at(&snapshot, path),
                json_at(&baseline, path),
                "未登记 Live 的字段必须保留旧值: {path}"
            );
        }
    }
    /// task-28：cooldown + moderation 两个投票阈值的表驱动 apply 断言。
    /// moderation 的区域/点位等未登记字段必须保留旧值。
    #[test]
    fn newly_live_cooldown_and_moderation_fields_reach_the_snapshot() {
        use serde_json::json;

        let live = LiveConfigs::default();
        let baseline = serde_json::to_value(AppConfig::default()).expect("默认配置序列化");
        let mut changed_value = serde_json::to_value(AppConfig::default()).expect("序列化默认配置");
        let live_values: Vec<(&str, serde_json::Value)> = vec![
            ("ai_chat.cooldown_seconds", json!(9_999)),
            ("moderation.stable_vote_samples", json!(7)),
            ("moderation.required_vote_margin", json!(5)),
        ];
        for (path, value) in &live_values {
            assert_ne!(
                json_at(&baseline, path),
                value,
                "测试值必须与默认值不同（证明 apply 真的覆盖）: {path}"
            );
            set_json_path(&mut changed_value, path, value.clone());
        }
        // moderation 未登记字段：改动后必须被 apply 丢弃。
        set_json_path(
            &mut changed_value,
            "moderation.friend_panel_region",
            json!({"x": 1, "y": 2, "width": 3, "height": 4}),
        );
        let changed: AppConfig =
            serde_json::from_value(changed_value.clone()).expect("改动配置反序列化");

        live.apply(&changed);
        let snapshot = serde_json::to_value(&*live.snapshot()).expect("快照序列化");

        for (path, _) in &live_values {
            assert_eq!(
                json_at(&snapshot, path),
                json_at(&changed_value, path),
                "新增 Live 字段 apply 后必须等于改动值: {path}"
            );
        }
        assert_eq!(
            json_at(&snapshot, "moderation.friend_panel_region"),
            json_at(&baseline, "moderation.friend_panel_region"),
            "moderation 未登记字段必须保留旧值"
        );
    }

    /// task-33：播放/缓存 7 项的表驱动 apply 断言。
    /// audio_cache 只实时三个数值，启用/目录/并发数保留旧值；父级 None 不清空已生效缓存。
    #[test]
    fn newly_live_playback_and_cache_fields_reach_the_snapshot() {
        use serde_json::json;

        let mut base = AppConfig::default();
        base.playback.audio_cache = Some(crate::config::AudioCacheFileConfig {
            enabled: true,
            directory: std::path::PathBuf::from("deps/cache/audio"),
            max_bytes_mb: 1_000,
            max_concurrent_downloads: 2,
            request_timeout_ms: 30_000,
            seek_wait_timeout_ms: 5_000,
        });
        let live = LiveConfigs::from_config(&base);
        let baseline = serde_json::to_value(&base).expect("基线配置序列化");
        let mut changed_value = serde_json::to_value(&base).expect("序列化基线配置");
        let live_values: Vec<(&str, serde_json::Value)> = vec![
            ("playback.login_timeout_ms", json!(190_000)),
            ("playback.audio_cache.max_bytes_mb", json!(2_000)),
            ("playback.audio_cache.request_timeout_ms", json!(31_000)),
            ("playback.audio_cache.seek_wait_timeout_ms", json!(6_000)),
            ("timing.playback.uri_stable_samples", json!(2)),
            ("timing.playback.transport_stable_samples", json!(3)),
            ("timing.playback.stale_timeout_ms", json!(9_000)),
        ];
        for (path, value) in &live_values {
            assert_ne!(
                json_at(&baseline, path),
                value,
                "测试值必须与默认值不同（证明 apply 真的覆盖）: {path}"
            );
            set_json_path(&mut changed_value, path, value.clone());
        }
        // audio_cache 未登记字段：改动后必须被 apply 丢弃（保留旧值）。
        set_json_path(
            &mut changed_value,
            "playback.audio_cache.enabled",
            json!(false),
        );
        set_json_path(
            &mut changed_value,
            "playback.audio_cache.directory",
            json!("changed/cache"),
        );
        set_json_path(
            &mut changed_value,
            "playback.audio_cache.max_concurrent_downloads",
            json!(9),
        );
        let changed: AppConfig =
            serde_json::from_value(changed_value.clone()).expect("改动配置反序列化");

        live.apply(&changed);
        let snapshot = serde_json::to_value(&*live.snapshot()).expect("快照序列化");

        for (path, _) in &live_values {
            assert_eq!(
                json_at(&snapshot, path),
                json_at(&changed_value, path),
                "新增 Live 字段 apply 后必须等于改动值: {path}"
            );
        }
        for path in [
            "playback.audio_cache.enabled",
            "playback.audio_cache.directory",
            "playback.audio_cache.max_concurrent_downloads",
        ] {
            assert_eq!(
                json_at(&snapshot, path),
                json_at(&baseline, path),
                "audio_cache 未登记字段必须保留旧值: {path}"
            );
        }

        // 父级 audio_cache 变 None（结构性变更，走重载）不得把已生效的缓存清空：
        // 第二次 apply 后缓存字段应保持第一次 apply 后的实时值。
        let cache_before_second_apply =
            json_at(&snapshot, "playback.audio_cache.max_bytes_mb").clone();
        let mut disabled = AppConfig::default();
        disabled.playback.login_timeout_ms = 123_456;
        live.apply(&disabled);
        let snapshot_after = serde_json::to_value(&*live.snapshot()).expect("快照序列化");
        assert_eq!(
            json_at(&snapshot_after, "playback.audio_cache.max_bytes_mb"),
            &cache_before_second_apply,
            "父级 None 不清空已生效缓存"
        );
        assert_eq!(
            json_at(&snapshot_after, "playback.login_timeout_ms"),
            &json!(123_456)
        );
    }
}
