//! 统一游戏健康与恢复协调器（支柱一 · Wave 1-C1）。
//!
//! ## 为什么要有它
//!
//! 游戏异常恢复原先分散在三个互不知情的位置，各自维护局部计时与计数：
//!
//! | 触发源 | 原位置 | 局部状态 |
//! |---|---|---|
//! | 外部聊天源失联自动重启 | `composition/application/listener.rs` | 冷却时间、桥重拉等待、告警节流 |
//! | 加载界面卡死重启 | `ui/routines/startup.rs` | `loading_since`、`loading_restarts` |
//! | 持续无法识别的受控 Esc 探针 | `ui/routines/residency.rs` | `unknown_since`、`unknown_escape_used` |
//!
//! 三者之间原本没有任何协调：一次重启正在进行时，另一个触发源并不知道；
//! 「界面持续无法识别已经多久」也没有统一出口，线上长时间卡死时只能靠零散
//! 调试日志还原现场。
//!
//! 本模块只收拢**状态**，判定条件与阈值仍由调用方传进来（配置项、预算常量、
//! 日志文案都留在原模块，默认行为逐条等价）；协调器额外提供两件原先没有的东西：
//!
//! 1. **互斥**：[`RecoveryLease`] 保证同一时刻只有一个触发源在动手；
//! 2. **统一观测**：[`RecoveryHealthSnapshot`] 汇报当前恢复健康态。
//!
//! ## 时间可注入
//!
//! 所有状态迁移都接收调用方给的 [`Instant`]，另有 `*_now()` 便捷包装。单测因此
//! 可以按毫秒推进虚拟时间，不需要任何 `thread::sleep`。
//!
//! ## 现在谁在用
//!
//! - `ui::routines::startup`：加载卡死观测与重启计数；
//! - `ui::routines::residency`：持续无法识别观测、受控 Esc 探针与恢复结果记录；
//! - `composition/application/listener.rs`（Wave 1b）：外部失联重启、冷却、
//!   告警节流与全量健康汇报；同一实例由进程级装配注入，三处共享一份状态。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 持续无法识别界面的观测认定期限。
///
/// 取值与 `ui::routines::residency` 的受控 Esc 探针窗口一致（600 毫秒），
/// 但这里只用于**观测汇报**（「持续无法识别多久了」），不参与任何按键或
/// 自动重启决策；受控探针的预算仍由 `residency` 自己判定。
pub(crate) const DEFAULT_UNKNOWN_SUSTAINED_LIMIT: Duration = Duration::from_millis(600);

/// 恢复触发源。
///
/// 四支各自对应一条既有恢复路径；协调器靠它区分「谁在动手」「最近一次重启是
/// 谁发起的」，并在健康快照里如实汇报。
///
/// 只有前三支会真正动手（占用恢复互斥、关窗或按键）；[`Self::SustainedUnknown`]
/// 是**纯观测支**：它记的是「界面已经持续无法识别多久」，不占用互斥、不触发任何
/// 输入。分开放是因为两者语义不同——把它当成恢复动作会让健康快照谎报「正在恢复」。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RecoveryTrigger {
    /// 外部聊天源失联超过回落阈值（`listener.rs` 主循环）。
    ExternalChatDisconnect,
    /// 启动链停在加载界面超过阈值（`startup.rs`）。
    StartupLoadingStuck,
    /// 驻留恢复对持续无法识别的界面用掉一次受控 Esc 探针（`residency.rs`）。
    ResidencyUnknownProbe,
    /// 界面持续无法识别且已超过认定期限（纯观测，不按键、不占互斥）。
    SustainedUnknown,
}

impl RecoveryTrigger {
    /// 日志与面板用的中文名。
    ///
    /// 内部枚举值不能直接进日志（日志规范禁止 `{:?}`），调用点统一走这里，
    /// 免得同一件事在三个模块里各写一份译名。
    #[allow(dead_code)] // Wave 1b：listener.rs 的恢复日志接入后即被调用。
    pub(crate) fn chinese_name(self) -> &'static str {
        match self {
            Self::ExternalChatDisconnect => "外部聊天源失联",
            Self::StartupLoadingStuck => "启动链停在加载界面",
            Self::ResidencyUnknownProbe => "驻留恢复的受控 Esc 探针",
            Self::SustainedUnknown => "界面持续无法识别",
        }
    }
}

/// 一次恢复/重启的独占租约。
///
/// 由协调器发出，`Drop` 时自动归还。重启动作跨越「关窗」与「重新拉起」两个
/// 阶段，租约必须**一直持有到动作结束**：提前丢弃会让互斥提前放开，别的触发源
/// 就可能在同一时刻再关一次游戏。
#[derive(Debug)]
#[must_use = "租约在恢复动作结束前丢弃会让恢复互斥提前放开"]
pub(crate) struct RecoveryLease {
    coordinator: GameRecoveryCoordinator,
    trigger: RecoveryTrigger,
    lease: u64,
}

impl RecoveryLease {
    /// 本次动作的触发源。
    pub(crate) fn trigger(&self) -> RecoveryTrigger {
        self.trigger
    }

    /// 把本张租约转成「已记账的重启」：释放互斥并更新重启时间、触发源与总数。
    ///
    /// 用于「触发条件成立 → 动作已完成」的路径，省得调用方先手动调用
    /// [`GameRecoveryCoordinator::record_restart_completed`] 再 `drop`。
    pub(crate) fn complete_restart(self, now: Instant) {
        self.coordinator.record_restart_completed(now, self.trigger);
        drop(self);
    }
}

impl Drop for RecoveryLease {
    fn drop(&mut self) {
        let mut state = self.coordinator.lock();
        // 只归还自己那张租约：更早的租约被丢弃时，不能把后来者的互斥一并放开。
        if state
            .active
            .is_some_and(|active| active.lease == self.lease)
        {
            state.active = None;
        }
    }
}

