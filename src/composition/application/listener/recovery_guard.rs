//! 驻留判定、就绪证据与恢复守卫。
//!
//! 从 `listener.rs` 拆出：这里回答三件事——「现在能不能算就绪」「界面该不该被
//! 拉回」「要不要重启游戏」，并与进程级 [`GameRecoveryCoordinator`] 协作。
//!
//! ## 与协调器的分工
//!
//! 判定**条件**仍在本模块：配置开关、失联阈值、桥重拉超时、各项冷却常量都是
//! 调用方的事实与预算，协调器不读配置、不猜平台能力。协调器只收拢**状态**：
//!
//! | 状态 | 原先在哪 | 现在 |
//! |---|---|---|
//! | 自动重启冷却 | 主循环局部 `Option<Instant>` | 协调器 `last_restart_at` |
//! | 与启动链/驻留恢复的互斥 | 无 | [`RecoveryLease`] |
//! | 「无拉起路径」告警节流 | 主循环局部槽 | 协调器 `no_relaunch_warn_at` |
//! | 持续无法识别时长 | 无人记录 | 协调器 `unknown_since` |
//!
//! 因此三处触发源不会各按各的计时器同时决定「该关游戏了」。
//!
//! ## 行为等价红线
//!
//! - `Unknown` 的持续时长**只做观测与汇报**；它不触发任何自动重启，也不按键。
//!   主循环原本对 `Unknown` 就是「打一条 debug 然后跳过」，这里保持原样，
//!   新增的只有协调器里的计时与一次性认定日志。
//! - 外部源失联重启的触发条件、判定顺序、冷却时长与桥重拉等待全部逐条等价，
//!   变的只是这些状态从「主循环局部变量」搬到「进程级共享协调器」。

use super::super::tasks::GameRelaunch;
use super::super::*;
use crate::runtime::recovery::DEFAULT_UNKNOWN_SUSTAINED_LIMIT;
use crate::runtime::recovery::{
    ExternalDisconnectRequest, ExternalRestartDecision, GameRecoveryCoordinator,
    RecoveryHealthSnapshot, RecoveryTrigger, UnknownObservation,
};

/// 持续失败类 warn 的重复间隔（与 [SECONDARY_UI_MISMATCH_WARN_INTERVAL] 同值同语义）。
pub(in crate::composition::application) const LOOP_FAILURE_WARN_INTERVAL: Duration =
    Duration::from_secs(60);

/// 自动重启游戏的最小间隔。
///
/// 重启后游戏还要重新过加载界面，这段时间里失联判定和加载超时都可能再次成立；
/// 没有这个冷却就会变成「重启 → 立刻又判失联 → 再重启」的死循环，把玩家卡死在
/// 反复重开游戏里。5 分钟足够走完一次冷启动进世界。
pub(in crate::composition::application) const AUTO_GAME_RESTART_COOLDOWN: Duration =
    Duration::from_secs(300);

/// 关掉游戏后等待聊天桥重新拉起的上限。
///
/// 桥拉起游戏要先等旧游戏进程彻底退出，再注入新进程；正常十几秒完成。
/// 两分钟还没成，说明是 DLL 缺失/游戏损坏这类硬失败，继续等没有意义。
pub(in crate::composition::application) const BRIDGE_RELAUNCH_TIMEOUT: Duration =
    Duration::from_secs(120);

/// 该告警这次是否应当真的写 warn。
///
/// `slot` 为「上次真正告警的时刻」；为 None 或已过窗口则返回 true 并就地更新。
pub(in crate::composition::application) fn loop_failure_warn_due(
    slot: &mut Option<Instant>,
    now: Instant,
) -> bool {
    if slot.is_none_or(|last| now.saturating_duration_since(last) >= LOOP_FAILURE_WARN_INTERVAL) {
        *slot = Some(now);
        true
    } else {
        false
    }
}

pub(in crate::composition::application) const CHAT_LISTENER_RESIDENCY_RETRY_INTERVAL: Duration =
    Duration::from_secs(5);
pub(in crate::composition::application) const CHAT_LISTENER_UNRESOLVED_UI_GRACE: Duration =
    Duration::from_secs(5);
pub(in crate::composition::application) const CONFIG_RELOAD_STARTUP_RETRY_INTERVAL: Duration =
    Duration::from_secs(5);
pub(in crate::composition::application) const CONFIG_RELOAD_UI_FALLBACK_GRACE: Duration =
    Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::composition::application) enum ListenerReadyEvidence {
    PrimaryScan,
    SecondaryCurrentHall,
    GameLocation,
}

