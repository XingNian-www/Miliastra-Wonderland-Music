use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, after_input_failure,
    before_input_failure, capture_normalized, wait_ms,
};
use super::residency::{ResidencyTarget, SettleEnvironment};
use super::state_observation::wait_for_stable_ui_kind;
use super::transaction::{EntryPolicy, FinishPolicy, UiTask, UiTaskSpec};
use crate::adapters::game_path::resolve_game_executable;
use crate::change_detection::{change_stats, rect_chat_change_fingerprint};
use crate::geometry::{Point, Rect, crop_canvas};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::recovery::{GameRecoveryCoordinator, LoadingStuckDecision};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiStateKind, UiSubmitError, sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::state::{
    CONFIRM_MESSAGE_MIN, CONFIRM_MESSAGE_SCALE, find_enter_game_text as locate_enter_game_text,
    world_is_teyvat,
};
use crate::ui::template::{best_template_candidate_downscaled, best_template_hit};
use enigo::Key;

const TEMPLATE_STABLE_HITS: u32 = 2;

/// 「加载界面过久」在一次启动任务内最多重开几次游戏。
///
/// 只重试一次：真的起不来（DLL 注入失败、游戏损坏）时反复重开只会把玩家
/// 卡在无限重启里，宁可放弃并如实报错，让人能看见问题。
const LOADING_RESTART_MAX_ATTEMPTS: u8 = 1;

struct TemplateAbsence<'a> {
    template: &'a Path,
    region: Rect,
    threshold: f32,
    timeout_ms: u64,
    stage: &'static str,
    failure_message: &'static str,
}

