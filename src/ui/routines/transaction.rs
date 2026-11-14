//! 统一 UI 任务事务包装器（P1 核心）。
//!
//! 设计见 `technical-docs/ui-task-return-design.md`：
//! 例程只声明“要去的目标 + 收尾策略”，包装器负责
//! 入口观测一次（兼作目标检查）→ 必要时导航 → 执行内层例程 →
//! **finally** 统一走 `settle_to` 回到入口/指定界面。
//!
//! 恢复/收尾只调用 `residency::settle_to`（唯一引擎），本文件不再自带 settle 状态机；
//! hall / friend_delivery / invite / moderation / startup / custom_action /
//! secondary_unread 等例程都通过 `UiTaskSpec` 声明策略，不再各自实现收尾。
//!
//! 两条硬约束：
//! - 收尾必须走显式 `finally`，不能用 `Drop`（worker panic 时上下文/设备不可用）。
//! - 入口观测必须在**任何输入之前**；目标检查复用入口观测，不重复截图。

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use enigo::Key;

use super::friend_delivery::{UiResidencyOutcome, UiResidencyTarget};
use super::residency::{
    ConversationRef, RecoveryReport, ResidencyTarget, SettleEnvironment, SettleOptions,
    identify_conversation, settle_to, settle_to_with,
};
use super::state_observation::{SecondarySurface, UiSurface, surface_of};
use crate::runtime::ui::{
    InputCertainty, UiRoutine, UiRoutineContext, UiRoutineFailure, UiStateKind, UiStateObservation,
};

/// 入口稳定观察轮询。
const ENTRY_POLL: Duration = Duration::from_millis(100);

/// 入口采集策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryPolicy {
    /// 抓一次稳定入口快照，并复用它做“是否已经在目标界面”的判断（默认）。
    Observe,
    /// 抓一次稳定入口快照，但**要求入口已经是目标**；不满足时按 `BeforeInput` 直接拒绝，不做任何导航。
    /// 用于“只允许从一级界面执行”的例程（例如管理操作）。
    RequireTarget,
    /// 不观测，直接认为入口就是目标（调用方自己保证）；不会做入口导航。
    AssumeTarget,
    /// 完全旁路：不观测、不导航、不按键（调试/只读观察用）。
    Passthrough,
}

/// 收尾策略。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FinishPolicy {
    /// 回到入口界面；入口不可重建时退到 `fallback`。
    RestoreEntry { fallback: ResidencyTarget },
    /// 固定在某个驻留目标（值守基线类任务）。
    Fixed(ResidencyTarget),
    /// 任务结束后**不再导航**，也不承诺回到某个入口（启动链进游戏/进千星等）。
    /// 与 `KeepCurrent` 的区别：Leave 是业务语义“离开前不安排收尾”，
    /// KeepCurrent 只用于“恢复器本身不导航”的调试/诊断路径。
    Leave,
    /// 保持收尾时的界面，不做任何导航（恢复器/诊断路径）。
    KeepCurrent,
    /// 由内层脚本自己声明/完成收尾，包装器不再动作（CustomAction 的真实语义）。
    ScriptDeclared,
}

/// 任务开始前的激活策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivationPolicy {
    /// 不激活（默认）。
    None,
    /// 激活/等待前台 `wait_ms` 毫秒。
    Activate { wait_ms: u64 },
}

/// 一次 UI 任务的事务声明。
#[derive(Clone, Debug)]
pub(crate) struct UiTaskSpec {
    pub(crate) target: ResidencyTarget,
    pub(crate) entry: EntryPolicy,
    pub(crate) finish: FinishPolicy,
    pub(crate) activation: ActivationPolicy,
    pub(crate) total_budget: Duration,
    /// 唯一恢复引擎环境（transaction 不再自带恢复配置）。
    pub(crate) environment: SettleEnvironment,
}

impl UiTaskSpec {
    /// 默认总预算：足够一次导航 + 内层例程 + 一次收尾。
    pub(crate) const DEFAULT_TOTAL_BUDGET: Duration = Duration::from_secs(8);

    pub(crate) fn new(
        target: impl Into<ResidencyTarget>,
        finish: FinishPolicy,
        environment: SettleEnvironment,
    ) -> Self {
        Self {
            target: target.into(),
            entry: EntryPolicy::Observe,
            finish,
            activation: ActivationPolicy::None,
            total_budget: Self::DEFAULT_TOTAL_BUDGET,
            environment,
        }
    }

    pub(crate) fn with_entry(mut self, entry: EntryPolicy) -> Self {
        self.entry = entry;
        self
    }

    pub(crate) fn with_activation(mut self, activation: ActivationPolicy) -> Self {
        self.activation = activation;
        self
    }

    pub(crate) fn with_total_budget(mut self, total_budget: Duration) -> Self {
        self.total_budget = total_budget.max(Duration::from_millis(1));
        self
    }
}

/// 入口界面快照。`residency` 只在 `Primary`/`Secondary` 时存在：
/// 其它状态（Unknown/Loading/GameGate/Overworld/Panel/MainMenu）不可作为恢复目标，
/// 但它们仍要参与“是否可导航”的判断。
#[derive(Clone, Debug)]
pub(crate) struct EntryUiState {
    pub(crate) kind: UiStateKind,
    pub(crate) residency: Option<ResidencyTarget>,
    /// 入口所在的会话（仅当调用方明确声明/可确认时才有值；观测到的 Secondary 不猜身份）。
    pub(crate) conversation: Option<ConversationRef>,
    pub(crate) overlays: Vec<String>,
    pub(crate) label: String,
    /// 分类证据里的规则名；用于区分主菜单千星与主菜单大世界（两者 kind 都是 MainMenu）。
    pub(crate) final_rule: String,
    pub(crate) frame_id: Option<u64>,
    pub(crate) observed_at: Instant,
}

impl EntryUiState {
    fn from_classification(
        kind: UiStateKind,
        label: String,
        final_rule: String,
        frame_id: Option<u64>,
        observed_at: Instant,
        conversation: Option<ConversationRef>,
    ) -> Self {
        let residency = match kind {
            UiStateKind::Primary => Some(ResidencyTarget::Primary),
            UiStateKind::Secondary => Some(ResidencyTarget::Secondary(
                conversation.clone().unwrap_or(ConversationRef::Unknown),
            )),
            _ => None,
        };
        Self {
            kind,
            residency,
            conversation,
            overlays: Vec::new(),
            label,
            final_rule,
            frame_id,
            observed_at,
        }
    }

    fn assumed(target: ResidencyTarget) -> Self {
        Self {
            kind: kind_for(&target),
            residency: Some(target.clone()),
            conversation: match &target {
                ResidencyTarget::Primary => None,
                ResidencyTarget::Secondary(conversation) => Some(conversation.clone()),
            },
            overlays: Vec::new(),
            label: "assumed".to_string(),
            final_rule: String::new(),
            frame_id: None,
            observed_at: Instant::now(),
        }
    }