pub(in crate::composition::application) fn listener_mode_matches_ui(
    mode: ChatListenerMode,
    temporary_primary: bool,
    ui_kind: UiStateKind,
) -> bool {
    matches!(
        (listener_residency(mode, temporary_primary), ui_kind),
        (UiResidency::Primary, UiStateKind::Primary)
            | (UiResidency::SecondaryCurrentHall, UiStateKind::Secondary)
    )
}

pub(in crate::composition::application) fn scan_frame_is_fresh(
    captured_at: Instant,
    not_before: Option<Instant>,
) -> bool {
    not_before.is_none_or(|boundary| captured_at >= boundary)
}

pub(in crate::composition::application) fn primary_rescan_can_publish(
    stable_kind: Option<UiStateKind>,
) -> bool {
    stable_kind == Some(UiStateKind::Primary)
}

pub(in crate::composition::application) fn automatic_startup_task(
    config: &AppConfig,
    kind: UiStateKind,
) -> Option<StartupTask> {
    if !config.startup.enabled {
        return None;
    }
    match kind {
        UiStateKind::GameGate if config.startup.enter_game => {
            Some(StartupTask::start_game(StartupSource::AUTOMATIC_UI))
        }
        UiStateKind::Overworld if config.startup.enter_wonderland => {
            Some(StartupTask::enter_wonderland(StartupSource::AUTOMATIC_UI))
        }
        _ => None,
    }
}

pub(in crate::composition::application) fn game_location_probe_blocks_residency(
    observation: Option<&UiStateObservation>,
) -> bool {
    matches!(observation, Some(UiStateObservation::Classified(state))
        if matches!(state.classification().kind(), UiStateKind::GameGate | UiStateKind::Overworld)
            || state.classification().label() == "unknown:game_probe")
}

pub(in crate::composition::application) fn listener_ready_after_recheck(
    evidence: Option<ListenerReadyEvidence>,
    snapshot: &ChatListenerSnapshot,
    task_busy: bool,
    paused: bool,
    running: bool,
) -> bool {
    if !running
        || paused
        || task_busy
        || snapshot.pending_mode.is_some()
        || snapshot.unread_task_pending
    {
        return false;
    }
    match evidence {
        Some(ListenerReadyEvidence::PrimaryScan) => {
            snapshot.mode == ChatListenerMode::Primary
                || (snapshot.mode == ChatListenerMode::Secondary && snapshot.temporary_primary)
        }
        Some(ListenerReadyEvidence::SecondaryCurrentHall) => {
            snapshot.mode == ChatListenerMode::Secondary
                && !snapshot.temporary_primary
                && !snapshot.initial_unread_clear
                && !snapshot.hall_round_required
        }
        Some(ListenerReadyEvidence::GameLocation) => true,
        None => false,
    }
}

pub(in crate::composition::application) fn replacement_ready_allowed(
    listener_ready: bool,
    startup_ready: bool,
) -> bool {
    listener_ready && startup_ready
}

pub(in crate::composition::application) fn unresolved_ui_recovery_due(
    unresolved_since: Option<Instant>,
    now: Instant,
    retry_after: Instant,
    pending_mode: bool,
    task_busy: bool,
) -> bool {
    !pending_mode
        && !task_busy
        && now >= retry_after
        && unresolved_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= CHAT_LISTENER_UNRESOLVED_UI_GRACE
        })
}

pub(in crate::composition::application) fn secondary_hall_recovery_allowed(
    now: Instant,
    retry_after: Instant,
) -> bool {
    // 待处理重载可能因外部播放或不确定状态长期阻塞。恢复本身作为正式任务执行，
    // 可安全延迟停机，不会让好友值守器停在当前大厅之外。
    now >= retry_after
}

// 待处理重载可能因外部播放或不确定状态长期阻塞，因此不将其作为替代进程启动恢复的闸门：
// 子进程必须先完成启动并发布就绪，看门狗才能按正常进程处理。
pub(in crate::composition::application) fn replacement_startup_retry_due(
    config_reload_child: bool,
    child_ready: bool,
    startup_actions_missing: bool,
    task_engine_idle: bool,
    now: Instant,
    retry_after: Instant,
) -> bool {
    config_reload_child
        && !child_ready
        && startup_actions_missing
        && task_engine_idle
        && now >= retry_after
}