struct TemplateHit<'a> {
    template: &'a Path,
    region: Rect,
    threshold: f32,
    timeout_ms: u64,
    poll_ms: u64,
    certainty: InputCertainty,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EnterGame {
    pub(crate) require_gate: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct EnterWonderland {
    pub(crate) require_overworld: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnterGameEffect {
    WindowReady,
    Entered,
    Skipped,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnterGameOutcome {
    effect: EnterGameEffect,
}

impl EnterGameOutcome {
    pub(crate) fn effect(&self) -> &EnterGameEffect {
        &self.effect
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnterWonderlandEffect {
    Entered,
    Skipped,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EnterWonderlandOutcome {
    effect: EnterWonderlandEffect,
    residency: Option<UiResidencyOutcome>,
}

impl EnterWonderlandOutcome {
    pub(crate) fn effect(&self) -> &EnterWonderlandEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> Option<&UiResidencyOutcome> {
        self.residency.as_ref()
    }
}

#[derive(Clone)]
pub(crate) struct StartupUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
    /// 进程级共享的恢复协调器（见 `runtime::recovery`）。
    ///
    /// 默认自建一份，等价于原先「加载重启计数只活在这一个例程里」；
    /// `with_recovery` 注入同一实例后，加载卡死与外部源失联重启、驻留 Esc 探针
    /// 共享冷却与互斥（Wave 1b 在进程装配处注入）。
    recovery: GameRecoveryCoordinator,
}

impl StartupUi {
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: StartupRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
            recovery: GameRecoveryCoordinator::default(),
        }
    }

    /// 用进程级共享的恢复协调器替换默认实例。
    pub(crate) fn with_recovery(mut self, recovery: GameRecoveryCoordinator) -> Self {
        self.recovery = recovery;
        self
    }

    pub(crate) fn submit_enter_game(
        &self,
        request: EnterGame,
    ) -> Result<UiOperation<EnterGameOutcome>, UiSubmitError> {
        self.runtime.submit(EnterGameTask {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
            recovery: self.recovery.clone(),
        })
    }

    pub(crate) fn submit_enter_wonderland(
        &self,
        request: EnterWonderland,
    ) -> Result<UiOperation<EnterWonderlandOutcome>, UiSubmitError> {
        self.runtime.submit(EnterWonderlandTask {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
            recovery: self.recovery.clone(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct StartupRoutineConfig {
    startup: StartupUiConfig,
    residency: FriendDeliveryRoutineConfig,
    target_process: String,
}

#[derive(Clone)]
pub(crate) struct StartupUiConfig {
    pub(crate) launch_game: bool,
    /// 外部聊天源配置。启用时游戏改由 Miliastra-Wonderland-ChatBridge 拉起，
    /// 因为反作弊只允许"创建游戏进程的一方"注入 DLL。
    pub(crate) external_launcher: Option<crate::config::ChatSourceConfig>,
    pub(crate) enter_game: bool,
    pub(crate) exe_path: PathBuf,
    pub(crate) game_args: String,
    pub(crate) launch_wait_ms: u64,
    pub(crate) launch_retries: u32,
    pub(crate) enter_game_timeout_ms: u64,
    pub(crate) enter_wonderland_timeout_ms: u64,
    pub(crate) wonderland_map_star_retries: u32,
    pub(crate) wonderland_map_star_retry_ms: u64,
    pub(crate) wonderland_hall_retries: u32,
    pub(crate) wonderland_hall_retry_ms: u64,
    pub(crate) wonderland_transition_timeout_ms: u64,
    pub(crate) wonderland_confirm_stable_timeout_ms: u64,
    pub(crate) final_primary_timeout_ms: u64,
    /// 加载界面停留超过这个时长就重启游戏；0 = 关闭该功能。
    pub(crate) loading_restart_timeout_ms: u64,
    /// 是否启用「加载界面过久自动重启游戏」。
    pub(crate) restart_game_on_loading: bool,
    pub(crate) poll_ms: u64,
    pub(crate) stable_mean_threshold: f32,
    pub(crate) stable_changed_ratio_threshold: f32,
    pub(crate) template_threshold: f32,
    pub(crate) wonderland_confirm_threshold: f32,
    pub(crate) world_wish_threshold: f32,
    pub(crate) templates: StartupUiTemplates,
    pub(crate) enter_game_text_region: Rect,
    pub(crate) world_wish_region: Rect,
    pub(crate) wonderland_hall_ocr_region: Rect,
    /// 通用「确认」按钮的搜索区（与好友操作确认弹窗共用）。
    pub(crate) confirm_button_region: Rect,
    /// 确认弹窗关键词文案行搜索区，用于点击前判定子类型。
    pub(crate) confirm_message_region: Rect,
    pub(crate) main_ui_region: Rect,
    pub(crate) wonderland_map_star_region: Rect,
}

#[derive(Clone)]
pub(crate) struct StartupUiTemplates {
    pub(crate) wonderland_map_star: PathBuf,
    pub(crate) confirm_button: PathBuf,
    /// 「进入大厅」确认弹窗关键词文案；启动流程只接受这一子类型。
    pub(crate) confirm_message_random_hall: PathBuf,
    pub(crate) paimon_menu: PathBuf,
    pub(crate) world_wish: PathBuf,
}

impl StartupRoutineConfig {
    pub(crate) fn resolve(
        startup: StartupUiConfig,
        residency: FriendDeliveryRoutineConfig,
        target_process: String,
    ) -> Self {
        Self {
            startup,
            residency,
            target_process,
        }
    }
}

struct EnterGameRoutine {
    request: EnterGame,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

impl sealed::UiRoutineSealed for EnterGameRoutine {}

impl UiRoutine for EnterGameRoutine {
    type Output = EnterGameOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let mut config = self.config;
        if self.request.require_gate {
            match confirm_automatic_entry(context, &self.ocr, &config, UiStateKind::GameGate) {
                Ok(true) => {
                    config.startup.launch_game = false;
                    config.startup.enter_game = true;
                }
                Ok(false) => {
                    return EnterGameOutcome {
                        effect: EnterGameEffect::Skipped,
                    };
                }
                Err(failure) => {
                    return EnterGameOutcome {
                        effect: EnterGameEffect::Failed(failure),
                    };
                }
            }
        }
        let effect = match execute_enter_game(context, &self.ocr, &config, &self.recovery) {
            Ok(effect) => effect,
            Err(failure) => EnterGameEffect::Failed(failure),
        };
        EnterGameOutcome { effect }
    }
}

struct EnterWonderlandRoutine {
    request: EnterWonderland,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
}

impl sealed::UiRoutineSealed for EnterWonderlandRoutine {}

impl UiRoutine for EnterWonderlandRoutine {
    type Output = EnterWonderlandOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        if self.request.require_overworld {
            match confirm_automatic_entry(context, &self.ocr, &self.config, UiStateKind::Overworld)
            {
                Ok(true) => {}
                Ok(false) => {
                    // 自动进入的前提已不成立：本次什么都不做，也**不允许收尾再动界面**。
                    // spec 声明了 `Fixed(Primary)`，不收口就会按 Esc 打断玩家当前界面
                    // （例如正在二级聊天/面板里）；B15-P1-6。
                    context.force_finish();
                    return EnterWonderlandOutcome {
                        effect: EnterWonderlandEffect::Skipped,
                        residency: None,
                    };
                }
                Err(failure) => {
                    return EnterWonderlandOutcome {
                        effect: EnterWonderlandEffect::Failed(failure),
                        residency: None,
                    };
                }
            }
        }
        let mut goal_attempted = false;
        let effect =
            match execute_enter_wonderland(context, &self.ocr, &self.config, &mut goal_attempted) {
                Ok(()) => EnterWonderlandEffect::Entered,
                Err(failure) => EnterWonderlandEffect::Failed(failure),
            };
        // 已强制完成（检测到千星）时不再做收尾导航：直接结束。
        if context.is_force_finished() {
            return EnterWonderlandOutcome {
                effect,
                residency: None,
            };
        }
        let residency = wait_for_primary(
            context,
            &self.config,
            !goal_attempted,
            self.config.startup.final_primary_timeout_ms,
        );
        EnterWonderlandOutcome {
            effect,
            residency: Some(residency),
        }
    }
}

/// 启动链任务的收尾策略：任务结束后没有“可回入口”可恢复，界面停在任务结束处。
///
/// `Leave` 是正式建模（不是兼容层）：不导航、不恢复入口，只如实记录离开时界面；
/// 配合 `EntryPolicy::Passthrough` 保证包装器**不观测、不导航**。
fn enter_game_task_spec(environment: SettleEnvironment, startup: &StartupUiConfig) -> UiTaskSpec {
    UiTaskSpec::new(ResidencyTarget::Primary, FinishPolicy::Leave, environment)
        .with_entry(EntryPolicy::Passthrough)
        .with_total_budget(enter_game_total_budget(startup))
}

/// 进千星的任务目标是一级大厅，收尾语义是 `Fixed(Primary)`；由唯一 residency
/// 引擎验证（已在 Primary 时零动作）。
fn enter_wonderland_task_spec(
    environment: SettleEnvironment,
    config: &StartupRoutineConfig,
) -> UiTaskSpec {
    UiTaskSpec::new(
        ResidencyTarget::Primary,
        FinishPolicy::Fixed(ResidencyTarget::Primary),
        environment,
    )
    .with_entry(EntryPolicy::Passthrough)
    .with_total_budget(enter_wonderland_total_budget(config))
}

/// 收尾观测与失败帧采样的最小保留时间。
const STARTUP_FINISH_MARGIN: Duration = Duration::from_secs(5);

/// `EnterGame` 包装器的总预算。
///
/// 旧实现硬编码 15 秒，而内层仅首次观测就可能要 `enter_game_timeout_ms`（默认
/// 45 秒），开着「加载过久重启」时还要再加一个完整的加载阈值 + 重开周期；
/// 一旦内层正常跑完，包装器收尾阶段拿到的剩余预算是 0，连“停在哪个界面”
/// 都观测不到，成功进入也会被记成收尾失败（B15-P1-6）。
fn enter_game_total_budget(startup: &StartupUiConfig) -> Duration {
    let launch = Duration::from_millis(
        startup
            .launch_wait_ms
            .saturating_mul(u64::from(startup.launch_retries).saturating_add(1)),
    );
    let restart_cycle = if startup.restart_game_on_loading && startup.loading_restart_timeout_ms > 0
    {
        // 关窗核实（5s）+ 重开后的完整观测窗口。
        Duration::from_millis(
            startup
                .loading_restart_timeout_ms
                .saturating_add(WINDOW_GONE_LIMIT.as_millis() as u64)
                .saturating_add(startup.enter_game_timeout_ms),
        )
    } else {
        Duration::ZERO
    };
    enter_game_budget_for(startup) + launch + restart_cycle + STARTUP_FINISH_MARGIN
}

/// `EnterWonderland` 包装器的总预算：逐阶段超时之和 + 收尾余量。
///
/// 内层每一段都有自己的期限（派蒙菜单、地图打开、星标定位、大厅选项、
/// 确认弹窗、过场稳定、一级驻留）；包装器的预算必须覆盖这些上限，
/// 否则收尾阶段会在 0 预算下观测，凭空产生一次“驻留失败”。
fn enter_wonderland_total_budget(config: &StartupRoutineConfig) -> Duration {
    let startup = &config.startup;
    let observation = config.residency.timeout_ms();
    let mut total_ms = observation.saturating_mul(2); // 派蒙菜单 + 确认按钮
    total_ms = total_ms.saturating_add(startup.wonderland_transition_timeout_ms.saturating_mul(3));
    total_ms = total_ms.saturating_add(startup.enter_wonderland_timeout_ms.saturating_mul(2));
    total_ms = total_ms
        .saturating_add(startup.wonderland_confirm_stable_timeout_ms)
        .saturating_add(startup.final_primary_timeout_ms)
        .saturating_add(startup.poll_ms)
        .saturating_add(STARTUP_FINISH_MARGIN.as_millis() as u64);
    Duration::from_millis(total_ms)
}

/// 包装器在“开始前取消/入口失败”时没有内层输出；旧调用点只认识旧 Outcome 类型，
/// 这里把它如实降级为业务失败，而不是凭空成功。
fn task_not_started_failure(failure: Option<UiRoutineFailure>) -> UiRoutineFailure {
    failure.unwrap_or_else(|| {
        UiRoutineFailure::new(
            InputCertainty::Cancelled,
            "ui_task_start",
            "UI 任务在开始前已取消，内层例程没有执行",
        )
    })
}

struct EnterGameTask {
    request: EnterGame,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

impl sealed::UiRoutineSealed for EnterGameTask {}

impl UiRoutine for EnterGameTask {
    type Output = EnterGameOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        // 进入游戏期间的收尾恢复也必须记进进程级协调器：加载卡死重启用的就是
        // 这一份，两边各建一份会让冷却与互斥对彼此不可见。
        let environment = SettleEnvironment::new(self.ocr.clone(), self.config.residency.clone())
            .with_recovery(self.recovery.clone());
        let spec = enter_game_task_spec(environment, &self.config.startup);
        let outcome = UiTask::new(
            spec,
            EnterGameRoutine {
                request: self.request,
                ocr: self.ocr,
                config: self.config,
                recovery: self.recovery,
            },
        )
        .execute(context);
        match outcome.output {
            Some(output) => output,
            None => EnterGameOutcome {
                effect: EnterGameEffect::Failed(task_not_started_failure(outcome.entry_failure)),
            },
        }
    }
}

struct EnterWonderlandTask {
    request: EnterWonderland,
    ocr: OcrRuntimeHandle,
    config: StartupRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

impl sealed::UiRoutineSealed for EnterWonderlandTask {}

impl UiRoutine for EnterWonderlandTask {
    type Output = EnterWonderlandOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        // 与进入游戏、加载卡死观测共用同一份协调器（收尾恢复也记账）。
        let environment = SettleEnvironment::new(self.ocr.clone(), self.config.residency.clone())
            .with_recovery(self.recovery);
        let spec = enter_wonderland_task_spec(environment, &self.config);
        let outcome = UiTask::new(
            spec,
            EnterWonderlandRoutine {
                request: self.request,
                ocr: self.ocr,
                config: self.config,
            },
        )
        .execute(context);
        match outcome.output {
            Some(output) => output,
            None => {
                let failure = task_not_started_failure(outcome.entry_failure);
                EnterWonderlandOutcome {
                    effect: EnterWonderlandEffect::Failed(failure.clone()),
                    residency: Some(UiResidencyOutcome::Failed(failure)),
                }
            }
        }
    }
}

fn confirm_automatic_entry(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
    expected: UiStateKind,
) -> Result<bool, UiRoutineFailure> {
    for _ in 0..TEMPLATE_STABLE_HITS {
        let image = capture_normalized(
            context,
            &config.residency,
            "recheck_automatic_entry",
            InputCertainty::BeforeInput,
        )?;
        let visible = match expected {
            UiStateKind::GameGate => {
                find_enter_game_text(ocr, &image, config, InputCertainty::BeforeInput)?.is_some()
            }
            UiStateKind::Overworld => template_visible(
                &image,
                config.startup.world_wish_region,
                &config.startup.templates.world_wish,
                config.startup.world_wish_threshold,
                InputCertainty::BeforeInput,
            )?,
            _ => unreachable!("automatic entry only starts from the gate or overworld"),
        };
        if !visible {
            // 界面已变化说明玩家自己切走了，跳过是正常行为，不是失败；
            // 但「跳过了什么」对排查有价值，保持 info 偏噪音，降到 debug。
            log::debug!(
                "自动进入游戏任务跳过（步骤：复查自动进入条件）：界面已变化，不再满足进入前提，本次不执行自动进入；本次期望的界面是「{}」，实际已不是它",
                expected.label()
            );
            return Ok(false);
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "recheck_automatic_entry",
            InputCertainty::BeforeInput,
        )?;
    }
    Ok(true)
}

/// 当前界面是否已到达目标；命中就立即强制完成，不再继续任何步骤。
///
/// 用户要求：进入游戏只要检测到主世界就无条件停止；进入千星只要检测到千星就
/// 无条件停止。这里直接读最近一次 UI 分类，不额外截图、不发输入。
///
/// 判据由调用方给出，**只有真正的目标界面才算到达**：进千星的唯一到达条件是
/// [`UiStateKind::Primary`]（千星一级大厅）。地图（`Secondary`·千星地图）、
/// 派蒙菜单（`MainMenu`）、模态面板（`Panel`）都是流程**中间**界面，命中它们就
/// `force_finish` 等于在用户文档流程第 2 步宣告成功
/// （`docs/user-guide.md:34/41`：按 M 开地图 → 点星标 → 选「千星奇域/大厅」→
/// 点「确认」→ 停留在可继续操作的界面）。
fn force_finish_if_kind(context: &mut UiRoutineContext<'_>, kinds: &[UiStateKind]) -> bool {
    let Some(observation) = context.latest_ui_state() else {
        return false;
    };
    let crate::runtime::ui::UiStateObservation::Classified(state) = observation else {
        return false;
    };
    if kinds.contains(&state.classification().kind()) {
        context.force_finish();
        return true;
    }
    false
}

/// 等待游戏窗口真正消失；超时返回 false。
///
/// `close_window` 返回成功只代表“关闭请求已发出”，窗口可能要几十到几百毫秒
/// 才从系统里消失。若不核实就调用 `ensure_game_window`，会直接复用旧窗口并
/// 返回成功——重启实际没发生，却消耗掉了唯一一次重开预算（B15-P1-7）。
///
/// 等待必须**可取消**：这里占着 UI 例程和扫描租约，用 `std::thread::sleep`
/// 会让取消请求一直压到超时之后。
fn wait_game_window_gone(
    context: &mut UiRoutineContext<'_>,
    limit: Duration,
) -> Result<bool, UiRoutineFailure> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if context.device().ensure_window().is_err() {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let step = remaining
            .min(Duration::from_millis(200))
            .max(Duration::from_millis(1));
        wait_ms(
            context,
            step.as_millis() as u64,
            "wait_restart_window_gone",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    Ok(context.device().ensure_window().is_err())
}

/// 等待旧窗口消失的最长时间；超时即放弃这次重启（不消耗重开预算）。
const WINDOW_GONE_LIMIT: Duration = Duration::from_secs(5);

/// 关掉旧游戏窗口 → 核实它真的消失 → 重新拉起。
///
/// 三步必须**无条件**按顺序执行：`close_window` 的返回值不代表窗口已消失，
/// 只代表关闭请求已送出。缺少核实就会复用濒死旧窗口（假重启，B15-P1-7）。
fn restart_game_window(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    window_gone_limit: Duration,
) -> Result<(), UiRoutineFailure> {
    if let Err(error) = context.device().close_window() {
        log::warn!("加载过久重启时关闭游戏窗口失败，继续核实窗口是否已消失：{error:#}");
    }
    if !wait_game_window_gone(context, window_gone_limit)? {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "restart_game_on_loading_unavailable",
            "界面卡在加载，但关闭游戏后窗口仍然存在；继续重开会复用旧进程（等于没有重启），已放弃自动重启",
        ));
    }
    // 与首次启动同一路径：开着外部聊天源时由桥重新创建进程并注入 DLL。
    ensure_game_window(context, config)
}

/// 「进入游戏」阶段的观测预算。
///
/// 平时就是 `enter_game_timeout_ms`；开启「加载过久重启」时至少要能看到
/// 加载阈值那一刻，否则判定条件永远不成立（见调用处注释）。
fn enter_game_budget(config: &StartupRoutineConfig) -> Duration {
    enter_game_budget_for(&config.startup)
}

fn enter_game_budget_for(startup: &StartupUiConfig) -> Duration {
    let base = startup.enter_game_timeout_ms;
    if !startup.restart_game_on_loading || startup.loading_restart_timeout_ms == 0 {
        return Duration::from_millis(base);
    }
    // 首段必须**严格活过**「加载阈值 + 一次轮询」：只取到阈值那一刻会与循环
    // 条件 `Instant::now() < deadline` 打平，判定永远赶不上触发
    //（默认 45s 期限 vs 60s 阈值时更极端：连阈值都到不了）。
    // 用户明确要求默认阈值为 60 秒，因此这个阶段的观测窗口必须容得下它。
    let needed = startup
        .loading_restart_timeout_ms
        .saturating_add(startup.poll_ms.max(1));
    Duration::from_millis(base.max(needed))
}

fn execute_enter_game(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
    recovery: &GameRecoveryCoordinator,
) -> Result<EnterGameEffect, UiRoutineFailure> {
    ensure_game_window(context, config)?;
    context
        .device()
        .focus(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("focus_game_window", error))?;
    if !config.startup.enter_game {
        return Ok(EnterGameEffect::WindowReady);
    }

    // 加载界面过久的自愈：必须在**启动流程内部**判，因为这段期间主循环拿不到
    // 扫描租约（UI 例程独占界面），主循环根本看不到 Loading 状态。
    //
    // 做法：
    //   1. 观察到 Loading 就把起点交给恢复协调器（`loading_since` 原先住在这里）；
    //   2. 超过阈值就关闭游戏并重新拉起（走与首次启动完全相同的
    //      `ensure_game_window`，因此桥注入路径自然复用，不会绕过 DLL 注入）；
    //   3. 只在本次任务内重试 `LOADING_RESTART_MAX_ATTEMPTS` 次，避免自我循环成
    //      无限重开；预算由协调器持有，其它触发源（Wave 1b 的外部源失联重启）
    //      也看同一份互斥，不会同时关游戏。
    //
    // 时间语义与原先逐条等价：起点是「第一次观察到加载界面」的时刻，离开加载
    // 界面立即清零，重启真正完成时清零（重开要重新走一遍加载）。
    recovery.begin_loading_watch(LOADING_RESTART_MAX_ATTEMPTS);
    // 观测窗口必须**容得下**加载阈值：默认 enter_game_timeout_ms=45s、
    // 加载阈值=60s，若直接用前者当期限，循环 45 秒就退出，计时
    // 永远到不了 60 秒——功能即使打开也永不触发。开启该功能时把首段期限
    // 放宽到「加载阈值 + 一次轮询」，保证至少有机会命中。
    let mut deadline = Instant::now() + enter_game_budget(config);
    let mut paimon_streak = 0_u32;
    let mut clicked = false;
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "observe_enter_game",
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )?;
        // 「加载界面过久」判定必须放在**抓帧之后**：`latest_ui_state()` 读的是
        // 最近一次抓帧分类的结果，抓帧前读拿到的是上一轮（甚至启动前）的旧状态，
        // 会拿错误的状态去判 60 秒超时。这里刚捕获完，分类已同步更新。
        if config.startup.restart_game_on_loading && config.startup.loading_restart_timeout_ms > 0 {
            let is_loading = context.latest_ui_state().is_some_and(|observation| {
                matches!(
                    observation,
                    crate::runtime::ui::UiStateObservation::Classified(state)
                        if state.stable_kind() == Some(UiStateKind::Loading)
                )
            });
            let timeout = Duration::from_millis(config.startup.loading_restart_timeout_ms);
            match recovery.observe_loading_state(
                Instant::now(),
                is_loading,
                timeout,
                LOADING_RESTART_MAX_ATTEMPTS,
            ) {
                // 不在加载界面 / 未到阈值 / 预算已用尽：继续正常观测，不干预。
                LoadingStuckDecision::NotLoading
                | LoadingStuckDecision::Waiting { .. }
                | LoadingStuckDecision::BudgetExhausted { .. } => {}
                LoadingStuckDecision::TriggerRestart {
                    attempt,
                    elapsed,
                    lease,
                } => {
                    // 只认聊天桥：反作弊只允许「创建游戏进程的一方」注入 DLL，
                    // 本进程用 `device().launch_game()` 直接拉起来的进程拿不到注入，
                    // 等于把一个没有聊天源的游戏重新跑起来（用户明确禁止这条路径）。
                    // 放弃时租约随作用域归还，且这次没有真正重启，不消耗重开预算。
                    if config.startup.external_launcher.is_none() {
                        return Err(UiRoutineFailure::new(
                            InputCertainty::BeforeInput,
                            "restart_game_on_loading_unavailable",
                            "界面卡在加载，但当前没有可用的自动拉起路径：必须启用外部聊天源并打开「自动启动 Miliastra-Wonderland-ChatBridge」才能自动重启游戏（只有聊天桥能创建可注入的游戏进程）。已放弃自动重启以免游戏无法恢复",
                        ));
                    }
                    log::warn!(
                        "启动游戏时界面停在加载界面已 {} 秒（判定阈值 {} 秒），重启游戏重试（第 {} 次）",
                        elapsed.as_secs(),
                        timeout.as_secs(),
                        attempt
                    );
                    // 关闭失败不算致命：加载卡死的常见形态正是游戏已经崩溃、
                    // 窗口不复存在，那种情况下"关不掉"恰恰等于"已经关了"。
                    // 但**无论 close_window 返回什么**都必须核实窗口真的消失：
                    // 它只代表关闭请求已发出；若窗口还在，`ensure_game_window`
                    // 会直接复用旧进程并返回成功，于是"重启"实际没发生，
                    // 却已经消耗掉唯一一次重开机会（假重启，B15-P1-7）。
                    restart_game_window(context, config, WINDOW_GONE_LIMIT)?;
                    context
                        .device()
                        .focus(config.residency.after_activate_ms)
                        .map_err(|error| before_input_failure("focus_game_window", error))?;
                    // 重启**确实完成**之后才记账：预算消耗、加载计时清零都在这里，
                    // 同时把这次重启报到协调器，供其它触发源共享冷却与健康汇报。
                    lease.complete_restart(Instant::now());
                    // 截止时间顺延一个完整周期：重开游戏要重新走一遍加载，
                    // 沿用旧截止时间会把这次重试直接判成超时失败。
                    deadline = Instant::now()
                        + Duration::from_millis(config.startup.enter_game_timeout_ms);
                    paimon_streak = 0;
                    clicked = false;
                    continue;
                }
            }
        }
        // 用户要求：只要检测到主世界就无条件停止，完成。
        if force_finish_if_kind(context, &[UiStateKind::Overworld]) {
            return Ok(EnterGameEffect::Entered);
        }
        if template_visible(
            &image,
            config.startup.main_ui_region,
            &config.startup.templates.paimon_menu,
            config.startup.template_threshold,
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )? {
            paimon_streak = paimon_streak.saturating_add(1);
            if paimon_streak >= TEMPLATE_STABLE_HITS {
                return Ok(EnterGameEffect::Entered);
            }
        } else {
            paimon_streak = 0;
        }

        if let Some(point) = find_enter_game_text(
            ocr,
            &image,
            config,
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )? {
            context
                .device()
                .click_point(point.x, point.y)
                .map_err(|error| {
                    UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "click_enter_game",
                        format!("{error:#}"),
                    )
                })?;
            clicked = true;
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "confirm_enter_game",
            if clicked {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )?;
    }
    // 诊断：重开过一次之后仍然没进游戏，与"从头就没进去"是两回事，
    // 日志里必须能区分，否则排查时看不出自动重启到底有没有生效。
    let loading_restarts = recovery.loading_restart_attempts();
    let reason = if loading_restarts > 0 {
        format!(
            "paimon menu template did not become stable before timeout（已自动重启游戏 {} 次仍未进入，加载界面可能持续卡死）",
            loading_restarts
        )
    } else {
        "paimon menu template did not become stable before timeout".to_string()
    };
    Err(UiRoutineFailure::new(
        if clicked {
            InputCertainty::AfterInputUnknown
        } else {
            InputCertainty::ConfirmedFailure
        },
        "confirm_enter_game",
        reason,
    ))
}

/// 让 Miliastra-Wonderland-ChatBridge 拉起游戏，并等待窗口出现。
///
/// Miliastra-Wonderland-ChatBridge 会用 CREATE_SUSPENDED 启动游戏、注入 DLL、再恢复主线程，
/// 之后游戏窗口才出现。BOT 这里只负责等。
fn ensure_game_window_via_chat_api(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    chat_source: &crate::config::ChatSourceConfig,
) -> Result<(), UiRoutineFailure> {
    // `Ok(false)` 表示桥没有真的被拉起（已监听 / 退避中 / 等待游戏退出 /
    // 退出码 4 已永久停止）。这时不能盲等窗口——那会白等满
    // `launch_retries × launch_wait_ms`（默认 60 秒）才超时，日志里只看到
    // 一句"窗口一直没出现"，看不出真正原因是桥没启动。
    let launched =
        crate::runtime::chat_source::launcher::ensure_running(chat_source, &config.target_process)
            .map_err(|error| {
                UiRoutineFailure::new(
                    InputCertainty::BeforeInput,
                    "launch_chat_api",
                    format!("{error:#}"),
                )
            })?;
    if !launched {
        return Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "launch_chat_api",
            "Miliastra-Wonderland-ChatBridge 未被启动（已在监听、处于启动退避，或上次以「链路丢失」结束而停止自动重启）；请确认游戏已退出、聊天桥可正常启动",
        ));
    }
    for _ in 0..config.startup.launch_retries.max(1) {
        wait_ms(
            context,
            config.startup.launch_wait_ms,
            "wait_game_window",
            InputCertainty::AfterInputUnknown,
        )?;
        if context.device().ensure_window().is_ok() {
            return Ok(());
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "wait_game_window",
        "Miliastra-Wonderland-ChatBridge 启动了游戏但窗口一直没出现",
    ))
}

fn ensure_game_window(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    if context.device().ensure_window().is_ok() {
        return Ok(());
    }
    // 外部聊天源接管游戏启动：只有 Miliastra-Wonderland-ChatBridge 能创建可注入的游戏进程。
    if let Some(chat_source) = config.startup.external_launcher.as_ref() {
        return ensure_game_window_via_chat_api(context, config, chat_source);
    }
    if !config.startup.launch_game {
        return Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "ensure_game_window",
            "game window is missing and startup.launch_game is false",
        ));
    }
    let executable = resolve_game_executable(&config.startup.exe_path, &config.target_process)
        .map_err(|error| before_input_failure("resolve_game_path", error))?;
    if !executable.exists() {
        return Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "resolve_game_path",
            format!("game executable does not exist: {}", executable.display()),
        ));
    }
    let args = split_command_args(&config.startup.game_args)
        .map_err(|error| before_input_failure("parse_game_args", error))?;
    context
        .device()
        .launch_game(&executable, &args)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "launch_game",
                format!("{error:#}"),
            )
        })?;
    for _ in 0..config.startup.launch_retries.max(1) {
        wait_ms(
            context,
            config.startup.launch_wait_ms,
            "wait_game_window",
            InputCertainty::AfterInputUnknown,
        )?;
        if context.device().ensure_window().is_ok() {
            return Ok(());
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "wait_game_window",
        "launched game process but target window did not appear",
    ))
}

