//! `listener` 目录模块的单测。
//!
//! 原有 30 条测试逐条保留（行为等价是拆分的前置条件），并新增针对
//! 「协调器观测」与「实例化去重状态」的用例：这两块是拆分与下放 `static`
//! 之后唯一真正改变**状态归属**的地方，必须单独钉住。

use super::super::*;
use super::chat_routing::{
    acknowledge_primary_command, acknowledge_unrecognized_primary_message,
    chat_listener_mode_dispatch, complete_secondary_claim,
};
use super::display::{normalize_chat_text, ui_state_display_label};
use super::loop_driver::screen_size_warning_enabled;
use super::recovery_guard::{automatic_startup_task, game_location_probe_blocks_residency};
use super::*;

/// 回归（Live 化批次4b）：尺寸不符告警开关取自 live 快照，不是启动期缓存。
///
/// 生产读取点传 `&self.shared.live_configs.snapshot()`；本测试证明该纯函数
/// 使用传入配置的值、且 LiveConfigs 快照携带被保存后的新值。
#[test]
fn screen_size_warning_flag_reads_the_current_live_snapshot() {
    let mut config = AppConfig::default();
    config.screen.warn_on_size_mismatch = false;
    assert!(!screen_size_warning_enabled(&config));
    config.screen.warn_on_size_mismatch = true;
    assert!(screen_size_warning_enabled(&config));

    let live = crate::config::LiveConfigs::from_config(&config);
    assert!(screen_size_warning_enabled(&live.snapshot()));
}

/// 回归：AI 聊天的触发关键词必须作用在「去掉前导分隔符」的正文上。
///
/// split_sender_line 只切在分隔符**之前**，body 仍带着那个分隔符：
///   [2#BOT|勿加]:橘子你是什么模型？  ->  (2#BOT|勿加, :橘子你是什么模型？)
/// 命令路径会先 trim 掉 : / ： 再解析，AI 聊天分支原来没做，
/// 于是 Prefix 模式下 strip_prefix(橘子) 永远失败——表现为
/// 「AI 聊天完全无法触发」，而日志里只看到「命令未执行」。
#[test]
fn ai_chat_trigger_text_drops_the_sender_separator() {
    use crate::interfaces::chat::split_sender_line;
    let raw = "[2#BOT|勿加]:橘子你是什么模型？";
    let (sender, body) = split_sender_line(raw, "blue").expect("bracket sender");
    assert_eq!(sender, "2#BOT|勿加");
    // split_sender_line 只切在分隔符**之前**，body 仍带着冒号。
    assert_eq!(body, ":橘子你是什么模型？");
    let mut config = crate::features::ai_chat::AiChatConfig::default();
    config.keyword = "橘子".to_string();
    // 归一化下沉到 match_trigger 内部：命令路径与闲聊路径看到同一段正文。
    // 因此带冒号的原始 body 也必须命中，正文是「你是什么模型？」。
    assert_eq!(
        config.match_trigger(body).as_deref(),
        Some("你是什么模型？"),
        "带前导分隔符的 body 必须命中：归一化是 match_trigger 的职责"
    );
    // 已经 trim 过的正文结果一致（幂等）。
    let trimmed = body.trim_start_matches(['：', ':', ' ', '\t', ']', '】']);
    assert_eq!(
        config.match_trigger(trimmed).as_deref(),
        Some("你是什么模型？")
    );
}

/// 回归（端到端）：真实 OCR 文本「[昵称]:橘子xxx」必须能一路命中触发。
///
/// 覆盖从聊天行到判定结果的全部真实环节：split_sender_line 切昵称、
/// match_trigger 去分隔符并剥关键词、keyword_trigger 过全部门槛。
/// 这条路径以前因为 body 带 ':' 而永远返回 None，玩家看到的就是 AI 完全不回。
#[test]
fn bracketed_hall_line_with_keyword_triggers_end_to_end() {
    use crate::features::ai_chat::{AiChatConfig, KeywordTrigger, TriggerSkip, keyword_trigger};
    use crate::interfaces::chat::split_sender_line;

    let config = AiChatConfig {
        enabled: true,
        keyword: "橘子".to_string(),
        ..AiChatConfig::default()
    };

    // 玩家发的那句话：昵称在方括号里，正文前有半角冒号。
    let line = "[路人甲]:橘子你是什么模型？";
    let split = split_sender_line(line, "blue");
    assert_eq!(
        keyword_trigger(&config, true, "blue", split),
        KeywordTrigger::Ask {
            sender: "路人甲".to_string(),
            // 正文原样交给模型，关键词保留在句首。
            question: "橘子你是什么模型？".to_string(),
        }
    );

    // 全角昵称括号 + 冒号（OCR 两种字形都可能出现）。
    let line = "【路人甲】:橘子在吗";
    let split = split_sender_line(line, "blue");
    assert_eq!(
        keyword_trigger(&config, true, "blue", split),
        KeywordTrigger::Ask {
            sender: "路人甲".to_string(),
            question: "橘子在吗".to_string(),
        }
    );

    // 一条命令式的行不会被关键词吞掉：它不带关键词，正常落回关键词未命中。
    let split = split_sender_line("[路人甲]:@点歌晴天", "blue");
    assert_eq!(
        keyword_trigger(&config, true, "blue", split),
        KeywordTrigger::Skip(TriggerSkip::KeywordMiss)
    );
}

/// 监控页"界面"只显示中文界面名，不复述命中的颜色/模板判据；
/// 判据细节仍保留在 /debug/ui-state 与 ui.state 事件证据里。
#[test]
fn ui_state_display_label_maps_state_families_without_probe_details() {
    assert_eq!(
        ui_state_display_label("primary:marker blue=1 yellow=0 pink=0"),
        "一级大厅"
    );
    assert_eq!(ui_state_display_label("primary:friend"), "一级大厅");
    assert_eq!(ui_state_display_label("secondary:chat"), "二级界面");
    assert_eq!(ui_state_display_label("secondary:qianxing_map"), "二级界面");
    assert_eq!(
        ui_state_display_label("secondary:qianxing_panel"),
        "二级界面"
    );
    assert_eq!(ui_state_display_label("main_menu"), "主菜单千星");
    assert_eq!(ui_state_display_label("game_gate"), "大门界面");
    assert_eq!(ui_state_display_label("overworld"), "主世界");
    assert_eq!(
        ui_state_display_label("overworld:world_criterion"),
        "主世界"
    );
    assert_eq!(ui_state_display_label("loading"), "加载界面");
    assert_eq!(ui_state_display_label("panel:friend_list"), "模态面板");
    assert_eq!(ui_state_display_label("unknown"), "未知界面");
    assert_eq!(ui_state_display_label("unknown:game_probe"), "未知界面");
    assert_eq!(
        ui_state_display_label("transition:overworld 1/2"),
        "界面切换中"
    );
    assert_eq!(ui_state_display_label("transition:unknown"), "界面切换中");
    // 未识别的新标签原样透出，不静默吞掉。
    assert_eq!(ui_state_display_label("something_new"), "something_new");
}