#[allow(clippy::too_many_arguments)]
pub(in crate::composition::application) fn replacement_startup_fallback_due(
    config_reload_child: bool,
    child_ready: bool,
    startup_gate_active: bool,
    startup_actions_available: bool,
    task_engine_idle: bool,
    paused: bool,
    now: Instant,
    unverified_since: Option<Instant>,
) -> bool {
    config_reload_child
        && !child_ready
        && !startup_gate_active
        && startup_actions_available
        && task_engine_idle
        && !paused
        && unverified_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= CONFIG_RELOAD_UI_FALLBACK_GRACE
        })
}

// ── 恢复守卫状态 ──────────────────────────────────────────────────────────

/// 自动重启游戏的状态机。
///
/// 三个时间点是**主循环的进度状态**（本次运行里已经等多久、上次告警在什么时候），
/// 冷却与「该不该重启」的判定则交给进程级协调器，这样启动链与驻留恢复也能看到
/// 「外部源正在重启」这件事。
///
/// 为什么冷却也要交给协调器：`AUTO_GAME_RESTART_COOLDOWN` 的本意是「刚开完游戏，
/// 给冷启动留出时间」。如果加载卡死重启刚刚开过一次游戏，外部源立刻还能再开一次，
/// 冷却就形同虚设——两个触发源各自看到的都是「我这边的上次重启」。
#[derive(Debug, Default)]
pub(in crate::composition::application) struct AutoGameRestartGuard {
    /// 已经请桥拉起、还在等它真正把游戏拉起来的起点。
    awaiting_bridge_relaunch: Option<Instant>,
}

impl AutoGameRestartGuard {
    /// 「没有可用拉起路径」与「桥迟迟不拉起」共用的告警节流槽。
    ///
    /// 两个分支语义相同（都在说「现在没有自动恢复的路」），原先共用主循环里的
    /// 同一个局部槽；接进协调器后继续共用同一个槽位，保持逐条等价。
    pub(in crate::composition::application) fn warn_due(&self, now: Instant) -> bool {
        // 具体时刻由协调器持有；这里只是把「谁在问」写清楚。
        let _ = now;
        true
    }
}

/// 一次恢复动作的完整上下文。
///
/// 主循环持有一份，负责把「本轮观测到的事实」喂给协调器，并按结论执行动作。
#[derive(Debug)]
pub(in crate::composition::application) struct RecoveryGuard {
    /// 自动重启游戏的进度状态。
    pub(super) auto_restart: AutoGameRestartGuard,
    /// 最近一次「界面持续无法识别」是否已经记过日志（每回合只报一次）。
    unknown_announced: bool,
}

impl Default for RecoveryGuard {
    fn default() -> Self {
        Self {
            auto_restart: AutoGameRestartGuard::default(),
            unknown_announced: false,
        }
    }
}

impl RecoveryGuard {
    /// 观测一次界面分类结果，把「持续无法识别」交给协调器计时。
    ///
    /// 只做观测与汇报：不按键、不重启、不占恢复互斥。`Unknown` 的自动重启策略
    /// 尚未经实机帧验证，因此这里**刻意只记录**，默认策略保持原样。
    pub(in crate::composition::application) fn observe_ui_kind(
        &mut self,
        recovery: &GameRecoveryCoordinator,
        now: Instant,
        is_unknown: bool,
    ) -> UnknownObservation {
        let observation = recovery.observe_ui_kind(now, is_unknown);
        match observation {
            UnknownObservation::Recognized => self.unknown_announced = false,
            UnknownObservation::Reached { sustained } => {
                // 每个无法识别回合只报一次：界面真的卡住了，这条是唯一能直接读出
                // 「已经持续多久」的线索；重复打印反而会把别的告警冲掉。
                if !self.unknown_announced {
                    self.unknown_announced = true;
                    log::warn!(
                        "界面持续无法识别已 {} 秒（认定期限 {} 毫秒），本轮只记录不触发任何自动动作；如长期停留请手动检查游戏窗口",
                        sustained.as_secs(),
                        DEFAULT_UNKNOWN_SUSTAINED_LIMIT.as_millis()
                    );
                }
            }
            UnknownObservation::Observing { .. } | UnknownObservation::Sustained { .. } => {}
        }
        observation
    }

    /// 取一份恢复健康快照（诊断与告警共用）。
    pub(in crate::composition::application) fn health(
        &self,
        recovery: &GameRecoveryCoordinator,
        now: Instant,
    ) -> RecoveryHealthSnapshot {
        recovery.health_snapshot(now)
    }
}