fn execute_enter_wonderland(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
    goal_attempted: &mut bool,
) -> Result<(), UiRoutineFailure> {
    context
        .device()
        .ensure_window()
        .map_err(|error| before_input_failure("ensure_wonderland_window", error))?;
    context
        .device()
        .focus(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("focus_wonderland_window", error))?;
    // 已经在千星**一级大厅**才直接完成：地图/派蒙菜单/面板都是流程中间界面，
    // 在那里强制完成等于把「按 M 打开地图」当成进千星成功（`docs/user-guide.md:41`）。
    if force_finish_if_kind(context, &[UiStateKind::Primary]) {
        return Ok(());
    }
    wait_for_paimon_menu(context, config)?;

    context
        .device()
        .press_key(Key::Unicode('m'))
        .map_err(|error| after_input_failure("open_wonderland_map", error))?;

    // **先确认地图真的开了，再去找星标。**
    //
    // 原来这里直接进 `wait_template_hit` 重试 120 次 x 2.5 秒（最长 5 分钟），
    // 而 `M` 是有可能不生效的（例如聊天框正开着，`M` 被打进了输入框）。
    // 那种情况下机器人会白等 5 分钟，最后把失败报成「找不到星标」——
    // **诊断是错的**，看不到「地图根本没打开」这个真正的原因。
    //
    // 为什么不需要给提瓦特地图单独建判据：第 71 节实测，提瓦特地图 4 张 + 千星地图 1 张
    // **全部不是 `overworld`**，世界判据一律报 `not_applicable`（地图盖住了左上角 HUD）；
    // 而真·提瓦特大世界 3 张对照全部是 `overworld` / `teyvat`。
    // 所以「世界判据由真变假」就是地图已开的充分可观测证据。
    // （第 71 节也试过为提瓦特地图单独建区域/二维统计判据，全部过拟合失败，见该节。）
    wait_for_wonderland_map_open(context, config)?;
    // 地图刚打开时若分类器已经报一级（地图没盖住大厅），才算已到达；地图本身
    // （`Secondary`/千星地图）不是到达证据。
    if force_finish_if_kind(context, &[UiStateKind::Primary]) {
        return Ok(());
    }

    let map_attempts = capped_attempts(
        config.startup.wonderland_map_star_retries,
        config.startup.enter_wonderland_timeout_ms,
        config.startup.wonderland_map_star_retry_ms,
    );
    let map_interval_ms = config.startup.wonderland_map_star_retry_ms.max(100);
    let map_timeout_ms = (map_attempts as u64).saturating_mul(map_interval_ms).min(
        config
            .startup
            .enter_wonderland_timeout_ms
            .max(map_interval_ms),
    );
    let map_star = wait_template_hit(
        context,
        config,
        TemplateHit {
            template: &config.startup.templates.wonderland_map_star,
            region: config.startup.wonderland_map_star_region,
            threshold: config.startup.template_threshold,
            timeout_ms: map_timeout_ms,
            poll_ms: map_interval_ms,
            certainty: InputCertainty::AfterInputUnknown,
        },
    )?;
    let Some(map_star) = map_star else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_wonderland_map_star",
            "wonderland map star template was not found",
        ));
    };
    context
        .device()
        .click_point(map_star.x, map_star.y)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "click_wonderland_map_star",
                format!("{error:#}"),
            )
        })?;
    *goal_attempted = true;
    // 点了星标之后只有落到一级大厅才算成功；侧栏/确认弹窗仍要继续走完。
    if force_finish_if_kind(context, &[UiStateKind::Primary]) {
        return Ok(());
    }
    wait_template_absent(
        context,
        config,
        TemplateAbsence {
            template: &config.startup.templates.wonderland_map_star,
            region: config.startup.wonderland_map_star_region,
            threshold: config.startup.template_threshold,
            timeout_ms: config.startup.wonderland_transition_timeout_ms,
            stage: "confirm_wonderland_map_star_absent",
            failure_message: "wonderland map star template did not disappear",
        },
    )?;

    let hall_attempts = capped_attempts(
        config.startup.wonderland_hall_retries,
        config.startup.enter_wonderland_timeout_ms,
        config.startup.wonderland_hall_retry_ms,
    );
    let Some(hall_point) = wait_wonderland_hall_text(context, ocr, config, hall_attempts)? else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_wonderland_hall",
            "OCR did not find a 千星奇域/大厅 option",
        ));
    };
    context
        .device()
        .click_point(hall_point.x, hall_point.y)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "select_wonderland_hall",
                format!("{error:#}"),
            )
        })?;

    let Some(confirm_point) =
        wait_confirm_button(context, config, InputCertainty::AfterInputUnknown)?
    else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_wonderland_confirm",
            "confirm button with the enter-hall dialog subtype was not found",
        ));
    };
    context
        .device()
        .click_point(confirm_point.x, confirm_point.y)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "confirm_enter_wonderland",
                format!("{error:#}"),
            )
        })?;
    confirm_wonderland_transition(context, config)
}