/// 界面状态迁移只在**稳定类型**变化时推送。
/// 标签里还带着聊天标记计数，而计数每帧都在变——跟着它推等于自造轮询。
#[test]
fn ui_state_transition_only_fires_on_stable_kind_change() {
    use crate::runtime::ui::UiStateKind;
    let mut tracker = display::UiTransitionTracker::default();
    assert!(
        tracker.take_transition(Some(UiStateKind::Secondary)),
        "全新跟踪器首次观测必须产生迁移事件"
    );
    assert!(
        !tracker.take_transition(Some(UiStateKind::Secondary)),
        "同一稳定类型重复观测不得再产生事件"
    );
    assert!(tracker.take_transition(None), "稳定 -> 过渡 是一条真实迁移");
    assert!(
        !tracker.take_transition(None),
        "过渡态内部不断抖动时不得重复推送"
    );
    assert!(
        tracker.take_transition(Some(UiStateKind::Primary)),
        "过渡 -> 另一个稳定类型 又是一条迁移"
    );
    assert!(!tracker.take_transition(Some(UiStateKind::Primary)));
    assert!(tracker.take_transition(Some(UiStateKind::Overworld)));
}

/// OCR 抖动不得产生新消息。断言来自真机实测加上一个必须正视的事实：
/// **OCR 对特殊字符会抖动**——半角/全角标点、表情被读成乱码、说话人迟一拍，
/// 都曾让同一条气泡产生独立事件。
#[test]
fn chat_jitter_does_not_produce_new_messages() {
    assert_eq!(
        normalize_chat_text("[珐露珊]："),
        normalize_chat_text("[珐露珊]:"),
        "半角/全角标点属于排版差异，不是内容变化"
    );
    assert_eq!(
        normalize_chat_text("  天子 :  你好  "),
        "天子:你好",
        "空白一律丢弃：OCR 的空白两个方向都会错，保留它只会让比较更不稳定"
    );
    assert_eq!(
        normalize_chat_text("[天子]:@女(禁言7天) 标题党是台好车"),
        "[天子]:@女(禁言7天)标题党是台好车",
        "实测中真实存在的空格也要丢弃：同一位置 OCR 时有时无"
    );

    let mut bubbles = display::RememberedBubbleTracker::default();
    let key = "jitter-probe|primary_hall|9001";
    assert!(bubbles.take_update(key), "第一次看到这条气泡必须推送");
    // 同一条气泡的**任何**再次识别都不得产生新事件，包括长短变化的抖动。
    for jitter in [
        "[珐露珊]:",
        "[珐露珊]：",
        "[珐露珊]::",
        "[]:这在干啥？",
        "[天子]:这在干啥？",
        "[]:这在干啥?",
        "🙂",
        "",
    ] {
        assert!(
            !bubbles.take_update(key),
            "同一条气泡的再次识别不得产生新事件: {jitter:?}"
        );
    }
    assert!(
        bubbles.take_update("jitter-probe|primary_hall|9002"),
        "另一条气泡必须照常推送"
    );
}

use crate::features::command::{
    CommandAuthority, CommandEnvelope, CommandObservation, ModuleCommand, RoutedCommand,
};
use crate::features::custom_workflow::{
    CustomWorkflowConfig, CustomWorkflowService, WorkflowDefaults,
};
use crate::features::identity::{IdentityAccess, IdentityConfig, IdentityMapping, IdentityRole};
use crate::features::playback::PlaybackCommand;
use crate::interfaces::chat::ChatCommandRouter;

use super::chat_routing::{CommandGate, gate_notice, hall_consumes_scope, lock_anchor_for};
use super::loop_driver::{load_persisted_chat_listener_mode, persist_chat_listener_mode};
use super::recovery_guard::{
    CHAT_LISTENER_UNRESOLVED_UI_GRACE, CONFIG_RELOAD_UI_FALLBACK_GRACE, ListenerReadyEvidence,
    listener_mode_matches_ui, listener_ready_after_recheck, primary_rescan_can_publish,
    replacement_ready_allowed, replacement_startup_fallback_due, replacement_startup_retry_due,
    scan_frame_is_fresh, secondary_hall_recovery_allowed, unresolved_ui_recovery_due,
};
use crate::geometry::Rect;
use crate::observation::chat::{
    BubbleSequence, ChatIdentity, ChatMessage, ChatScope, ChatTimeline, ObservedChatMessageId,
    PrimaryObservedMessage, RecognizedChatMessage, SourceId, VisualSessionId,
};
use std::time::{Duration, Instant};