/// 加载界面卡死的判定结果。
///
/// 四个分支与 `startup.rs` 原来的局部变量判据一一对应：`Waiting` 是「还在加载
/// 但没到阈值」，`NotLoading` 会同时清零计时，`TriggerRestart` 才允许关窗重开，
/// `BudgetExhausted` 表示本轮任务的重启预算已经用完。
#[derive(Debug)]
#[must_use = "加载卡死判定必须被消费：忽略它等于放弃自愈"]
pub(crate) enum LoadingStuckDecision {
    /// 当前不在加载界面；本次观测已清零计时。
    NotLoading,
    /// 仍在加载界面且未到阈值；`elapsed` 从本轮第一次观察到加载界面算起。
    Waiting { elapsed: Duration },
    /// 卡死成立：调用方应关闭并重新拉起游戏，动作期间持有 `lease`。
    ///
    /// `attempt` 是这一次重启的序号（**本次尚未记账**）：只有动作真正完成、
    /// 调用方通过 [`RecoveryLease::complete_restart`] 回报之后，预算才会被消耗、
    /// 加载计时才会清零。这样「卡死成立但没有可用拉起路径」这类没有真正重启的
    /// 分支不会凭空吃掉一次预算，也不会把正在计时的卡死当成已解决。
    TriggerRestart {
        attempt: u8,
        elapsed: Duration,
        lease: RecoveryLease,
    },
    /// 本轮任务的加载重启预算已用尽，不再触发。
    BudgetExhausted { attempts: u8 },
}

/// 外部聊天源失联重启的判定输入。
///
/// 这些都是**调用方的事实**（配置是否启用、失联多久、有没有可用的拉起路径），
/// 协调器不读配置、不猜平台能力，只把事实与自身状态合起来给一个结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExternalDisconnectRequest {
    /// 已失联多久；`None` 表示当前没有失联证据（未启用、已恢复或从未连上）。
    pub(crate) disconnected_for: Option<Duration>,
    /// 失联判定阈值：与回落 OCR 共用同一条线，不另设第二套计时。
    pub(crate) threshold: Duration,
    /// 当前是否存在可用的自动拉起路径。
    pub(crate) relaunch_available: bool,
}

/// 外部聊天源失联重启的判定结果。
#[derive(Debug)]
#[must_use = "失联重启判定必须被消费：忽略它等于放弃自愈"]
pub(crate) enum ExternalRestartDecision {
    /// 当前没有失联证据（或失联时长未到阈值）：不动。
    NoDisconnect,
    /// 最近一次重启仍在冷却期内，剩余 `remaining`。
    CoolingDown { remaining: Duration },
    /// 另一个触发源正在恢复/重启，本次让路（下一轮再判）。
    RecoveryInProgress { active: RecoveryTrigger },
    /// 失联已成立，但没有可用的自动拉起路径；调用方只按节流告警，不关游戏。
    NoRelaunchPath,
    /// 允许重启：调用方执行关窗与拉起，动作期间持有 `lease`，完成后调用
    /// [`GameRecoveryCoordinator::record_restart_completed`] 记账。
    Restart {
        disconnected_for: Duration,
        lease: RecoveryLease,
    },
}

/// 界面无法识别状态的观测结果。
///
/// 只描述「持续无法识别」这一条观测线，不参与任何按键决策：受控 Esc 探针是否
/// 允许、何时停止仍由 `residency` 自己的预算判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnknownObservation {
    /// 当前界面可识别（离开无法识别界面时清零计时）。
    Recognized,
    /// 仍在无法识别界面内，尚未达到认定期限。
    Observing { sustained: Duration },
    /// 持续无法识别**刚刚**达到认定期限；每个无法识别回合只报一次。
    Reached { sustained: Duration },
    /// 该回合内持续无法识别已越过认定期限的后续观测。
    Sustained { sustained: Duration },
}

/// 驻留恢复被拒绝的原因分类（只做健康汇报，不改判定）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResidencyRejection {
    /// 界面持续无法识别，且受控 Esc 探针预算已用尽或不允许探针。
    UnknownEscapeBudgetExhausted,
    /// 受控 Esc 探针之后界面仍然无法识别。
    UnknownProbeIneffective,
    /// 其它拒绝（目标不可重建、熔断、时间预算超时等）。
    Other,
}

/// 恢复健康快照：为诊断、告警与后续策略扩展留的一等可测入口。
///
/// 全部字段都是协调器当前状态的只读投影，`unknown_sustained` 由调用方给的
/// `now` 现算，因此同一个协调器在不同时刻取快照会得到不同时长。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecoveryHealthSnapshot {
    /// 当前占用恢复互斥的触发源；`None` 表示此刻没有恢复在动手。
    pub(crate) active_recovery: Option<RecoveryTrigger>,
    /// 最近一次**已完成**重启的时间。
    pub(crate) last_restart_at: Option<Instant>,
    /// 最近一次已完成重启的触发源。
    pub(crate) last_restart_trigger: Option<RecoveryTrigger>,
    /// 已完成的重启总数。
    pub(crate) total_restarts: u32,
    /// 本轮启动任务已用掉的加载重启次数。
    pub(crate) loading_restarts: u8,
    /// 本轮启动任务允许的加载重启上限。
    pub(crate) max_loading_restarts: u8,
    /// 当前已持续无法识别多久；`None` 表示此刻界面可识别。
    pub(crate) unknown_sustained: Option<Duration>,
    /// 已用掉的受控 Esc 探针次数。
    pub(crate) unknown_escape_probes: u32,
    /// 因「探针预算已用尽/不允许探针」被拒绝的次数。
    pub(crate) unknown_escape_probes_exhausted_count: u32,
    /// 因「探针之后仍无法识别」被拒绝的次数。
    pub(crate) unknown_probe_ineffective_count: u32,
    /// 驻留恢复成功落定的次数。
    pub(crate) residency_settled: u32,
    /// 驻留恢复被拒绝的次数（只统计判定拒绝，不含时间预算超时与设备故障）。
    pub(crate) residency_rejected: u32,
    /// 驻留恢复累计按下的 Esc 次数。
    pub(crate) residency_esc_presses: u32,
    /// 最近一次恢复动作的触发源（重启与探针都算）。
    pub(crate) last_recovery_action: Option<RecoveryTrigger>,
    /// 最近一次进入「持续无法识别已认定」的时间。
    pub(crate) last_sustained_unknown_at: Option<Instant>,
    /// 最近一次受控 Esc 探针的时间。
    pub(crate) last_unknown_escape_probe_at: Option<Instant>,
    /// 最近一次驻留恢复被拒绝的时间。
    pub(crate) last_residency_rejected_at: Option<Instant>,
    /// 最近一次驻留恢复成功落定的时间。
    pub(crate) last_residency_settled_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveRecovery {
    trigger: RecoveryTrigger,
    lease: u64,
}