    /// 入口是否值得作为“恢复回去”的目标。
    pub(crate) fn restorable(&self) -> bool {
        self.residency.is_some()
    }

    /// 入口可在恢复时重建的目标。
    ///
    /// 二级入口必须带上真实会话（`conversation` 为空时明确是 `Unknown`，
    /// 表示"只保证在二级"），不允许把 Unknown 猜成当前大厅。
    pub(crate) fn restore_target(&self) -> Option<ResidencyTarget> {
        match &self.residency {
            None => None,
            Some(ResidencyTarget::Primary) => Some(ResidencyTarget::Primary),
            // 观察到 Secondary 但识别不出会话：保守恢复到"二级类型"（AnySecondary，
            // 明确不声明具体会话），不冒充 CurrentHall。
            Some(ResidencyTarget::Secondary(_)) => Some(ResidencyTarget::Secondary(
                self.conversation
                    .clone()
                    .unwrap_or(ConversationRef::AnySecondary),
            )),
        }
    }
}

/// 失败现场帧的证据：默认只有元数据；诊断开启时附带落盘路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FrameEvidence {
    pub(crate) frame_id: Option<u64>,
    pub(crate) captured_at: Option<Instant>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// `MILIASTRA_UI_DIAGNOSTICS=1` 时落盘到 `<exe>/deps/diagnostics`。
    pub(crate) path: Option<PathBuf>,
}

/// 收尾结果。失败不抛异常，由调用方决定要不要重试（见 `UiTaskOutcome::safe_retry`）。
#[derive(Clone, Debug, Default)]
pub(crate) struct FinishReport {
    /// 是否进入了收尾阶段（Leave / ScriptDeclared / 无收尾义务时为 false）。
    pub(crate) attempted: bool,
    /// 是否确认落到目标界面（Leave 时表示“观察到的界面可作为聊天驻留”）。
    pub(crate) settled: bool,
    pub(crate) target: Option<ResidencyTarget>,
    /// 收尾实际发出的输入动作数（由唯一恢复引擎报告）。
    pub(crate) actions: u32,
    /// 声明 Leave：任务结束后不恢复入口，仅记录观察到的界面。
    pub(crate) left: bool,
    /// 脚本声明收尾（CustomAction）：包装器没有确认驻留，不能当 `settled`。
    pub(crate) script_declared: bool,
    /// 入口不可重建而退到 `fallback`。
    pub(crate) fallback_used: bool,
    pub(crate) failure: Option<UiRoutineFailure>,
    /// 唯一恢复引擎的完整报告（成功时）。
    pub(crate) recovery: Option<RecoveryReport>,
    /// 失败诊断（来自引擎失败报告）。
    pub(crate) history: Vec<String>,
    pub(crate) last_snapshot: Option<String>,
    /// 失败/异常时的最近一帧证据（默认仅元数据，诊断开关开启才落盘）。
    pub(crate) last_frame: Option<FrameEvidence>,
}

/// UI 任务统一返回值。
#[derive(Clone, Debug)]
pub(crate) struct UiTaskOutcome<T> {
    /// 入口快照；执行前取消 / 入口观测失败时为 None。
    pub(crate) entry: Option<EntryUiState>,
    /// 内层例程的输出；只要内层开始执行就有 `Some`（即使它返回 Err）。
    pub(crate) output: Option<T>,
    /// 内层例程 panic 的失败（`InputCertainty::Panicked`）；仍会进入收尾。
    pub(crate) output_failure: Option<UiRoutineFailure>,
    /// 内层根本没有开始执行时的原因（入口观测/目标导航失败）。
    pub(crate) entry_failure: Option<UiRoutineFailure>,
    pub(crate) finish: FinishReport,
    /// 收尾后的驻留结论。
    pub(crate) residency: UiResidencyOutcome,
    /// 本次任务从开始到现在**尝试过**的输入次数（由设备计数器差值得到）。
    pub(crate) input_epoch: u64,
}

impl<T> UiTaskOutcome<T> {
    pub(crate) fn output(&self) -> Option<&T> {
        self.output.as_ref()
    }

    /// 兼容旧调用点：直接取驻留结论。
    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }

    /// 只有“本次任务没有发过任何输入”时才允许自动重试：
    /// 一旦发过输入，结果可能已经部分生效（`InputCertainty::AfterInputUnknown`），
    /// 重试必须交给人/上层决策；收尾失败同理。
    pub(crate) fn safe_retry(&self) -> bool {
        self.input_epoch == 0 && self.finish.failure.is_none()
    }
}

/// 包装后的 UI 任务。`R` 是真正干活的例程。
pub(crate) struct UiTask<R: UiRoutine> {
    spec: UiTaskSpec,
    inner: R,
}

impl<R: UiRoutine> UiTask<R> {
    pub(crate) fn new(spec: UiTaskSpec, inner: R) -> Self {
        Self { spec, inner }
    }
}

impl<R: UiRoutine> crate::runtime::ui::sealed::UiRoutineSealed for UiTask<R> {}

impl<R: UiRoutine> UiRoutine for UiTask<R> {
    type Output = UiTaskOutcome<R::Output>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let started_epoch = context.input_epoch();
        let deadline = Instant::now() + self.spec.total_budget;
        let mut outcome = UiTaskOutcome {
            entry: None,
            output: None,
            output_failure: None,
            entry_failure: None,
            finish: FinishReport::default(),
            residency: UiResidencyOutcome::Failed(failure(
                InputCertainty::BeforeInput,
                "ui_task_start",
                "任务尚未进入收尾",
            )),
            input_epoch: 0,
        };

        // 执行前取消：无入口、无输入、无收尾义务。
        if context.is_cancelled() {
            outcome.residency = UiResidencyOutcome::Failed(failure(
                InputCertainty::Cancelled,
                "ui_task_start",
                "任务在开始前已取消",
            ));
            return finish_outcome(outcome, context, started_epoch);
        }

        if let ActivationPolicy::Activate { wait_ms } = self.spec.activation
            && let Err(error) = context.device().activate(wait_ms)
        {
            outcome.entry_failure = Some(failure(
                InputCertainty::BeforeInput,
                "ui_task_activate",
                format!("激活游戏窗口失败: {error:#}"),
            ));
            outcome.residency = UiResidencyOutcome::Failed(
                outcome
                    .entry_failure
                    .clone()
                    .expect("entry failure was just set"),
            );
            return finish_outcome(outcome, context, started_epoch);
        }