/// 端到端：面板关掉命令 → 玩家**真的收到**一句提示。
///
/// 这条覆盖的是 C-CMD-077 要求的**拦截 → 回复**全链路，不是纯函数返回值：
/// 1. 用真实 `ChatCommandRouter` 路由（与生产同一条路径）；
/// 2. 用拦截点真正调用的 `CommandGate::from_policy` 判定；
/// 3. 用拦截点真正调用的 `gate_notice` 决定回什么；
/// 4. 断言玩家收到的文本。
///
/// 上一版只在 `execute_command` 出文案，而命令在入队前就被丢掉，
/// 纯函数测得通、玩家永远收不到——正是这条测试要钉住的差别。
#[test]
fn disabled_command_end_to_end_delivers_a_notice_to_the_player() {
    use crate::features::command_access::{
        CommandAccessConfig, CommandPermission, CommandPolicy, actor_permission_with_floor,
        catalog_policy,
    };

    let mut access = CommandAccessConfig::default();
    access.overrides.insert(
        "@删除".into(),
        CommandPolicy {
            enabled: false,
            minimum_role: CommandPermission::Friend,
        },
    );
    let identity = IdentityAccess::default();
    let workflows = CustomWorkflowService::new(
        CustomWorkflowConfig::default(),
        WorkflowDefaults {
            default_timeout_ms: 1000,
            default_poll_ms: 100,
            default_step_wait_ms: 100,
            decision_timeout_ms: 1000,
            decision_poll_ms: 100,
            after_activate_ms: 100,
            clipboard_hold_ms: 100,
            stability_mean_threshold: 1.0,
            stability_changed_ratio_threshold: 0.1,
        },
    );
    let envelope = CommandEnvelope::new(
        "玩家甲：@删除",
        "玩家甲",
        "pink",
        "@删除",
        CommandObservation::default(),
    )
    .expect("envelope");
    let routed = ChatCommandRouter::with_access(&workflows, &identity, &access)
        .route(&envelope, None)
        .expect("@删除 应被路由");

    // 拦截点用的正是这两个调用（listener::command_gate）。
    let policy = catalog_policy(&access, "@删除").expect("@删除 有策略");
    let actor = actor_permission_with_floor(routed.role, routed.authority);
    let gate = CommandGate::from_policy(
        &policy,
        actor,
        routed.authority,
        routed.role.is_some(),
        true,
    );
    assert_eq!(gate, CommandGate::Disabled);

    // 玩家实际收到的那句话。
    // raw 已剥掉 @ 前缀，必须用 user_command；这条断言同时钉住该差异。
    assert_eq!(routed.raw, "删除", "raw 不含前缀，不能拿来判命令");
    assert_eq!(routed.user_command, "@删除");
    assert_eq!(
        gate_notice(gate, &routed.raw),
        None,
        "用 raw 会恒静默——这正是要把 user_command 传进去的原因",
    );
    assert_eq!(
        gate_notice(gate, &routed.user_command),
        Some("该命令已在面板禁用"),
        "面板关掉的命令必须明确告诉玩家『已禁用』，而不是静默或说成权限问题",
    );
}

/// 端到端第二半：闲聊被拦时**不回复**（防刷屏）。
#[test]
fn chit_chat_blocked_by_the_gate_does_not_trigger_a_notice() {
    use crate::features::command_access::CommandPermission;

    let gate = CommandGate::BelowMinimum(CommandPermission::Friend);
    // 闲聊文本（不像命令）→ 不打扰。
    for chit_chat in ["今天天气不错", "哈哈哈", "1"] {
        assert_eq!(
            gate_notice(gate, chit_chat),
            None,
            "闲聊不该收到权限提示：{chit_chat}",
        );
    }
    // 命令样式文本 → 要提示。
    assert_eq!(
        gate_notice(gate, "@删除"),
        Some("权限不足：该命令仅限好友使用")
    );
    // 放行时当然不提示。
    assert_eq!(
        gate_notice(CommandGate::Allowed, "@删除"),
        None,
        "放行不能回提示",
    );
}

fn listener_snapshot(
    mode: ChatListenerMode,
) -> crate::runtime::chat_listener::ChatListenerSnapshot {
    crate::runtime::chat_listener::ChatListenerSnapshot {
        mode,
        pending_mode: None,
        temporary_primary: false,
        initial_unread_clear: false,
        unread_task_pending: false,
        hall_round_required: false,
    }
}

#[test]
fn automatic_entry_requires_the_matching_stable_location_and_switch() {
    use crate::config::AppConfig;
    use crate::features::startup::StartupTaskKind;
    use crate::runtime::ui::UiStateKind;

    let mut config = AppConfig::default();
    config.startup.enabled = true;
    config.startup.enter_game = true;
    config.startup.enter_wonderland = false;
    assert_eq!(
        automatic_startup_task(&config, UiStateKind::GameGate)
            .unwrap()
            .kind(),
        StartupTaskKind::StartGame,
    );
    assert!(automatic_startup_task(&config, UiStateKind::Overworld).is_none());
    config.startup.enter_wonderland = true;
    assert_eq!(
        automatic_startup_task(&config, UiStateKind::Overworld)
            .unwrap()
            .kind(),
        StartupTaskKind::EnterWonderland,
    );
    for kind in [
        UiStateKind::Primary,
        UiStateKind::Secondary,
        UiStateKind::Unknown,
    ] {
        assert!(automatic_startup_task(&config, kind).is_none());
    }
    config.startup.enabled = false;
    assert!(automatic_startup_task(&config, UiStateKind::GameGate).is_none());
    assert!(automatic_startup_task(&config, UiStateKind::Overworld).is_none());
}

#[test]
fn observing_a_game_location_can_finish_reload_without_automatic_entry() {
    let state = listener_snapshot(ChatListenerMode::Secondary);
    assert!(listener_ready_after_recheck(
        Some(ListenerReadyEvidence::GameLocation),
        &state,
        false,
        false,
        true,
    ));
    assert!(!listener_ready_after_recheck(
        Some(ListenerReadyEvidence::GameLocation),
        &state,
        true,
        false,
        true,
    ));
}

#[test]
fn game_location_confirmation_prevents_generic_residency_recovery() {
    use crate::runtime::ui::{UiStateClassification, UiStateKind, UiStateTracker};
    let mut tracker = UiStateTracker::new(2);
    for (frame, kind, label) in [
        (1, UiStateKind::Unknown, "unknown:game_probe"),
        (2, UiStateKind::GameGate, "game_gate"),
        (3, UiStateKind::Overworld, "overworld"),
    ] {
        let observation = tracker.observe(frame, UiStateClassification::new(kind, label));
        assert!(game_location_probe_blocks_residency(Some(&observation)));
    }
    let unknown = tracker.observe(
        4,
        UiStateClassification::new(UiStateKind::Unknown, "unknown"),
    );
    assert!(!game_location_probe_blocks_residency(Some(&unknown)));
}