#[derive(Debug)]
struct RecoveryState {
    active: Option<ActiveRecovery>,
    next_lease: u64,
    last_restart_at: Option<Instant>,
    last_restart_trigger: Option<RecoveryTrigger>,
    total_restarts: u32,
    loading_since: Option<Instant>,
    loading_restarts: u8,
    max_loading_restarts: u8,
    unknown_since: Option<Instant>,
    unknown_reached_reported: bool,
    unknown_escape_probes: u32,
    unknown_escape_probes_exhausted: u32,
    unknown_probe_ineffective: u32,
    residency_settled: u32,
    residency_rejected: u32,
    residency_esc_presses: u32,
    last_recovery_action: Option<RecoveryTrigger>,
    last_sustained_unknown_at: Option<Instant>,
    last_unknown_escape_probe_at: Option<Instant>,
    last_residency_rejected_at: Option<Instant>,
    last_residency_settled_at: Option<Instant>,
    no_relaunch_warn_at: Option<Instant>,
}

impl Default for RecoveryState {
    fn default() -> Self {
        Self {
            active: None,
            next_lease: 0,
            last_restart_at: None,
            last_restart_trigger: None,
            total_restarts: 0,
            loading_since: None,
            loading_restarts: 0,
            max_loading_restarts: 0,
            unknown_since: None,
            unknown_reached_reported: false,
            unknown_escape_probes: 0,
            unknown_escape_probes_exhausted: 0,
            unknown_probe_ineffective: 0,
            residency_settled: 0,
            residency_rejected: 0,
            residency_esc_presses: 0,
            last_recovery_action: None,
            last_sustained_unknown_at: None,
            last_unknown_escape_probe_at: None,
            last_residency_rejected_at: None,
            last_residency_settled_at: None,
            no_relaunch_warn_at: None,
        }
    }
}

impl RecoveryState {
    /// 抢一次恢复互斥；已经有触发源在动手时返回 `None`。
    fn begin(&mut self, trigger: RecoveryTrigger) -> Option<u64> {
        if self.active.is_some() {
            return None;
        }
        self.next_lease = self.next_lease.wrapping_add(1);
        self.active = Some(ActiveRecovery {
            trigger,
            lease: self.next_lease,
        });
        Some(self.next_lease)
    }

    fn snapshot(&self, now: Instant) -> RecoveryHealthSnapshot {
        RecoveryHealthSnapshot {
            active_recovery: self.active.map(|active| active.trigger),
            last_restart_at: self.last_restart_at,
            last_restart_trigger: self.last_restart_trigger,
            total_restarts: self.total_restarts,
            loading_restarts: self.loading_restarts,
            max_loading_restarts: self.max_loading_restarts,
            unknown_sustained: self
                .unknown_since
                .map(|since| now.saturating_duration_since(since)),
            unknown_escape_probes: self.unknown_escape_probes,
            unknown_escape_probes_exhausted_count: self.unknown_escape_probes_exhausted,
            unknown_probe_ineffective_count: self.unknown_probe_ineffective,
            residency_settled: self.residency_settled,
            residency_rejected: self.residency_rejected,
            residency_esc_presses: self.residency_esc_presses,
            last_recovery_action: self.last_recovery_action,
            last_sustained_unknown_at: self.last_sustained_unknown_at,
            last_unknown_escape_probe_at: self.last_unknown_escape_probe_at,
            last_residency_rejected_at: self.last_residency_rejected_at,
            last_residency_settled_at: self.last_residency_settled_at,
        }
    }
}

/// 游戏健康与恢复协调器：`Clone` 且共享同一份状态。
///
/// 进程级共享靠 `Clone`（内部 `Arc`）实现：调用方在装配处创建一次，把 clone
/// 交给 `listener` / `startup` / `residency`，三处就共享冷却、互斥与观测。
/// 没有注入时各处用 [`GameRecoveryCoordinator::default`] 自建一个，等价于原先
/// 各模块的局部变量。
#[derive(Clone, Debug, Default)]
pub(crate) struct GameRecoveryCoordinator {
    state: Arc<Mutex<RecoveryState>>,
}

impl GameRecoveryCoordinator {
    fn lock(&self) -> MutexGuard<'_, RecoveryState> {
        // 恢复状态是纯计数与时间戳，中毒后仍然可用：一次 panic 不该让游戏失去自愈能力。
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ---- 启动链：加载界面卡死 ----

    /// 开一轮「加载界面卡死」观测窗口，并把重启预算重置为该任务的上限。
    ///
    /// 加载重启预算是**单次启动任务**的预算（原实现是同一次 `execute_enter_game`
    /// 内的局部计数），所以每次启动任务开始都要显式开窗，不能让上一次任务的
    /// 计数泄漏到下一次。
    pub(crate) fn begin_loading_watch(&self, max_attempts: u8) {
        let mut state = self.lock();
        state.loading_since = None;
        state.loading_restarts = 0;
        state.max_loading_restarts = max_attempts;
    }