        let mut navigate_to_target = false;
        match self.spec.entry {
            EntryPolicy::Passthrough => {
                // 旁路：不观测、不导航；但若收尾策略要求固定目标，仍走统一收尾
                // （例如邀请任务：入口不做任何导航，进新大厅后统一落到 Primary）。
                let (output, panic_failure) = execute_inner(self.inner, context);
                outcome.output = output;
                outcome.output_failure = panic_failure;
                outcome.finish = match &self.spec.finish {
                    FinishPolicy::KeepCurrent | FinishPolicy::ScriptDeclared => {
                        no_action_finish(self.spec.finish.clone(), outcome.entry.as_ref())
                    }
                    FinishPolicy::Leave
                    | FinishPolicy::Fixed(_)
                    | FinishPolicy::RestoreEntry { .. } => run_finish(
                        context,
                        self.spec.finish.clone(),
                        outcome.entry.as_ref(),
                        &self.spec.environment,
                        deadline,
                    ),
                };
                outcome.residency = finish_residency(&outcome.finish);
                return finish_outcome(outcome, context, started_epoch);
            }
            EntryPolicy::AssumeTarget => {
                outcome.entry = Some(EntryUiState::assumed(self.spec.target.clone()));
            }
            EntryPolicy::Observe | EntryPolicy::RequireTarget => {
                let entry_budget = entry_budget(deadline);
                match observe_entry(context, entry_budget, &self.spec.environment) {
                    Ok(entry) => {
                        let matches_target = kind_matches(entry.kind, &self.spec.target);
                        outcome.entry = Some(entry);
                        if self.spec.entry == EntryPolicy::RequireTarget && !matches_target {
                            let entry = outcome
                                .entry
                                .as_ref()
                                .expect("entry was just stored");
                            outcome.entry_failure = Some(failure(
                                InputCertainty::BeforeInput,
                                "ui_task_entry",
                                format!(
                                    "入口界面不满足任务前置要求（需要 {}，当前：{}）；未发送任何输入",
                                    target_label(&self.spec.target),
                                    entry.label
                                ),
                            ));
                            outcome.residency = UiResidencyOutcome::Failed(
                                outcome
                                    .entry_failure
                                    .clone()
                                    .expect("entry failure was just set"),
                            );
                            return finish_outcome(outcome, context, started_epoch);
                        }
                        if self.spec.entry == EntryPolicy::RequireTarget
                            && let ResidencyTarget::Secondary(conversation) =
                                &self.spec.target
                            && *conversation != ConversationRef::Unknown
                        {
                            // 输入前无法从分类证据确认具体会话；保守拒绝，零按键。
                            let entry = outcome
                                .entry
                                .as_ref()
                                .expect("entry was just stored");
                            outcome.entry_failure = Some(failure(
                                InputCertainty::BeforeInput,
                                "ui_task_entry",
                                format!(
                                    "入口会话无法在输入前确认（需要 {}；当前仅确认二级：{}）；未发送任何输入",
                                    target_label(&self.spec.target),
                                    entry.label
                                ),
                            ));
                            outcome.residency = UiResidencyOutcome::Failed(
                                outcome
                                    .entry_failure
                                    .clone()
                                    .expect("entry failure was just set"),
                            );
                            return finish_outcome(outcome, context, started_epoch);
                        }
                        navigate_to_target = !matches_target;
                    }
                    Err(error) => {
                        outcome.entry_failure = Some(error);
                        outcome.residency = UiResidencyOutcome::Failed(
                            outcome
                                .entry_failure
                                .clone()
                                .expect("entry failure was just set"),
                        );
                        return finish_outcome(outcome, context, started_epoch);
                    }
                }
            }
        }

        // 目标检查复用入口观测：已在目标就不导航。
        if navigate_to_target && outcome.entry.is_some() {
            if let Err(error) = settle_to(
                context,
                &self.spec.environment,
                self.spec.target.clone(),
            ) {
                outcome.entry_failure = Some(error);
                // 导航阶段可能已经发过输入（例如按了 Esc 才发现回不去），
                // 不能当成“执行前失败”直接返回：仍要进入 finally 收尾。
            }
        }

        if outcome.entry_failure.is_none() {
            let (output, panic_failure) = execute_inner(self.inner, context);
            outcome.output = output;
            outcome.output_failure = panic_failure;
        }

        // 强制完成：例程已确认到达目标界面（主世界 / 千星），不再执行收尾导航，
        // 也不让后续任务误以为还需要恢复驻留。这是用户要求的“检测到就无条件停”。
        if context.is_force_finished() {
            outcome.finish = FinishReport {
                settled: true,
                target: Some(self.spec.target.clone()),
                ..FinishReport::default()
            };
            outcome.residency = UiResidencyOutcome::Confirmed(match &self.spec.target {
                ResidencyTarget::Primary => UiResidencyTarget::Primary,
                ResidencyTarget::Secondary(_) => UiResidencyTarget::SecondaryCurrentHall,
            });
            return finish_outcome(outcome, context, started_epoch);
        }
        let input_epochs = context.input_epoch().saturating_sub(started_epoch);
        let finish_obligation =
            input_epochs > 0 || outcome.output.is_some() || outcome.output_failure.is_some();
        if finish_obligation {
            outcome.finish = run_finish(
                context,
                self.spec.finish.clone(),
                outcome.entry.as_ref(),
                &self.spec.environment,
                deadline,
            );
            outcome.residency = finish_residency(&outcome.finish);
        } else if let Some(failure) = outcome.entry_failure.clone() {
            outcome.residency = UiResidencyOutcome::Failed(failure);
        }
        finish_outcome(outcome, context, started_epoch)
    }
}

fn finish_outcome<T>(
    mut outcome: UiTaskOutcome<T>,
    context: &UiRoutineContext<'_>,
    started_epoch: u64,
) -> UiTaskOutcome<T> {
    outcome.input_epoch = context.input_epoch().saturating_sub(started_epoch);
    let failed = outcome.finish.failure.is_some()
        || outcome.entry_failure.is_some()
        || outcome.output_failure.is_some();
    if failed && outcome.finish.last_frame.is_none() {
        outcome.finish.last_frame = capture_failure_frame(context);
    }
    outcome
}

fn diagnostics_enabled() -> bool {
    std::env::var("MILIASTRA_UI_DIAGNOSTICS")
        .map(|value| {
            let value = value.trim();
            value == "1" || value.eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

fn diagnostics_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|parent| parent.join("deps").join("diagnostics"))
}

fn write_diagnostics_frame(
    frame: &crate::runtime::ui::CapturedFrame,
) -> Option<PathBuf> {
    let directory = diagnostics_dir()?;
    std::fs::create_dir_all(&directory).ok()?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    // 隐私：文件名只含时间与帧号，不含任何正文/UID/昵称。
    let path = directory.join(format!("ui-{millis}-frame-{}.png", frame.id()));
    frame.image().save(&path).ok()?;
    Some(path)
}

/// 最近一帧的证据：元数据总是记录；只有诊断开关开启时才落盘 PNG。
fn capture_failure_frame(context: &UiRoutineContext<'_>) -> Option<FrameEvidence> {
    let frame = context.last_captured_frame()?;
    let path = diagnostics_enabled()
        .then(|| write_diagnostics_frame(&frame))
        .flatten();
    Some(FrameEvidence {
        frame_id: Some(frame.id()),
        captured_at: Some(frame.captured_at()),
        width: frame.image().width(),
        height: frame.image().height(),
        path,
    })
}

/// 内层例程执行：panic 不穿透包装器，转成 `InputCertainty::Panicked` 失败并继续收尾。
fn execute_inner<R: UiRoutine>(
    inner: R,
    context: &mut UiRoutineContext<'_>,
) -> (Option<R::Output>, Option<UiRoutineFailure>) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.execute(context))) {
        Ok(output) => (Some(output), None),
        Err(payload) => (
            None,
            Some(UiRoutineFailure::panicked("ui_task_panic", &*payload)),
        ),
    }
}

