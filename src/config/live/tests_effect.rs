//! W0-B：effect 声明 ↔ `apply()` 实现的表驱动对照（399 条路径）。
//!
//! # 这条测试回答什么问题
//!
//! schema 里 399 个字段都声明了 effect（Live / IdleReload / PlaybackIdleReload /
//! Restart）。声明是给用户看的承诺：**Live = 保存后立即生效**。但
//! [`LiveConfigs::apply`] 是手写的 121 行赋值，声明与实现可能静默分叉——
//! 面板说「立即生效」而运行态没变（虚报），或者面板说「重载后生效」而
//! `apply` 顺带把整段 clone 进去（提前生效，历史上 `tts.http_proxy` 就是这样）。
//!
//! 本模块对 `config_sections()` 的**每一条路径**做同一件事：
//!
//! 1. 用 typed setter 把该字段改成「一个非默认值」；
//! 2. 用 typed getter 读回一个可比表示（`Debug`，不经 JSON 往返，
//!    因此 `http`/`logging` 与 `_legacy_bot_name` 这类字段不会被悄悄重置）；
//! 3. `LiveConfigs::from_config(默认/基线)` + `apply(改动后)`；
//! 4. 断言：声明 Live ⇒ 快照等于改动值；声明非 Live ⇒ 快照仍是旧值。
//!
//! # 为什么不用 JSON 往返
//!
//! 方案 §W0 明确要求「typed 路径表（`&'static str` 路径 + 明确 get/set）」：
//! 先用 `serde_json::to_value(AppConfig)` 再改路径、再反序列化，会重置
//! 序列化时被跳过/改名的字段（`_legacy_bot_name`）与不在配置库里的段
//! （`http`/`logging` 走 bootstrap），于是「字段没变」可能只是构造过程把它
//! 冲掉了——测试会假通过。这里每条路径都直接读写结构体字段。
//!
//! # 基线前置条件
//!
//! 极少数路径的值嵌在 `Option` 容器里（`playback.audio_cache.*`：默认父级为
//! `null`，只有用户启用缓存后子字段才有意义）。对这些路径，基线先把容器
//! 构造成「启用后的默认值」——见 [`base_config`]。这不是放宽断言：apply 的
//! 合并条件本来就是「新旧两侧都存在」，前置条件只是复现真实使用场景。
//!
//! # 已知不可观测项
//!
//! [`SINGLETON_ENUM_PATHS`] 记录**取值空间只有一个值**的枚举字段：它们声明
//! Live，但今天无法构造出不同的值，矩阵对它们不可观测。测试会持续校验
//! 「它们确实还是不可观测」：一旦枚举新增取值，这条断言失败，提示把它移出
//! 清单、纳入正式矩阵——只减不增。
//!
//! [`KNOWN_DEVIATIONS`] 记录「声明与实现确实不一致」的路径，**初始为空**。
//! 任何新增条目都必须带证据与理由；测试同时保证「登记了却已经通过」会失败，
//! 所以它只能收缩，不能膨胀。

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::config::config_sections;

/// 一条路径的 typed 构造器与探针。
struct PathCase {
    /// schema 中的点路径（与 `config_sections()` 一一对应）。
    path: &'static str,
    /// 写入一个可构造的非默认值。
    set: fn(&mut AppConfig),
    /// 读回该值的稳定表示（`Debug`；不经 JSON 往返）。
    probe: fn(&AppConfig) -> String,
}

/// 构造一条用例。
fn case(path: &'static str, set: fn(&mut AppConfig), probe: fn(&AppConfig) -> String) -> PathCase {
    PathCase { path, set, probe }
}

/// `Debug` 探针：比字符串比较更严格（类型/结构差异都能看出来），
/// 且不依赖 `PartialEq`（部分配置类型没有实现它）。
fn debug_of<T: std::fmt::Debug + ?Sized>(value: &T) -> String {
    format!("{value:?}")
}

/// 声明为 Live、但取值空间只有一个值的枚举字段（不可观测）。
///
/// 这些不是缺陷：新建一个取值就是新语义，今天不存在「另一个合法值」。
/// 测试会断言它们**仍然**不可观测（改值前后探针相同），一旦枚举扩容即失败。
const SINGLETON_ENUM_PATHS: &[&str] = &[
    "ai_chat.vision.capture_scope",
    "tts.format",
    "tts.trigger_scope",
];

/// 已确认「声明与实现不一致」的路径。**只减不增**。
///
/// 初始为空：W0 的目标是「零偏差」。若某条确实无法达标，必须写清
/// 证据（哪一段、apply 第几步、为什么今天修不了）——它属于 W3 的修复清单，
/// 不是在这里放宽断言的理由。
const KNOWN_DEVIATIONS: &[KnownDeviation] = &[
    // 目前为空。新增格式：
    // KnownDeviation {
    //     path: "xxx.yyy",
    //     reason: "证据：…；影响：…；建议修复：W3 的 …",
    // },
];

struct KnownDeviation {
    path: &'static str,
    #[allow(dead_code)]
    reason: &'static str,
}

/// `playback.audio_cache.*` 子字段的默认父级是 `null`（缓存默认不启用），
/// 必须先按「启用缓存」的默认值构造出父级容器（与 Web「启用」预填、
/// `audio_cache_defaults_match_runtime_values` 同一份数据）。
///
/// 三个数值子字段虽然声明 Live，但 `apply()` 的合并条件是「新旧两侧都启用」；
/// 没有这个前置条件，字段根本不存在、矩阵无法观测（会把「不可测」误报成「没生效」）。
fn probe_audio_cache() -> crate::config::AudioCacheFileConfig {
    crate::config::AudioCacheFileConfig {
        enabled: true,
        directory: std::path::PathBuf::from("deps/cache/audio"),
        max_bytes_mb: 20 * 1024,
        max_concurrent_downloads: 2,
        request_timeout_ms: 15_000,
        seek_wait_timeout_ms: 10_000,
    }
}

/// `playback.audio_cache` 的子路径：默认父级为 `null`，必须先启用容器。
const AUDIO_CACHE_CHILD_PREFIX: &str = "playback.audio_cache.";

/// 基线配置：默认配置 + 该路径所需的容器前置条件。
fn base_config(path: &str) -> AppConfig {
    let mut base = AppConfig::default();
    if path.starts_with(AUDIO_CACHE_CHILD_PREFIX) {
        base.playback.audio_cache = Some(probe_audio_cache());
    }
    base
}

fn probe_identity_mapping() -> crate::features::identity::IdentityMapping {
    crate::features::identity::IdentityMapping {
        remark: "probe".to_string(),
        id: uuid::Uuid::from_u128(42),
        role: crate::features::identity::IdentityRole::Friend,
        real_nickname: String::new(),
        display_name: "探针身份".to_string(),
        remark_from_chatbridge: false,
        accounts: Vec::new(),
    }
}

fn probe_workflow_definition() -> crate::features::custom_workflow::CustomWorkflowDefinition {
    crate::features::custom_workflow::CustomWorkflowDefinition {
        enabled: true,
        name: "probe".to_string(),
        commands: vec!["@探针".to_string()],
        allow_args: false,
        message_types: Vec::new(),
        confirm_before_run: false,
        confirm_message: String::new(),
        confirm_message_types: Vec::new(),
        confirm_timeout_ms: None,
        confirm_poll_ms: None,
        steps: Vec::new(),
        success_message: String::new(),
    }
}