    /// 观测一次加载界面状态并给出是否该重启。
    ///
    /// 判据与 `startup.rs` 原局部实现逐条等价：
    /// - 不在加载界面 → 清零计时并返回 [`LoadingStuckDecision::NotLoading`]；
    /// - 仍在加载但未到 `timeout` → [`LoadingStuckDecision::Waiting`]；
    /// - 到达 `timeout` 且预算未用尽 → 交出恢复租约并返回
    ///   [`LoadingStuckDecision::TriggerRestart`]；**预算在动作真正完成时才消耗**
    ///   （见 [`RecoveryLease::complete_restart`]），所以「卡死成立但没有可用
    ///   拉起路径」不会白白吃掉一次重开机会；
    /// - 预算已用尽 → [`LoadingStuckDecision::BudgetExhausted`]。
    ///
    /// 触发时若另一个触发源正持有恢复互斥，本次返回 `Waiting` 让路，下一轮
    /// 再判——这样「加载重启进行中」与「外部源失联重启」不会同时关游戏。
    pub(crate) fn observe_loading_state(
        &self,
        now: Instant,
        is_loading: bool,
        timeout: Duration,
        max_attempts: u8,
    ) -> LoadingStuckDecision {
        let mut state = self.lock();
        state.max_loading_restarts = max_attempts;
        if !is_loading {
            // 离开加载界面就清零，短加载不会累积成误判。
            state.loading_since = None;
            return LoadingStuckDecision::NotLoading;
        }
        let since = *state.loading_since.get_or_insert(now);
        let elapsed = now.saturating_duration_since(since);
        if state.loading_restarts >= max_attempts {
            return LoadingStuckDecision::BudgetExhausted {
                attempts: state.loading_restarts,
            };
        }
        if elapsed < timeout {
            return LoadingStuckDecision::Waiting { elapsed };
        }
        let Some(lease) = state.begin(RecoveryTrigger::StartupLoadingStuck) else {
            return LoadingStuckDecision::Waiting { elapsed };
        };
        // 计时保留到动作真正完成：重启没成功（例如没有可用拉起路径）时，
        // 下一次观测仍应立刻判定「还是卡死」，而不是从零重新计一遍。
        let attempt = state.loading_restarts.saturating_add(1);
        LoadingStuckDecision::TriggerRestart {
            attempt,
            elapsed,
            lease: RecoveryLease {
                coordinator: self.clone(),
                trigger: RecoveryTrigger::StartupLoadingStuck,
                lease,
            },
        }
    }

    /// 本轮启动任务已经触发过几次加载重启。
    pub(crate) fn loading_restart_attempts(&self) -> u8 {
        self.lock().loading_restarts
    }

    // ---- 主循环：外部聊天源失联 ----

    /// 判定外部聊天源失联是否该重启游戏。
    ///
    /// 判定顺序与 `listener.rs` 现有实现一致：**先冷却**（冷却期内连失联证据都
    /// 不再判，避免冷却结束后拿冷却前的旧起点立刻触发）→ 失联是否成立 → 是否
    /// 另有恢复在动手 → 有没有可用的自动拉起路径 → 交出恢复租约。
    ///
    /// 调用方的暂停/任务空闲守卫不在这里：那是「此刻该不该动」的业务闸门，
    /// 与恢复状态无关。真正关窗与拉起也由调用方执行，完成后调用
    /// [`Self::record_restart_completed`] 记账。
    ///
    /// 本方法是 Wave 1b 的接口准备，当前生产路径由 `listener.rs` 自己的局部
    /// 冷却实现，行为与它逐条等价。
    #[allow(dead_code)] // Wave 1b：listener.rs 接入后即被调用。
    pub(crate) fn evaluate_external_disconnect_restart(
        &self,
        now: Instant,
        cooldown: Duration,
        request: ExternalDisconnectRequest,
    ) -> ExternalRestartDecision {
        let mut state = self.lock();
        if let Some(last) = state.last_restart_at {
            let elapsed = now.saturating_duration_since(last);
            if elapsed < cooldown {
                return ExternalRestartDecision::CoolingDown {
                    remaining: cooldown - elapsed,
                };
            }
        }
        let Some(disconnected_for) = request.disconnected_for else {
            return ExternalRestartDecision::NoDisconnect;
        };
        if disconnected_for < request.threshold {
            return ExternalRestartDecision::NoDisconnect;
        }
        if let Some(active) = state.active {
            return ExternalRestartDecision::RecoveryInProgress {
                active: active.trigger,
            };
        }
        if !request.relaunch_available {
            return ExternalRestartDecision::NoRelaunchPath;
        }
        let Some(lease) = state.begin(RecoveryTrigger::ExternalChatDisconnect) else {
            return ExternalRestartDecision::RecoveryInProgress {
                active: state
                    .active
                    .map(|active| active.trigger)
                    .unwrap_or(RecoveryTrigger::ExternalChatDisconnect),
            };
        };
        ExternalRestartDecision::Restart {
            disconnected_for,
            lease: RecoveryLease {
                coordinator: self.clone(),
                trigger: RecoveryTrigger::ExternalChatDisconnect,
                lease,
            },
        }
    }

    /// 记录一次**已完成**的重启：更新最近重启时间、触发源与总数。
    ///
    /// 这也是唯一消耗加载重启预算的路径（加载重启完成时经
    /// [`RecoveryLease::complete_restart`] 调用到这里），同时清零加载计时：
    /// 因此「触发条件成立但最终没有重启」既不会占用预算，也不会把正在计时的
    /// 卡死误判成已经解决。与其它触发源共享的冷却同样由这里更新。
    pub(crate) fn record_restart_completed(&self, now: Instant, trigger: RecoveryTrigger) {
        let mut state = self.lock();
        state.last_restart_at = Some(now);
        state.last_restart_trigger = Some(trigger);
        state.total_restarts = state.total_restarts.saturating_add(1);
        state.last_recovery_action = Some(trigger);
        if trigger == RecoveryTrigger::StartupLoadingStuck {
            state.loading_restarts = state.loading_restarts.saturating_add(1);
            // 这一次重启已经把加载界面从头带过一遍，计时从下次观察到加载界面重来。
            state.loading_since = None;
        }
    }

    /// 「当前没有可用的自动拉起路径」告警是否该输出。
    ///
    /// 与 `listener.rs` 的 `loop_failure_warn_due` 同一规则：首次立即输出留痕，
    /// 之后每个 `warn_interval` 最多一次。主循环默认 60 毫秒一轮，没有节流会
    /// 每秒刷十几条把其余告警冲掉。
    ///
    /// 现有实现里「等待聊天桥重新拉起」与「没有自动拉起路径」共用同一个计时槽，
    /// 接入时沿用同一槽位即可保持逐条等价。
    #[allow(dead_code)] // Wave 1b：listener.rs 接入后即被调用。
    pub(crate) fn should_warn_no_relaunch(&self, now: Instant, warn_interval: Duration) -> bool {
        let mut state = self.lock();
        if state
            .no_relaunch_warn_at
            .is_none_or(|last| now.saturating_duration_since(last) >= warn_interval)
        {
            state.no_relaunch_warn_at = Some(now);
            return true;
        }
        false
    }