/// 按 `M` 之后确认「地图真的盖住了大世界」。
///
/// 证据（第 71 节，`technical-docs/ui-state-criteria.md` 第 27 章）：
///
/// | 帧 | 实际是什么 | app 判定 |
/// |---|---|---|
/// | 提瓦特地图 x4（`c01-map` / `c02-map-fresh` / `tv_map_1..4`）| 地图 | **不是 `overworld`**，世界判据 `not_applicable` |
/// | 千星地图 x1（`map-screen.png`）| 地图 | **不是 `overworld`**，世界判据 `not_applicable` |
/// | 真·提瓦特大世界 x3（`t-00` / `t-10` / `snow-01`）| 大世界 | `overworld` / 世界判据 `teyvat` |
///
/// 所以这里**不需要**为提瓦特地图另建判据，只要看世界判据有没有由真变假。
/// 连续 `TEMPLATE_STABLE_HITS` 次为假才算数，避免中间夹一帧过渡画面就误判。
fn wait_for_wonderland_map_open(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_transition_timeout_ms);
    let mut left_world = 0_u32;
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "observe_wonderland_map",
            InputCertainty::AfterInputUnknown,
        )?;
        if !world_is_teyvat(&image) {
            left_world = left_world.saturating_add(1);
            if left_world >= TEMPLATE_STABLE_HITS {
                return Ok(());
            }
        } else {
            left_world = 0;
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "open_wonderland_map",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "open_wonderland_map",
        "pressed M but the world criterion still holds: the map did not open (key likely swallowed by an input box)",
    ))
}