fn entry_budget(deadline: Instant) -> Duration {
    // 入口观测通常 2-4 帧；给总预算的 1/3 封顶 2s，避免入口不稳定时吃掉全部时间。
    let remaining = remaining_until(deadline);
    remaining
        .min(Duration::from_secs(2))
        .max(Duration::from_millis(1))
}

fn remaining_until(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn kind_matches(kind: UiStateKind, target: &ResidencyTarget) -> bool {
    match target {
        ResidencyTarget::Primary => kind == UiStateKind::Primary,
        // 明确声明"在二级即可"的导航目标才按 kind 短路。
        ResidencyTarget::Secondary(ConversationRef::AnySecondary) => {
            kind == UiStateKind::Secondary
        }
        // 具体会话（当前大厅/好友/未识别）必须由恢复引擎校验，不能让"任意 Secondary"过关。
        ResidencyTarget::Secondary(_) => false,
    }
}

fn residency_for(kind: UiStateKind) -> Option<ResidencyTarget> {
    match kind {
        UiStateKind::Primary => Some(ResidencyTarget::Primary),
        UiStateKind::Secondary => {
            // 观测只能确认“在二级”；具体会话身份未知，不能冒充当前大厅。
            Some(ResidencyTarget::Secondary(ConversationRef::Unknown))
        }
        _ => None,
    }
}

fn kind_for(target: &ResidencyTarget) -> UiStateKind {
    match target {
        ResidencyTarget::Primary => UiStateKind::Primary,
        ResidencyTarget::Secondary(_) => UiStateKind::Secondary,
    }
}

fn target_label(target: &ResidencyTarget) -> String {
    match target {
        ResidencyTarget::Primary => "千星大厅（一级大厅）".to_string(),
        ResidencyTarget::Secondary(conversation) => conversation.label(),
    }
}

fn failure(
    certainty: InputCertainty,
    stage: &'static str,
    reason: impl Into<String>,
) -> UiRoutineFailure {
    UiRoutineFailure::new(certainty, stage, reason)
}

/// 一次稳定观测：入口快照与目标检查共用本次截图。
fn observe_entry(
    context: &mut UiRoutineContext<'_>,
    budget: Duration,
    environment: &SettleEnvironment,
) -> Result<EntryUiState, UiRoutineFailure> {
    let deadline = Instant::now() + budget;
    loop {
        let observed = observe_current(context, "ui_task_observe_entry")?;
        if observed.stable {
            // 入口子型：二级聊天识别具体会话；F2 多人面板/千星地图/覆盖层按子型记录。
            let conversation = match surface_of(observed.kind, &observed.label, &observed.final_rule)
            {
                UiSurface::Secondary(SecondarySurface::Chat) => {
                    identify_conversation(context, environment)
                }
                UiSurface::Secondary(SecondarySurface::MultiplayerPanel) => {
                    Some(ConversationRef::MultiplayerPanel)
                }
                UiSurface::Secondary(SecondarySurface::HallMap) => Some(ConversationRef::HallMap),
                UiSurface::Overlay(overlay) => Some(ConversationRef::Overlay(overlay)),
                _ => None,
            };
            return Ok(EntryUiState::from_classification(
                observed.kind,
                observed.label,
                observed.final_rule,
                observed.frame_id,
                observed.observed_at,
                conversation,
            ));
        }
        if Instant::now() >= deadline {
            return Err(failure(
                InputCertainty::BeforeInput,
                "ui_task_observe_entry",
                format!("入口界面状态未稳定（当前：{}）", observed.label),
            ));
        }
        context.wait_cancellable(ENTRY_POLL)?;
    }
}

struct ObservedState {
    kind: UiStateKind,
    label: String,
    final_rule: String,
    frame_id: Option<u64>,
    observed_at: Instant,
    stable: bool,
}

fn observe_current(
    context: &mut UiRoutineContext<'_>,
    stage: &'static str,
) -> Result<ObservedState, UiRoutineFailure> {
    let frame = context
        .capture_frame()
        .map_err(|error| failure(InputCertainty::BeforeInput, stage, format!("{error:#}")))?;
    let observation = context.latest_ui_state().ok_or_else(|| {
        failure(
            InputCertainty::BeforeInput,
            stage,
            "UI runtime 未配置模板状态分类器，无法判断界面",
        )
    })?;
    match observation {
        UiStateObservation::Classified(state) => {
            let classification = state.classification();
            Ok(ObservedState {
                kind: classification.kind(),
                label: classification.label().to_string(),
                final_rule: classification.evidence().final_rule().to_string(),
                frame_id: Some(frame.id()),
                observed_at: frame.captured_at(),
                stable: state.stable_kind().is_some(),
            })
        }
        UiStateObservation::Failed { reason, .. } => Err(failure(
            InputCertainty::BeforeInput,
            stage,
            format!("模板界面状态分类失败: {reason}"),
        )),
    }
}

fn run_finish(
    context: &mut UiRoutineContext<'_>,
    policy: FinishPolicy,
    entry: Option<&EntryUiState>,
    environment: &SettleEnvironment,
    deadline: Instant,
) -> FinishReport {
    let mut report = FinishReport::default();
    if !context.in_finish_phase() {
        context.begin_finish_phase(remaining_until(deadline));
    }
    let target = match policy {
        FinishPolicy::ScriptDeclared => {
            // CustomAction 的真实语义：脚本自己决定收尾，包装器不导航、不观测。
            // 包装器无法确认脚本最后停在哪，绝不能把入口冒充成“已驻留”。
            report.script_declared = true;
            report.settled = false;
            report.target = None;
            return report;
        }
        FinishPolicy::Leave => {
            // 不恢复入口，只如实记录离开时的界面（不承诺驻留）。
            report.left = true;
            return match observe_current(context, "ui_task_leave") {
                Ok(observed) => {
                    report.target = residency_for(observed.kind);
                    report.settled = report.target.is_some();
                    if report.target.is_none() {
                        report.failure = Some(failure(
                            InputCertainty::BeforeInput,
                            "ui_task_leave",
                            format!(
                                "任务声明 Leave：当前界面（{}）不是一级/二级聊天，不能作为聊天驻留结论",
                                observed.label
                            ),
                        ));
                    }
                    report
                }
                Err(error) => {
                    report.failure = Some(error);
                    report
                }
            };
        }
        FinishPolicy::KeepCurrent => {
            // 恢复器/诊断路径：保持当前界面，只观测一次记录实际驻留。
            return match observe_current(context, "ui_task_finish") {
                Ok(observed) => {
                    // 入口/声明会话仍是同一界面类型时，保留已知会话；否则只按 kind
                    // 推断（二级 -> Unknown，不冒充当前大厅）。
                    report.target = match entry {
                        Some(entry) if entry.kind == observed.kind => entry.restore_target(),
                        _ => residency_for(observed.kind),
                    };
                    report.settled = report.target.is_some();
                    if report.settled {
                        return report;
                    }
                    report.failure = Some(failure(
                        InputCertainty::BeforeInput,
                        "ui_task_finish",
                        format!("保持当前界面，但当前界面不可作为驻留结论（{}）", observed.label),
                    ));
                    report
                }
                Err(error) => {
                    report.failure = Some(error);
                    report
                }
            };
        }
        FinishPolicy::Fixed(target) => target,
        FinishPolicy::RestoreEntry { fallback } => {
            let entry_target = entry.and_then(EntryUiState::restore_target);
            // 二级界面（多人面板/地图）与覆盖层本身不可重建：显式走 fallback，并标记降级。
            let entry_specific = entry_target
                .as_ref()
                .is_some_and(|target| matches!(target, ResidencyTarget::Secondary(conversation) if !conversation.is_chat()));
            match entry_target {
                Some(target) if !entry_specific => target,
                _ => {
                    report.fallback_used = true;
                    fallback
                }
            }
        },
    };
    report.attempted = true;
    report.target = Some(target.clone());
    // fallback 是"入口会话无法重建"时的兜底：只做一次确定性导航，不再额外滚动列表。
    let options = SettleOptions {
        allow_conversation_scroll: !report.fallback_used,
        ..SettleOptions::default()
    };
    match settle_to_with(context, environment, target, options) {
        Ok(mut recovery) => {
            report.settled = true;
            // 入口子型（二级界面/覆盖层）不可重建时走 fallback：如实标记降级，
            // 不把"回不到入口子界面"伪装成成功。
            if report.fallback_used {
                recovery.degraded = true;
            }
            report.target = Some(recovery.target.clone());
            report.actions = recovery
                .esc_presses
                .saturating_add(recovery.open_chat_presses)
                .saturating_add(recovery.conversation_actions);
            report.last_snapshot = recovery.last_snapshot.clone();
            report.history = recovery.history.clone();
            report.recovery = Some(recovery);
            report
        }
        Err(failure_report) => {
            report.failure = Some(failure_report.failure);
            report.last_snapshot = failure_report.last_snapshot.as_ref().map(|s| s.describe());
            report.history = failure_report.history;
            report.last_frame = failure_report
                .frame
                .as_ref()
                .map(|image| frame_evidence_from_image(image, context));
            report
        }
    }
}

fn no_action_finish(policy: FinishPolicy, entry: Option<&EntryUiState>) -> FinishReport {
    let script_declared = matches!(policy, FinishPolicy::ScriptDeclared);
    let mut report = FinishReport {
        settled: !script_declared,
        script_declared,
        ..FinishReport::default()
    };
    report.left = matches!(policy, FinishPolicy::Leave);
    report.target = if script_declared {
        // 脚本自己收尾但未向包装器确认落点：报告“未知”，不冒充入口。
        None
    } else {
        match policy {
            FinishPolicy::Fixed(target) => Some(target),
            FinishPolicy::RestoreEntry { fallback } => Some(
                entry
                    .and_then(EntryUiState::restore_target)
                    .unwrap_or(fallback),
            ),
            FinishPolicy::Leave | FinishPolicy::KeepCurrent | FinishPolicy::ScriptDeclared => {
                entry.and_then(EntryUiState::restore_target)
            }
        }
    };
    report
}

fn finish_residency(report: &FinishReport) -> UiResidencyOutcome {
    let Some(target) = report.target.clone() else {
        if report.left {
            return UiResidencyOutcome::Failed(failure(
                InputCertainty::BeforeInput,
                "ui_task_leave",
                "任务声明 Leave：不恢复入口，也没有可确认的聊天驻留界面",
            ));
        }
        return UiResidencyOutcome::Failed(failure(
            InputCertainty::BeforeInput,
            "ui_task_finish",
            "收尾策略未声明目标界面",
        ));
    };
    if report.settled {
        return match &target {
            ResidencyTarget::Primary => UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
            ResidencyTarget::Secondary(conversation) if *conversation == ConversationRef::Unknown => {
                UiResidencyOutcome::Failed(failure(
                    InputCertainty::AfterInputUnknown,
                    "ui_task_finish",
                    "收尾只确认在二级，入口会话身份未知，不能声明为当前大厅",
                ))
            }
            ResidencyTarget::Secondary(_) => {
                UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)
            }
        };
    }
    let failure = report.failure.clone().unwrap_or_else(|| {
        failure(
            InputCertainty::AfterInputUnknown,
            "ui_task_finish",
            "收尾未确认界面驻留",
        )
    });
    UiResidencyOutcome::Failed(failure)
}