/// 全量路径表（399 条，顺序与 `config_sections()` 一致）。
fn cases() -> Vec<PathCase> {
    vec![
        case(
            "visual.name",
            |c| {
                c.visual.name = "probe-value".to_string();
            },
            |c| debug_of(&c.visual.name),
        ),
        case(
            "command_access.overrides",
            |c| {
                c.command_access.overrides.insert(
                    "@QQ点歌".to_string(),
                    crate::features::command_access::CommandPolicy {
                        enabled: false,
                        minimum_role: crate::features::command_access::CommandPermission::Admin,
                    },
                );
            },
            |c| format!("{:?}", c.command_access.overrides.get("@QQ点歌")),
        ),
        case(
            "command_access.default_song_source",
            |c| {
                c.command_access.default_song_source = Some("netease".to_string());
            },
            |c| debug_of(&c.command_access.default_song_source),
        ),
        case(
            "chat_source.enabled",
            |c| {
                c.chat_source.enabled = !c.chat_source.enabled;
            },
            |c| debug_of(&c.chat_source.enabled),
        ),
        case(
            "chat_source.url",
            |c| {
                c.chat_source.url = "probe-value".to_string();
            },
            |c| debug_of(&c.chat_source.url),
        ),
        case(
            "chat_source.token",
            |c| {
                c.chat_source.token = "probe-secret".to_string();
            },
            |c| debug_of(&c.chat_source.token),
        ),
        case(
            "chat_source.reconnect_ms",
            |c| {
                c.chat_source.reconnect_ms += 1;
            },
            |c| debug_of(&c.chat_source.reconnect_ms),
        ),
        case(
            "chat_source.lease_timeout_ms",
            |c| {
                c.chat_source.lease_timeout_ms += 1;
            },
            |c| debug_of(&c.chat_source.lease_timeout_ms),
        ),
        case(
            "chat_source.auto_start",
            |c| {
                c.chat_source.auto_start = !c.chat_source.auto_start;
            },
            |c| debug_of(&c.chat_source.auto_start),
        ),
        case(
            "chat_source.executable",
            |c| {
                c.chat_source.executable = "probe-value".to_string();
            },
            |c| debug_of(&c.chat_source.executable),
        ),
        case(
            "chat_source.disconnect_action",
            |c| {
                c.chat_source.disconnect_action =
                    crate::config::ChatSourceDisconnectAction::RestartGame;
            },
            |c| debug_of(&c.chat_source.disconnect_action),
        ),
        case(
            "chat_source.self_uid",
            |c| {
                c.chat_source.self_uid += 1;
            },
            |c| debug_of(&c.chat_source.self_uid),
        ),
        case(
            "window.target_process",
            |c| {
                c.window.target_process = "probe-value".to_string();
            },
            |c| debug_of(&c.window.target_process),
        ),
        case(
            "window.content_width",
            |c| {
                c.window.content_width += 1;
            },
            |c| debug_of(&c.window.content_width),
        ),
        case(
            "window.content_height",
            |c| {
                c.window.content_height += 1;
            },
            |c| debug_of(&c.window.content_height),
        ),
        case(
            "window.auto_activate_window",
            |c| {
                c.window.auto_activate_window = !c.window.auto_activate_window;
            },
            |c| debug_of(&c.window.auto_activate_window),
        ),
        case(
            "window.capture_method",
            |c| {
                c.window.capture_method =
                    serde_json::from_str("\"desktop\"").expect("capture_method 变体");
            },
            |c| debug_of(&c.window.capture_method),
        ),
        case(
            "window.focus_point",
            |c| {
                c.window.focus_point = crate::config::PointConfig { x: 7, y: 9 };
            },
            |c| debug_of(&c.window.focus_point),
        ),
        case(
            "screen.expected_width",
            |c| {
                c.screen.expected_width += 1;
            },
            |c| debug_of(&c.screen.expected_width),
        ),
        case(
            "screen.expected_height",
            |c| {
                c.screen.expected_height += 1;
            },
            |c| debug_of(&c.screen.expected_height),
        ),
        case(
            "screen.warn_on_size_mismatch",
            |c| {
                c.screen.warn_on_size_mismatch = !c.screen.warn_on_size_mismatch;
            },
            |c| debug_of(&c.screen.warn_on_size_mismatch),
        ),
        case(
            "screen.chat_rect",
            |c| {
                c.screen.chat_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.chat_rect),
        ),
        case(
            "screen.friend_rect",
            |c| {
                c.screen.friend_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.friend_rect),
        ),
        case(
            "screen.secondary_back_rect",
            |c| {
                c.screen.secondary_back_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.secondary_back_rect),
        ),
        case(
            "screen.secondary_hall_rect",
            |c| {
                c.screen.secondary_hall_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.secondary_hall_rect),
        ),
        case(
            "screen.secondary_chat_submit_rect",
            |c| {
                c.screen.secondary_chat_submit_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.secondary_chat_submit_rect),
        ),
        case(
            "screen.main_menu_rect",
            |c| {
                c.screen.main_menu_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.main_menu_rect),
        ),
        case(
            "screen.hall_bar_rect",
            |c| {
                c.screen.hall_bar_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.hall_bar_rect),
        ),
        case(
            "screen.qianxing_f2_invite_rect",
            |c| {
                c.screen.qianxing_f2_invite_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.qianxing_f2_invite_rect),
        ),
        case(
            "screen.world_wish_rect",
            |c| {
                c.screen.world_wish_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.world_wish_rect),
        ),
        case(
            "screen.hall_name_rect",
            |c| {
                c.screen.hall_name_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.hall_name_rect),
        ),
        case(
            "screen.hall_member_count_rect",
            |c| {
                c.screen.hall_member_count_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.hall_member_count_rect),
        ),
        case(
            "screen.hall_time_rect",
            |c| {
                c.screen.hall_time_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.hall_time_rect),
        ),
        case(
            "screen.hall_member_list_rect",
            |c| {
                c.screen.hall_member_list_rect = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.screen.hall_member_list_rect),
        ),
        case(
            "stability.default_count",
            |c| {
                c.stability.default_count += 1;
            },
            |c| debug_of(&c.stability.default_count),
        ),
        case(
            "stability.ui_state_count",
            |c| {
                c.stability.ui_state_count += 1;
            },
            |c| debug_of(&c.stability.ui_state_count),
        ),
        case(
            "stability.secondary_hall_count",
            |c| {
                c.stability.secondary_hall_count += 1;
            },
            |c| debug_of(&c.stability.secondary_hall_count),
        ),
        case(
            "timing.watchdog_restart_ms",
            |c| {
                c.timing.watchdog_restart_ms += 1;
            },
            |c| debug_of(&c.timing.watchdog_restart_ms),
        ),
        case(
            "timing.loop_idle_ms",
            |c| {
                c.timing.loop_idle_ms += 1;
            },
            |c| debug_of(&c.timing.loop_idle_ms),
        ),
        case(
            "timing.chat_scan.fallback_ms",
            |c| {
                c.timing.chat_scan.fallback_ms += 1;
            },
            |c| debug_of(&c.timing.chat_scan.fallback_ms),
        ),
        case(
            "timing.chat_scan.change_debounce_ms",
            |c| {
                c.timing.chat_scan.change_debounce_ms += 1;
            },
            |c| debug_of(&c.timing.chat_scan.change_debounce_ms),
        ),
        case(
            "timing.chat_scan.change_cooldown_ms",
            |c| {
                c.timing.chat_scan.change_cooldown_ms += 1;
            },
            |c| debug_of(&c.timing.chat_scan.change_cooldown_ms),
        ),
        case(
            "timing.command.ui_timeout_ms",
            |c| {
                c.timing.command.ui_timeout_ms += 1;
            },
            |c| debug_of(&c.timing.command.ui_timeout_ms),
        ),
        case(
            "timing.command.return_retry_ms",
            |c| {
                c.timing.command.return_retry_ms += 1;
            },
            |c| debug_of(&c.timing.command.return_retry_ms),
        ),
        case(
            "timing.command.post_settle_ms",
            |c| {
                c.timing.command.post_settle_ms += 1;
            },
            |c| debug_of(&c.timing.command.post_settle_ms),
        ),
        case(
            "timing.command.help_batch_ms",
            |c| {
                c.timing.command.help_batch_ms += 1;
            },
            |c| debug_of(&c.timing.command.help_batch_ms),
        ),
        case(
            "timing.input.after_activate_ms",
            |c| {
                c.timing.input.after_activate_ms += 1;
            },
            |c| debug_of(&c.timing.input.after_activate_ms),
        ),
        case(
            "timing.input.focus_ms",
            |c| {
                c.timing.input.focus_ms += 1;
            },
            |c| debug_of(&c.timing.input.focus_ms),
        ),
        case(
            "timing.input.open_chat_ms",
            |c| {
                c.timing.input.open_chat_ms += 1;
            },
            |c| debug_of(&c.timing.input.open_chat_ms),
        ),
        case(
            "timing.input.click_ms",
            |c| {
                c.timing.input.click_ms += 1;
            },
            |c| debug_of(&c.timing.input.click_ms),
        ),
        case(
            "timing.input.text_ms",
            |c| {
                c.timing.input.text_ms += 1;
            },
            |c| debug_of(&c.timing.input.text_ms),
        ),
        case(
            "timing.input.send_ms",
            |c| {
                c.timing.input.send_ms += 1;
            },
            |c| debug_of(&c.timing.input.send_ms),
        ),
        case(
            "timing.workflow.default_timeout_ms",
            |c| {
                c.timing.workflow.default_timeout_ms += 1;
            },
            |c| debug_of(&c.timing.workflow.default_timeout_ms),
        ),
        case(
            "timing.workflow.default_poll_ms",
            |c| {
                c.timing.workflow.default_poll_ms += 1;
            },
            |c| debug_of(&c.timing.workflow.default_poll_ms),
        ),
        case(
            "timing.workflow.default_step_wait_ms",
            |c| {
                c.timing.workflow.default_step_wait_ms += 1;
            },
            |c| debug_of(&c.timing.workflow.default_step_wait_ms),
        ),
        case(
            "timing.hall.page_settle_ms",
            |c| {
                c.timing.hall.page_settle_ms += 1;
            },
            |c| debug_of(&c.timing.hall.page_settle_ms),
        ),
        case(
            "timing.hall.ocr_sample_interval_ms",
            |c| {
                c.timing.hall.ocr_sample_interval_ms += 1;
            },
            |c| debug_of(&c.timing.hall.ocr_sample_interval_ms),
        ),
        case(
            "timing.invite.open_chat_ms",
            |c| {
                c.timing.invite.open_chat_ms += 1;
            },
            |c| debug_of(&c.timing.invite.open_chat_ms),
        ),
        case(
            "timing.invite.step_ms",
            |c| {
                c.timing.invite.step_ms += 1;
            },
            |c| debug_of(&c.timing.invite.step_ms),
        ),
        case(
            "timing.invite.confirm_timeout_ms",
            |c| {
                c.timing.invite.confirm_timeout_ms += 1;
            },
            |c| debug_of(&c.timing.invite.confirm_timeout_ms),
        ),
        case(
            "timing.invite.confirm_poll_ms",
            |c| {
                c.timing.invite.confirm_poll_ms += 1;
            },
            |c| debug_of(&c.timing.invite.confirm_poll_ms),
        ),
        case(
            "timing.moderation.vote_timeout_ms",
            |c| {
                c.timing.moderation.vote_timeout_ms += 1;
            },
            |c| debug_of(&c.timing.moderation.vote_timeout_ms),
        ),
        case(
            "timing.moderation.vote_poll_ms",
            |c| {
                c.timing.moderation.vote_poll_ms += 1;
            },
            |c| debug_of(&c.timing.moderation.vote_poll_ms),
        ),
        case(
            "timing.moderation.search_result_timeout_ms",
            |c| {
                c.timing.moderation.search_result_timeout_ms += 1;
            },
            |c| debug_of(&c.timing.moderation.search_result_timeout_ms),
        ),
        case(
            "timing.moderation.confirm_wait_ms",
            |c| {
                c.timing.moderation.confirm_wait_ms += 1;
            },
            |c| debug_of(&c.timing.moderation.confirm_wait_ms),
        ),
        case(
            "timing.playback.status_poll_ms",
            |c| {
                c.timing.playback.status_poll_ms += 1;
            },
            |c| debug_of(&c.timing.playback.status_poll_ms),
        ),
        case(
            "timing.playback.monitor_tick_ms",
            |c| {
                c.timing.playback.monitor_tick_ms += 1;
            },
            |c| debug_of(&c.timing.playback.monitor_tick_ms),
        ),
        case(
            "timing.playback.monitor_status_ms",
            |c| {
                c.timing.playback.monitor_status_ms += 1;
            },
            |c| debug_of(&c.timing.playback.monitor_status_ms),
        ),
        case(
            "timing.playback.lyrics_lead_seconds",
            |c| {
                c.timing.playback.lyrics_lead_seconds += 0.01;
            },
            |c| debug_of(&c.timing.playback.lyrics_lead_seconds),
        ),
        case(
            "timing.playback.uri_stable_samples",
            |c| {
                c.timing.playback.uri_stable_samples += 1;
            },
            |c| debug_of(&c.timing.playback.uri_stable_samples),
        ),
        case(
            "timing.playback.transport_stable_samples",
            |c| {
                c.timing.playback.transport_stable_samples += 1;
            },
            |c| debug_of(&c.timing.playback.transport_stable_samples),
        ),
        case(
            "timing.playback.stale_timeout_ms",
            |c| {
                c.timing.playback.stale_timeout_ms += 1;
            },
            |c| debug_of(&c.timing.playback.stale_timeout_ms),
        ),
        case(
            "timing.decision.timeout_ms",
            |c| {
                c.timing.decision.timeout_ms += 1;
            },
            |c| debug_of(&c.timing.decision.timeout_ms),
        ),
        case(
            "timing.decision.poll_ms",
            |c| {
                c.timing.decision.poll_ms += 1;
            },
            |c| debug_of(&c.timing.decision.poll_ms),
        ),
        case(
            "timing.external.volume_smooth_step_ms",
            |c| {
                c.timing.external.volume_smooth_step_ms += 1;
            },
            |c| debug_of(&c.timing.external.volume_smooth_step_ms),
        ),
        case(
            "timing.external.ai_request_timeout_ms",
            |c| {
                c.timing.external.ai_request_timeout_ms += 1;
            },
            |c| debug_of(&c.timing.external.ai_request_timeout_ms),
        ),
        case(
            "ocr.det_model",
            |c| {
                c.ocr.det_model = Some(std::path::PathBuf::from("probe/override.bin"));
            },
            |c| debug_of(&c.ocr.det_model),
        ),
        case(
            "ocr.rec_model",
            |c| {
                c.ocr.rec_model = Some(std::path::PathBuf::from("probe/override.bin"));
            },
            |c| debug_of(&c.ocr.rec_model),
        ),
        case(
            "ocr.charset",
            |c| {
                c.ocr.charset = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.ocr.charset),
        ),
        case(
            "ocr.min_confidence",
            |c| {
                c.ocr.min_confidence += 0.01;
            },
            |c| debug_of(&c.ocr.min_confidence),
        ),
        case(
            "ocr.threads",
            |c| {
                c.ocr.threads += 1;
            },
            |c| debug_of(&c.ocr.threads),
        ),
        case(
            "ocr.request_timeout_ms",
            |c| {
                c.ocr.request_timeout_ms += 1;
            },
            |c| debug_of(&c.ocr.request_timeout_ms),
        ),
        case(
            "ocr.shutdown_timeout_ms",
            |c| {
                c.ocr.shutdown_timeout_ms += 1;
            },
            |c| debug_of(&c.ocr.shutdown_timeout_ms),
        ),
        case(
            "ocr.backend_priority",
            |c| {
                c.ocr.backend_priority = vec!["probe".to_string()];
            },
            |c| debug_of(&c.ocr.backend_priority),
        ),
        case(
            "ocr.openvino.det_model",
            |c| {
                c.ocr.openvino.det_model = Some(std::path::PathBuf::from("probe/override.bin"));
            },
            |c| debug_of(&c.ocr.openvino.det_model),
        ),
        case(
            "ocr.openvino.det_weights",
            |c| {
                c.ocr.openvino.det_weights = Some(std::path::PathBuf::from("probe/override.bin"));
            },
            |c| debug_of(&c.ocr.openvino.det_weights),
        ),
        case(
            "ocr.openvino.rec_model",
            |c| {
                c.ocr.openvino.rec_model = Some(std::path::PathBuf::from("probe/override.bin"));
            },
            |c| debug_of(&c.ocr.openvino.rec_model),
        ),
        case(
            "ocr.openvino.rec_weights",
            |c| {
                c.ocr.openvino.rec_weights = Some(std::path::PathBuf::from("probe/override.bin"));
            },
            |c| debug_of(&c.ocr.openvino.rec_weights),
        ),
        case(
            "ocr.openvino.rec_fixed_width",
            |c| {
                c.ocr.openvino.rec_fixed_width = Some(960);
            },
            |c| debug_of(&c.ocr.openvino.rec_fixed_width),
        ),
        case(
            "ocr.openvino.device",
            |c| {
                c.ocr.openvino.device = "probe-value".to_string();
            },
            |c| debug_of(&c.ocr.openvino.device),
        ),
        case(
            "ocr.openvino.cache_dir",
            |c| {
                c.ocr.openvino.cache_dir = Some(std::path::PathBuf::from("probe/override.bin"));
            },
            |c| debug_of(&c.ocr.openvino.cache_dir),
        ),
        case(
            "ocr.det_max_side_len",
            |c| {
                c.ocr.det_max_side_len += 1;
            },
            |c| debug_of(&c.ocr.det_max_side_len),
        ),
        case(
            "ocr.det_score_threshold",
            |c| {
                c.ocr.det_score_threshold += 0.01;
            },
            |c| debug_of(&c.ocr.det_score_threshold),
        ),
        case(
            "ocr.det_unclip_ratio",
            |c| {
                c.ocr.det_unclip_ratio += 0.01;
            },
            |c| debug_of(&c.ocr.det_unclip_ratio),
        ),
        case(
            "ocr.det_min_area",
            |c| {
                c.ocr.det_min_area += 1;
            },
            |c| debug_of(&c.ocr.det_min_area),
        ),
        case(
            "ocr.det_box_border",
            |c| {
                c.ocr.det_box_border += 1;
            },
            |c| debug_of(&c.ocr.det_box_border),
        ),
        case(
            "ocr.change_mean_threshold",
            |c| {
                c.ocr.change_mean_threshold += 0.01;
            },
            |c| debug_of(&c.ocr.change_mean_threshold),
        ),
        case(
            "ocr.change_pixel_threshold",
            |c| {
                c.ocr.change_pixel_threshold += 0.01;
            },
            |c| debug_of(&c.ocr.change_pixel_threshold),
        ),
        case(
            "ocr.text_left_gap",
            |c| {
                c.ocr.text_left_gap += 1;
            },
            |c| debug_of(&c.ocr.text_left_gap),
        ),
        case(
            "ocr.block_top_padding",
            |c| {
                c.ocr.block_top_padding += 1;
            },
            |c| debug_of(&c.ocr.block_top_padding),
        ),
        case(
            "ocr.block_bottom_padding",
            |c| {
                c.ocr.block_bottom_padding += 1;
            },
            |c| debug_of(&c.ocr.block_bottom_padding),
        ),
        case(
            "ocr.max_block_height",
            |c| {
                c.ocr.max_block_height += 1;
            },
            |c| debug_of(&c.ocr.max_block_height),
        ),
        case(
            "ocr.same_line_y_tolerance",
            |c| {
                c.ocr.same_line_y_tolerance += 1;
            },
            |c| debug_of(&c.ocr.same_line_y_tolerance),
        ),
        case(
            "ocr.marker_search_width",
            |c| {
                c.ocr.marker_search_width += 1;
            },
            |c| debug_of(&c.ocr.marker_search_width),
        ),
        case(
            "ocr.marker_dedupe_min_x",
            |c| {
                c.ocr.marker_dedupe_min_x += 1;
            },
            |c| debug_of(&c.ocr.marker_dedupe_min_x),
        ),
        case(
            "ocr.marker_dedupe_min_y",
            |c| {
                c.ocr.marker_dedupe_min_y += 1;
            },
            |c| debug_of(&c.ocr.marker_dedupe_min_y),
        ),
        case(
            "ocr.marker_dedupe_x",
            |c| {
                c.ocr.marker_dedupe_x += 1;
            },
            |c| debug_of(&c.ocr.marker_dedupe_x),
        ),
        case(
            "ocr.marker_dedupe_y",
            |c| {
                c.ocr.marker_dedupe_y += 1;
            },
            |c| debug_of(&c.ocr.marker_dedupe_y),
        ),
        case(
            "ocr.next_marker_min_gap",
            |c| {
                c.ocr.next_marker_min_gap += 1;
            },
            |c| debug_of(&c.ocr.next_marker_min_gap),
        ),
        case(
            "ocr.right_padding",
            |c| {
                c.ocr.right_padding += 1;
            },
            |c| debug_of(&c.ocr.right_padding),
        ),
        case(
            "ocr.batch_recognize",
            |c| {
                c.ocr.batch_recognize = !c.ocr.batch_recognize;
            },
            |c| debug_of(&c.ocr.batch_recognize),
        ),
        case(
            "templates.panel_downsample_factors",
            |c| {
                c.templates
                    .panel_downsample_factors
                    .insert("probe".to_string(), 7);
            },
            |c| debug_of(&c.templates.panel_downsample_factors),
        ),
        case(
            "templates.blue_marker",
            |c| {
                c.templates.blue_marker = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.blue_marker),
        ),
        case(
            "templates.yellow_marker",
            |c| {
                c.templates.yellow_marker = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.yellow_marker),
        ),
        case(
            "templates.pink_marker",
            |c| {
                c.templates.pink_marker = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.pink_marker),
        ),
        case(
            "templates.secondary_back",
            |c| {
                c.templates.secondary_back = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.secondary_back),
        ),
        case(
            "templates.secondary_hall",
            |c| {
                c.templates.secondary_hall = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.secondary_hall),
        ),
        case(
            "templates.secondary_chat_submit",
            |c| {
                c.templates.secondary_chat_submit = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.secondary_chat_submit),
        ),
        case(
            "templates.qianxing_f2_hall_list",
            |c| {
                c.templates.qianxing_f2_hall_list = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.qianxing_f2_hall_list),
        ),
        case(
            "templates.qianxing_f2_invite_player",
            |c| {
                c.templates.qianxing_f2_invite_player =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.qianxing_f2_invite_player),
        ),
        case(
            "templates.qianxing_exit_menu",
            |c| {
                c.templates.qianxing_exit_menu = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.qianxing_exit_menu),
        ),
        case(
            "templates.qianxing_map_label",
            |c| {
                c.templates.qianxing_map_label = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.qianxing_map_label),
        ),
        case(
            "templates.qianxing_profile_page_wishlist",
            |c| {
                c.templates.qianxing_profile_page_wishlist =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.qianxing_profile_page_wishlist),
        ),
        case(
            "templates.qianxing_profile_page_wardrobe",
            |c| {
                c.templates.qianxing_profile_page_wardrobe =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.qianxing_profile_page_wardrobe),
        ),
        case(
            "templates.teyvat_profile_adventure_rank",
            |c| {
                c.templates.teyvat_profile_adventure_rank =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.teyvat_profile_adventure_rank),
        ),
        case(
            "templates.teyvat_profile_world_level",
            |c| {
                c.templates.teyvat_profile_world_level =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.teyvat_profile_world_level),
        ),
        case(
            "templates.teyvat_profile_add_friend",
            |c| {
                c.templates.teyvat_profile_add_friend =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.teyvat_profile_add_friend),
        ),
        case(
            "templates.qianxing_profile_domain_level",
            |c| {
                c.templates.qianxing_profile_domain_level =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.qianxing_profile_domain_level),
        ),
        case(
            "templates.qianxing_profile_add_friend",
            |c| {
                c.templates.qianxing_profile_add_friend =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.qianxing_profile_add_friend),
        ),
        case(
            "templates.qianxing_profile_view_profile",
            |c| {
                c.templates.qianxing_profile_view_profile =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.qianxing_profile_view_profile),
        ),
        case(
            "templates.main_menu",
            |c| {
                c.templates.main_menu = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.main_menu),
        ),
        case(
            "templates.world_wish",
            |c| {
                c.templates.world_wish = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.world_wish),
        ),
        case(
            "templates.invite_view_star",
            |c| {
                c.templates.invite_view_star = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.invite_view_star),
        ),
        case(
            "templates.invite_goto_hall",
            |c| {
                c.templates.invite_goto_hall = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.invite_goto_hall),
        ),
        case(
            "templates.invite_enter_hall",
            |c| {
                c.templates.invite_enter_hall = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.invite_enter_hall),
        ),
        case(
            "templates.friend_panel",
            |c| {
                c.templates.friend_panel = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.friend_panel),
        ),
        case(
            "templates.friend_search_panel",
            |c| {
                c.templates.friend_search_panel = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.friend_search_panel),
        ),
        case(
            "templates.friend_more_settings",
            |c| {
                c.templates.friend_more_settings = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.friend_more_settings),
        ),
        case(
            "templates.friend_block_chat",
            |c| {
                c.templates.friend_block_chat = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.friend_block_chat),
        ),
        case(
            "templates.friend_blacklist",
            |c| {
                c.templates.friend_blacklist = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.friend_blacklist),
        ),
        case(
            "templates.confirm_button",
            |c| {
                c.templates.confirm_button = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.confirm_button),
        ),
        case(
            "templates.confirm_message_random_hall",
            |c| {
                c.templates.confirm_message_random_hall =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.confirm_message_random_hall),
        ),
        case(
            "templates.confirm_message_return_teyvat",
            |c| {
                c.templates.confirm_message_return_teyvat =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.confirm_message_return_teyvat),
        ),
        case(
            "templates.confirm_message_block",
            |c| {
                c.templates.confirm_message_block = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.templates.confirm_message_block),
        ),
        case(
            "templates.marker_threshold",
            |c| {
                c.templates.marker_threshold += 0.01;
            },
            |c| debug_of(&c.templates.marker_threshold),
        ),
        case(
            "output.send_enabled",
            |c| {
                c.output.send_enabled = !c.output.send_enabled;
            },
            |c| debug_of(&c.output.send_enabled),
        ),
        case(
            "output.focus_point",
            |c| {
                c.output.focus_point = crate::config::PointConfig { x: 7, y: 9 };
            },
            |c| debug_of(&c.output.focus_point),
        ),
        case(
            "output.chat_click_2",
            |c| {
                c.output.chat_click_2 = crate::config::PointConfig { x: 7, y: 9 };
            },
            |c| debug_of(&c.output.chat_click_2),
        ),
        case(
            "moderation.stable_vote_samples",
            |c| {
                c.moderation.stable_vote_samples += 1;
            },
            |c| debug_of(&c.moderation.stable_vote_samples),
        ),
        case(
            "moderation.required_vote_margin",
            |c| {
                c.moderation.required_vote_margin += 1;
            },
            |c| debug_of(&c.moderation.required_vote_margin),
        ),
        case(
            "moderation.friend_panel_region",
            |c| {
                c.moderation.friend_panel_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.moderation.friend_panel_region),
        ),
        case(
            "moderation.search_panel_region",
            |c| {
                c.moderation.search_panel_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.moderation.search_panel_region),
        ),
        case(
            "moderation.search_input_point",
            |c| {
                c.moderation.search_input_point = crate::config::PointConfig { x: 7, y: 9 };
            },
            |c| debug_of(&c.moderation.search_input_point),
        ),
        case(
            "moderation.search_button_point",
            |c| {
                c.moderation.search_button_point = crate::config::PointConfig { x: 7, y: 9 };
            },
            |c| debug_of(&c.moderation.search_button_point),
        ),
        case(
            "moderation.more_settings_region",
            |c| {
                c.moderation.more_settings_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.moderation.more_settings_region),
        ),
        case(
            "moderation.block_chat_region",
            |c| {
                c.moderation.block_chat_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.moderation.block_chat_region),
        ),
        case(
            "moderation.blacklist_region",
            |c| {
                c.moderation.blacklist_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.moderation.blacklist_region),
        ),
        case(
            "moderation.confirm_button_region",
            |c| {
                c.moderation.confirm_button_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.moderation.confirm_button_region),
        ),
        case(
            "moderation.confirm_message_region",
            |c| {
                c.moderation.confirm_message_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.moderation.confirm_message_region),
        ),
        case(
            "playback.credential_directory",
            |c| {
                c.playback.credential_directory = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.playback.credential_directory),
        ),
        case(
            "playback.login_helper_executable",
            |c| {
                c.playback.login_helper_executable = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.playback.login_helper_executable),
        ),
        case(
            "playback.login_timeout_ms",
            |c| {
                c.playback.login_timeout_ms += 1;
            },
            |c| debug_of(&c.playback.login_timeout_ms),
        ),
        case(
            "playback.audio_cache",
            |c| {
                c.playback.audio_cache = Some(probe_audio_cache());
            },
            |c| debug_of(&c.playback.audio_cache),
        ),
        case(
            "playback.audio_cache.enabled",
            |c| {
                let cache = c.playback.audio_cache.as_mut().expect("基线已启用音频缓存");
                cache.enabled = !cache.enabled;
            },
            |c| debug_of(&c.playback.audio_cache.as_ref().map(|cache| cache.enabled)),
        ),
        case(
            "playback.audio_cache.directory",
            |c| {
                let cache = c.playback.audio_cache.as_mut().expect("基线已启用音频缓存");
                cache.directory = std::path::PathBuf::from("probe/cache");
            },
            |c| {
                debug_of(
                    &c.playback
                        .audio_cache
                        .as_ref()
                        .map(|cache| cache.directory.clone()),
                )
            },
        ),
        case(
            "playback.audio_cache.max_bytes_mb",
            |c| {
                let cache = c.playback.audio_cache.as_mut().expect("基线已启用音频缓存");
                cache.max_bytes_mb += 1;
            },
            |c| {
                debug_of(
                    &c.playback
                        .audio_cache
                        .as_ref()
                        .map(|cache| cache.max_bytes_mb),
                )
            },
        ),
        case(
            "playback.audio_cache.max_concurrent_downloads",
            |c| {
                let cache = c.playback.audio_cache.as_mut().expect("基线已启用音频缓存");
                cache.max_concurrent_downloads += 1;
            },
            |c| {
                debug_of(
                    &c.playback
                        .audio_cache
                        .as_ref()
                        .map(|cache| cache.max_concurrent_downloads),
                )
            },
        ),
        case(
            "playback.audio_cache.request_timeout_ms",
            |c| {
                let cache = c.playback.audio_cache.as_mut().expect("基线已启用音频缓存");
                cache.request_timeout_ms += 1;
            },
            |c| {
                debug_of(
                    &c.playback
                        .audio_cache
                        .as_ref()
                        .map(|cache| cache.request_timeout_ms),
                )
            },
        ),
        case(
            "playback.audio_cache.seek_wait_timeout_ms",
            |c| {
                let cache = c.playback.audio_cache.as_mut().expect("基线已启用音频缓存");
                cache.seek_wait_timeout_ms += 1;
            },
            |c| {
                debug_of(
                    &c.playback
                        .audio_cache
                        .as_ref()
                        .map(|cache| cache.seek_wait_timeout_ms),
                )
            },
        ),
        case(
            "http.enabled",
            |c| {
                c.http.enabled = !c.http.enabled;
            },
            |c| debug_of(&c.http.enabled),
        ),
        case(
            "http.host",
            |c| {
                c.http.host = "probe-value".to_string();
            },
            |c| debug_of(&c.http.host),
        ),
        case(
            "http.port",
            |c| {
                c.http.port += 1;
            },
            |c| debug_of(&c.http.port),
        ),
        case(
            "http.access_token",
            |c| {
                c.http.access_token = "probe-secret".to_string();
            },
            |c| debug_of(&c.http.access_token),
        ),
        case(
            "logging.dir",
            |c| {
                c.logging.dir = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.logging.dir),
        ),
        case(
            "logging.level",
            |c| {
                c.logging.level = "trace".to_string();
            },
            |c| debug_of(&c.logging.level),
        ),
        case(
            "logging.rotate_daily",
            |c| {
                c.logging.rotate_daily = !c.logging.rotate_daily;
            },
            |c| debug_of(&c.logging.rotate_daily),
        ),
        case(
            "logging.retain_days",
            |c| {
                c.logging.retain_days += 1;
            },
            |c| debug_of(&c.logging.retain_days),
        ),
        case(
            "tui.enabled",
            |c| {
                c.tui.enabled = !c.tui.enabled;
            },
            |c| debug_of(&c.tui.enabled),
        ),
        case(
            "tui.refresh_ms",
            |c| {
                c.tui.refresh_ms += 1;
            },
            |c| debug_of(&c.tui.refresh_ms),
        ),
        case(
            "tui.log_lines",
            |c| {
                c.tui.log_lines += 1;
            },
            |c| debug_of(&c.tui.log_lines),
        ),
        case(
            "state.hall_state_path",
            |c| {
                c.state.hall_state_path = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.state.hall_state_path),
        ),
        case(
            "state.executed_commands_log_path",
            |c| {
                c.state.executed_commands_log_path = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.state.executed_commands_log_path),
        ),
        case(
            "diagnostics.ui_frames",
            |c| {
                c.diagnostics.ui_frames = !c.diagnostics.ui_frames;
            },
            |c| debug_of(&c.diagnostics.ui_frames),
        ),
        case(
            "queue.max_size",
            |c| {
                c.queue.max_size += 1;
            },
            |c| debug_of(&c.queue.max_size),
        ),
        case(
            "queue.pool_max_size",
            |c| {
                c.queue.pool_max_size += 1;
            },
            |c| debug_of(&c.queue.pool_max_size),
        ),
        case(
            "queue.protect_current_song_until_finished",
            |c| {
                c.queue.protect_current_song_until_finished =
                    !c.queue.protect_current_song_until_finished;
            },
            |c| debug_of(&c.queue.protect_current_song_until_finished),
        ),
        case(
            "queue.external_playback_protect_after_seconds",
            |c| {
                c.queue.external_playback_protect_after_seconds += 1;
            },
            |c| debug_of(&c.queue.external_playback_protect_after_seconds),
        ),
        case(
            "song_dedup.enabled",
            |c| {
                c.song_dedup.enabled = !c.song_dedup.enabled;
            },
            |c| debug_of(&c.song_dedup.enabled),
        ),
        case(
            "song_dedup.window_seconds",
            |c| {
                c.song_dedup.window_seconds += 1;
            },
            |c| debug_of(&c.song_dedup.window_seconds),
        ),
        case(
            "song_dedup.max_count",
            |c| {
                c.song_dedup.max_count += 1;
            },
            |c| debug_of(&c.song_dedup.max_count),
        ),
        case(
            "song_dedup.console_bypass",
            |c| {
                c.song_dedup.console_bypass = !c.song_dedup.console_bypass;
            },
            |c| debug_of(&c.song_dedup.console_bypass),
        ),
        case(
            "song_dedup.history_path",
            |c| {
                c.song_dedup.history_path = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.song_dedup.history_path),
        ),
        case(
            "idiom_chain.enabled",
            |c| {
                c.idiom_chain.enabled = !c.idiom_chain.enabled;
            },
            |c| debug_of(&c.idiom_chain.enabled),
        ),
        case(
            "idiom_chain.lexicon_path",
            |c| {
                c.idiom_chain.lexicon_path = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.idiom_chain.lexicon_path),
        ),
        case(
            "idiom_chain.history_limit",
            |c| {
                c.idiom_chain.history_limit += 1;
            },
            |c| debug_of(&c.idiom_chain.history_limit),
        ),
        case(
            "idiom_chain.idle_timeout_seconds",
            |c| {
                c.idiom_chain.idle_timeout_seconds += 1;
            },
            |c| debug_of(&c.idiom_chain.idle_timeout_seconds),
        ),
        case(
            "idiom_chain.allow_consecutive_player",
            |c| {
                c.idiom_chain.allow_consecutive_player = !c.idiom_chain.allow_consecutive_player;
            },
            |c| debug_of(&c.idiom_chain.allow_consecutive_player),
        ),
        case(
            "idiom_chain.allow_anyone_stop",
            |c| {
                c.idiom_chain.allow_anyone_stop = !c.idiom_chain.allow_anyone_stop;
            },
            |c| debug_of(&c.idiom_chain.allow_anyone_stop),
        ),
        case(
            "landlord.enabled",
            |c| {
                c.landlord.enabled = !c.landlord.enabled;
            },
            |c| debug_of(&c.landlord.enabled),
        ),
        case(
            "landlord.lobby_timeout_seconds",
            |c| {
                c.landlord.lobby_timeout_seconds += 1;
            },
            |c| debug_of(&c.landlord.lobby_timeout_seconds),
        ),
        case(
            "landlord.turn_timeout_seconds",
            |c| {
                c.landlord.turn_timeout_seconds += 1;
            },
            |c| debug_of(&c.landlord.turn_timeout_seconds),
        ),
        case(
            "landlord.trustee_after_timeouts",
            |c| {
                c.landlord.trustee_after_timeouts += 1;
            },
            |c| debug_of(&c.landlord.trustee_after_timeouts),
        ),
        case(
            "landlord.hand_cooldown_seconds",
            |c| {
                c.landlord.hand_cooldown_seconds += 1;
            },
            |c| debug_of(&c.landlord.hand_cooldown_seconds),
        ),
        case(
            "undercover.enabled",
            |c| {
                c.undercover.enabled = !c.undercover.enabled;
            },
            |c| debug_of(&c.undercover.enabled),
        ),
        case(
            "undercover.word_bank_path",
            |c| {
                c.undercover.word_bank_path = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.undercover.word_bank_path),
        ),
        case(
            "undercover.used_state_path",
            |c| {
                c.undercover.used_state_path = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.undercover.used_state_path),
        ),
        case(
            "undercover.min_players",
            |c| {
                c.undercover.min_players += 1;
            },
            |c| debug_of(&c.undercover.min_players),
        ),
        case(
            "undercover.double_min_players",
            |c| {
                c.undercover.double_min_players += 1;
            },
            |c| debug_of(&c.undercover.double_min_players),
        ),
        case(
            "undercover.max_players",
            |c| {
                c.undercover.max_players += 1;
            },
            |c| debug_of(&c.undercover.max_players),
        ),
        case(
            "undercover.lobby_timeout_seconds",
            |c| {
                c.undercover.lobby_timeout_seconds += 1;
            },
            |c| debug_of(&c.undercover.lobby_timeout_seconds),
        ),
        case(
            "undercover.phase_timeout_seconds",
            |c| {
                c.undercover.phase_timeout_seconds += 1;
            },
            |c| debug_of(&c.undercover.phase_timeout_seconds),
        ),
        case(
            "undercover.progress_interval_seconds",
            |c| {
                c.undercover.progress_interval_seconds += 1;
            },
            |c| debug_of(&c.undercover.progress_interval_seconds),
        ),
        case(
            "undercover.description_max_width",
            |c| {
                c.undercover.description_max_width += 1;
            },
            |c| debug_of(&c.undercover.description_max_width),
        ),
        case(
            "turtle_soup.enabled",
            |c| {
                c.turtle_soup.enabled = !c.turtle_soup.enabled;
            },
            |c| debug_of(&c.turtle_soup.enabled),
        ),
        case(
            "turtle_soup.question_bank_path",
            |c| {
                c.turtle_soup.question_bank_path = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.turtle_soup.question_bank_path),
        ),
        case(
            "turtle_soup.used_state_path",
            |c| {
                c.turtle_soup.used_state_path = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.turtle_soup.used_state_path),
        ),
        case(
            "turtle_soup.idle_timeout_seconds",
            |c| {
                c.turtle_soup.idle_timeout_seconds += 1;
            },
            |c| debug_of(&c.turtle_soup.idle_timeout_seconds),
        ),
        case(
            "turtle_soup.max_session_seconds",
            |c| {
                c.turtle_soup.max_session_seconds += 1;
            },
            |c| debug_of(&c.turtle_soup.max_session_seconds),
        ),
        case(
            "turtle_soup.max_concurrency",
            |c| {
                c.turtle_soup.max_concurrency += 1;
            },
            |c| debug_of(&c.turtle_soup.max_concurrency),
        ),
        case(
            "turtle_soup.max_pending",
            |c| {
                c.turtle_soup.max_pending += 1;
            },
            |c| debug_of(&c.turtle_soup.max_pending),
        ),
        case(
            "turtle_soup.batch_max_parts",
            |c| {
                c.turtle_soup.batch_max_parts += 1;
            },
            |c| debug_of(&c.turtle_soup.batch_max_parts),
        ),
        case(
            "turtle_soup.nickname_stable_count",
            |c| {
                c.turtle_soup.nickname_stable_count += 1;
            },
            |c| debug_of(&c.turtle_soup.nickname_stable_count),
        ),
        case(
            "turtle_soup.content_stable_count",
            |c| {
                c.turtle_soup.content_stable_count += 1;
            },
            |c| debug_of(&c.turtle_soup.content_stable_count),
        ),
        case(
            "turtle_soup.request_timeout_seconds",
            |c| {
                c.turtle_soup.request_timeout_seconds += 1;
            },
            |c| debug_of(&c.turtle_soup.request_timeout_seconds),
        ),
        case(
            "turtle_soup.retry_count",
            |c| {
                c.turtle_soup.retry_count += 1;
            },
            |c| debug_of(&c.turtle_soup.retry_count),
        ),
        case(
            "turtle_soup.retry_delay_ms",
            |c| {
                c.turtle_soup.retry_delay_ms += 1;
            },
            |c| debug_of(&c.turtle_soup.retry_delay_ms),
        ),
        case(
            "turtle_soup.custom_prompt",
            |c| {
                c.turtle_soup.custom_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.turtle_soup.custom_prompt),
        ),
        case(
            "turtle_soup.system_prompt",
            |c| {
                c.turtle_soup.system_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.turtle_soup.system_prompt),
        ),
        case(
            "turtle_soup.review_prompt",
            |c| {
                c.turtle_soup.review_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.turtle_soup.review_prompt),
        ),
        case(
            "turtle_soup.model",
            |c| {
                c.turtle_soup.model = "probe-value".to_string();
            },
            |c| debug_of(&c.turtle_soup.model),
        ),
        case(
            "turtle_soup.reasoning",
            |c| {
                c.turtle_soup.reasoning = crate::config::ai::ReasoningEffort::High;
            },
            |c| debug_of(&c.turtle_soup.reasoning),
        ),
        case(
            "turtle_soup.max_tokens",
            |c| {
                c.turtle_soup.max_tokens += 1;
            },
            |c| debug_of(&c.turtle_soup.max_tokens),
        ),
        case(
            "number_bomb.enabled",
            |c| {
                c.number_bomb.enabled = !c.number_bomb.enabled;
            },
            |c| debug_of(&c.number_bomb.enabled),
        ),
        case(
            "number_bomb.min",
            |c| {
                c.number_bomb.min += 1;
            },
            |c| debug_of(&c.number_bomb.min),
        ),
        case(
            "number_bomb.max",
            |c| {
                c.number_bomb.max += 1;
            },
            |c| debug_of(&c.number_bomb.max),
        ),
        case(
            "number_bomb.turn_timeout_seconds",
            |c| {
                c.number_bomb.turn_timeout_seconds += 1;
            },
            |c| debug_of(&c.number_bomb.turn_timeout_seconds),
        ),
        case(
            "number_bomb.min_players",
            |c| {
                c.number_bomb.min_players += 1;
            },
            |c| debug_of(&c.number_bomb.min_players),
        ),
        case(
            "number_bomb.max_players",
            |c| {
                c.number_bomb.max_players += 1;
            },
            |c| debug_of(&c.number_bomb.max_players),
        ),
        case(
            "number_bomb.extra_range_per_player",
            |c| {
                c.number_bomb.extra_range_per_player += 1;
            },
            |c| debug_of(&c.number_bomb.extra_range_per_player),
        ),
        case(
            "number_bomb.upper_bound_cap",
            |c| {
                c.number_bomb.upper_bound_cap += 1;
            },
            |c| debug_of(&c.number_bomb.upper_bound_cap),
        ),
        case(
            "ai.provider",
            |c| {
                c.ai.provider = "deepseek".to_string();
            },
            |c| debug_of(&c.ai.provider),
        ),
        case(
            "ai.api_key",
            |c| {
                c.ai.api_key = "probe-secret".to_string();
            },
            |c| debug_of(&c.ai.api_key),
        ),
        case(
            "ai.endpoint",
            |c| {
                c.ai.endpoint = "probe-value".to_string();
            },
            |c| debug_of(&c.ai.endpoint),
        ),
        case(
            "ai.responses_endpoint",
            |c| {
                c.ai.responses_endpoint = "probe-value".to_string();
            },
            |c| debug_of(&c.ai.responses_endpoint),
        ),
        case(
            "ai.model",
            |c| {
                c.ai.model = "probe-value".to_string();
            },
            |c| debug_of(&c.ai.model),
        ),
        case(
            "ai.http_proxy",
            |c| {
                c.ai.http_proxy = "probe-value".to_string();
            },
            |c| debug_of(&c.ai.http_proxy),
        ),
        case(
            "ai.wire_api",
            |c| {
                c.ai.wire_api = crate::config::ai::WireApi::ChatCompletions;
            },
            |c| debug_of(&c.ai.wire_api),
        ),
        case(
            "ai.extra_body",
            |c| {
                c.ai.extra_body
                    .insert("probe".to_string(), serde_json::json!({"probe": true}));
            },
            |c| format!("{:?}", c.ai.extra_body.get("probe")),
        ),
        case(
            "ai.reasoning_rules",
            |c| {
                c.ai.reasoning_rules.push(crate::config::ai::ReasoningRule {
                    from: "probe-from".to_string(),
                    to: "probe-to".to_string(),
                    match_type: "exact".to_string(),
                    model: "probe-model".to_string(),
                });
            },
            |c| debug_of(&c.ai.reasoning_rules),
        ),
        case(
            "song_ai.model",
            |c| {
                c.song_ai.model = "probe-value".to_string();
            },
            |c| debug_of(&c.song_ai.model),
        ),
        case(
            "song_ai.reasoning",
            |c| {
                c.song_ai.reasoning = crate::config::ai::ReasoningEffort::High;
            },
            |c| debug_of(&c.song_ai.reasoning),
        ),
        case(
            "song_ai.local_recommend",
            |c| {
                c.song_ai.local_recommend = !c.song_ai.local_recommend;
            },
            |c| debug_of(&c.song_ai.local_recommend),
        ),
        case(
            "song_ai.local_recommend_min_score",
            |c| {
                c.song_ai.local_recommend_min_score += 0.01;
            },
            |c| debug_of(&c.song_ai.local_recommend_min_score),
        ),
        case(
            "song_ai.system_prompt",
            |c| {
                c.song_ai.system_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.song_ai.system_prompt),
        ),
        case(
            "song_ai.recognize_prompt",
            |c| {
                c.song_ai.recognize_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.song_ai.recognize_prompt),
        ),
        case(
            "song_ai.match_prompt",
            |c| {
                c.song_ai.match_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.song_ai.match_prompt),
        ),
        case(
            "song_ai.candidate_pick_prompt",
            |c| {
                c.song_ai.candidate_pick_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.song_ai.candidate_pick_prompt),
        ),
        case(
            "song_review.enabled",
            |c| {
                c.song_review.enabled = !c.song_review.enabled;
            },
            |c| debug_of(&c.song_review.enabled),
        ),
        case(
            "song_review.max_allowed_level",
            |c| {
                c.song_review.max_allowed_level += 1;
            },
            |c| debug_of(&c.song_review.max_allowed_level),
        ),
        case(
            "song_review.failure_policy",
            |c| {
                c.song_review.failure_policy =
                    serde_json::from_str("\"allow\"").expect("failure_policy 变体");
            },
            |c| debug_of(&c.song_review.failure_policy),
        ),
        case(
            "song_review.retry_count",
            |c| {
                c.song_review.retry_count += 1;
            },
            |c| debug_of(&c.song_review.retry_count),
        ),
        case(
            "song_review.retry_delay_ms",
            |c| {
                c.song_review.retry_delay_ms += 1;
            },
            |c| debug_of(&c.song_review.retry_delay_ms),
        ),
        case(
            "song_review.reply_reason_max_chars",
            |c| {
                c.song_review.reply_reason_max_chars += 1;
            },
            |c| debug_of(&c.song_review.reply_reason_max_chars),
        ),
        case(
            "song_review.system_prompt",
            |c| {
                c.song_review.system_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.song_review.system_prompt),
        ),
        case(
            "song_review.review_prompt",
            |c| {
                c.song_review.review_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.song_review.review_prompt),
        ),
        case(
            "song_review.policy_prompt",
            |c| {
                c.song_review.policy_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.song_review.policy_prompt),
        ),
        case(
            "song_review.custom_prompt",
            |c| {
                c.song_review.custom_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.song_review.custom_prompt),
        ),
        case(
            "song_review.model",
            |c| {
                c.song_review.model = "probe-value".to_string();
            },
            |c| debug_of(&c.song_review.model),
        ),
        case(
            "song_review.reasoning",
            |c| {
                c.song_review.reasoning = crate::config::ai::ReasoningEffort::High;
            },
            |c| debug_of(&c.song_review.reasoning),
        ),
        case(
            "song_review.web_search",
            |c| {
                c.song_review.web_search = !c.song_review.web_search;
            },
            |c| debug_of(&c.song_review.web_search),
        ),
        case(
            "song_review.max_tokens",
            |c| {
                c.song_review.max_tokens += 1;
            },
            |c| debug_of(&c.song_review.max_tokens),
        ),
        case(
            "matching.min_song_name_score",
            |c| {
                c.matching.min_song_name_score += 0.01;
            },
            |c| debug_of(&c.matching.min_song_name_score),
        ),
        case(
            "matching.short_chinese_song_max_miss",
            |c| {
                c.matching.short_chinese_song_max_miss += 1;
            },
            |c| debug_of(&c.matching.short_chinese_song_max_miss),
        ),
        case(
            "matching.long_chinese_song_min_score",
            |c| {
                c.matching.long_chinese_song_min_score += 0.01;
            },
            |c| debug_of(&c.matching.long_chinese_song_min_score),
        ),
        case(
            "matching.max_ocr_noise_chars",
            |c| {
                c.matching.max_ocr_noise_chars += 1;
            },
            |c| debug_of(&c.matching.max_ocr_noise_chars),
        ),
        case(
            "matching.enable_fuzzy_singer",
            |c| {
                c.matching.enable_fuzzy_singer = !c.matching.enable_fuzzy_singer;
            },
            |c| debug_of(&c.matching.enable_fuzzy_singer),
        ),
        case(
            "matching.short_chinese_singer_max_miss",
            |c| {
                c.matching.short_chinese_singer_max_miss += 1;
            },
            |c| debug_of(&c.matching.short_chinese_singer_max_miss),
        ),
        case(
            "matching.long_chinese_singer_min_score",
            |c| {
                c.matching.long_chinese_singer_min_score += 0.01;
            },
            |c| debug_of(&c.matching.long_chinese_singer_min_score),
        ),
        case(
            "matching.en_max_edit_fraction",
            |c| {
                c.matching.en_max_edit_fraction += 0.01;
            },
            |c| debug_of(&c.matching.en_max_edit_fraction),
        ),
        case(
            "matching.en_singer_max_edit_fraction",
            |c| {
                c.matching.en_singer_max_edit_fraction += 0.01;
            },
            |c| debug_of(&c.matching.en_singer_max_edit_fraction),
        ),
        case(
            "hotkeys.enabled",
            |c| {
                c.hotkeys.enabled = !c.hotkeys.enabled;
            },
            |c| debug_of(&c.hotkeys.enabled),
        ),
        case(
            "hotkeys.pause_key",
            |c| {
                c.hotkeys.pause_key = "probe-value".to_string();
            },
            |c| debug_of(&c.hotkeys.pause_key),
        ),
        case(
            "hotkeys.exit_key",
            |c| {
                c.hotkeys.exit_key = "probe-value".to_string();
            },
            |c| debug_of(&c.hotkeys.exit_key),
        ),
        case(
            "startup.enabled",
            |c| {
                c.startup.enabled = !c.startup.enabled;
            },
            |c| debug_of(&c.startup.enabled),
        ),
        case(
            "startup.launch_game",
            |c| {
                c.startup.launch_game = !c.startup.launch_game;
            },
            |c| debug_of(&c.startup.launch_game),
        ),
        case(
            "startup.enter_game",
            |c| {
                c.startup.enter_game = !c.startup.enter_game;
            },
            |c| debug_of(&c.startup.enter_game),
        ),
        case(
            "startup.enter_wonderland",
            |c| {
                c.startup.enter_wonderland = !c.startup.enter_wonderland;
            },
            |c| debug_of(&c.startup.enter_wonderland),
        ),
        case(
            "startup.exe_path",
            |c| {
                c.startup.exe_path = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.startup.exe_path),
        ),
        case(
            "startup.game_args",
            |c| {
                c.startup.game_args = "probe-value".to_string();
            },
            |c| debug_of(&c.startup.game_args),
        ),
        case(
            "startup.launch_wait_ms",
            |c| {
                c.startup.launch_wait_ms += 1;
            },
            |c| debug_of(&c.startup.launch_wait_ms),
        ),
        case(
            "startup.launch_retries",
            |c| {
                c.startup.launch_retries += 1;
            },
            |c| debug_of(&c.startup.launch_retries),
        ),
        case(
            "startup.enter_game_timeout_ms",
            |c| {
                c.startup.enter_game_timeout_ms += 1;
            },
            |c| debug_of(&c.startup.enter_game_timeout_ms),
        ),
        case(
            "startup.enter_wonderland_timeout_ms",
            |c| {
                c.startup.enter_wonderland_timeout_ms += 1;
            },
            |c| debug_of(&c.startup.enter_wonderland_timeout_ms),
        ),
        case(
            "startup.wonderland_map_star_retries",
            |c| {
                c.startup.wonderland_map_star_retries += 1;
            },
            |c| debug_of(&c.startup.wonderland_map_star_retries),
        ),
        case(
            "startup.wonderland_map_star_retry_ms",
            |c| {
                c.startup.wonderland_map_star_retry_ms += 1;
            },
            |c| debug_of(&c.startup.wonderland_map_star_retry_ms),
        ),
        case(
            "startup.wonderland_hall_retries",
            |c| {
                c.startup.wonderland_hall_retries += 1;
            },
            |c| debug_of(&c.startup.wonderland_hall_retries),
        ),
        case(
            "startup.wonderland_hall_retry_ms",
            |c| {
                c.startup.wonderland_hall_retry_ms += 1;
            },
            |c| debug_of(&c.startup.wonderland_hall_retry_ms),
        ),
        case(
            "startup.wonderland_transition_timeout_ms",
            |c| {
                c.startup.wonderland_transition_timeout_ms += 1;
            },
            |c| debug_of(&c.startup.wonderland_transition_timeout_ms),
        ),
        case(
            "startup.wonderland_confirm_stable_timeout_ms",
            |c| {
                c.startup.wonderland_confirm_stable_timeout_ms += 1;
            },
            |c| debug_of(&c.startup.wonderland_confirm_stable_timeout_ms),
        ),
        case(
            "startup.final_primary_timeout_ms",
            |c| {
                c.startup.final_primary_timeout_ms += 1;
            },
            |c| debug_of(&c.startup.final_primary_timeout_ms),
        ),
        case(
            "startup.restart_game_on_loading",
            |c| {
                c.startup.restart_game_on_loading = !c.startup.restart_game_on_loading;
            },
            |c| debug_of(&c.startup.restart_game_on_loading),
        ),
        case(
            "startup.loading_restart_timeout_ms",
            |c| {
                c.startup.loading_restart_timeout_ms += 1;
            },
            |c| debug_of(&c.startup.loading_restart_timeout_ms),
        ),
        case(
            "startup.poll_ms",
            |c| {
                c.startup.poll_ms += 1;
            },
            |c| debug_of(&c.startup.poll_ms),
        ),
        case(
            "startup.stable_mean_threshold",
            |c| {
                c.startup.stable_mean_threshold += 0.01;
            },
            |c| debug_of(&c.startup.stable_mean_threshold),
        ),
        case(
            "startup.stable_changed_ratio_threshold",
            |c| {
                c.startup.stable_changed_ratio_threshold += 0.01;
            },
            |c| debug_of(&c.startup.stable_changed_ratio_threshold),
        ),
        case(
            "startup.template_threshold",
            |c| {
                c.startup.template_threshold += 0.01;
            },
            |c| debug_of(&c.startup.template_threshold),
        ),
        case(
            "startup.wonderland_confirm_threshold",
            |c| {
                c.startup.wonderland_confirm_threshold += 0.01;
            },
            |c| debug_of(&c.startup.wonderland_confirm_threshold),
        ),
        case(
            "startup.templates.wonderland_map_star",
            |c| {
                c.startup.templates.wonderland_map_star =
                    std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.startup.templates.wonderland_map_star),
        ),
        case(
            "startup.templates.paimon_menu",
            |c| {
                c.startup.templates.paimon_menu = std::path::PathBuf::from("probe/override.bin");
            },
            |c| debug_of(&c.startup.templates.paimon_menu),
        ),
        case(
            "startup.enter_game_text_region",
            |c| {
                c.startup.enter_game_text_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.startup.enter_game_text_region),
        ),
        case(
            "startup.wonderland_hall_ocr_region",
            |c| {
                c.startup.wonderland_hall_ocr_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.startup.wonderland_hall_ocr_region),
        ),
        case(
            "startup.main_ui_region",
            |c| {
                c.startup.main_ui_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.startup.main_ui_region),
        ),
        case(
            "startup.wonderland_map_star_region",
            |c| {
                c.startup.wonderland_map_star_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.startup.wonderland_map_star_region),
        ),
        case(
            "invite.friend_name_stable_count",
            |c| {
                c.invite.friend_name_stable_count += 1;
            },
            |c| debug_of(&c.invite.friend_name_stable_count),
        ),
        case(
            "invite.fuzzy_friend_match",
            |c| {
                c.invite.fuzzy_friend_match = !c.invite.fuzzy_friend_match;
            },
            |c| debug_of(&c.invite.fuzzy_friend_match),
        ),
        case(
            "invite.friend_list_region",
            |c| {
                c.invite.friend_list_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.invite.friend_list_region),
        ),
        case(
            "invite.friend_chat_region",
            |c| {
                c.invite.friend_chat_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.invite.friend_chat_region),
        ),
        case(
            "invite.view_star_region",
            |c| {
                c.invite.view_star_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.invite.view_star_region),
        ),
        case(
            "invite.goto_hall_region",
            |c| {
                c.invite.goto_hall_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.invite.goto_hall_region),
        ),
        case(
            "invite.enter_hall_region",
            |c| {
                c.invite.enter_hall_region = crate::config::RectConfig {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                };
            },
            |c| debug_of(&c.invite.enter_hall_region),
        ),
        case(
            "friend_delivery.auto_retry_count",
            |c| {
                c.friend_delivery.auto_retry_count += 1;
            },
            |c| debug_of(&c.friend_delivery.auto_retry_count),
        ),
        case(
            "custom_workflows.enabled",
            |c| {
                c.custom_workflows.enabled = !c.custom_workflows.enabled;
            },
            |c| debug_of(&c.custom_workflows.enabled),
        ),
        case(
            "custom_workflows.default_threshold",
            |c| {
                c.custom_workflows.default_threshold += 0.01;
            },
            |c| debug_of(&c.custom_workflows.default_threshold),
        ),
        case(
            "custom_workflows.wait_template_absent_stable_default",
            |c| {
                c.custom_workflows.wait_template_absent_stable_default =
                    !c.custom_workflows.wait_template_absent_stable_default;
            },
            |c| debug_of(&c.custom_workflows.wait_template_absent_stable_default),
        ),
        case(
            "custom_workflows.max_hold_key_seconds",
            |c| {
                c.custom_workflows.max_hold_key_seconds += 1;
            },
            |c| debug_of(&c.custom_workflows.max_hold_key_seconds),
        ),
        case(
            "custom_workflows.templates",
            |c| {
                c.custom_workflows.templates.insert(
                    "probe".to_string(),
                    std::path::PathBuf::from("probe/template.png"),
                );
            },
            |c| format!("{:?}", c.custom_workflows.templates.get("probe")),
        ),
        case(
            "custom_workflows.workflows",
            |c| {
                c.custom_workflows
                    .workflows
                    .push(probe_workflow_definition());
            },
            |c| debug_of(&c.custom_workflows.workflows),
        ),
        case(
            "identity.mappings",
            |c| {
                c.identity.mappings.push(probe_identity_mapping());
            },
            |c| debug_of(&c.identity.mappings),
        ),
        case(
            "kugou.device_guid",
            |c| {
                c.kugou.device_guid = "probe-value".to_string();
            },
            |c| debug_of(&c.kugou.device_guid),
        ),
        case(
            "kugou.device_dev",
            |c| {
                c.kugou.device_dev = "probe-value".to_string();
            },
            |c| debug_of(&c.kugou.device_dev),
        ),
        case(
            "kugou.device_mac",
            |c| {
                c.kugou.device_mac = "probe-value".to_string();
            },
            |c| debug_of(&c.kugou.device_mac),
        ),
        case(
            "ai_chat.enabled",
            |c| {
                c.ai_chat.enabled = !c.ai_chat.enabled;
            },
            |c| debug_of(&c.ai_chat.enabled),
        ),
        case(
            "ai_chat.keyword",
            |c| {
                c.ai_chat.keyword = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.keyword),
        ),
        case(
            "ai_chat.trigger_mode",
            |c| {
                c.ai_chat.trigger_mode = crate::features::ai_chat::TriggerMode::Contains;
            },
            |c| debug_of(&c.ai_chat.trigger_mode),
        ),
        case(
            "ai_chat.min_access",
            |c| {
                c.ai_chat.min_access = crate::features::ai_chat::AiChatAccess::Owner;
            },
            |c| debug_of(&c.ai_chat.min_access),
        ),
        case(
            "ai_chat.actor_token_budget",
            |c| {
                c.ai_chat.actor_token_budget += 1;
            },
            |c| debug_of(&c.ai_chat.actor_token_budget),
        ),
        case(
            "ai_chat.actor_summary_budget",
            |c| {
                c.ai_chat.actor_summary_budget += 1;
            },
            |c| debug_of(&c.ai_chat.actor_summary_budget),
        ),
        case(
            "ai_chat.actor_history_token_budget",
            |c| {
                c.ai_chat.actor_history_token_budget += 1;
            },
            |c| debug_of(&c.ai_chat.actor_history_token_budget),
        ),
        case(
            "ai_chat.inject_actor_history",
            |c| {
                c.ai_chat.inject_actor_history = !c.ai_chat.inject_actor_history;
            },
            |c| debug_of(&c.ai_chat.inject_actor_history),
        ),
        case(
            "ai_chat.hall_token_budget",
            |c| {
                c.ai_chat.hall_token_budget += 1;
            },
            |c| debug_of(&c.ai_chat.hall_token_budget),
        ),
        case(
            "ai_chat.hall_summary_budget",
            |c| {
                c.ai_chat.hall_summary_budget += 1;
            },
            |c| debug_of(&c.ai_chat.hall_summary_budget),
        ),
        case(
            "ai_chat.bot_token_budget",
            |c| {
                c.ai_chat.bot_token_budget += 1;
            },
            |c| debug_of(&c.ai_chat.bot_token_budget),
        ),
        case(
            "ai_chat.cooldown_seconds",
            |c| {
                c.ai_chat.cooldown_seconds += 1;
            },
            |c| debug_of(&c.ai_chat.cooldown_seconds),
        ),
        case(
            "ai_chat.allow_commands",
            |c| {
                c.ai_chat.allow_commands = !c.ai_chat.allow_commands;
            },
            |c| debug_of(&c.ai_chat.allow_commands),
        ),
        case(
            "ai_chat.model",
            |c| {
                c.ai_chat.model = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.model),
        ),
        case(
            "ai_chat.reasoning",
            |c| {
                c.ai_chat.reasoning = crate::config::ai::ReasoningEffort::High;
            },
            |c| debug_of(&c.ai_chat.reasoning),
        ),
        case(
            "ai_chat.max_tokens",
            |c| {
                c.ai_chat.max_tokens += 1;
            },
            |c| debug_of(&c.ai_chat.max_tokens),
        ),
        case(
            "ai_chat.system_prompt",
            |c| {
                c.ai_chat.system_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.system_prompt),
        ),
        case(
            "ai_chat.answer_prompt",
            |c| {
                c.ai_chat.answer_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.answer_prompt),
        ),
        case(
            "ai_chat.extract_prompt",
            |c| {
                c.ai_chat.extract_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.extract_prompt),
        ),
        case(
            "ai_chat.tool_prompts.look",
            |c| {
                c.ai_chat.tool_prompts.look = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.tool_prompts.look),
        ),
        case(
            "ai_chat.tool_prompts.speak",
            |c| {
                c.ai_chat.tool_prompts.speak = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.tool_prompts.speak),
        ),
        case(
            "ai_chat.tool_prompts.command",
            |c| {
                c.ai_chat.tool_prompts.command = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.tool_prompts.command),
        ),
        case(
            "ai_chat.vision.enabled",
            |c| {
                c.ai_chat.vision.enabled = !c.ai_chat.vision.enabled;
            },
            |c| debug_of(&c.ai_chat.vision.enabled),
        ),
        case(
            "ai_chat.vision.mode",
            |c| {
                c.ai_chat.vision.mode = crate::features::ai_chat::VisionMode::Tool;
            },
            |c| debug_of(&c.ai_chat.vision.mode),
        ),
        case(
            "ai_chat.vision.direct_to_main",
            |c| {
                c.ai_chat.vision.direct_to_main = !c.ai_chat.vision.direct_to_main;
            },
            |c| debug_of(&c.ai_chat.vision.direct_to_main),
        ),
        case(
            "ai_chat.vision.capture_scope",
            |c| {
                c.ai_chat.vision.capture_scope =
                    crate::features::ai_chat::VisionCaptureScope::PrimaryScreen;
            },
            |c| debug_of(&c.ai_chat.vision.capture_scope),
        ),
        case(
            "ai_chat.vision.max_edge",
            |c| {
                c.ai_chat.vision.max_edge += 1;
            },
            |c| debug_of(&c.ai_chat.vision.max_edge),
        ),
        case(
            "ai_chat.vision.jpeg_quality",
            |c| {
                c.ai_chat.vision.jpeg_quality += 1;
            },
            |c| debug_of(&c.ai_chat.vision.jpeg_quality),
        ),
        case(
            "ai_chat.vision.max_bytes",
            |c| {
                c.ai_chat.vision.max_bytes += 1;
            },
            |c| debug_of(&c.ai_chat.vision.max_bytes),
        ),
        case(
            "ai_chat.vision.timeout_ms",
            |c| {
                c.ai_chat.vision.timeout_ms += 1;
            },
            |c| debug_of(&c.ai_chat.vision.timeout_ms),
        ),
        case(
            "ai_chat.vision.model",
            |c| {
                c.ai_chat.vision.model = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.vision.model),
        ),
        case(
            "ai_chat.vision.allow_full_screen",
            |c| {
                c.ai_chat.vision.allow_full_screen = !c.ai_chat.vision.allow_full_screen;
            },
            |c| debug_of(&c.ai_chat.vision.allow_full_screen),
        ),
        case(
            "ai_chat.ai_reply.mode",
            |c| {
                c.ai_chat.ai_reply.mode = crate::features::ai_chat::AiReplyMode::Rewrite;
            },
            |c| debug_of(&c.ai_chat.ai_reply.mode),
        ),
        case(
            "ai_chat.ai_reply.include_template",
            |c| {
                c.ai_chat.ai_reply.include_template = !c.ai_chat.ai_reply.include_template;
            },
            |c| debug_of(&c.ai_chat.ai_reply.include_template),
        ),
        case(
            "ai_chat.ai_reply.include_command",
            |c| {
                c.ai_chat.ai_reply.include_command = !c.ai_chat.ai_reply.include_command;
            },
            |c| debug_of(&c.ai_chat.ai_reply.include_command),
        ),
        case(
            "ai_chat.ai_reply.system_prompt",
            |c| {
                c.ai_chat.ai_reply.system_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.ai_reply.system_prompt),
        ),
        case(
            "ai_chat.ai_reply.user_prompt",
            |c| {
                c.ai_chat.ai_reply.user_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.ai_reply.user_prompt),
        ),
        case(
            "ai_chat.ai_reply.max_tokens",
            |c| {
                c.ai_chat.ai_reply.max_tokens += 1;
            },
            |c| debug_of(&c.ai_chat.ai_reply.max_tokens),
        ),
        case(
            "ai_chat.ai_reply.timeout_seconds",
            |c| {
                c.ai_chat.ai_reply.timeout_seconds += 1;
            },
            |c| debug_of(&c.ai_chat.ai_reply.timeout_seconds),
        ),
        case(
            "ai_chat.protocols.memory",
            |c| {
                c.ai_chat.protocols.memory = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.protocols.memory),
        ),
        case(
            "ai_chat.protocols.identity",
            |c| {
                c.ai_chat.protocols.identity = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.protocols.identity),
        ),
        case(
            "ai_chat.protocols.time",
            |c| {
                c.ai_chat.protocols.time = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.protocols.time),
        ),
        case(
            "ai_chat.protocols.action",
            |c| {
                c.ai_chat.protocols.action = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.protocols.action),
        ),
        case(
            "ai_chat.protocols.reply",
            |c| {
                c.ai_chat.protocols.reply = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.protocols.reply),
        ),
        case(
            "ai_chat.protocols.tool",
            |c| {
                c.ai_chat.protocols.tool = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.protocols.tool),
        ),
        case(
            "ai_chat.tool_descriptions.look",
            |c| {
                c.ai_chat.tool_descriptions.look = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.tool_descriptions.look),
        ),
        case(
            "ai_chat.tool_descriptions.speak",
            |c| {
                c.ai_chat.tool_descriptions.speak = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.tool_descriptions.speak),
        ),
        case(
            "ai_chat.tool_descriptions.command",
            |c| {
                c.ai_chat.tool_descriptions.command = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.tool_descriptions.command),
        ),
        case(
            "ai_chat.summary_prompt",
            |c| {
                c.ai_chat.summary_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.summary_prompt),
        ),
        case(
            "ai_chat.vision_prompt",
            |c| {
                c.ai_chat.vision_prompt = "probe-value".to_string();
            },
            |c| debug_of(&c.ai_chat.vision_prompt),
        ),
        case(
            "tts.enabled",
            |c| {
                c.tts.enabled = !c.tts.enabled;
            },
            |c| debug_of(&c.tts.enabled),
        ),
        case(
            "tts.endpoint",
            |c| {
                c.tts.endpoint = "probe-value".to_string();
            },
            |c| debug_of(&c.tts.endpoint),
        ),
        case(
            "tts.http_proxy",
            |c| {
                c.tts.http_proxy = "probe-value".to_string();
            },
            |c| debug_of(&c.tts.http_proxy),
        ),
        case(
            "tts.api_key",
            |c| {
                c.tts.api_key = "probe-secret".to_string();
            },
            |c| debug_of(&c.tts.api_key),
        ),
        case(
            "tts.model",
            |c| {
                c.tts.model = "probe-value".to_string();
            },
            |c| debug_of(&c.tts.model),
        ),
        case(
            "tts.reference_id",
            |c| {
                c.tts.reference_id = "probe-value".to_string();
            },
            |c| debug_of(&c.tts.reference_id),
        ),
        case(
            "tts.voice",
            |c| {
                c.tts.voice = "probe-value".to_string();
            },
            |c| debug_of(&c.tts.voice),
        ),
        case(
            "tts.format",
            |c| {
                c.tts.format = crate::features::tts::TtsFormat::Wav;
            },
            |c| debug_of(&c.tts.format),
        ),
        case(
            "tts.duck_volume",
            |c| {
                c.tts.duck_volume += 1;
            },
            |c| debug_of(&c.tts.duck_volume),
        ),
        case(
            "tts.duck_fade_ms",
            |c| {
                c.tts.duck_fade_ms += 1;
            },
            |c| debug_of(&c.tts.duck_fade_ms),
        ),
        case(
            "tts.max_chars",
            |c| {
                c.tts.max_chars += 1;
            },
            |c| debug_of(&c.tts.max_chars),
        ),
        case(
            "tts.timeout_ms",
            |c| {
                c.tts.timeout_ms += 1;
            },
            |c| debug_of(&c.tts.timeout_ms),
        ),
        case(
            "tts.queue_max",
            |c| {
                c.tts.queue_max += 1;
            },
            |c| debug_of(&c.tts.queue_max),
        ),
        case(
            "tts.trigger_scope",
            |c| {
                c.tts.trigger_scope = crate::features::tts::TtsTriggerScope::AiChat;
            },
            |c| debug_of(&c.tts.trigger_scope),
        ),
        case(
            "emoji.enabled",
            |c| {
                c.emoji.enabled = !c.emoji.enabled;
            },
            |c| debug_of(&c.emoji.enabled),
        ),
        case(
            "emoji.ai_enabled",
            |c| {
                c.emoji.ai_enabled = !c.emoji.ai_enabled;
            },
            |c| debug_of(&c.emoji.ai_enabled),
        ),
        case(
            "emoji.ai_allowed",
            |c| {
                c.emoji.ai_allowed = vec!["probe".to_string()];
            },
            |c| debug_of(&c.emoji.ai_allowed),
        ),
        case(
            "emoji.capture_debug",
            |c| {
                c.emoji.capture_debug = !c.emoji.capture_debug;
            },
            |c| debug_of(&c.emoji.capture_debug),
        ),
    ]
}