fn wait_for_paimon_menu(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(config.residency.timeout_ms());
    let mut streak = 0_u32;
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "observe_wonderland_primary",
            InputCertainty::ConfirmedFailure,
        )?;
        if template_visible(
            &image,
            config.startup.main_ui_region,
            &config.startup.templates.paimon_menu,
            config.startup.template_threshold,
            InputCertainty::BeforeInput,
        )? {
            streak = streak.saturating_add(1);
            if streak >= TEMPLATE_STABLE_HITS {
                return Ok(());
            }
        } else {
            streak = 0;
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "observe_wonderland_primary",
            InputCertainty::ConfirmedFailure,
        )?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "observe_wonderland_primary",
        "paimon menu template did not become stable before timeout",
    ))
}

fn confirm_wonderland_transition(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let region = config.startup.confirm_button_region;
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_transition_timeout_ms);
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_wonderland_absent",
            InputCertainty::AfterInputUnknown,
        )?;
        if !template_visible(
            &image,
            region,
            &config.startup.templates.confirm_button,
            config.startup.wonderland_confirm_threshold,
            InputCertainty::AfterInputUnknown,
        )? {
            return wait_region_stable(context, config, region);
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            "confirm_wonderland_absent",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_wonderland_absent",
        "wonderland confirmation template did not disappear",
    ))
}

fn wait_region_stable(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    region: Rect,
) -> Result<(), UiRoutineFailure> {
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_confirm_stable_timeout_ms);
    let image = capture_normalized(
        context,
        &config.residency,
        "observe_wonderland_transition",
        InputCertainty::AfterInputUnknown,
    )?;
    let mut previous = rect_chat_change_fingerprint(&image, region)
        .map_err(|error| after_input_failure("observe_wonderland_transition", error))?;
    while Instant::now() < deadline {
        wait_ms(
            context,
            config.startup.poll_ms,
            "confirm_wonderland_stable",
            InputCertainty::AfterInputUnknown,
        )?;
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_wonderland_stable",
            InputCertainty::AfterInputUnknown,
        )?;
        let current = rect_chat_change_fingerprint(&image, region)
            .map_err(|error| after_input_failure("confirm_wonderland_stable", error))?;
        let stats = change_stats(&previous, &current);
        if stats.mean_abs_diff <= config.startup.stable_mean_threshold
            && stats.changed_ratio <= config.startup.stable_changed_ratio_threshold
        {
            return Ok(());
        }
        previous = current;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_wonderland_stable",
        "wonderland transition region did not become stable",
    ))
}

fn wait_for_primary(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    allow_escape: bool,
    timeout_ms: u64,
) -> UiResidencyOutcome {
    let result = if allow_escape {
        match wait_for_stable_ui_kind(
            context,
            config.residency.state_observation(),
            None,
            timeout_ms,
            "confirm_startup_primary",
            InputCertainty::ConfirmedFailure,
        ) {
            Ok(UiStateKind::Primary) => Ok(UiStateKind::Primary),
            // 改动前主菜单是被判成 Secondary 的，这里把 MainMenu 并进同一支，
            // 保持启动流程原有行为：按 Escape 关掉浮层再等一级聊天出现（§44.10）。
            Ok(UiStateKind::Secondary | UiStateKind::MainMenu) => {
                if let Err(error) = context.device().press_key(Key::Escape) {
                    return UiResidencyOutcome::Failed(UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "recover_startup_primary",
                        format!("{error:#}"),
                    ));
                }
                wait_for_stable_ui_kind(
                    context,
                    config.residency.state_observation(),
                    Some(UiStateKind::Primary),
                    timeout_ms,
                    "confirm_startup_primary",
                    InputCertainty::AfterInputUnknown,
                )
            }
            Ok(UiStateKind::Unknown) => unreachable!("unknown UI state is never stable"),
            // Loading 也归到失败：等待的稳定状态就是「到大厅了」，
            // 而稳定停在加载界面说明卡住了，不该被当成还没开始（§55）。
            Ok(
                UiStateKind::GameGate
                | UiStateKind::Overworld
                | UiStateKind::Loading
                | UiStateKind::Panel,
            ) => Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "confirm_startup_primary",
                "game is still at the gate, loading, in the overworld, or showing a modal panel",
            )),
            Err(failure) => Err(failure),
        }
    } else {
        wait_for_stable_ui_kind(
            context,
            config.residency.state_observation(),
            Some(UiStateKind::Primary),
            timeout_ms,
            "confirm_startup_primary",
            InputCertainty::AfterInputUnknown,
        )
    };
    match result {
        Ok(UiStateKind::Primary) => UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
        Ok(_) => unreachable!("primary wait returned a non-primary state"),
        Err(failure) => UiResidencyOutcome::Failed(failure),
    }
}

fn find_enter_game_text(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &StartupRoutineConfig,
    certainty: InputCertainty,
) -> Result<Option<Point>, UiRoutineFailure> {
    locate_enter_game_text(ocr, image, config.startup.enter_game_text_region).map_err(|error| {
        UiRoutineFailure::new(certainty, "ocr_enter_game_text", format!("{error:#}"))
    })
}

fn wait_template_absent(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    wait: TemplateAbsence<'_>,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(wait.timeout_ms.max(1));
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            wait.stage,
            InputCertainty::AfterInputUnknown,
        )?;
        if !template_visible(
            &image,
            wait.region,
            wait.template,
            wait.threshold,
            InputCertainty::AfterInputUnknown,
        )? {
            return Ok(());
        }
        wait_ms(
            context,
            config.startup.poll_ms,
            wait.stage,
            InputCertainty::AfterInputUnknown,
        )?;
    }
    // 超时是**排障入口**：trace 级只在显式开启时可见，这里用 debug 给出
    // 「哪个模板、哪个区域、期望多少」——没有这些，投递/启动卡住时无从判断
    // 是模板不匹配还是界面根本不对。
    log::debug!(
        "等待模板「{}」消失超时（步骤：{}）：搜索区域 ({}, {}) {}×{}，期望分数 {:.3}，超时 {}ms 内一直判为可见；本次按「仍可见」继续处理",
        wait.template.display(),
        wait.stage,
        wait.region.x,
        wait.region.y,
        wait.region.width,
        wait.region.height,
        wait.threshold,
        wait.timeout_ms
    );
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        wait.stage,
        wait.failure_message,
    ))
}