fn frame_evidence_from_image(
    image: &image::DynamicImage,
    context: &UiRoutineContext<'_>,
) -> FrameEvidence {
    let frame = context.last_captured_frame();
    FrameEvidence {
        frame_id: frame.as_ref().map(|frame| frame.id()),
        captured_at: frame.as_ref().map(|frame| frame.captured_at()),
        width: image.width(),
        height: image.height(),
        path: diagnostics_enabled()
            .then(|| write_diagnostics_image(image))
            .flatten(),
    }
}

fn write_diagnostics_image(image: &image::DynamicImage) -> Option<PathBuf> {
    let directory = diagnostics_dir()?;
    std::fs::create_dir_all(&directory).ok()?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    // 隐私：文件名只含时间，不含任何正文/UID/昵称。
    let path = directory.join(format!("ui-{millis}.png"));
    image.save(&path).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use image::DynamicImage;

    use super::*;
    use crate::config::AppConfig;
    use crate::runtime::ui::{
        UiDevice, UiRoutine, UiRoutineContext, UiRoutineFailure, UiStateClassification,
        UiStateClassifier, UiStateEvidence, UiStateKind, execute_routine_for_test,
        sealed::UiRoutineSealed,
    };
    use crate::ui::routines::friend_delivery::{
        FriendDeliveryRoutineConfig, UiResidencyTarget,
    };
    use crate::ui::routines::residency::{ConversationRef, ResidencyTarget, SettleEnvironment};

    const CANVAS_WIDTH: u32 = 1920;
    const CANVAS_HEIGHT: u32 = 1080;

    // ---- 测试替身 ----

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RecordedAction {
        Capture,
        Key(Key),
        Activate(u64),
    }

    #[derive(Default)]
    struct RecordingDevice {
        actions: Arc<Mutex<Vec<RecordedAction>>>,
    }

    impl RecordingDevice {
        fn keys(&self) -> Vec<Key> {
            self.actions
                .lock()
                .unwrap()
                .iter()
                .filter_map(|action| match action {
                    RecordedAction::Key(key) => Some(*key),
                    _ => None,
                })
                .collect()
        }

        fn activations(&self) -> Vec<u64> {
            self.actions
                .lock()
                .unwrap()
                .iter()
                .filter_map(|action| match action {
                    RecordedAction::Activate(wait_ms) => Some(*wait_ms),
                    _ => None,
                })
                .collect()
        }
    }

    impl UiDevice for RecordingDevice {
        fn capture(&mut self) -> anyhow::Result<DynamicImage> {
            self.actions.lock().unwrap().push(RecordedAction::Capture);
            Ok(DynamicImage::new_rgba8(CANVAS_WIDTH, CANVAS_HEIGHT))
        }

        fn activate(&mut self, wait_ms: u64) -> anyhow::Result<()> {
            self.actions
                .lock()
                .unwrap()
                .push(RecordedAction::Activate(wait_ms));
            Ok(())
        }

        fn ensure_ready(&mut self, _after_activate_ms: u64) -> anyhow::Result<()> {
            Ok(())
        }

        fn ensure_foreground(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        fn press_key(&mut self, key: Key) -> anyhow::Result<()> {
            self.actions.lock().unwrap().push(RecordedAction::Key(key));
            Ok(())
        }
    }

    struct ScriptedClassifier {
        states: Vec<UiStateClassification>,
        index: usize,
    }

    impl ScriptedClassifier {
        fn new(states: Vec<UiStateClassification>) -> Self {
            Self { states, index: 0 }
        }
    }

    impl UiStateClassifier for ScriptedClassifier {
        fn classify(&mut self, _image: &DynamicImage) -> anyhow::Result<UiStateClassification> {
            let index = self.index.min(self.states.len() - 1);
            self.index += 1;
            Ok(self.states[index].clone())
        }
    }

    fn state_with_rule(kind: UiStateKind, label: &str, rule: &str) -> UiStateClassification {
        UiStateClassification::with_evidence(
            kind,
            label,
            UiStateEvidence::new(Vec::new(), None, rule),
        )
    }

    fn primary() -> UiStateClassification {
        state_with_rule(UiStateKind::Primary, "primary", "primary")
    }

    fn secondary() -> UiStateClassification {
        state_with_rule(UiStateKind::Secondary, "secondary", "secondary")
    }

    fn panel() -> UiStateClassification {
        state_with_rule(UiStateKind::Panel, "panel", "panel")
    }

    fn loading() -> UiStateClassification {
        state_with_rule(UiStateKind::Loading, "loading", "loading")
    }

    fn unknown() -> UiStateClassification {
        state_with_rule(UiStateKind::Unknown, "unknown", "unknown")
    }

    fn main_menu() -> UiStateClassification {
        state_with_rule(UiStateKind::MainMenu, "main_menu", "main_menu_template")
    }

    fn paimon_menu() -> UiStateClassification {
        state_with_rule(
            UiStateKind::MainMenu,
            "paimon_menu",
            "paimon_menu_grid",
        )
    }

    fn environment_with(timeout_ms: u64) -> SettleEnvironment {
        let mut config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.workflow.default_timeout_ms = timeout_ms;
        config.timing.workflow.default_poll_ms = 20;
        SettleEnvironment::without_ocr(FriendDeliveryRoutineConfig::from_app(&config))
    }

    fn environment() -> SettleEnvironment {
        environment_with(2_000)
    }

    struct Noop;

    impl UiRoutineSealed for Noop {}

    impl UiRoutine for Noop {
        type Output = ();

        fn execute(self, _context: &mut UiRoutineContext<'_>) -> Self::Output {}
    }

    struct ForceFinish;

    impl UiRoutineSealed for ForceFinish {}

    impl UiRoutine for ForceFinish {
        type Output = ();

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            context.force_finish();
        }
    }

    struct Panics;

    impl UiRoutineSealed for Panics {}

    impl UiRoutine for Panics {
        type Output = ();

        fn execute(self, _context: &mut UiRoutineContext<'_>) -> Self::Output {
            panic!("inner routine exploded");
        }
    }

    struct PressThenReturn;

    impl UiRoutineSealed for PressThenReturn {}

    impl UiRoutine for PressThenReturn {
        type Output = ();

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            context.device().press_key(Key::Escape).unwrap();
        }
    }

    struct CancelAfterStart(Arc<AtomicBool>);

    impl UiRoutineSealed for CancelAfterStart {}

    impl UiRoutine for CancelAfterStart {
        type Output = ();

        fn execute(self, _context: &mut UiRoutineContext<'_>) -> Self::Output {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn run_task_with<R: UiRoutine>(
        script: Vec<UiStateClassification>,
        spec: UiTaskSpec,
        inner: R,
        cancelled: Arc<AtomicBool>,
    ) -> (UiTaskOutcome<R::Output>, RecordingDevice) {
        let execution = execute_routine_for_test(
            RecordingDevice::default(),
            Some(Box::new(ScriptedClassifier::new(script))),
            2,
            cancelled,
            UiTask::new(spec, inner),
        );
        (execution.output, execution.device)
    }

    fn run(script: Vec<UiStateClassification>, spec: UiTaskSpec) -> (UiTaskOutcome<()>, RecordingDevice) {
        run_task_with(script, spec, Noop, Arc::new(AtomicBool::new(false)))
    }

    fn restore_primary_spec() -> UiTaskSpec {
        UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::RestoreEntry {
                fallback: ResidencyTarget::Primary,
            },
            environment(),
        )
    }

    // ---- 入口 ----

    #[test]
    fn entry_is_observed_once_before_any_input() {
        let script = vec![primary(), primary(), primary(), primary()];
        let (outcome, device) = run(script, restore_primary_spec());
        assert!(device.keys().is_empty());
        let entry = outcome.entry.as_ref().expect("entry observed");
        assert_eq!(entry.kind, UiStateKind::Primary);
        assert!(entry.frame_id.is_some());
        assert!(outcome.finish.settled);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn entry_failure_before_input_is_safe_to_retry() {
        struct Failing;
        impl UiStateClassifier for Failing {
            fn classify(&mut self, _image: &DynamicImage) -> anyhow::Result<UiStateClassification> {
                anyhow::bail!("classifier unavailable");
            }
        }
        let execution = execute_routine_for_test(
            RecordingDevice::default(),
            Some(Box::new(Failing)),
            2,
            Arc::new(AtomicBool::new(false)),
            UiTask::new(restore_primary_spec(), Noop),
        );
        let outcome = execution.output;
        assert!(outcome.entry_failure.is_some());
        assert_eq!(outcome.input_epoch, 0);
        assert!(outcome.safe_retry());
        assert!(execution.device.keys().is_empty());
    }

    #[test]
    fn require_target_rejects_secondary_without_pressing_keys() {
        let script = vec![secondary(); 6];
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::RestoreEntry {
                fallback: ResidencyTarget::Primary,
            },
            environment(),
        )
        .with_entry(EntryPolicy::RequireTarget);
        let (outcome, device) = run(script, spec);
        assert!(device.keys().is_empty(), "入口不满足时不允许任何按键");
        assert!(outcome.output.is_none());
        let failure = outcome.entry_failure.as_ref().expect("entry failure");
        assert_eq!(failure.certainty(), InputCertainty::BeforeInput);
        assert!(outcome.safe_retry());
    }

    #[test]
    fn require_target_secondary_conversation_is_rejected_without_input() {
        let script = vec![secondary(); 6];
        let spec = UiTaskSpec::new(
            ResidencyTarget::Secondary(ConversationRef::CurrentHall),
            FinishPolicy::Fixed(ResidencyTarget::Secondary(ConversationRef::CurrentHall)),
            environment(),
        )
        .with_entry(EntryPolicy::RequireTarget);
        let (outcome, device) = run(script, spec);
        assert!(device.keys().is_empty());
        let failure = outcome.entry_failure.as_ref().expect("entry failure");
        assert_eq!(failure.certainty(), InputCertainty::BeforeInput);
    }

    // ---- 恢复/收尾 ----

    #[test]
    fn primary_to_primary_has_no_actions() {
        let script = vec![primary(); 4];
        let (outcome, device) = run(script, restore_primary_spec());
        assert!(device.keys().is_empty());
        assert!(outcome.finish.attempted);
    }

    #[test]
    fn secondary_to_primary_uses_exactly_one_escape() {
        // 入口二级 -> 入口会话识别会额外消耗一帧 -> 导航 Esc 一次 -> 收尾已在 Primary。
        let mut script = vec![secondary(); 5];
        script.extend(vec![primary(); 10]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            environment(),
        );
        let (outcome, device) = run(script, spec);
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn main_menu_escape_restores_primary() {
        let mut script = vec![main_menu(); 2];
        script.extend(vec![main_menu(); 2]);
        script.extend(vec![primary(); 8]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::RestoreEntry {
                fallback: ResidencyTarget::Primary,
            },
            environment(),
        );
        let (outcome, device) = run(script, spec);
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn paimon_menu_is_rejected_without_pressing_keys() {
        let script = vec![paimon_menu(); 10];
        let (outcome, device) = run(script, restore_primary_spec());
        assert!(device.keys().is_empty(), "主菜单大世界不得当作千星菜单按 Esc");
        assert!(outcome.entry_failure.is_some() || outcome.finish.failure.is_some());
    }

    #[test]
    fn loading_never_sends_input() {
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::RestoreEntry {
                fallback: ResidencyTarget::Primary,
            },
            environment_with(200),
        );
        let script = vec![loading(); 40];
        let (outcome, device) = run(script, spec);
        assert!(device.keys().is_empty());
        assert!(outcome.finish.failure.is_some() || outcome.entry_failure.is_some());
    }

    #[test]
    fn transient_unknown_is_waited_out_without_pressing_keys() {
        let mut script = vec![unknown(), unknown()];
        script.extend(vec![primary(); 8]);
        let (outcome, device) = run(script, restore_primary_spec());
        assert!(device.keys().is_empty());
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn unknown_probe_is_bounded_to_one_escape() {
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            environment_with(2_000),
        )
        .with_entry(EntryPolicy::Passthrough);
        let script = vec![unknown(); 100];
        let (_, device) = run(script, spec);
        assert!(device.keys().len() <= 1, "Unknown 的受控 Esc 探针有界");
        assert!(!device.keys().is_empty(), "持续 Unknown 应使用一次受控探针");
    }

    #[test]
    fn panel_stack_uses_bounded_escape() {
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            environment_with(1_500),
        )
        .with_entry(EntryPolicy::Passthrough);
        let script = vec![panel(); 100];
        let (outcome, device) = run(script, spec);
        assert!(!device.keys().is_empty());
        assert!(device.keys().len() <= 3, "Esc 必须是有界的");
        assert!(!outcome.finish.settled);
    }

    #[test]
    fn panel_closes_then_returns_primary() {
        let mut script = vec![panel(); 4];
        script.extend(vec![primary(); 8]);
        let (outcome, device) = run(script, restore_primary_spec());
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    // ---- task-103：二级界面/覆盖层 Esc 落点 ----

    fn secondary_chat_label() -> UiStateClassification {
        state_with_rule(UiStateKind::Secondary, "secondary:chat", "secondary_chat_anchor")
    }

    fn secondary_multiplayer_panel() -> UiStateClassification {
        state_with_rule(
            UiStateKind::Secondary,
            "secondary:qianxing_panel",
            "secondary_multimedia_panel_template",
        )
    }

    fn secondary_hall_map() -> UiStateClassification {
        state_with_rule(
            UiStateKind::Secondary,
            "secondary:qianxing_map",
            "secondary_hall_map_template",
        )
    }

    fn overlay(label: &'static str) -> UiStateClassification {
        state_with_rule(UiStateKind::Panel, label, label)
    }

    #[test]
    fn friend_conversation_escape_lands_on_primary_hall() {
        // 用户确认：好友私聊 Esc -> 一级大厅（不是当前大厅）。
        let mut script = vec![secondary_chat_label(); 2];
        script.extend(vec![primary(); 10]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            environment(),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run(script, spec);
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn multiplayer_panel_escape_lands_on_primary_hall() {
        let mut script = vec![secondary_multiplayer_panel(); 2];
        script.extend(vec![primary(); 10]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            environment(),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run(script, spec);
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn hall_map_escape_lands_on_primary_hall() {
        let mut script = vec![secondary_hall_map(); 2];
        script.extend(vec![primary(); 10]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            environment(),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run(script, spec);
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn overlay_on_primary_escapes_one_level_to_primary() {
        let mut script = vec![overlay("panel:friend_list"); 2];
        script.extend(vec![primary(); 10]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            environment(),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run(script, spec);
        assert_eq!(device.keys(), vec![Key::Escape], "覆盖层只退一级");
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn overlay_on_secondary_escapes_one_level_to_secondary() {
        let mut script = vec![overlay("panel:avatar_popup"); 2];
        script.extend(vec![secondary_chat_label(); 8]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Secondary(ConversationRef::AnySecondary),
            FinishPolicy::Fixed(ResidencyTarget::Secondary(ConversationRef::AnySecondary)),
            environment(),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run(script, spec);
        assert_eq!(device.keys(), vec![Key::Escape], "叠在二级上的覆盖层退一级回二级");
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)
        ));
    }

    #[test]
    fn overlay_is_not_a_restorable_target() {
        let script = vec![overlay("panel:friend_list"); 8];
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Secondary(ConversationRef::Overlay(
                crate::ui::routines::state_observation::OverlaySurface::FriendPanel,
            ))),
            environment(),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run(script, spec);
        assert!(device.keys().is_empty(), "覆盖层不可重建，不应按键");
        assert!(matches!(outcome.residency(), UiResidencyOutcome::Failed(_)));
    }

    #[test]
    fn entry_multiplayer_panel_falls_back_and_marks_degraded() {
        // 入口是二级界面·多人面板：本身不可重建，显式 fallback 并标记降级。
        let mut script = vec![secondary_multiplayer_panel(); 4];
        script.extend(vec![primary(); 10]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::RestoreEntry {
                fallback: ResidencyTarget::Primary,
            },
            environment(),
        );
        let (outcome, device) = run(script, spec);
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(outcome.finish.fallback_used, "不可重建入口必须走 fallback");
        assert!(
            outcome
                .finish
                .recovery
                .as_ref()
                .is_some_and(|recovery| recovery.degraded),
            "回不到入口子界面时必须标记 degraded"
        );
    }

    // ---- 策略 ----

    #[test]
    fn keep_current_finish_has_no_actions_but_reports_actual_residency() {
        let mut script = vec![primary(), primary()];
        script.extend(vec![secondary(); 6]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::KeepCurrent,
            environment(),
        );
        let (outcome, device) = run(script, spec);
        assert!(device.keys().is_empty());
        assert!(outcome.finish.attempted == false);
        assert!(
            matches!(outcome.residency(), UiResidencyOutcome::Failed(_)),
            "KeepCurrent 只观测到二级但无会话身份时，不能冒充当前大厅：{:?}",
            outcome.residency()
        );
    }

    #[test]
    fn leave_does_not_navigate_and_reports_observed_interface() {
        let script = vec![primary(); 4];
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Leave,
            environment(),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run(script, spec);
        assert!(device.keys().is_empty());
        assert!(outcome.finish.left);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn leave_from_overworld_is_reported_as_non_residency() {
        struct OverwolrdClassifier;
        impl UiStateClassifier for OverwolrdClassifier {
            fn classify(&mut self, _image: &DynamicImage) -> anyhow::Result<UiStateClassification> {
                Ok(state_with_rule(
                    UiStateKind::Overworld,
                    "overworld",
                    "overworld",
                ))
            }
        }
        let spec = UiTaskSpec::new(ResidencyTarget::Primary, FinishPolicy::Leave, environment())
            .with_entry(EntryPolicy::Passthrough);
        let execution = execute_routine_for_test(
            RecordingDevice::default(),
            Some(Box::new(OverwolrdClassifier)),
            2,
            Arc::new(AtomicBool::new(false)),
            UiTask::new(spec, Noop),
        );
        assert!(execution.device.keys().is_empty());
        assert!(execution.output.finish.left);
        assert!(execution.output.finish.failure.is_some());
    }

    #[test]
    fn passthrough_still_settles_a_fixed_finish_target() {
        let mut script = vec![secondary(), secondary()];
        script.extend(vec![primary(); 8]);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            environment(),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run(script, spec);
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(outcome.entry.is_none());
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn script_declared_finish_does_not_observe_or_press() {
        let script = vec![secondary(); 6];
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::ScriptDeclared,
            environment(),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run(script, spec);
        assert!(device.keys().is_empty());
        assert!(outcome.finish.script_declared);
        assert!(
            !outcome.finish.settled,
            "脚本没有向包装器确认落点，不能伪造 settled"
        );
        assert!(outcome.finish.target.is_none(), "包装器不得替脚本声明目标");
        assert!(
            matches!(outcome.residency(), UiResidencyOutcome::Failed(_)),
            "未确认收尾不能对外报 Confirmed(入口): {:?}",
            outcome.residency()
        );
    }

    #[test]
    fn assumed_secondary_conversation_is_carried_into_entry_state() {
        let script = vec![secondary(); 8];
        let spec = UiTaskSpec::new(
            ResidencyTarget::Secondary(ConversationRef::CurrentHall),
            FinishPolicy::KeepCurrent,
            environment(),
        )
        .with_entry(EntryPolicy::AssumeTarget);
        let (outcome, device) = run(script, spec);
        assert!(device.keys().is_empty());
        let entry = outcome.entry.as_ref().expect("assumed entry");
        assert_eq!(
            entry.conversation,
            Some(ConversationRef::CurrentHall),
            "调用方声明的会话必须进入 EntryUiState"
        );
        assert!(
            matches!(
                outcome.residency(),
                UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)
            ),
            "residency={:?} finish={:?}",
            outcome.residency(),
            outcome.finish
        );
    }

    #[test]
    fn activation_policy_passes_the_configured_wait() {
        let script = vec![primary(); 4];
        let spec = restore_primary_spec()
            .with_activation(ActivationPolicy::Activate { wait_ms: 250 });
        let (outcome, device) = run(script, spec);
        assert_eq!(device.activations(), vec![250]);
        assert!(outcome.finish.settled);
    }

    // ---- 异常/取消 ----

    #[test]
    fn inner_panic_is_reported_and_still_runs_finish() {
        let script = vec![primary(); 8];
        let (outcome, device) = run_task_with(
            script,
            restore_primary_spec(),
            Panics,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(device.keys().is_empty());
        assert!(outcome.output.is_none());
        let failure = outcome.output_failure.as_ref().expect("panic failure");
        assert_eq!(failure.certainty(), InputCertainty::Panicked);
        assert!(outcome.finish.attempted);
        assert!(outcome.finish.settled);
        let frame = outcome
            .finish
            .last_frame
            .as_ref()
            .expect("失败必须带最近帧元数据");
        assert!(frame.width > 0 && frame.height > 0);
        assert!(frame.path.is_none(), "默认不开诊断，不允许落盘");
    }

    #[test]
    /// 强制完成：例程已确认到达目标界面，收尾导航必须整段跳过。
    #[test]
    fn forced_finish_skips_finish_navigation() {
        let (outcome, device) = run_task_with(
            vec![primary()],
            restore_primary_spec(),
            ForceFinish,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(outcome.finish.settled);
        assert!(!outcome.finish.attempted, "强制完成不应进入收尾阶段");
        assert_eq!(outcome.finish.actions, 0);
        assert!(matches!(
            outcome.residency,
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    fn finish_still_runs_after_business_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let script = vec![primary(); 8];
        let (outcome, device) = run_task_with(
            script,
            restore_primary_spec(),
            CancelAfterStart(cancelled.clone()),
            cancelled,
        );
        assert!(device.keys().is_empty());
        assert!(outcome.finish.attempted);
        assert!(outcome.finish.settled);
    }

    #[test]
    fn finish_timeout_is_reported_without_treating_failure_as_recovered() {
        let spec = UiTaskSpec::new(
            ResidencyTarget::Primary,
            FinishPolicy::Fixed(ResidencyTarget::Primary),
            environment_with(200),
        )
        .with_entry(EntryPolicy::Passthrough);
        let mut script = vec![loading(); 40];
        script.extend(vec![loading(); 20]);
        let (outcome, device) = run(script, spec);
        assert!(device.keys().is_empty());
        assert!(outcome.finish.attempted);
        assert!(!outcome.finish.settled);
        assert!(outcome.finish.failure.is_some());
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Failed(_)
        ));
    }

    #[test]
    fn partial_input_disables_safe_retry() {
        let script = vec![primary(); 8];
        let (outcome, _device) = run_task_with(
            script,
            restore_primary_spec(),
            PressThenReturn,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(outcome.input_epoch, 1);
        assert!(!outcome.safe_retry());
    }
}