    // ---- 统一观测：持续无法识别 ----

    /// 观测一次界面分类结果，维护「持续无法识别」的起点与已持续时长。
    ///
    /// 进入无法识别记录起点；离开立即清零。越过 [`DEFAULT_UNKNOWN_SUSTAINED_LIMIT`]
    /// 时每个回合只报一次 [`UnknownObservation::Reached`]，之后是 `Sustained`。
    /// 全程只记录，不改变任何按键与重启决策。
    pub(crate) fn observe_ui_kind(&self, now: Instant, is_unknown: bool) -> UnknownObservation {
        let mut state = self.lock();
        if !is_unknown {
            state.unknown_since = None;
            state.unknown_reached_reported = false;
            return UnknownObservation::Recognized;
        }
        let since = *state.unknown_since.get_or_insert(now);
        let sustained = now.saturating_duration_since(since);
        if sustained < DEFAULT_UNKNOWN_SUSTAINED_LIMIT {
            return UnknownObservation::Observing { sustained };
        }
        if state.unknown_reached_reported {
            return UnknownObservation::Sustained { sustained };
        }
        state.unknown_reached_reported = true;
        state.last_sustained_unknown_at = Some(now);
        UnknownObservation::Reached { sustained }
    }

    /// 记录一次受控 Esc 探针的消耗（`residency.rs` 按下 Esc 后调用）。
    pub(crate) fn record_unknown_escape_probe(&self, now: Instant) {
        let mut state = self.lock();
        state.unknown_escape_probes = state.unknown_escape_probes.saturating_add(1);
        state.last_unknown_escape_probe_at = Some(now);
        state.last_recovery_action = Some(RecoveryTrigger::ResidencyUnknownProbe);
    }

    /// 记录一次驻留恢复成功落定（`residency.rs` 返回成功报告前调用）。
    ///
    /// `esc_presses` 是本轮恢复按下的 Esc 次数，累进全局健康态；成功落定同时把
    /// 「持续无法识别」观测清零——界面已经恢复可识别，不需要等下一帧才归零。
    pub(crate) fn record_residency_settled(&self, now: Instant, esc_presses: u32) {
        let mut state = self.lock();
        state.residency_settled = state.residency_settled.saturating_add(1);
        state.residency_esc_presses = state.residency_esc_presses.saturating_add(esc_presses);
        state.last_residency_settled_at = Some(now);
        state.unknown_since = None;
        state.unknown_reached_reported = false;
    }

    /// 记录一次驻留恢复被判定拒绝（`residency.rs` 的拒绝分支调用）。
    pub(crate) fn record_residency_rejected(&self, now: Instant, rejection: ResidencyRejection) {
        let mut state = self.lock();
        state.residency_rejected = state.residency_rejected.saturating_add(1);
        state.last_residency_rejected_at = Some(now);
        match rejection {
            ResidencyRejection::UnknownEscapeBudgetExhausted => {
                state.unknown_escape_probes_exhausted =
                    state.unknown_escape_probes_exhausted.saturating_add(1);
            }
            ResidencyRejection::UnknownProbeIneffective => {
                state.unknown_probe_ineffective = state.unknown_probe_ineffective.saturating_add(1);
            }
            ResidencyRejection::Other => {}
        }
    }

    // ---- 只读健康快照 ----

    /// 取当前健康快照；「持续无法识别多久」按调用方给的 `now` 现算。
    #[allow(dead_code)] // Wave 1b：listener.rs 的诊断与告警接入后即被调用。
    pub(crate) fn health_snapshot(&self, now: Instant) -> RecoveryHealthSnapshot {
        self.lock().snapshot(now)
    }