fn wait_wonderland_hall_text(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &StartupRoutineConfig,
    attempts: u32,
) -> Result<Option<Point>, UiRoutineFailure> {
    for _ in 0..attempts.max(1) {
        let image = capture_normalized(
            context,
            &config.residency,
            "locate_wonderland_hall",
            InputCertainty::AfterInputUnknown,
        )?;
        if let Some(point) = find_wonderland_hall_text(ocr, &image, config)? {
            return Ok(Some(point));
        }
        wait_ms(
            context,
            config.startup.wonderland_hall_retry_ms.max(100),
            "locate_wonderland_hall",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    Ok(None)
}

fn find_wonderland_hall_text(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    config: &StartupRoutineConfig,
) -> Result<Option<Point>, UiRoutineFailure> {
    let region = config.startup.wonderland_hall_ocr_region;
    let crop = crop_canvas(image, region)
        .map_err(|error| after_input_failure("crop_wonderland_hall", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| after_input_failure("ocr_wonderland_hall", error.into()))?;
    Ok(lines.into_iter().find_map(|line| {
        is_wonderland_hall_text(&line.text).then(|| {
            Point::new(
                region.x + line.bbox.center().x,
                region.y + line.bbox.center().y,
            )
        })
    }))
}

fn is_wonderland_hall_text(text: &str) -> bool {
    let normalized = normalize_lock_text(text);
    normalized.contains("千星奇域") || normalized.contains("大厅")
}

/// 定位通用「确认」按钮，并在点击前先判确认弹窗子类型。
///
/// 启动流程不再单独保存一张「千星确认」模板：它和好友操作弹窗共用
/// `templates.confirm_button` + `moderation.confirm_button_region`。通用按钮
/// 会出现在多种确认弹窗上，所以按钮命中后还要求「进入大厅」关键词文案同时命中；
/// 文案不命中就继续等，超时按未找到处理，绝不盲点。
fn wait_confirm_button(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    certainty: InputCertainty,
) -> Result<Option<Point>, UiRoutineFailure> {
    let deadline =
        Instant::now() + Duration::from_millis(config.startup.wonderland_transition_timeout_ms);
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "locate_wonderland_confirm",
            certainty,
        )?;
        let Some(button) = best_template_hit(
            &image,
            Some(config.startup.confirm_button_region),
            &config.startup.templates.confirm_button,
            config.startup.wonderland_confirm_threshold,
        )
        .map_err(|error| {
            UiRoutineFailure::new(certainty, "locate_wonderland_confirm", format!("{error:#}"))
        })?
        else {
            wait_ms(
                context,
                config.startup.poll_ms,
                "locate_wonderland_confirm",
                certainty,
            )?;
            continue;
        };
        let message = best_template_candidate_downscaled(
            &image,
            Some(config.startup.confirm_message_region),
            &config.startup.templates.confirm_message_random_hall,
            CONFIRM_MESSAGE_SCALE,
        )
        .map_err(|error| {
            UiRoutineFailure::new(certainty, "locate_wonderland_confirm", format!("{error:#}"))
        })?;
        let subtype_ok = message
            .as_ref()
            .is_some_and(|hit| hit.quality_ok && hit.score >= CONFIRM_MESSAGE_MIN);
        if !subtype_ok {
            log::debug!(
                "确认按钮已命中但弹窗子类型不是「进入大厅」（步骤：定位启动流程确认按钮）：本次不点击，继续等待"
            );
            wait_ms(
                context,
                config.startup.poll_ms,
                "locate_wonderland_confirm",
                certainty,
            )?;
            continue;
        }
        return Ok(Some(button.center()));
    }
    log::debug!(
        "等待「进入大厅」确认弹窗超时（步骤：定位启动流程确认按钮）：{}ms 内按钮与文案未同时命中；本次按「未找到」继续",
        config.startup.wonderland_transition_timeout_ms
    );
    Ok(None)
}

fn wait_template_hit(
    context: &mut UiRoutineContext<'_>,
    config: &StartupRoutineConfig,
    wait: TemplateHit<'_>,
) -> Result<Option<Point>, UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(wait.timeout_ms);
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "locate_startup_template",
            wait.certainty,
        )?;
        if let Some(hit) =
            best_template_hit(&image, Some(wait.region), wait.template, wait.threshold).map_err(
                |error| {
                    UiRoutineFailure::new(
                        wait.certainty,
                        "locate_startup_template",
                        format!("{error:#}"),
                    )
                },
            )?
        {
            return Ok(Some(hit.center()));
        }
        wait_ms(
            context,
            wait.poll_ms.max(1),
            "locate_startup_template",
            wait.certainty,
        )?;
    }
    log::debug!(
        "等待模板「{}」出现超时（步骤：定位启动流程模板）：搜索区域 ({}, {}) {}×{}，期望分数 {:.3}，超时 {}ms 内未命中；本次按「未找到」继续",
        wait.template.display(),
        wait.region.x,
        wait.region.y,
        wait.region.width,
        wait.region.height,
        wait.threshold,
        wait.timeout_ms
    );
    Ok(None)
}

fn template_visible(
    image: &image::DynamicImage,
    region: Rect,
    template: &Path,
    threshold: f32,
    certainty: InputCertainty,
) -> Result<bool, UiRoutineFailure> {
    best_template_hit(image, Some(region), template, threshold)
        .map(|hit| hit.is_some())
        .map_err(|error| {
            UiRoutineFailure::new(certainty, "match_startup_template", format!("{error:#}"))
        })
}

fn split_command_args(value: &str) -> anyhow::Result<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ch if ch.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if escaped {
        current.push('\\');
    }
    if quoted {
        anyhow::bail!("startup.game_args contains an unclosed quote");
    }
    if !current.is_empty() {
        args.push(current);
    }
    Ok(args)
}