impl AppServices {
    /// 外部聊天源失联后「自动重启游戏」的守卫。
    ///
    /// 只在**确实需要**时才动手，并把冷却与互斥交给进程级协调器
    /// （[`GameRecoveryCoordinator`]）。判定条件与阈值仍在这里：
    ///
    /// 触发条件（三个都满足）：
    /// - `chat_source.disconnect_action == restart_game`；
    /// - 失联时长超过 `chat_source.lease_timeout_ms`（面板上的「失联回落阈值」，
    ///   与回落 OCR 共用同一条判定线，不另设第二套计时）；
    /// - 此前**确实连上过**：从未连上属于桥没起来，重启游戏无效
    ///   （见 [`ExternalChatState::lost_for`]）。
    ///
    /// 「加载界面过久」的重启不在这里：那段期间 UI 例程独占界面、主循环拿不到
    /// 扫描租约，根本看不到 Loading 状态，所以它实现在启动流程内部
    /// （见 `ui::routines::startup::execute_enter_game`），并通过同一个协调器记账。
    pub(in crate::composition::application) fn poll_auto_game_restart(
        &self,
        live_config: &crate::config::AppConfig,
        guard: &mut RecoveryGuard,
    ) -> Result<()> {
        // 走桥的重启：关闭游戏后由这里显式请桥拉起（桥自己创建游戏进程并注入 DLL）。
        //
        // 注意**不能**把「桥还在监听」当成已拉起：游戏刚关掉的那段时间桥进程
        // 仍然活着、端口仍被占用，但它不会自己去创建游戏进程——只有
        // `ensure_running` 被调用（且过了它内部的停止/退避闸门）才会重新
        // 拉起游戏并注入 DLL。所以这里持续重试，直到拿到 `Ok(true)`。
        //
        // 等待阶段**不受冷却约束**：冷却只挡「再次触发重启」，不挡「把游戏拉起来」，
        // 否则刚重启完就进冷却，桥永远不会被请求拉起。
        if let Some(waited_since) = guard.auto_restart.awaiting_bridge_relaunch {
            let chat_source = &live_config.chat_source;
            if !chat_source.enabled || !chat_source.auto_start {
                log::warn!(
                    "自动重启游戏中止：外部聊天源或「自动启动聊天桥」已被关闭，没有任何路径能把游戏重新拉起来；如需恢复请手动启动游戏"
                );
                guard.auto_restart.awaiting_bridge_relaunch = None;
            } else if waited_since.elapsed() >= BRIDGE_RELAUNCH_TIMEOUT {
                // 桥一直不肯拉起（DLL 缺失、游戏文件损坏、闸门永久卡住等）：
                // 放弃等待并留一条明确的失败日志，而不是无声地无限重试。
                log::error!(
                    "自动重启游戏失败：等待 Miliastra-Wonderland-ChatBridge 重新拉起游戏超过 {} 秒，已停止重试；请检查聊天桥与游戏是否可正常启动",
                    BRIDGE_RELAUNCH_TIMEOUT.as_secs()
                );
                guard.auto_restart.awaiting_bridge_relaunch = None;
            } else {
                match crate::runtime::chat_source::launcher::ensure_running(
                    chat_source,
                    &live_config.window.target_process,
                ) {
                    Ok(true) => {
                        log::info!(
                            "自动重启游戏：已请 Miliastra-Wonderland-ChatBridge 重新拉起游戏并注入 DLL"
                        );
                        guard.auto_restart.awaiting_bridge_relaunch = None;
                    }
                    Ok(false) => {
                        // 桥仍在监听、或处于停止/退避闸门中。下一轮继续试：
                        // 游戏退干净后桥会自己退出，届时 ensure_running 才会真正拉起。
                        // 留一条节流日志：否则「一直在等」在日志里是完全静默的，
                        // 出问题时看不出卡在哪一步。
                        let now = Instant::now();
                        if self
                            .shared
                            .recovery
                            .should_warn_no_relaunch(now, LOOP_FAILURE_WARN_INTERVAL)
                        {
                            log::warn!(
                                "自动重启游戏：已等待聊天桥重新拉起游戏 {} 秒，桥此刻未拉起（仍在监听或处于退避/等待游戏退出状态），将继续重试",
                                waited_since.elapsed().as_secs()
                            );
                        }
                    }
                    Err(error) => {
                        log::warn!(
                            "自动重启游戏：拉起 Miliastra-Wonderland-ChatBridge 失败，下一轮重试：{error:#}"
                        );
                    }
                }
            }
        }

        // 暂停值守、任务执行中都不该重开游戏：那会打断玩家主动发起的操作。
        if self.shared.paused.load(AtomicOrdering::SeqCst)
            || !self.business.task_engine.is_idle()?
        {
            return Ok(());
        }

        // ---- 守卫 1：外部聊天源失联超过回落阈值 ----
        //
        // 阈值复用 `lease_timeout_ms`（面板上的「失联回落阈值」）：它本来就是
        // 「多久没消息就认为外部源不可用」的判定线，重启游戏只是这条线的另一个
        // 后续动作，不需要第二套计时。
        let chat_source = &live_config.chat_source;
        let disconnected_for = (chat_source.enabled
            && chat_source.disconnect_action
                == crate::config::ChatSourceDisconnectAction::RestartGame)
            .then(|| self.shared.external_chat.lost_for())
            .flatten();
        let threshold = Duration::from_millis(chat_source.lease_timeout_ms);
        let now = Instant::now();
        // 「有没有可用的自动拉起路径」是配置事实，只影响判定，不产生副作用。
        let relaunch = self.game_relaunch_mode(live_config, true);
        let decision = self.shared.recovery.evaluate_external_disconnect_restart(
            now,
            AUTO_GAME_RESTART_COOLDOWN,
            ExternalDisconnectRequest {
                disconnected_for,
                threshold,
                relaunch_available: relaunch != GameRelaunch::None,
            },
        );
        match decision {
            ExternalRestartDecision::NoDisconnect => Ok(()),
            ExternalRestartDecision::CoolingDown { .. }
            | ExternalRestartDecision::RecoveryInProgress { .. } => {
                // 冷却期内或另一个触发源正在恢复：本轮不动。
                // 冷却完全静默（原先就是这个语义），让路只留 debug。
                if let ExternalRestartDecision::RecoveryInProgress { active } = decision {
                    log::debug!(
                        "自动重启游戏让路：{}正在进行中，本轮不重复关游戏",
                        active.chinese_name()
                    );
                }
                Ok(())
            }
            ExternalRestartDecision::NoRelaunchPath => {
                let lost = disconnected_for.unwrap_or_default();
                if self
                    .shared
                    .recovery
                    .should_warn_no_relaunch(now, LOOP_FAILURE_WARN_INTERVAL)
                {
                    log::warn!(
                        "外部聊天源已失联 {} 秒，但当前没有可用的自动拉起路径（需要 chat_source.auto_start），不重启游戏；本条在问题持续期间最多每分钟重复一次",
                        lost.as_secs()
                    );
                }
                Ok(())
            }
            ExternalRestartDecision::Restart {
                disconnected_for,
                lease,
            } => {
                log::warn!(
                    "外部聊天源已失联 {} 秒（失联回落阈值 {} 秒），按配置选择「自动重启游戏」而非回落 OCR；本次动作触发源：{}",
                    disconnected_for.as_secs(),
                    threshold.as_secs(),
                    RecoveryTrigger::ExternalChatDisconnect.chinese_name()
                );
                self.close_game_for_restart(
                    "外部聊天源失联超过回落阈值",
                    &self.shared.external_chat,
                );
                self.request_game_relaunch(relaunch)?;
                // 关窗与重新拉起都发出去了才记账：预算、冷却与健康汇报同步推进。
                lease.complete_restart(Instant::now());
                guard.auto_restart.awaiting_bridge_relaunch =
                    (relaunch == GameRelaunch::ViaChatBridge).then(Instant::now);
                Ok(())
            }
        }
    }

    pub(in crate::composition::application) fn poll_hall_expiry(
        &self,
        pause_applied: &mut bool,
        notice_sent: &mut bool,
        check_after: &mut Instant,
    ) {
        if Instant::now() < *check_after {
            return;
        }
        let retry_delay = match self.maybe_pause_expired_hall(pause_applied, notice_sent) {
            Ok(applied) => {
                *pause_applied = applied;
                if !applied {
                    *notice_sent = false;
                }
                Duration::from_secs(1)
            }
            Err(error) => {
                log::error!(
                    "大厅到期处理失败（步骤：暂停播放并提示到期），本次没有暂停也没有提示，5 秒后重试：{error:#}"
                );
                Duration::from_secs(5)
            }
        };
        *check_after = Instant::now() + retry_delay;
    }
}