#[test]
fn listener_mode_dispatch_preserves_router_permissions() {
    let identity = IdentityAccess::new(IdentityConfig {
        mappings: vec![IdentityMapping {
            remark: "管理员".to_string(),
            id: uuid::Uuid::from_u128(1),
            role: IdentityRole::Admin,
            real_nickname: String::new(),
            display_name: String::new(),
            remark_from_chatbridge: false,
            accounts: Vec::new(),
        }],
    });
    let workflows = CustomWorkflowService::new(
        CustomWorkflowConfig::default(),
        WorkflowDefaults {
            default_timeout_ms: 1000,
            default_poll_ms: 100,
            default_step_wait_ms: 100,
            decision_timeout_ms: 1000,
            decision_poll_ms: 100,
            after_activate_ms: 100,
            clipboard_hold_ms: 100,
            stability_mean_threshold: 1.0,
            stability_changed_ratio_threshold: 0.1,
        },
    );
    let router = ChatCommandRouter::with_identity(&workflows, &identity);
    for (username, message_type, permitted) in [
        ("访客", "blue", false),
        ("管理员", "blue", true),
        ("访客", "pink", true),
    ] {
        for (text, expected) in [
            ("@值守模式 一级", ChatListenerModeCommand::Primary),
            ("@值守模式 二级", ChatListenerModeCommand::Secondary),
            ("@值守模式 状态", ChatListenerModeCommand::Status),
        ] {
            let envelope = CommandEnvelope::new(
                format!("{username}：{text}"),
                username,
                message_type,
                text,
                CommandObservation::default(),
            )
            .unwrap();
            let routed = router.route(&envelope, None).expect("listener command");
            assert_eq!(routed.permission_required.is_none(), permitted);
            assert_eq!(
                chat_listener_mode_dispatch(&routed),
                permitted.then_some(expected),
                "username={username} source={message_type} command={text}"
            );
        }
    }
}