fn capped_attempts(configured_retries: u32, timeout_ms: u64, interval_ms: u64) -> u32 {
    let interval_ms = interval_ms.max(1);
    let attempts = timeout_ms.max(interval_ms).div_ceil(interval_ms) as u32;
    configured_retries.max(1).min(attempts)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::config::AppConfig;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::ui::{
        UiDevice, UiRuntime, UiStateClassification, UiStateClassifier, execute_routine_for_test,
    };
    use crate::ui::routines::friend_delivery::FriendDeliveryRoutineConfig;
    use anyhow::Result;
    use image::DynamicImage;

    use super::*;

    struct PaimonOnlyDevice {
        frame: DynamicImage,
        /// 按下 M 之后改用的真实帧（例如提瓦特地图）；None 表示画面不变。
        after_m: Option<DynamicImage>,
        keys: Arc<Mutex<Vec<Key>>>,
    }

    struct EmptyOcr;

    impl OcrDevice for EmptyOcr {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Ok(Vec::new())
        }
    }

    impl UiDevice for PaimonOnlyDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            if let Some(after_m) = &self.after_m {
                if self.keys.lock().unwrap().contains(&Key::Unicode('m')) {
                    return Ok(after_m.clone());
                }
            }
            Ok(self.frame.clone())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            self.keys.lock().unwrap().push(key);
            Ok(())
        }

        fn ensure_window(&mut self) -> Result<()> {
            Ok(())
        }

        fn focus(&mut self, _after_activate_ms: u64) -> Result<()> {
            Ok(())
        }
    }

    fn test_startup_config(app: &AppConfig) -> StartupRoutineConfig {
        let startup = &app.startup;
        StartupRoutineConfig::resolve(
            StartupUiConfig {
                external_launcher: None,
                launch_game: startup.launch_game,
                enter_game: startup.enter_game,
                exe_path: startup.exe_path.clone(),
                game_args: startup.game_args.clone(),
                launch_wait_ms: startup.launch_wait_ms,
                launch_retries: startup.launch_retries,
                enter_game_timeout_ms: startup.enter_game_timeout_ms,
                enter_wonderland_timeout_ms: startup.enter_wonderland_timeout_ms,
                wonderland_map_star_retries: startup.wonderland_map_star_retries,
                wonderland_map_star_retry_ms: startup.wonderland_map_star_retry_ms,
                wonderland_hall_retries: startup.wonderland_hall_retries,
                wonderland_hall_retry_ms: startup.wonderland_hall_retry_ms,
                wonderland_transition_timeout_ms: startup.wonderland_transition_timeout_ms,
                wonderland_confirm_stable_timeout_ms: startup.wonderland_confirm_stable_timeout_ms,
                final_primary_timeout_ms: startup.final_primary_timeout_ms,
                loading_restart_timeout_ms: startup.loading_restart_timeout_ms,
                restart_game_on_loading: startup.restart_game_on_loading,
                poll_ms: 1,
                stable_mean_threshold: startup.stable_mean_threshold,
                stable_changed_ratio_threshold: startup.stable_changed_ratio_threshold,
                template_threshold: startup.template_threshold,
                wonderland_confirm_threshold: startup.wonderland_confirm_threshold,
                world_wish_threshold: app.templates.marker_threshold,
                templates: StartupUiTemplates {
                    wonderland_map_star: startup.templates.wonderland_map_star.clone(),
                    paimon_menu: startup.templates.paimon_menu.clone(),
                    confirm_button: app.templates.confirm_button.clone(),
                    confirm_message_random_hall: app.templates.confirm_message_random_hall.clone(),
                    world_wish: app.templates.world_wish.clone(),
                },
                enter_game_text_region: startup.enter_game_text_region.into(),
                world_wish_region: app.screen.world_wish_rect.into(),
                wonderland_hall_ocr_region: startup.wonderland_hall_ocr_region.into(),
                confirm_button_region: app.moderation.confirm_button_region.into(),
                confirm_message_region: app.moderation.confirm_message_region.into(),
                main_ui_region: startup.main_ui_region.into(),
                wonderland_map_star_region: startup.wonderland_map_star_region.into(),
            },
            FriendDeliveryRoutineConfig::from_app(app),
            app.window.target_process.clone(),
        )
    }

    /// 开启「加载过久重启」时，观测窗口必须容得下阈值；否则判定永远不成立。
    ///
    /// 这是真实踩过的坑：默认 `enter_game_timeout_ms=45s` 而阈值 `60s`，
    /// 循环先到 45 秒就退出，功能即使打开也永不触发。
    #[test]
    fn loading_restart_budget_covers_the_threshold() {
        let app = crate::config::bundled_config();
        let mut config = test_startup_config(&app);
        config.startup.restart_game_on_loading = true;
        config.startup.loading_restart_timeout_ms = 60_000;
        config.startup.enter_game_timeout_ms = 45_000;
        config.startup.poll_ms = 1_000;

        let budget = enter_game_budget(&config);
        assert!(
            budget >= Duration::from_millis(61_000),
            "观测窗口必须严格超过阈值（含一次轮询），当前 {budget:?}"
        );
    }

    /// 关闭该功能时预算保持原样，不影响既有「每阶段一分钟」的分配。
    #[test]
    fn loading_restart_budget_is_untouched_when_disabled() {
        let app = crate::config::bundled_config();
        let mut config = test_startup_config(&app);
        config.startup.restart_game_on_loading = false;
        config.startup.loading_restart_timeout_ms = 60_000;
        config.startup.enter_game_timeout_ms = 45_000;

        assert_eq!(enter_game_budget(&config), Duration::from_millis(45_000));
    }

    #[test]
    fn enter_wonderland_accepts_a_paimon_only_primary_screen() {
        if crate::test_support::corpus::skip_without_corpus(
            "enter_wonderland_accepts_a_paimon_only_primary_screen",
        ) {
            return;
        }
        let app = crate::config::bundled_config();
        let mut config = test_startup_config(&app);
        config.startup.enter_wonderland_timeout_ms = 1;
        config.startup.wonderland_map_star_retries = 1;
        config.startup.wonderland_map_star_retry_ms = 1;

        // 真实提瓦特大世界帧：派蒙图标天然在左上角，但没有千星家园模板。
        // 按 M 之后切到真实提瓦特地图帧——地图上没有千星星标，
        // 所以例程必须失败在 locate_wonderland_map_star。
        let frame = image::open("tests/fixtures/ui/overworld-teyvat-1920x1080.jpg")
            .expect("打开真实提瓦特帧失败");
        let map = image::open("tests/fixtures/ui/map-teyvat-1920x1080.png")
            .expect("打开真实提瓦特地图帧失败");

        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start(
            PaimonOnlyDevice {
                frame,
                after_m: Some(map),
                keys: keys.clone(),
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        config.startup.final_primary_timeout_ms = 1;
        let outcome = ui_runtime
            .handle()
            .submit(EnterWonderlandRoutine {
                request: EnterWonderland::default(),
                ocr: ocr_runtime.handle(),
                config,
            })
            .unwrap()
            .wait()
            .unwrap();

        let EnterWonderlandEffect::Failed(failure) = outcome.effect() else {
            panic!("the fixture intentionally has no wonderland home template");
        };
        assert_eq!(failure.stage(), "locate_wonderland_map_star");
        assert_eq!(failure.certainty(), InputCertainty::AfterInputUnknown);
        assert_eq!(keys.lock().unwrap().as_slice(), &[Key::Unicode('m')]);
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn automatic_entry_skips_both_actions_when_the_screen_changed() {
        if crate::test_support::corpus::skip_without_corpus(
            "automatic_entry_skips_both_actions_when_the_screen_changed",
        ) {
            return;
        }
        let app = crate::config::bundled_config();
        let config = test_startup_config(&app);
        let keys = Arc::new(Mutex::new(Vec::new()));
        // 真实过场（暗色加载屏）帧：游戏还在加载，既没有"点击进入"文案，
        // 也没有大世界顶栏锚点——"界面已变化"的真实形态，两个自动进入动作都必须跳过。
        let ui_runtime = UiRuntime::start(
            PaimonOnlyDevice {
                frame: image::open("tests/fixtures/ui/loading-dark-1920x1080.jpg")
                    .expect("打开真实过场帧失败"),
                after_m: None,
                keys: keys.clone(),
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let startup = StartupUi::new(ui_runtime.handle(), ocr_runtime.handle(), config);
        let gate = startup
            .submit_enter_game(EnterGame { require_gate: true })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(gate.effect(), &EnterGameEffect::Skipped);
        let world = startup
            .submit_enter_wonderland(EnterWonderland {
                require_overworld: true,
            })
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(world.effect(), &EnterWonderlandEffect::Skipped);
        assert!(world.residency().is_none());
        assert!(keys.lock().unwrap().is_empty());
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn entering_game_finishes_in_the_overworld_without_claiming_chat_residency() {
        if crate::test_support::corpus::skip_without_corpus(
            "entering_game_finishes_in_the_overworld_without_claiming_chat_residency",
        ) {
            return;
        }
        let app = crate::config::bundled_config();
        let mut config = test_startup_config(&app);
        config.startup.enter_game = true;
        // 真实提瓦特大世界帧：进入游戏后的落地画面，派蒙图标在左上角。
        let frame = image::open("tests/fixtures/ui/overworld-teyvat-1920x1080.jpg")
            .expect("打开真实提瓦特帧失败");
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start(
            PaimonOnlyDevice {
                frame,
                after_m: None,
                keys: keys.clone(),
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let outcome = StartupUi::new(ui_runtime.handle(), ocr_runtime.handle(), config)
            .submit_enter_game(EnterGame::default())
            .unwrap()
            .wait()
            .unwrap();
        assert_eq!(outcome.effect(), &EnterGameEffect::Entered);
        assert!(keys.lock().unwrap().is_empty());
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    /// B15-P0-2：进千星时**只有**一级大厅（`Primary`）算到达。
    ///
    /// 用一帧"界面还停在地图、不是一级大厅"的场景驱动真实入口
    /// `execute_enter_wonderland`：旧判据把 `Secondary`（地图）当到达 →
    /// 立即 `force_finish` 并返回 `Ok`（对外报 `Entered`）；新判据必须继续走流程，
    /// 最终如实失败，绝不把中间界面当成"已进入千星"。
    #[test]
    fn wonderland_map_alone_does_not_count_as_arrival() {
        if crate::test_support::corpus::skip_without_corpus(
            "wonderland_map_alone_does_not_count_as_arrival",
        ) {
            return;
        }
        let app = crate::config::bundled_config();
        let mut config = test_startup_config(&app);
        config.startup.enter_wonderland_timeout_ms = 1;
        config.startup.wonderland_map_star_retries = 1;
        config.startup.wonderland_map_star_retry_ms = 1;
        config.startup.final_primary_timeout_ms = 1;

        // 真实提瓦特地图帧：地图盖住了大世界 HUD，既不是一级大厅也不是主世界。
        let map =
            image::open("tests/fixtures/ui/map-teyvat-1920x1080.png").expect("打开真实地图帧失败");
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start(
            PaimonOnlyDevice {
                frame: map,
                after_m: None,
                keys: keys.clone(),
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();

        let outcome = ui_runtime
            .handle()
            .submit(EnterWonderlandRoutine {
                request: EnterWonderland::default(),
                ocr: ocr_runtime.handle(),
                config,
            })
            .unwrap()
            .wait()
            .unwrap();

        assert!(
            !matches!(outcome.effect(), EnterWonderlandEffect::Entered),
            "地图帧不是千星一级大厅，绝不能报已进入千星：{:?}",
            outcome.effect()
        );
        assert!(
            matches!(outcome.effect(), EnterWonderlandEffect::Failed(_)),
            "该场景没有千星家园模板，必须如实失败：{:?}",
            outcome.effect()
        );
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn wonderland_hall_text_accepts_either_marker() {
        assert!(is_wonderland_hall_text("千星奇域·大厅"));
        assert!(is_wonderland_hall_text("千星奇域"));
        assert!(is_wonderland_hall_text("大厅"));
        assert!(!is_wonderland_hall_text("千星"));
    }

    #[test]
    fn split_command_args_keeps_quoted_text() {
        assert_eq!(
            split_command_args(r#"-screen-fullscreen 0 "-window-title test""#).unwrap(),
            ["-screen-fullscreen", "0", "-window-title test"]
        );
    }

    #[test]
    fn split_command_args_rejects_unclosed_quote() {
        assert!(split_command_args(r#""abc"#).is_err());
    }

    /// P1 迁移批次 3：启动链没有“可回入口”，收尾只能保持任务结束界面；
    /// 进千星的目标是一级，但实际等待/失败仍由例程的 wait_for_primary 负责。
    #[test]
    fn startup_tasks_declare_leave_and_primary_finish() {
        let app = crate::config::bundled_config();
        let config = test_startup_config(&app);
        let environment =
            SettleEnvironment::without_ocr(FriendDeliveryRoutineConfig::from_app(&app));
        let game = enter_game_task_spec(environment.clone(), &config.startup);
        assert_eq!(game.entry, EntryPolicy::Passthrough);
        assert_eq!(
            game.finish,
            FinishPolicy::Leave,
            "EnterGame 结束后没有可回入口，必须正式声明 Leave"
        );

        let wonderland = enter_wonderland_task_spec(environment, &config);
        assert_eq!(wonderland.entry, EntryPolicy::Passthrough);
        assert_eq!(
            wonderland.finish,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            "进千星固定收尾到 Primary，由唯一引擎验证（已在 Primary 时零动作）"
        );

        // B15-P1-6：预算必须覆盖内层各阶段超时，不能再是 15 秒硬编码。
        assert!(
            game.total_budget >= Duration::from_millis(config.startup.enter_game_timeout_ms),
            "EnterGame 预算必须容得下首次观测：{:?}",
            game.total_budget
        );
        assert!(
            game.total_budget > Duration::from_secs(15),
            "旧的 15 秒预算会让收尾阶段没有剩余时间：{:?}",
            game.total_budget
        );
        assert!(
            wonderland.total_budget
                >= Duration::from_millis(config.startup.enter_wonderland_timeout_ms),
            "EnterWonderland 预算必须容得下各阶段超时：{:?}",
            wonderland.total_budget
        );
        assert!(
            wonderland.total_budget
                >= Duration::from_millis(config.startup.final_primary_timeout_ms),
            "EnterWonderland 预算必须容得下最后的一级驻留等待：{:?}",
            wonderland.total_budget
        );
    }

    /// 走向包装器的真实入口（StartupUi）也不能多按一个键：失败时设备输入必须
    /// 与例程自身完全一致（这里只有一次 M）。
    #[test]
    fn wrapped_wonderland_failure_keeps_the_routine_input_log() {
        if crate::test_support::corpus::skip_without_corpus(
            "wrapped_wonderland_failure_keeps_the_routine_input_log",
        ) {
            return;
        }
        let app = crate::config::bundled_config();
        let mut config = test_startup_config(&app);
        config.startup.enter_wonderland_timeout_ms = 1;
        config.startup.wonderland_map_star_retries = 1;
        config.startup.wonderland_map_star_retry_ms = 1;
        config.startup.final_primary_timeout_ms = 1;

        let frame = image::open("tests/fixtures/ui/overworld-teyvat-1920x1080.jpg")
            .expect("打开真实提瓦特帧失败");
        let map = image::open("tests/fixtures/ui/map-teyvat-1920x1080.png")
            .expect("打开真实提瓦特地图帧失败");
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start(
            PaimonOnlyDevice {
                frame,
                after_m: Some(map),
                keys: keys.clone(),
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let startup = StartupUi::new(ui_runtime.handle(), ocr_runtime.handle(), config);

        let outcome = startup
            .submit_enter_wonderland(EnterWonderland::default())
            .unwrap()
            .wait()
            .unwrap();

        let EnterWonderlandEffect::Failed(failure) = outcome.effect() else {
            panic!("夹具故意没有千星家园模板，必须失败在定位星标");
        };
        assert_eq!(failure.stage(), "locate_wonderland_map_star");
        assert!(outcome.residency().is_some());
        assert_eq!(
            keys.lock().unwrap().as_slice(),
            &[Key::Unicode('m')],
            "包装器不得在例程之外补发任何导航输入"
        );
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    // ---- B15-P1-6 / P1-7：跳过不收尾、重启必须核实 ----

    struct AlwaysSecondaryClassifier;

    impl UiStateClassifier for AlwaysSecondaryClassifier {
        fn classify(&mut self, _image: &DynamicImage) -> Result<UiStateClassification> {
            Ok(UiStateClassification::new(
                UiStateKind::Secondary,
                "secondary:chat",
            ))
        }
    }

    /// `Skipped` 表示“本次不执行进入千星”，收尾绝不能因为 spec 的
    /// `Fixed(Primary)` 去按 Esc 关闭玩家当前界面。
    ///
    /// 旧实现只在没有状态分类器时碰巧零输入（settle 在首帧报错）；生产环境
    /// 配有分类器，会真的发导航输入，所以这里必须显式装分类器。
    #[test]
    fn skipped_enter_wonderland_does_not_touch_the_current_screen() {
        if crate::test_support::corpus::skip_without_corpus(
            "skipped_enter_wonderland_does_not_touch_the_current_screen",
        ) {
            return;
        }
        let app = crate::config::bundled_config();
        let config = test_startup_config(&app);
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start_with_state_classifier(
            PaimonOnlyDevice {
                // 过场帧：既没有大世界顶栏锚点，也没有“点击进入”文案 ⇒ Skipped。
                frame: image::open("tests/fixtures/ui/loading-dark-1920x1080.jpg")
                    .expect("打开真实过场帧失败"),
                after_m: None,
                keys: keys.clone(),
            },
            4,
            AlwaysSecondaryClassifier,
            2,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let startup = StartupUi::new(ui_runtime.handle(), ocr_runtime.handle(), config);

        let outcome = startup
            .submit_enter_wonderland(EnterWonderland {
                require_overworld: true,
            })
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(outcome.effect(), &EnterWonderlandEffect::Skipped);
        assert!(
            keys.lock().unwrap().is_empty(),
            "Skipped 之后不允许任何收尾导航按键，实际：{:?}",
            keys.lock().unwrap()
        );
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    struct StickyWindowDevice {
        window_alive: bool,
        ensure_calls: Arc<Mutex<u32>>,
    }

    impl UiDevice for StickyWindowDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(8, 8))
        }

        fn ensure_window(&mut self) -> Result<()> {
            *self.ensure_calls.lock().unwrap() += 1;
            if self.window_alive {
                Ok(())
            } else {
                anyhow::bail!("game window is gone")
            }
        }

        fn close_window(&mut self) -> Result<()> {
            // 关闭请求成功，但窗口没有立刻从系统里消失（假重启的典型形态）。
            Ok(())
        }
    }

    struct RestartWindowOnly {
        config: StartupRoutineConfig,
        limit: Duration,
    }

    impl sealed::UiRoutineSealed for RestartWindowOnly {}

    impl UiRoutine for RestartWindowOnly {
        type Output = std::result::Result<(), UiRoutineFailure>;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            restart_game_window(context, &self.config, self.limit)
        }
    }

    /// `close_window()` 返回 `Ok(())` 只代表“关闭请求已发出”，不代表窗口已消失。
    /// 旧实现只在返回 `Err` 时核实，于是会复用旧窗口、把假重启当成功。
    #[test]
    fn restart_verifies_the_old_window_is_gone_even_when_close_succeeds() {
        let app = crate::config::bundled_config();
        let config = test_startup_config(&app);
        let ensure_calls = Arc::new(Mutex::new(0));
        let execution = execute_routine_for_test(
            StickyWindowDevice {
                window_alive: true,
                ensure_calls: ensure_calls.clone(),
            },
            None,
            2,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            RestartWindowOnly {
                config,
                limit: Duration::from_millis(60),
            },
        );
        assert!(
            execution.output.is_err(),
            "窗口仍在时不得继续重启（会把旧进程复用成“假重启”）"
        );
        assert!(
            *ensure_calls.lock().unwrap() >= 2,
            "必须多次核实窗口是否真的消失"
        );
    }

    /// 关窗核实必须可取消：不能让取消请求压到 5 秒窗口等待之后。
    #[test]
    fn waiting_for_the_old_window_is_cancellable() {
        let app = crate::config::bundled_config();
        let config = test_startup_config(&app);
        let started = std::time::Instant::now();
        let execution = execute_routine_for_test(
            StickyWindowDevice {
                window_alive: true,
                ensure_calls: Arc::new(Mutex::new(0)),
            },
            None,
            2,
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            RestartWindowOnly {
                config,
                limit: Duration::from_secs(5),
            },
        );
        assert!(execution.output.is_err(), "取消后不得假装重启完成");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "取消后不能继续睡满关窗等待时限：{:?}",
            started.elapsed()
        );
    }
}