/// 主矩阵：399 条路径的 effect 声明必须与 `apply()` 的实际行为逐条一致。
///
/// - 声明 Live（121 条）：`apply` 后快照必须等于改动值；
/// - 声明非 Live（278 条）：`apply` 后快照必须仍是旧值（整段 clone 不能顺手带上）。
#[test]
fn live_effect_matrix_is_exact() {
    let schema: BTreeMap<String, Effect> = config_sections()
        .into_iter()
        .flat_map(|section| section.fields)
        .map(|field| (field.path, field.effect))
        .collect();
    let table = cases();

    // 用例表的形状：无重复、与 schema 双向相等。
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for item in &table {
        assert!(
            seen.insert(item.path),
            "用例表里出现重复路径：{}",
            item.path
        );
    }
    let table_paths: BTreeSet<&str> = table.iter().map(|item| item.path).collect();
    let schema_paths: BTreeSet<&str> = schema.keys().map(String::as_str).collect();
    assert_eq!(
        schema_paths,
        table_paths,
        "用例表必须与 schema 路径集合完全一致：缺 {} 条、多 {} 条",
        schema_paths.difference(&table_paths).count(),
        table_paths.difference(&schema_paths).count()
    );
    assert_eq!(table.len(), 399, "W0 基线：schema 共 399 条路径");
    let live_declared = schema
        .values()
        .filter(|effect| **effect == Effect::Live)
        .count();
    assert_eq!(live_declared, 121, "W0 基线：Live 字段 121 条");

    let mut failures: Vec<(&'static str, String)> = Vec::new();
    let mut unobservable: BTreeSet<&'static str> = BTreeSet::new();

    for item in &table {
        let effect = schema[item.path];
        let base = base_config(item.path);
        let base_probe = (item.probe)(&base);
        let mut changed = base.clone();
        (item.set)(&mut changed);
        let changed_probe = (item.probe)(&changed);

        // 单值枚举：今天不存在「另一个合法值」，矩阵不可观测。断言它确实还不
        // 可观测——枚举一旦扩容，这里就会失败，提示把路径移出清单纳入矩阵。
        if SINGLETON_ENUM_PATHS.contains(&item.path) {
            unobservable.insert(item.path);
            assert_eq!(
                effect,
                Effect::Live,
                "单值枚举路径 {0} 不再是 Live（当前 {effect:?}），请复核 SINGLETON_ENUM_PATHS",
                item.path
            );
            assert_eq!(
                base_probe, changed_probe,
                "单值枚举 {} 现在能构造出不同取值了：请移出 SINGLETON_ENUM_PATHS 并让它走正式矩阵",
                item.path
            );
            continue;
        }

        // 前置自检：setter 必须真的改到了值，否则「没变」无法归因于 apply。
        assert_ne!(
            base_probe, changed_probe,
            "用例的 setter 没有改变默认值，矩阵无法证明 apply 做过任何事：{}",
            item.path
        );

        let live = LiveConfigs::from_config(&base);
        live.apply(&changed);
        let snapshot = live.snapshot();
        let actual = (item.probe)(&snapshot);

        match effect {
            Effect::Live => {
                if actual != changed_probe {
                    failures.push((
                        item.path,
                        format!(
                            "声明 Live，但 apply 后快照没有采用改动值（期望 {changed_probe}，实际 {actual}）"
                        ),
                    ));
                }
            }
            other => {
                if actual != base_probe {
                    failures.push((
                        item.path,
                        format!(
                            "声明 {other:?}，但 apply 后快照已被热更新（期望旧值 {base_probe}，实际 {actual}）"
                        ),
                    ));
                }
            }
        }
    }

    // 单项清单必须与用例表一一对应。
    let declared_singletons: BTreeSet<&str> = SINGLETON_ENUM_PATHS.iter().copied().collect();
    assert_eq!(
        declared_singletons, unobservable,
        "SINGLETON_ENUM_PATHS 必须与用例表一一对应（多登记=漏测，少登记=清单过期）"
    );

    // KNOWN_DEVIATIONS：只减不增。
    let known: Vec<&str> = KNOWN_DEVIATIONS.iter().map(|item| item.path).collect();
    let known_set: BTreeSet<&str> = known.iter().copied().collect();
    assert_eq!(known.len(), known_set.len(), "KNOWN_DEVIATIONS 有重复条目");
    for path in &known {
        assert!(
            schema.contains_key(*path),
            "KNOWN_DEVIATIONS 记录了 schema 里不存在的路径：{path}"
        );
    }
    let failed: BTreeSet<&str> = failures.iter().map(|(path, _)| *path).collect();
    let stale: Vec<&&str> = known_set
        .iter()
        .filter(|path| !failed.contains(**path))
        .collect();
    assert!(
        stale.is_empty(),
        "KNOWN_DEVIATIONS 里这些路径已经达标，必须删除条目（清单只能收缩）：{stale:?}"
    );

    let unexplained: Vec<&(&str, String)> = failures
        .iter()
        .filter(|(path, _)| !known_set.contains(path))
        .collect();
    assert!(
        unexplained.is_empty(),
        "effect 声明与 apply 实现不一致（{} 条未登记）：\n{}",
        unexplained.len(),
        unexplained
            .iter()
            .map(|(path, detail)| format!("  - {path}：{detail}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    eprintln!(
        "effect 矩阵：{} 条（Live {} / 非 Live {}），不可观测 {} 条，已登记偏差 {} 条",
        table.len(),
        live_declared,
        table.len() - live_declared,
        SINGLETON_ENUM_PATHS.len(),
        KNOWN_DEVIATIONS.len()
    );
}

/// `apply()` 的合并策略：决定「新增字段」默认会怎么生效（见 round1_C §D1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyStrategy {
    /// 整段 clone：段内新增字段**自动 Live**。声明若写 IdleReload → 实现先于声明生效。
    WholeSegmentClone,
    /// 整段 clone 后逐字段回填非 Live：**反向清单**，漏一行即静默热更新（最危险）。
    CloneThenBackfill,
    /// 从旧快照出发只覆盖列出的 Live 字段（正向白名单）：新增字段默认不 Live，安全。
    LiveAllowlist,
}

/// 10 个高风险段的「声明 ↔ 实现策略」对照。
///
/// 这不是重复矩阵：矩阵只证明**今天**每条都对；这里固定住**策略**，让
/// 「给整段 clone 的段加一个 IdleReload 字段」或「给 tts 加第四个 IdleReload
/// 字段却没加回填行」在今天就能失败，而不是等到用户报告「面板说没生效、实际生效了」。
const SECTION_STRATEGIES: &[(&str, ApplyStrategy, &[&str])] = &[
    // 整段 clone 的 7 段：段内不允许出现任何非 Live 字段。
    ("ai_chat", ApplyStrategy::WholeSegmentClone, &[]),
    ("matching", ApplyStrategy::WholeSegmentClone, &[]),
    ("custom_workflows", ApplyStrategy::WholeSegmentClone, &[]),
    ("command_access", ApplyStrategy::WholeSegmentClone, &[]),
    ("identity", ApplyStrategy::WholeSegmentClone, &[]),
    ("visual", ApplyStrategy::WholeSegmentClone, &[]),
    ("diagnostics", ApplyStrategy::WholeSegmentClone, &[]),
    // 整段 clone + 回填：非 Live 集合必须**恰好**等于 apply 里的回填行。
    (
        "tts",
        ApplyStrategy::CloneThenBackfill,
        &["tts.endpoint", "tts.api_key", "tts.http_proxy"],
    ),
    // 白名单覆盖：声明的 Live 集合必须恰好等于 apply 逐字段赋值的清单。
    (
        "playback",
        ApplyStrategy::LiveAllowlist,
        &[
            "playback.login_timeout_ms",
            "playback.audio_cache.max_bytes_mb",
            "playback.audio_cache.request_timeout_ms",
            "playback.audio_cache.seek_wait_timeout_ms",
        ],
    ),
    (
        "moderation",
        ApplyStrategy::LiveAllowlist,
        &[
            "moderation.stable_vote_samples",
            "moderation.required_vote_margin",
        ],
    ),
];

/// 消费方探针：逐段验证「整段 clone」没有让非 Live 字段隐性变 Live。
#[test]
fn section_effect_strategy_matches_apply() {
    let sections: BTreeMap<String, Vec<(String, Effect)>> = config_sections()
        .into_iter()
        .map(|section| {
            (
                section.name,
                section
                    .fields
                    .into_iter()
                    .map(|field| (field.path, field.effect))
                    .collect(),
            )
        })
        .collect();

    for (name, strategy, listed) in SECTION_STRATEGIES {
        let fields = sections
            .get(*name)
            .unwrap_or_else(|| panic!("SECTION_STRATEGIES 里的段 {name} 不在 schema 中"));
        let live: BTreeSet<&str> = fields
            .iter()
            .filter(|(_, effect)| *effect == Effect::Live)
            .map(|(path, _)| path.as_str())
            .collect();
        let non_live: BTreeSet<&str> = fields
            .iter()
            .filter(|(_, effect)| *effect != Effect::Live)
            .map(|(path, _)| path.as_str())
            .collect();
        let listed: BTreeSet<&str> = listed.iter().copied().collect();

        match strategy {
            ApplyStrategy::WholeSegmentClone => {
                assert!(
                    non_live.is_empty(),
                    "段 {name} 在 apply() 里整段 clone：新增非 Live 字段会被隐性热更新（D1）。\
                     今天出现了 {non_live:?}——要么把它改成 Live，要么把 apply 改成白名单覆盖"
                );
                assert_eq!(
                    live.len(),
                    fields.len(),
                    "段 {name} 全部字段都必须是 Live 才允许整段 clone"
                );
            }
            ApplyStrategy::CloneThenBackfill => {
                assert_eq!(
                    non_live, listed,
                    "段 {name} 用「clone 后回填」策略：非 Live 集合必须恰好等于 apply() 的回填行。\
                     新增 IdleReload 字段而漏回填，会让它在保存瞬间生效（tts.http_proxy 事故）"
                );
            }
            ApplyStrategy::LiveAllowlist => {
                assert_eq!(
                    live, listed,
                    "段 {name} 用白名单覆盖：声明为 Live 的字段必须恰好等于 apply() 逐字段写入的清单\
                     （多=虚报，少=漏接线）"
                );
            }
        }
    }
}