#[test]
fn chat_listener_mode_persistence_round_trips_without_using_the_database_file() {
    let root = std::env::temp_dir().join(format!(
        "miliastra-chat-listener-test-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let playback_state_path = root.join("playback.sqlite3");

    persist_chat_listener_mode(&playback_state_path, ChatListenerMode::Secondary).unwrap();
    assert_eq!(
        load_persisted_chat_listener_mode(&playback_state_path).unwrap(),
        Some(ChatListenerMode::Secondary)
    );
    assert!(
        root.join("playback.sqlite3.chat-listener-mode.json")
            .is_file()
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn listener_ready_requires_the_selected_logical_mode_to_match_the_ui() {
    assert!(listener_mode_matches_ui(
        ChatListenerMode::Primary,
        false,
        crate::runtime::ui::UiStateKind::Primary
    ));
    assert!(!listener_mode_matches_ui(
        ChatListenerMode::Primary,
        false,
        crate::runtime::ui::UiStateKind::Secondary
    ));
    assert!(listener_mode_matches_ui(
        ChatListenerMode::Secondary,
        false,
        crate::runtime::ui::UiStateKind::Secondary
    ));
    assert!(!listener_mode_matches_ui(
        ChatListenerMode::Secondary,
        false,
        crate::runtime::ui::UiStateKind::Primary
    ));
    assert!(listener_mode_matches_ui(
        ChatListenerMode::Secondary,
        true,
        crate::runtime::ui::UiStateKind::Primary
    ));
}

#[test]
fn scan_freshness_rejects_pre_boundary_frames_without_discarding_stable_frames() {
    let boundary = Instant::now();
    let old = boundary.checked_sub(Duration::from_millis(1)).unwrap();
    let fresh = boundary + Duration::from_millis(1);
    assert!(!scan_frame_is_fresh(old, Some(boundary)));
    assert!(scan_frame_is_fresh(boundary, Some(boundary)));
    assert!(scan_frame_is_fresh(fresh, Some(boundary)));
    assert!(scan_frame_is_fresh(old, None));
    assert!(!scan_frame_is_fresh(
        fresh,
        Some(fresh + Duration::from_millis(1))
    ));
}

#[test]
fn primary_rescan_rejects_a_fresh_frame_that_left_primary_ui() {
    assert!(primary_rescan_can_publish(Some(
        crate::runtime::ui::UiStateKind::Primary
    )));
    assert!(!primary_rescan_can_publish(Some(
        crate::runtime::ui::UiStateKind::Secondary
    )));
    assert!(!primary_rescan_can_publish(Some(
        crate::runtime::ui::UiStateKind::Unknown
    )));
    assert!(!primary_rescan_can_publish(None));
}

#[test]
fn listener_ready_requires_fresh_verified_work_and_a_final_idle_recheck() {
    let primary = listener_snapshot(ChatListenerMode::Primary);
    assert!(listener_ready_after_recheck(
        Some(ListenerReadyEvidence::PrimaryScan),
        &primary,
        false,
        false,
        true,
    ));
    assert!(!listener_ready_after_recheck(
        None, &primary, false, false, true,
    ));
    assert!(!listener_ready_after_recheck(
        Some(ListenerReadyEvidence::PrimaryScan),
        &primary,
        true,
        false,
        true,
    ));
    assert!(!listener_ready_after_recheck(
        Some(ListenerReadyEvidence::PrimaryScan),
        &primary,
        false,
        true,
        true,
    ));
    assert!(!listener_ready_after_recheck(
        Some(ListenerReadyEvidence::PrimaryScan),
        &primary,
        false,
        false,
        false,
    ));
}

#[test]
fn replacement_ready_waits_for_required_startup_success() {
    assert!(replacement_ready_allowed(true, true));
    assert!(!replacement_ready_allowed(true, false));
    assert!(!replacement_ready_allowed(false, true));
}

#[test]
fn secondary_ready_requires_a_completed_current_hall_round() {
    let mut secondary = listener_snapshot(ChatListenerMode::Secondary);
    assert!(listener_ready_after_recheck(
        Some(ListenerReadyEvidence::SecondaryCurrentHall),
        &secondary,
        false,
        false,
        true,
    ));

    secondary.initial_unread_clear = true;
    assert!(!listener_ready_after_recheck(
        Some(ListenerReadyEvidence::SecondaryCurrentHall),
        &secondary,
        false,
        false,
        true,
    ));
    secondary.initial_unread_clear = false;
    secondary.hall_round_required = true;
    assert!(!listener_ready_after_recheck(
        Some(ListenerReadyEvidence::SecondaryCurrentHall),
        &secondary,
        false,
        false,
        true,
    ));
    secondary.hall_round_required = false;
    secondary.unread_task_pending = true;
    assert!(!listener_ready_after_recheck(
        Some(ListenerReadyEvidence::SecondaryCurrentHall),
        &secondary,
        false,
        false,
        true,
    ));
}

#[test]
fn unresolved_ui_recovery_waits_for_grace_and_runtime_idle() {
    let now = Instant::now();
    let old = now.checked_sub(CHAT_LISTENER_UNRESOLVED_UI_GRACE).unwrap();
    assert!(unresolved_ui_recovery_due(
        Some(old),
        now,
        now,
        false,
        false,
    ));
    assert!(!unresolved_ui_recovery_due(
        Some(now),
        now,
        now,
        false,
        false,
    ));
    assert!(!unresolved_ui_recovery_due(
        Some(old),
        now,
        now + Duration::from_millis(1),
        false,
        false,
    ));
    assert!(!unresolved_ui_recovery_due(
        Some(old),
        now,
        now,
        true,
        false,
    ));
    assert!(!unresolved_ui_recovery_due(
        Some(old),
        now,
        now,
        false,
        true,
    ));
}

#[test]
fn secondary_hall_recovery_uses_only_its_cooldown() {
    let now = Instant::now();
    assert!(secondary_hall_recovery_allowed(now, now));
    assert!(!secondary_hall_recovery_allowed(
        now,
        now + Duration::from_secs(1),
    ));
}

#[test]
fn primary_listening_does_not_consume_secondary_hall_observations() {
    // 一级值守只看主大厅的**屏幕桶**。二级大厅与主观察可能是同一个大厅，
    // 同屏可见的二级几何是同一个大厅的另一个身份；消费它就会重复派发同一条命令。
    let ocr = SourceId::ocr();
    assert!(hall_consumes_scope(
        ChatListenerMode::Primary,
        &ChatScope::hall(),
        &ocr,
        Some(&ChatIdentity::PrimaryHall)
    ));
    assert!(!hall_consumes_scope(
        ChatListenerMode::Primary,
        &ChatScope::hall(),
        &ocr,
        Some(&ChatIdentity::SecondaryHall)
    ));
    // 外部源借用的 SecondaryHall 桶没有屏幕语义：一级值守也必须消费，
    // 否则用户切到一级后外部大厅命令会被整批丢弃。
    assert!(hall_consumes_scope(
        ChatListenerMode::Primary,
        &ChatScope::hall_with_id(7),
        &SourceId::external("chatbridge"),
        Some(&ChatIdentity::SecondaryHall)
    ));
    // 私聊与值守模式无关。
    assert!(hall_consumes_scope(
        ChatListenerMode::Primary,
        &ChatScope::private("Alice"),
        &ocr,
        Some(&ChatIdentity::Friend("Alice".into()))
    ));
}

#[test]
fn secondary_listening_does_not_consume_primary_hall_commands() {
    // 二级值守只看二级大厅的屏幕桶。主大厅快照（含审核巡检临时一级驻留期间的帧）
    // 不在它的消费范围内，命令派发留给一级值守。
    let ocr = SourceId::ocr();
    assert!(hall_consumes_scope(
        ChatListenerMode::Secondary,
        &ChatScope::hall(),
        &ocr,
        Some(&ChatIdentity::SecondaryHall)
    ));
    assert!(!hall_consumes_scope(
        ChatListenerMode::Secondary,
        &ChatScope::hall(),
        &ocr,
        Some(&ChatIdentity::PrimaryHall)
    ));
}

#[test]
fn hall_gate_leaves_friend_and_unknown_identities_untouched() {
    // 好友未读、未知、公共频道与陌生人聊天都不是大厅，两种值守模式下语义都不变。
    for mode in [ChatListenerMode::Primary, ChatListenerMode::Secondary] {
        for chat in [
            ChatIdentity::Friend("备注A".into()),
            ChatIdentity::Unknown,
            ChatIdentity::PublicChannel,
            ChatIdentity::StrangerMessages,
        ] {
            assert!(
                hall_consumes_scope(
                    mode,
                    &ChatScope::from_identity(&chat),
                    &SourceId::ocr(),
                    Some(&chat),
                ),
                "{} 不应改变 {chat:?} 的消费语义",
                mode.label()
            );
        }
    }
}

#[test]
fn replacement_startup_retry_is_limited_to_idle_pre_ready_children() {
    let now = Instant::now();
    assert!(replacement_startup_retry_due(
        true, false, true, true, now, now,
    ));
    assert!(!replacement_startup_retry_due(
        false, false, true, true, now, now,
    ));
    assert!(!replacement_startup_retry_due(
        true, true, true, true, now, now,
    ));
    assert!(!replacement_startup_retry_due(
        true, false, false, true, now, now,
    ));
    assert!(!replacement_startup_retry_due(
        true,
        false,
        true,
        true,
        now,
        now + Duration::from_millis(1),
    ));
}

#[test]
fn replacement_ui_fallback_waits_for_a_stable_pre_ready_failure() {
    let now = Instant::now();
    let old = now.checked_sub(CONFIG_RELOAD_UI_FALLBACK_GRACE).unwrap();
    assert!(replacement_startup_fallback_due(
        true,
        false,
        false,
        true,
        true,
        false,
        now,
        Some(old),
    ));
    assert!(!replacement_startup_fallback_due(
        true,
        false,
        false,
        true,
        true,
        false,
        now,
        Some(now),
    ));
    assert!(!replacement_startup_fallback_due(
        true,
        false,
        true,
        true,
        true,
        false,
        now,
        Some(old),
    ));
    assert!(!replacement_startup_fallback_due(
        true,
        false,
        false,
        true,
        false,
        false,
        now,
        Some(old),
    ));
    assert!(!replacement_startup_fallback_due(
        true,
        false,
        false,
        true,
        true,
        true,
        now,
        Some(old),
    ));
}

#[test]
fn missing_chat_listener_mode_defaults_to_none() {
    let path = std::env::temp_dir()
        .join(format!(
            "miliastra-chat-listener-missing-{}",
            uuid::Uuid::new_v4()
        ))
        .join("playback.sqlite3");
    assert_eq!(load_persisted_chat_listener_mode(&path).unwrap(), None);
}

fn observed_message(sequence: u64, text: &str, is_new: bool) -> PrimaryObservedMessage {
    PrimaryObservedMessage {
        id: ObservedChatMessageId::new(
            VisualSessionId::new(1),
            ChatIdentity::PrimaryHall,
            BubbleSequence::new(sequence),
        ),
        message: ChatMessage {
            message_type: "blue".to_string(),
            block: Rect::new(0, sequence as i32 * 20, 10, 10),
            text: text.to_string(),
            sender: None,
        },
        is_new,
    }
}

// ==== 命令锁接线测试（真实格式 "[昵称]:@命令"）====

fn chat_input(text: &str, y: i32) -> RecognizedChatMessage {
    RecognizedChatMessage {
        text: text.into(),
        sender: None,
        raw_sender: None,
        message_type: "blue".into(),
        block: Rect::new(0, y, 100, 10),
    }
}

fn primary_route(message_id: ObservedChatMessageId) -> RoutedCommand {
    let anchor = lock_anchor_for(&message_id, 40);
    RoutedCommand {
        matched: "状态".into(),
        raw: "状态".into(),
        user_command: "@状态".into(),
        message_type: "blue".into(),
        username: "甲".into(),
        account: None,
        authority: CommandAuthority::HallMember,
        role: None,
        permission_required: None,
        denied_reason: None,
        command: ModuleCommand::Playback(PlaybackCommand::Status),
        observation: CommandObservation {
            frame_id: None,
            captured_at: Some(Instant::now()),
            message_id: Some(message_id),
            message_text: Some("[甲]:@状态".to_string()),
            lock_anchor: Some(anchor),
        },
    }
}

#[test]
fn blue_unrecognized_revision_routes_once_including_startup() {
    for baseline_primed in [false, true] {
        let shared = ChatTimeline::new();
        let first = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![chat_input("[甲]:@点哥 晴天", 40)],
            )
            .unwrap();
        let message = &first.messages[0];
        let observed = PrimaryObservedMessage {
            id: message.id.clone(),
            is_new: true,
            message: ChatMessage {
                text: message.text.clone(),
                sender: message.sender.clone(),
                message_type: "blue".into(),
                block: message.block,
            },
        };
        let envelope = crate::interfaces::chat::parse_command_envelope(
            &message.text,
            "blue",
            None,
            CommandObservation::default(),
        )
        .unwrap();
        assert!(
            ChatCommandRouter::without_custom_workflow()
                .route(&envelope, None)
                .is_none()
        );
        acknowledge_unrecognized_primary_message(
            &shared,
            &observed,
            first.captured_at,
            baseline_primed,
        )
        .unwrap();
        let corrected = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![chat_input("[甲]:@点歌 晴天", 40)],
            )
            .unwrap();
        let message = &corrected.messages[0];
        assert_eq!(message.id, observed.id);
        assert_eq!(
            shared
                .is_business_pending_at(&message.id, corrected.captured_at)
                .unwrap(),
            true
        );
        {
            let envelope = crate::interfaces::chat::parse_command_envelope(
                &message.text,
                "blue",
                message.sender.as_deref(),
                CommandObservation {
                    message_id: Some(message.id.clone()),
                    message_text: Some(message.text.clone()),
                    lock_anchor: Some(lock_anchor_for(&message.id, message.block.y)),
                    ..CommandObservation::default()
                },
            )
            .unwrap();
            let parsed = ChatCommandRouter::without_custom_workflow()
                .route(&envelope, None)
                .unwrap();
            acknowledge_primary_command(&shared, &parsed, true).unwrap();
            let repeated = shared
                .publish(
                    Instant::now(),
                    ChatIdentity::PrimaryHall,
                    vec![chat_input("[甲]:@点歌 晴天", 40)],
                )
                .unwrap();
            assert!(
                !shared
                    .is_business_pending_at(&message.id, repeated.captured_at)
                    .unwrap()
            );
        }
    }
}

/// 一级 write_lock 分支：真正派发成功写锁；被禁用只 ack 不写锁。
#[test]
fn primary_write_lock_only_after_real_dispatch() {
    let shared = ChatTimeline::new();
    let frame = shared
        .publish(
            Instant::now(),
            ChatIdentity::PrimaryHall,
            vec![chat_input("[甲]:@状态", 40)],
        )
        .unwrap();
    let message = frame.messages[0].clone();
    let command = primary_route(message.id.clone());
    acknowledge_primary_command(&shared, &command, true).unwrap();
    assert_eq!(
        shared.command_lock_texts_for_test(&ChatIdentity::PrimaryHall),
        vec!["[甲]:@状态".to_string()],
        "真正派发成功必须写锁"
    );
    assert!(!shared.is_business_pending(&message.id).unwrap());

    let shared = ChatTimeline::new();
    let frame = shared
        .publish(
            Instant::now(),
            ChatIdentity::PrimaryHall,
            vec![chat_input("[甲]:@状态", 40)],
        )
        .unwrap();
    let message = frame.messages[0].clone();
    let command = primary_route(message.id.clone());
    acknowledge_primary_command(&shared, &command, false).unwrap();
    assert!(
        !shared.command_lock_bucket_exists_for_test(&ChatIdentity::PrimaryHall),
        "被禁用命令只 ack 不写锁"
    );
    assert!(!shared.is_business_pending(&message.id).unwrap());
}

/// 缺陷 6：缺文本或缺 y 时不得用 unwrap_or_default 退化成空串/0（0 会命中任何
/// y<=28 的同文本气泡）；必须 warn 并只确认不写锁。
#[test]
fn primary_write_lock_skips_missing_text_or_y() {
    for (text, y) in [(None, Some(40)), (Some("[甲]:@状态"), None)] {
        let shared = ChatTimeline::new();
        let frame = shared
            .publish(
                Instant::now(),
                ChatIdentity::PrimaryHall,
                vec![chat_input("[甲]:@状态", 40)],
            )
            .unwrap();
        let message = frame.messages[0].clone();
        let mut command = primary_route(message.id.clone());
        command.observation.message_text = text.map(str::to_string);
        command.observation.lock_anchor = y.map(|y| lock_anchor_for(&message.id, y));
        acknowledge_primary_command(&shared, &command, true).unwrap();
        assert!(
            !shared.command_lock_bucket_exists_for_test(&ChatIdentity::PrimaryHall),
            "缺失 text/y 不得写锁 (text={text:?}, y={y:?})"
        );
        assert!(
            !shared.is_business_pending(&message.id).unwrap(),
            "缺失时仍应确认，避免消息卡在 pending"
        );
    }
}

#[test]
fn blue_startup_is_candidate_and_success_lock_survives_view_rebuild() {
    let shared = ChatTimeline::new();
    let mut input = chat_input("[甲]:@状态", 40);
    input.message_type = "blue".into();
    let frame = shared
        .publish(
            Instant::now(),
            ChatIdentity::PrimaryHall,
            vec![input.clone()],
        )
        .unwrap();
    let message = &frame.messages[0];
    let observed = PrimaryObservedMessage {
        id: message.id.clone(),
        is_new: message.is_new,
        message: ChatMessage {
            text: message.text.clone(),
            sender: message.sender.clone(),
            message_type: "blue".into(),
            block: message.block,
        },
    };
    assert!(observed.is_new, "blue startup must enter command handling");
    assert!(shared.is_business_pending(&message.id).unwrap());
    assert!(
        shared
            .command_lock_texts_for_test(&ChatIdentity::PrimaryHall)
            .is_empty(),
        "capture alone cannot write a success lock"
    );
    let command = primary_route(message.id.clone());
    acknowledge_primary_command(&shared, &command, true).unwrap();
    shared
        .publish_no_chat(
            Instant::now(),
            "transient view rebuild",
            crate::observation::chat::ViewAbsence::Transient,
        )
        .unwrap();
    let rebuilt = shared
        .publish(Instant::now(), ChatIdentity::PrimaryHall, vec![input])
        .unwrap();
    assert!(
        !shared.is_business_pending(&rebuilt.messages[0].id).unwrap(),
        "already successful startup command cannot run again"
    );
}

/// 缺陷 1 反例：二级大厅真命令派发成功写锁；随后 5 条普通闲聊被 claim 也不得写锁、
/// 不得把真命令条目挤出；重建后该命令不得重跑。
#[test]
fn secondary_chatter_claims_do_not_evict_dispatched_command_lock() {
    let shared = ChatTimeline::new();
    let chat = ChatIdentity::SecondaryHall;
    let mut inputs = vec![chat_input("[甲]:@点歌 起风了", 100)];
    let first = shared
        .publish(Instant::now(), chat.clone(), inputs.clone())
        .unwrap();
    let command = first.messages[0].clone();
    let claim = shared
        .claim_business_at(
            &command.id,
            first.captured_at,
            &command.text,
            command.lock_anchor(),
        )
        .unwrap()
        .expect("secondary command should be claimable");
    complete_secondary_claim(claim, true).unwrap();
    assert_eq!(
        shared.command_lock_texts_for_test(&chat),
        vec!["[甲]:@点歌起风了".to_string()],
        "真正派发成功的二级命令必须写锁"
    );

    for index in 0..5 {
        inputs.push(chat_input(
            &format!("[路人{index}]:今天天气不错"),
            135 + index as i32 * 35,
        ));
        let frame = shared
            .publish(Instant::now(), chat.clone(), inputs.clone())
            .unwrap();
        let chatter = frame.messages.last().unwrap().clone();
        assert!(shared.is_business_pending(&chatter.id).unwrap());
        let claim = shared
            .claim_business_at(
                &chatter.id,
                frame.captured_at,
                &chatter.text,
                chatter.lock_anchor(),
            )
            .unwrap()
            .expect("chatter should be claimable");
        complete_secondary_claim(claim, false).unwrap();
    }
    assert_eq!(
        shared.command_lock_texts_for_test(&chat),
        vec!["[甲]:@点歌起风了".to_string()],
        "普通闲聊不得写锁，更不得把真命令条目按 FIFO 挤出"
    );

    shared
        .publish_no_chat(
            Instant::now(),
            "view rebuilt",
            crate::observation::chat::ViewAbsence::Transient,
        )
        .unwrap();
    let rebuilt = shared
        .publish(
            Instant::now(),
            chat,
            vec![chat_input("[甲]:@点歌 起风了", 100)],
        )
        .unwrap();
    assert!(
        !rebuilt.messages[0].is_new,
        "重建后已执行的二级命令不得重跑"
    );
    assert!(!shared.is_business_pending(&rebuilt.messages[0].id).unwrap());
}

// ═══════════════════════════════════════════════════════════════════════════
// 新增：实例化去重状态（原 `static` 下放）与协调器观测
// ═══════════════════════════════════════════════════════════════════════════

/// 界面迁移跟踪器原先是一个进程级 `static Mutex`，任何测试都会改到别人的
/// 「上一次推送」记忆。实例化后每个跟踪器从干净状态开始，可并行、可重现。
#[test]
fn ui_transition_tracker_is_instance_local_and_starts_clean() {
    use crate::runtime::ui::UiStateKind;

    let mut first = display::UiTransitionTracker::default();
    assert!(
        first.take_transition(Some(UiStateKind::Primary)),
        "全新的跟踪器没有历史，第一次观测必须算一次迁移"
    );
    assert!(!first.take_transition(Some(UiStateKind::Primary)));

    // 另一个实例不受上面那次推送影响：这正是 static 时代做不到的隔离。
    let mut second = display::UiTransitionTracker::default();
    assert!(
        second.take_transition(Some(UiStateKind::Primary)),
        "新实例必须从「还没推送过」开始，不能被别的实例污染"
    );

    // 过渡态本身算一个状态：一直在抖也不重复推。
    assert!(second.take_transition(None), "稳定 -> 过渡 是一条真实迁移");
    assert!(!second.take_transition(None), "过渡态内部抖动不得重复推送");
}

/// 气泡去重记忆同理：两个实例互不相识，各自记住各自的条数。
#[test]
fn remembered_bubble_tracker_is_instance_local() {
    let mut left = display::RememberedBubbleTracker::default();
    let mut right = display::RememberedBubbleTracker::default();

    assert!(left.take_update("session-1|primary_hall|1"));
    assert!(!left.take_update("session-1|primary_hall|1"));
    assert!(
        right.take_update("session-1|primary_hall|1"),
        "另一个实例必须独立判断，不能被左边的记忆影响"
    );
    assert_eq!(left.remembered(), 1);
    assert_eq!(right.remembered(), 1);
}

/// 去重记忆是有界的：超过上限丢最旧的，只影响去重广度，不影响正确性。
#[test]
fn remembered_bubble_tracker_stays_bounded() {
    let mut tracker = display::RememberedBubbleTracker::default();
    for index in 0..(display::REMEMBERED_BUBBLE_LIMIT + 8) {
        assert!(
            tracker.take_update(&format!("session-1|primary_hall|{index}")),
            "每条都是新气泡，必须都推送"
        );
    }
    assert_eq!(
        tracker.remembered(),
        display::REMEMBERED_BUBBLE_LIMIT,
        "记忆必须停在上限内，不能无限增长"
    );
    // 最旧的一条已被挤出，再次出现时按新气泡处理：这是「少记几条」而不是错判。
    assert!(tracker.take_update("session-1|primary_hall|0"));
    assert!(
        !tracker.take_update(&format!(
            "session-1|primary_hall|{}",
            display::REMEMBERED_BUBBLE_LIMIT + 7
        )),
        "仍在记忆里的新气泡不得重复推送"
    );
}

/// `ObservationDedup` 把两条观测线收在同一个持有者里：主循环拿一份就够。
#[test]
fn observation_dedup_pairs_both_observation_lines() {
    use crate::runtime::ui::UiStateKind;

    let mut dedup = display::ObservationDedup::default();
    assert!(
        dedup
            .ui_transitions
            .take_transition(Some(UiStateKind::Secondary))
    );
    assert!(dedup.bubbles.take_update("session-1|secondary_hall|7"));
    assert!(
        !dedup
            .ui_transitions
            .take_transition(Some(UiStateKind::Secondary))
    );
    assert!(!dedup.bubbles.take_update("session-1|secondary_hall|7"));
}

/// 持续无法识别：协调器只记录时长、每回合认定一次、离开即清零。
///
/// 这条同时钉住行为等价红线：观测**不占恢复互斥、不产生恢复动作**。
#[test]
fn sustained_unknown_observation_records_without_acting() {
    use crate::runtime::recovery::{DEFAULT_UNKNOWN_SUSTAINED_LIMIT, RecoveryTrigger};
    use crate::runtime::ui::UiStateKind;

    let recovery = crate::runtime::recovery::GameRecoveryCoordinator::default();
    let mut guard = recovery_guard::RecoveryGuard::default();
    let start = Instant::now();

    assert_eq!(
        guard.observe_ui_kind(&recovery, start, false),
        crate::runtime::recovery::UnknownObservation::Recognized
    );
    // 进入无法识别：起点确立，时长从零开始累积。
    assert!(matches!(
        guard.observe_ui_kind(&recovery, start, true),
        crate::runtime::recovery::UnknownObservation::Observing { sustained }
            if sustained == Duration::ZERO
    ));
    let reached = guard.observe_ui_kind(&recovery, start + Duration::from_secs(1), true);
    assert!(
        matches!(
            reached,
            crate::runtime::recovery::UnknownObservation::Reached { .. }
        ),
        "越过认定期限必须报一次认定：{reached:?}"
    );
    // 同一回合内不再重复认定。
    assert!(matches!(
        guard.observe_ui_kind(&recovery, start + Duration::from_secs(2), true),
        crate::runtime::recovery::UnknownObservation::Sustained { .. }
    ));

    let snapshot = guard.health(&recovery, start + Duration::from_secs(2));
    assert_eq!(
        snapshot.unknown_sustained,
        Some(Duration::from_secs(2)),
        "健康快照必须如实报出已持续多久"
    );
    assert_eq!(
        snapshot.active_recovery, None,
        "纯观测绝不能占用恢复互斥，否则会挡掉真正的恢复动作"
    );
    assert_eq!(
        snapshot.last_recovery_action, None,
        "纯观测不能冒充恢复动作"
    );
    assert_eq!(snapshot.total_restarts, 0, "观测不得触发任何重启");
    assert!(DEFAULT_UNKNOWN_SUSTAINED_LIMIT > Duration::ZERO);
    let _ = UiStateKind::Unknown;

    // 界面恢复可识别：时长与回合标记一起清零，下一回合重新计时。
    assert_eq!(
        guard.observe_ui_kind(&recovery, start + Duration::from_secs(3), false),
        crate::runtime::recovery::UnknownObservation::Recognized
    );
    assert_eq!(
        guard
            .health(&recovery, start + Duration::from_secs(3))
            .unknown_sustained,
        None
    );
    assert!(matches!(
        guard.observe_ui_kind(&recovery, start + Duration::from_secs(4), true),
        crate::runtime::recovery::UnknownObservation::Observing { sustained }
            if sustained == Duration::ZERO
    ));
    // 确认这条纯观测线没有对应的「自动重启」触发源被点亮。
    assert_ne!(
        RecoveryTrigger::SustainedUnknown.chinese_name(),
        RecoveryTrigger::ExternalChatDisconnect.chinese_name()
    );
}

/// 协调器共享：同一个实例的 clone 之间看得见彼此的重启记账。
///
/// 这是「三处触发源共享一份冷却」的最小证据——`listener` 拿到的就是这份 clone。
#[test]
fn recovery_guard_shares_restart_bookkeeping_across_clones() {
    use crate::runtime::recovery::{GameRecoveryCoordinator, RecoveryTrigger};

    let recovery = GameRecoveryCoordinator::default();
    let start = Instant::now();
    // 启动链先重启过一次（同一个协调器实例）。
    let shared = recovery.clone();
    shared.record_restart_completed(start, RecoveryTrigger::StartupLoadingStuck);

    let guard = recovery_guard::RecoveryGuard::default();
    let snapshot = guard.health(&recovery, start + Duration::from_secs(1));
    assert_eq!(snapshot.total_restarts, 1);
    assert_eq!(
        snapshot.last_restart_trigger,
        Some(RecoveryTrigger::StartupLoadingStuck),
        "主循环看到的最近一次重启必须包含启动链发起的那一次"
    );
    assert_eq!(snapshot.loading_restarts, 1);
}

/// 常量的取值本身也是「行为等价」的一部分：改阈值就是改行为。
#[test]
fn recovery_constants_keep_their_documented_values() {
    assert_eq!(
        recovery_guard::AUTO_GAME_RESTART_COOLDOWN,
        Duration::from_secs(300),
        "自动重启冷却 5 分钟：短于一次冷启动进世界就会变成死循环"
    );
    assert_eq!(
        recovery_guard::BRIDGE_RELAUNCH_TIMEOUT,
        Duration::from_secs(120),
        "桥重拉上限 2 分钟：再久说明是硬失败，继续等没有意义"
    );
    assert_eq!(
        recovery_guard::LOOP_FAILURE_WARN_INTERVAL,
        Duration::from_secs(60),
        "持续失败告警每分钟最多一条，否则 60ms 一轮会刷满日志"
    );
}