    /// [`Self::health_snapshot`] 的便捷包装：用当前时刻取快照。
    #[allow(dead_code)] // Wave 1b：同上。
    pub(crate) fn health_snapshot_now(&self) -> RecoveryHealthSnapshot {
        self.health_snapshot(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinator() -> GameRecoveryCoordinator {
        GameRecoveryCoordinator::default()
    }

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    /// 加载卡死：计时从第一次观察到加载界面算起，离开加载界面立刻清零。
    #[test]
    fn loading_watch_starts_on_first_loading_sample_and_clears_when_it_leaves() {
        let recovery = coordinator();
        let start = Instant::now();
        recovery.begin_loading_watch(1);

        assert!(matches!(
            recovery.observe_loading_state(start, false, secs(60), 1),
            LoadingStuckDecision::NotLoading
        ));
        let started = recovery.observe_loading_state(start, true, secs(60), 1);
        let LoadingStuckDecision::Waiting { elapsed } = started else {
            panic!("第一次观察到加载界面只能等待：{started:?}");
        };
        assert_eq!(elapsed, Duration::ZERO);

        let mid = recovery.observe_loading_state(start + secs(30), true, secs(60), 1);
        let LoadingStuckDecision::Waiting { elapsed } = mid else {
            panic!("未到阈值必须继续等待：{mid:?}");
        };
        assert_eq!(elapsed, secs(30));

        // 中途离开加载界面：短加载不能累积成误判。
        assert!(matches!(
            recovery.observe_loading_state(start + secs(31), false, secs(60), 1),
            LoadingStuckDecision::NotLoading
        ));
        let restarted = recovery.observe_loading_state(start + secs(32), true, secs(60), 1);
        let LoadingStuckDecision::Waiting { elapsed } = restarted else {
            panic!("离开加载界面后应重新计时：{restarted:?}");
        };
        assert_eq!(elapsed, Duration::ZERO);
    }

    /// 到达阈值就触发一次重启，并交出恢复租约；租约在手时其它触发源让路。
    #[test]
    fn loading_timeout_triggers_one_restart_with_an_exclusive_lease() {
        let recovery = coordinator();
        let start = Instant::now();
        recovery.begin_loading_watch(1);

        recovery.observe_loading_state(start, true, secs(60), 1);
        let decision = recovery.observe_loading_state(start + secs(60), true, secs(60), 1);
        let LoadingStuckDecision::TriggerRestart {
            attempt,
            elapsed,
            lease,
        } = decision
        else {
            panic!("到达阈值必须触发重启：{decision:?}");
        };
        assert_eq!(attempt, 1);
        assert_eq!(elapsed, secs(60));
        assert_eq!(lease.trigger(), RecoveryTrigger::StartupLoadingStuck);
        assert_eq!(
            recovery.health_snapshot(start + secs(60)).active_recovery,
            Some(RecoveryTrigger::StartupLoadingStuck),
            "重启动作期间互斥必须被占用"
        );

        // 加载重启进行中：外部源失联重启必须让路，不能同时关游戏。
        let external = recovery.evaluate_external_disconnect_restart(
            start + secs(61),
            secs(60),
            ExternalDisconnectRequest {
                disconnected_for: Some(secs(120)),
                threshold: secs(10),
                relaunch_available: true,
            },
        );
        let ExternalRestartDecision::RecoveryInProgress { active } = external else {
            panic!("加载重启进行中时外部源必须让路：{external:?}");
        };
        assert_eq!(active, RecoveryTrigger::StartupLoadingStuck);

        // 动作真正完成：记账并放开互斥。
        lease.complete_restart(start + secs(62));
        let snapshot = recovery.health_snapshot(start + secs(63));
        assert_eq!(snapshot.active_recovery, None, "动作完成后互斥必须放开");
        assert_eq!(snapshot.total_restarts, 1);
        assert_eq!(snapshot.loading_restarts, 1);
        assert_eq!(
            snapshot.last_restart_trigger,
            Some(RecoveryTrigger::StartupLoadingStuck)
        );
        assert_eq!(snapshot.last_restart_at, Some(start + secs(62)));

        let external = recovery.evaluate_external_disconnect_restart(
            start + secs(63),
            Duration::ZERO,
            ExternalDisconnectRequest {
                disconnected_for: Some(secs(120)),
                threshold: secs(10),
                relaunch_available: true,
            },
        );
        assert!(
            matches!(external, ExternalRestartDecision::Restart { .. }),
            "互斥放开后外部源才能动手：{external:?}"
        );
    }

    /// 反过来：外部源重启进行中时，加载卡死必须等待而不是重复触发。
    #[test]
    fn external_restart_blocks_loading_restart_while_it_holds_the_lease() {
        let recovery = coordinator();
        let start = Instant::now();
        recovery.begin_loading_watch(1);

        let external = recovery.evaluate_external_disconnect_restart(
            start,
            secs(60),
            ExternalDisconnectRequest {
                disconnected_for: Some(secs(30)),
                threshold: secs(10),
                relaunch_available: true,
            },
        );
        let ExternalRestartDecision::Restart { lease, .. } = external else {
            panic!("失联超过阈值且可拉起时必须允许重启：{external:?}");
        };

        recovery.observe_loading_state(start, true, secs(60), 1);
        let blocked = recovery.observe_loading_state(start + secs(120), true, secs(60), 1);
        assert!(
            matches!(blocked, LoadingStuckDecision::Waiting { .. }),
            "外部源重启进行中时加载重启必须让路：{blocked:?}"
        );
        assert_eq!(recovery.loading_restart_attempts(), 0, "让路不能消耗预算");

        lease.complete_restart(start + secs(120));
        let allowed = recovery.observe_loading_state(start + secs(121), true, secs(60), 1);
        assert!(
            matches!(
                allowed,
                LoadingStuckDecision::TriggerRestart { attempt: 1, .. }
            ),
            "互斥放开后加载重启必须照常触发：{allowed:?}"
        );
    }

    /// 预算用尽后不再触发；新一轮启动任务重新开窗时预算恢复。
    #[test]
    fn loading_restart_budget_exhausts_and_resets_per_startup_watch() {
        let recovery = coordinator();
        let start = Instant::now();
        recovery.begin_loading_watch(1);

        recovery.observe_loading_state(start, true, secs(60), 1);
        let first = recovery.observe_loading_state(start + secs(60), true, secs(60), 1);
        let LoadingStuckDecision::TriggerRestart { lease, .. } = first else {
            panic!("第一次必须触发：{first:?}");
        };
        lease.complete_restart(start + secs(61));
        assert_eq!(recovery.loading_restart_attempts(), 1);

        // 重开之后仍然卡在加载界面：预算已用尽，只如实报告，不再重启。
        let exhausted = recovery.observe_loading_state(start + secs(180), true, secs(60), 1);
        assert!(
            matches!(
                exhausted,
                LoadingStuckDecision::BudgetExhausted { attempts: 1 }
            ),
            "预算用尽后必须拒绝继续重启：{exhausted:?}"
        );
        assert_eq!(recovery.loading_restart_attempts(), 1);

        // 下一次启动任务是独立预算，不能被上一次的计数泄漏限制。
        recovery.begin_loading_watch(1);
        assert_eq!(recovery.loading_restart_attempts(), 0);
        recovery.observe_loading_state(start + secs(200), true, secs(60), 1);
        let next = recovery.observe_loading_state(start + secs(260), true, secs(60), 1);
        assert!(
            matches!(
                next,
                LoadingStuckDecision::TriggerRestart { attempt: 1, .. }
            ),
            "新一轮启动任务必须重新获得完整预算：{next:?}"
        );
    }

    /// 「卡死成立但没有可用拉起路径」不能白白吃掉重开机会：
    /// 只有真正完成的重启才消耗预算。
    #[test]
    fn aborted_loading_restart_does_not_consume_the_budget() {
        let recovery = coordinator();
        let start = Instant::now();
        recovery.begin_loading_watch(1);

        recovery.observe_loading_state(start, true, secs(60), 1);
        let decision = recovery.observe_loading_state(start + secs(60), true, secs(60), 1);
        let LoadingStuckDecision::TriggerRestart { lease, .. } = decision else {
            panic!("到达阈值必须触发：{decision:?}");
        };
        // 调用方发现没有聊天桥可拉起，直接放弃：只归还租约，不记账。
        drop(lease);
        assert_eq!(
            recovery.loading_restart_attempts(),
            0,
            "未完成的重启不能消耗预算"
        );

        let retried = recovery.observe_loading_state(start + secs(61), true, secs(60), 1);
        assert!(
            matches!(
                retried,
                LoadingStuckDecision::TriggerRestart { attempt: 1, .. }
            ),
            "预算未被消耗时必须还能重试：{retried:?}"
        );
    }

    /// 外部源失联：冷却、无失联证据、无拉起路径三支的判定顺序。
    #[test]
    fn external_disconnect_restart_respects_cooldown_and_relaunch_path() {
        let recovery = coordinator();
        let start = Instant::now();
        let request = ExternalDisconnectRequest {
            disconnected_for: Some(secs(30)),
            threshold: secs(10),
            relaunch_available: true,
        };

        // 还没失联到阈值：不动。
        let below = recovery.evaluate_external_disconnect_restart(
            start,
            secs(60),
            ExternalDisconnectRequest {
                disconnected_for: Some(secs(5)),
                ..request
            },
        );
        assert!(
            matches!(below, ExternalRestartDecision::NoDisconnect),
            "未到阈值必须不动：{below:?}"
        );

        // 刚重启完：整个冷却期内都不再判（连失联证据都不看）。
        let first = recovery.evaluate_external_disconnect_restart(start, secs(60), request);
        let ExternalRestartDecision::Restart { lease, .. } = first else {
            panic!("达到阈值必须允许重启：{first:?}");
        };
        lease.complete_restart(start);

        let cooling =
            recovery.evaluate_external_disconnect_restart(start + secs(10), secs(60), request);
        let ExternalRestartDecision::CoolingDown { remaining } = cooling else {
            panic!("冷却期内必须冷却：{cooling:?}");
        };
        assert_eq!(remaining, secs(50));

        // 冷却结束、且没有失联证据：不动。
        let recovered = recovery.evaluate_external_disconnect_restart(
            start + secs(61),
            secs(60),
            ExternalDisconnectRequest {
                disconnected_for: None,
                ..request
            },
        );
        assert!(
            matches!(recovered, ExternalRestartDecision::NoDisconnect),
            "已恢复时不能重启：{recovered:?}"
        );

        // 失联成立但没有自动拉起路径：只告警，不关游戏。
        let no_path = recovery.evaluate_external_disconnect_restart(
            start + secs(61),
            secs(60),
            ExternalDisconnectRequest {
                relaunch_available: false,
                ..request
            },
        );
        assert!(
            matches!(no_path, ExternalRestartDecision::NoRelaunchPath),
            "没有拉起路径时必须放弃重启：{no_path:?}"
        );
    }

    /// 「没有可用拉起路径」告警节流：首次立即输出，窗口内静默，窗口后再输出。
    #[test]
    fn no_relaunch_warning_is_throttled_by_interval() {
        let recovery = coordinator();
        let start = Instant::now();
        assert!(recovery.should_warn_no_relaunch(start, secs(60)));
        assert!(!recovery.should_warn_no_relaunch(start + secs(1), secs(60)));
        assert!(!recovery.should_warn_no_relaunch(start + secs(59), secs(60)));
        assert!(recovery.should_warn_no_relaunch(start + secs(60), secs(60)));
    }

    /// 持续无法识别：累积时长、每回合只认定一次、恢复后清零。
    #[test]
    fn sustained_unknown_accumulates_reports_once_and_clears_on_recovery() {
        let recovery = coordinator();
        let start = Instant::now();

        assert_eq!(
            recovery.observe_ui_kind(start, false),
            UnknownObservation::Recognized
        );
        assert_eq!(
            recovery.observe_ui_kind(start, true),
            UnknownObservation::Observing {
                sustained: Duration::ZERO
            }
        );
        assert_eq!(
            recovery
                .health_snapshot(start + DEFAULT_UNKNOWN_SUSTAINED_LIMIT)
                .unknown_sustained,
            Some(DEFAULT_UNKNOWN_SUSTAINED_LIMIT)
        );

        let reached = recovery.observe_ui_kind(start + secs(1), true);
        assert!(
            matches!(
                reached,
                UnknownObservation::Reached { sustained } if sustained == secs(1)
            ),
            "越过认定期限必须报一次认定：{reached:?}"
        );
        assert_eq!(
            recovery.observe_ui_kind(start + secs(2), true),
            UnknownObservation::Sustained { sustained: secs(2) },
            "同一回合内只认定一次，之后是持续观测"
        );
        let snapshot = recovery.health_snapshot(start + secs(2));
        assert_eq!(snapshot.last_sustained_unknown_at, Some(start + secs(1)));
        assert_eq!(
            snapshot.last_recovery_action, None,
            "持续无法识别只是观测，不能冒充恢复动作"
        );
        assert_eq!(snapshot.active_recovery, None, "纯观测不得占用恢复互斥");
        assert_eq!(
            RecoveryTrigger::SustainedUnknown.chinese_name(),
            "界面持续无法识别"
        );

        // 恢复正常（可识别）：时长与回合标记一起清零。
        assert_eq!(
            recovery.observe_ui_kind(start + secs(3), false),
            UnknownObservation::Recognized
        );
        assert_eq!(
            recovery.health_snapshot(start + secs(3)).unknown_sustained,
            None
        );
        let again = recovery.observe_ui_kind(start + secs(4), true);
        assert_eq!(
            again,
            UnknownObservation::Observing {
                sustained: Duration::ZERO
            },
            "恢复后再进入无法识别必须重新计时"
        );
    }

    /// Esc 探针消耗与拒绝原因都会进入健康快照。
    #[test]
    fn escape_probe_budget_and_residency_outcomes_are_reported_in_the_snapshot() {
        let recovery = coordinator();
        let start = Instant::now();

        recovery.record_unknown_escape_probe(start);
        recovery.record_unknown_escape_probe(start + secs(1));
        recovery.record_residency_rejected(
            start + secs(2),
            ResidencyRejection::UnknownEscapeBudgetExhausted,
        );
        recovery.record_residency_rejected(
            start + secs(3),
            ResidencyRejection::UnknownEscapeBudgetExhausted,
        );
        recovery.record_residency_rejected(
            start + secs(4),
            ResidencyRejection::UnknownProbeIneffective,
        );
        recovery.record_residency_rejected(start + secs(5), ResidencyRejection::Other);
        recovery.record_residency_settled(start + secs(6), 3);

        let snapshot = recovery.health_snapshot(start + secs(7));
        assert_eq!(snapshot.unknown_escape_probes, 2);
        assert_eq!(snapshot.unknown_escape_probes_exhausted_count, 2);
        assert_eq!(snapshot.unknown_probe_ineffective_count, 1);
        assert_eq!(snapshot.residency_rejected, 4);
        assert_eq!(snapshot.residency_settled, 1);
        assert_eq!(snapshot.residency_esc_presses, 3);
        assert_eq!(
            snapshot.last_recovery_action,
            Some(RecoveryTrigger::ResidencyUnknownProbe),
            "最近一次恢复动作必须是探针（之后只记账，不再改动作）"
        );
        assert_eq!(snapshot.last_unknown_escape_probe_at, Some(start + secs(1)));
        assert_eq!(snapshot.last_residency_rejected_at, Some(start + secs(5)));
        assert_eq!(snapshot.last_residency_settled_at, Some(start + secs(6)));
    }

    /// 恢复成功落定会把「持续无法识别」观测清零：界面已可识别，不必等下一帧。
    #[test]
    fn settling_residency_clears_the_unknown_observation() {
        let recovery = coordinator();
        let start = Instant::now();
        recovery.observe_ui_kind(start, true);
        assert_eq!(
            recovery.health_snapshot(start + secs(1)).unknown_sustained,
            Some(secs(1))
        );

        recovery.record_residency_settled(start + secs(1), 1);
        assert_eq!(
            recovery.health_snapshot(start + secs(2)).unknown_sustained,
            None,
            "落定之后不得再报「仍持续无法识别」"
        );
        // 重新进入无法识别仍然从头计时，回合标记也一并复位。
        assert_eq!(
            recovery.observe_ui_kind(start + secs(3), true),
            UnknownObservation::Observing {
                sustained: Duration::ZERO
            }
        );
    }

    /// 重启记账：最近时间、触发源、总数，且 clone 之间共享同一份状态。
    #[test]
    fn completed_restarts_are_shared_across_coordinator_clones() {
        let recovery = coordinator();
        let shared = recovery.clone();
        let start = Instant::now();

        shared.record_restart_completed(start, RecoveryTrigger::ExternalChatDisconnect);
        shared.record_restart_completed(start + secs(90), RecoveryTrigger::StartupLoadingStuck);

        let snapshot = recovery.health_snapshot(start + secs(100));
        assert_eq!(snapshot.total_restarts, 2);
        assert_eq!(snapshot.loading_restarts, 1, "只有加载重启计入启动侧预算");
        assert_eq!(snapshot.last_restart_at, Some(start + secs(90)));
        assert_eq!(
            snapshot.last_restart_trigger,
            Some(RecoveryTrigger::StartupLoadingStuck)
        );
        assert_eq!(snapshot.active_recovery, None);
    }

    /// 互斥只归还自己的租约：更早的租约不能放开后来者的互斥。
    #[test]
    fn stale_lease_drop_never_releases_a_newer_lease() {
        let recovery = coordinator();
        let start = Instant::now();
        recovery.begin_loading_watch(1);

        recovery.observe_loading_state(start, true, secs(1), 1);
        let first = recovery.observe_loading_state(start + secs(1), true, secs(1), 1);
        let LoadingStuckDecision::TriggerRestart { lease: first, .. } = first else {
            panic!("第一次必须触发：{first:?}");
        };
        drop(first);

        let external = recovery.evaluate_external_disconnect_restart(
            start + secs(2),
            secs(60),
            ExternalDisconnectRequest {
                disconnected_for: Some(secs(30)),
                threshold: secs(10),
                relaunch_available: true,
            },
        );
        let ExternalRestartDecision::Restart { lease: second, .. } = external else {
            panic!("互斥放开后外部源必须能动手：{external:?}");
        };
        assert_eq!(
            recovery.health_snapshot(start + secs(2)).active_recovery,
            Some(RecoveryTrigger::ExternalChatDisconnect)
        );
        drop(second);
        assert_eq!(
            recovery.health_snapshot(start + secs(2)).active_recovery,
            None
        );
    }

    /// 默认快照是一份干净的零值：没有重启、没有恢复在动手、没有无法识别累积。
    #[test]
    fn default_snapshot_is_clean() {
        let recovery = coordinator();
        let start = Instant::now();
        let snapshot = recovery.health_snapshot(start);
        assert_eq!(snapshot.active_recovery, None);
        assert_eq!(snapshot.last_restart_at, None);
        assert_eq!(snapshot.last_restart_trigger, None);
        assert_eq!(snapshot.total_restarts, 0);
        assert_eq!(snapshot.loading_restarts, 0);
        assert_eq!(snapshot.max_loading_restarts, 0);
        assert_eq!(snapshot.unknown_sustained, None);
        assert_eq!(snapshot.unknown_escape_probes, 0);
        assert_eq!(snapshot.unknown_escape_probes_exhausted_count, 0);
        assert_eq!(snapshot.residency_settled, 0);
        assert_eq!(snapshot.residency_rejected, 0);
        assert_eq!(snapshot.last_recovery_action, None);
        assert_eq!(recovery.health_snapshot_now().total_restarts, 0);
    }

    /// 触发源中文名：四个触发源都必须有可读译名（日志规范禁止内部枚举直出）。
    #[test]
    fn every_trigger_has_a_chinese_name() {
        for trigger in [
            RecoveryTrigger::ExternalChatDisconnect,
            RecoveryTrigger::StartupLoadingStuck,
            RecoveryTrigger::ResidencyUnknownProbe,
            RecoveryTrigger::SustainedUnknown,
        ] {
            let name = trigger.chinese_name();
            assert!(
                name.chars()
                    .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch)),
                "{trigger:?} 的中文名必须含中文：{name}"
            );
        }
    }
}
