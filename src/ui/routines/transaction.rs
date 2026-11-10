//! 统一 UI 任务事务包装器（P1 核心）。
//!
//! 设计见 `technical-docs/ui-task-return-design.md`：
//! 例程只声明“要去的目标 + 收尾策略”，包装器负责
//! 入口观测一次（兼作目标检查）→ 必要时导航 → 执行内层例程 →
//! **finally** 统一走 `settle_to` 回到入口/指定界面。
//!
//! 本文件只提供核心状态机与基础动作表；hall / friend_delivery / invite /
//! moderation / startup / custom_action 等例程的迁移在后续任务里做，本文件不替换它们。
//!
//! 两条硬约束：
//! - 收尾必须走显式 `finally`，不能用 `Drop`（worker panic 时上下文/设备不可用）。
//! - 入口观测必须在**任何输入之前**；目标检查复用入口观测，不重复截图。

use std::time::{Duration, Instant};

use enigo::Key;

use super::friend_delivery::{UiResidencyOutcome, UiResidencyTarget};
use crate::runtime::ui::{
    InputCertainty, UiRoutine, UiRoutineContext, UiRoutineFailure, UiStateKind, UiStateObservation,
};

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinishPolicy {
    /// 回到入口界面；入口不可重建时退到 `fallback`。
    RestoreEntry { fallback: UiResidencyTarget },
    /// 固定在某个驻留目标（值守基线类任务）。
    Fixed(UiResidencyTarget),
    /// 保持收尾时的界面，不做任何导航（调试/只读）。
    KeepCurrent,
    /// 由内层脚本自己声明/完成收尾，包装器不再动作。
    ScriptDeclared,
}

/// 一次 UI 任务的事务声明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UiTaskSpec {
    pub(crate) target: UiResidencyTarget,
    pub(crate) entry: EntryPolicy,
    pub(crate) finish: FinishPolicy,
    pub(crate) activate: bool,
    pub(crate) total_budget: Duration,
}

impl UiTaskSpec {
    /// 默认总预算：足够一次导航 + 内层例程 + 一次收尾。
    pub(crate) const DEFAULT_TOTAL_BUDGET: Duration = Duration::from_secs(8);

    pub(crate) fn new(target: UiResidencyTarget, finish: FinishPolicy) -> Self {
        Self {
            target,
            entry: EntryPolicy::Observe,
            finish,
            activate: false,
            total_budget: Self::DEFAULT_TOTAL_BUDGET,
        }
    }

    pub(crate) fn with_entry(mut self, entry: EntryPolicy) -> Self {
        self.entry = entry;
        self
    }

    pub(crate) fn with_activate(mut self, activate: bool) -> Self {
        self.activate = activate;
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
    pub(crate) residency: Option<UiResidencyTarget>,
    pub(crate) label: String,
    /// 分类证据里的规则名；用于区分千星主菜单与派蒙菜单（两者 kind 都是 MainMenu）。
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
    ) -> Self {
        Self {
            kind,
            residency: residency_for(kind),
            label,
            final_rule,
            frame_id,
            observed_at,
        }
    }

    fn assumed(target: UiResidencyTarget) -> Self {
        Self {
            kind: kind_for(target),
            residency: Some(target),
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

    /// 入口是否允许被导航（Unknown/Loading/启动链界面不允许盲动作）。
    pub(crate) fn may_navigate(&self) -> bool {
        !matches!(
            self.kind,
            UiStateKind::Unknown
                | UiStateKind::Loading
                | UiStateKind::GameGate
                | UiStateKind::Overworld
        )
    }
}

/// 收尾结果。失败不抛异常，由调用方决定要不要重试（见 `UiTaskOutcome::safe_retry`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FinishReport {
    /// 是否进入了收尾阶段（KeepCurrent / ScriptDeclared / 无收尾义务时为 false）。
    pub(crate) attempted: bool,
    /// 是否确认落到目标界面。
    pub(crate) settled: bool,
    pub(crate) target: Option<UiResidencyTarget>,
    /// 收尾实际发出的输入动作数（Esc/Return 等）。
    pub(crate) actions: u32,
    /// 入口不可重建而退到 `fallback`。
    pub(crate) fallback_used: bool,
    pub(crate) failure: Option<UiRoutineFailure>,
}

/// UI 任务统一返回值。
#[derive(Clone, Debug)]
pub(crate) struct UiTaskOutcome<T> {
    /// 入口快照；执行前取消 / 入口观测失败时为 None。
    pub(crate) entry: Option<EntryUiState>,
    /// 内层例程的输出；只要内层开始执行就有 `Some`（即使它返回 Err）。
    pub(crate) output: Option<T>,
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
            return outcome;
        }

        if self.spec.activate
            && let Err(error) = context.device().activate(0)
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
                outcome.output = Some(self.inner.execute(context));
                outcome.finish = match self.spec.finish {
                    FinishPolicy::KeepCurrent | FinishPolicy::ScriptDeclared => {
                        no_action_finish(self.spec.finish, outcome.entry.as_ref())
                    }
                    FinishPolicy::Fixed(_) | FinishPolicy::RestoreEntry { .. } => run_finish(
                        context,
                        self.spec.finish,
                        outcome.entry.as_ref(),
                        deadline,
                    ),
                };
                outcome.residency = finish_residency(&outcome.finish);
                return finish_outcome(outcome, context, started_epoch);
            }
            EntryPolicy::AssumeTarget => {
                outcome.entry = Some(EntryUiState::assumed(self.spec.target));
            }
            EntryPolicy::Observe | EntryPolicy::RequireTarget => {
                let entry_budget = entry_budget(deadline);
                match observe_entry(context, entry_budget) {
                    Ok(entry) => {
                        let matches_target = kind_matches(entry.kind, self.spec.target);
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
                                    target_label(self.spec.target),
                                    entry.label
                                ),
                            ));
                            outcome.residency = UiResidencyOutcome::Failed(
                                outcome
                                    .entry_failure
                                    .clone()
                                    .expect("entry failure was just set"),
                            );
                            return outcome;
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
                        return outcome;
                    }
                }
            }
        }

        // 目标检查复用入口观测：已在目标就不导航。
        if navigate_to_target && outcome.entry.is_some() {
            let remaining = remaining_until(deadline);
            if let Err(error) = settle_to(context, self.spec.target, remaining) {
                outcome.entry_failure = Some(error);
                // 导航阶段可能已经发过输入（例如按了 Esc 才发现回不去），
                // 不能当成“执行前失败”直接返回：仍要进入 finally 收尾。
            }
        }

        if outcome.entry_failure.is_none() {
            outcome.output = Some(self.inner.execute(context));
        }

        let input_epochs = context.input_epoch().saturating_sub(started_epoch);
        let finish_obligation = input_epochs > 0 || outcome.output.is_some();
        if finish_obligation {
            outcome.finish = run_finish(
                context,
                self.spec.finish,
                outcome.entry.as_ref(),
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
    outcome
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

fn kind_matches(kind: UiStateKind, target: UiResidencyTarget) -> bool {
    match target {
        UiResidencyTarget::Primary => kind == UiStateKind::Primary,
        UiResidencyTarget::SecondaryCurrentHall => kind == UiStateKind::Secondary,
    }
}

fn residency_for(kind: UiStateKind) -> Option<UiResidencyTarget> {
    match kind {
        UiStateKind::Primary => Some(UiResidencyTarget::Primary),
        UiStateKind::Secondary => Some(UiResidencyTarget::SecondaryCurrentHall),
        _ => None,
    }
}

fn kind_for(target: UiResidencyTarget) -> UiStateKind {
    match target {
        UiResidencyTarget::Primary => UiStateKind::Primary,
        UiResidencyTarget::SecondaryCurrentHall => UiStateKind::Secondary,
    }
}

fn target_label(target: UiResidencyTarget) -> &'static str {
    match target {
        UiResidencyTarget::Primary => "一级界面",
        UiResidencyTarget::SecondaryCurrentHall => "二级当前大厅",
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
) -> Result<EntryUiState, UiRoutineFailure> {
    let deadline = Instant::now() + budget;
    loop {
        let observed = observe_current(context, "ui_task_observe_entry")?;
        if observed.stable {
            return Ok(EntryUiState::from_classification(
                observed.kind,
                observed.label,
                observed.final_rule,
                observed.frame_id,
                observed.observed_at,
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

/// settle_to 的单步动作。
enum SettleStep {
    Press(Key),
    Wait,
}

/// 统一恢复状态机：把当前界面恢复到 `target`。
///
/// 基础动作表（P1）：
/// - `Primary` ← Secondary 一次 Esc；← Primary 直接成功；
/// - `SecondaryCurrentHall` ← Primary 一次 Return；← Secondary 视为已在当前大厅
///   （会话级确认留给后续 P0/迁移任务）；
/// - `MainMenu`：千星主菜单 Esc；**派蒙菜单拒绝**（不是大厅菜单）；
/// - `Panel` 一次 Esc；`Loading` 只等待；`Unknown` 不盲按 Esc，只等待自愈；
/// - `GameGate`/`Overworld` 拒绝恢复，交给启动链。
///
/// 每次动作后都重新观测；同一 `(kind,label)` 连续两次没有变化就熔断失败。
fn settle_to(
    context: &mut UiRoutineContext<'_>,
    target: UiResidencyTarget,
    budget: Duration,
) -> Result<UiResidencyTarget, UiRoutineFailure> {
    let deadline = Instant::now() + budget;
    let mut actions = 0u32;
    let mut last_signature: Option<(UiStateKind, String)> = None;
    let mut no_progress = 0u32;
    loop {
        let observed = observe_current(context, "ui_task_settle")?;
        if kind_matches(observed.kind, target) {
            return Ok(target);
        }
        if !observed.stable {
            // 过渡态不动作：上一稳定状态可能就是目标，等它自愈（事故回放路径）。
            if Instant::now() >= deadline {
                return Err(failure(
                    InputCertainty::BeforeInput,
                    "ui_task_settle",
                    format!("界面状态持续不稳定，未执行导航（当前：{}）", observed.label),
                ));
            }
            context.wait_cancellable(SETTLE_POLL)?;
            continue;
        }
        match next_settle_step(&observed, target)? {
            SettleStep::Wait => {
                if Instant::now() >= deadline {
                    let certainty = if actions == 0 {
                        InputCertainty::BeforeInput
                    } else {
                        InputCertainty::AfterInputUnknown
                    };
                    return Err(failure(
                        certainty,
                        "ui_task_settle",
                        format!("等待界面自愈超时（当前：{}）", observed.label),
                    ));
                }
                context.wait_cancellable(SETTLE_POLL)?;
            }
            SettleStep::Press(key) => {
                if actions >= MAX_SETTLE_ACTIONS {
                    return Err(failure(
                        InputCertainty::AfterInputUnknown,
                        "ui_task_settle",
                        "收尾动作次数达到上限，界面仍未恢复",
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(failure(
                        InputCertainty::AfterInputUnknown,
                        "ui_task_settle",
                        "收尾预算耗尽，界面仍未恢复",
                    ));
                }
                // 动手前先判“无进展”：同一 (kind,label) 连续第三次就不再按键，
                // 保证 Unknown/Panel 振荡时 Esc 次数有界（最多 2 次同一状态）。
                let signature = (observed.kind, observed.label.clone());
                if last_signature.as_ref() == Some(&signature) {
                    no_progress += 1;
                    if no_progress >= 2 {
                        return Err(failure(
                            InputCertainty::AfterInputUnknown,
                            "ui_task_settle",
                            format!("界面无进展（{}），停止继续按键", observed.label),
                        ));
                    }
                } else {
                    last_signature = Some(signature);
                    no_progress = 0;
                }
                context.device().press_key(key).map_err(|error| {
                    failure(
                        InputCertainty::AfterInputUnknown,
                        "ui_task_settle",
                        format!("收尾按键失败: {error:#}"),
                    )
                })?;
                actions += 1;
                context.wait_cancellable(SETTLE_POLL)?;
            }
        }
    }
}

fn next_settle_step(
    observed: &ObservedState,
    target: UiResidencyTarget,
) -> Result<SettleStep, UiRoutineFailure> {
    use UiStateKind::{
        GameGate, Loading, MainMenu, Overworld, Panel, Primary, Secondary, Unknown,
    };
    match observed.kind {
        Primary => match target {
            UiResidencyTarget::Primary => Ok(SettleStep::Wait),
            UiResidencyTarget::SecondaryCurrentHall => Ok(SettleStep::Press(Key::Return)),
        },
        Secondary => match target {
            UiResidencyTarget::Primary => Ok(SettleStep::Press(Key::Escape)),
            UiResidencyTarget::SecondaryCurrentHall => Ok(SettleStep::Wait),
        },
        MainMenu => {
            // 千星主菜单与派蒙菜单 kind 相同，只能靠证据规则区分。
            if observed.final_rule == PAIMON_MENU_RULE {
                Err(failure(
                    InputCertainty::BeforeInput,
                    "ui_task_settle",
                    "派蒙菜单不是千星大厅菜单，拒绝按 Esc 当作大厅菜单恢复",
                ))
            } else {
                Ok(SettleStep::Press(Key::Escape))
            }
        }
        Panel => Ok(SettleStep::Press(Key::Escape)),
        Loading => Ok(SettleStep::Wait),
        // Unknown 不盲按 Esc：等它自愈（持续 Unknown 由超时报失败）。
        Unknown => Ok(SettleStep::Wait),
        GameGate | Overworld => Err(failure(
            InputCertainty::BeforeInput,
            "ui_task_settle",
            format!("启动链界面（{}）拒绝自动恢复", observed.label),
        )),
    }
}

fn run_finish(
    context: &mut UiRoutineContext<'_>,
    policy: FinishPolicy,
    entry: Option<&EntryUiState>,
    deadline: Instant,
) -> FinishReport {
    let mut report = FinishReport::default();
    let target = match policy {
        // 保持当前界面：不做导航，但要观测一次记录“收尾后实际在哪”，
        // 否则会误报成入口驻留（例如入口一级、任务去了二级、KeepCurrent 后仍报一级）。
        FinishPolicy::KeepCurrent => {
            match observe_current(context, "ui_task_finish") {
                Ok(observed) => {
                    report.target = residency_for(observed.kind);
                    report.settled = report.target.is_some();
                    if report.settled {
                        return report;
                    }
                    report.failure = Some(failure(
                        InputCertainty::BeforeInput,
                        "ui_task_finish",
                        format!("保持当前界面，但当前界面不可作为驻留结论（{}）", observed.label),
                    ));
                }
                Err(error) => report.failure = Some(error),
            }
            return report;
        }
        // 脚本自己声明完成收尾：不再观测、不再动作，沿用入口驻留结论。
        FinishPolicy::ScriptDeclared => {
            report.settled = true;
            report.target = entry.and_then(|entry| entry.residency);
            return report;
        }
        FinishPolicy::Fixed(target) => target,
        FinishPolicy::RestoreEntry { fallback } => match entry {
            Some(entry) if entry.restorable() => {
                entry.residency.expect("restorable entry has residency")
            }
            _ => {
                report.fallback_used = true;
                fallback
            }
        },
    };
    report.attempted = true;
    report.target = Some(target);
    if !context.in_finish_phase() {
        context.begin_finish_phase(remaining_until(deadline));
    }
    match settle_to(context, target, remaining_until(deadline)) {
        Ok(settled) => {
            report.settled = true;
            report.target = Some(settled);
        }
        Err(error) => {
            report.failure = Some(error);
        }
    }
    report
}

fn no_action_finish(policy: FinishPolicy, entry: Option<&EntryUiState>) -> FinishReport {
    let mut report = FinishReport {
        settled: true,
        ..FinishReport::default()
    };
    report.target = match policy {
        FinishPolicy::Fixed(target) => Some(target),
        FinishPolicy::RestoreEntry { fallback } => {
            Some(entry.and_then(|entry| entry.residency).unwrap_or(fallback))
        }
        FinishPolicy::KeepCurrent | FinishPolicy::ScriptDeclared => {
            entry.and_then(|entry| entry.residency)
        }
    };
    report
}

fn no_action_residency(report: &FinishReport) -> UiResidencyOutcome {
    match report.target {
        Some(target) => UiResidencyOutcome::Confirmed(target),
        None => UiResidencyOutcome::Failed(failure(
            InputCertainty::BeforeInput,
            "ui_task_finish",
            "旁路模式没有入口观测，未验证驻留",
        )),
    }
}

fn finish_residency(report: &FinishReport) -> UiResidencyOutcome {
    if report.settled {
        return match report.target {
            Some(target) => UiResidencyOutcome::Confirmed(target),
            None => UiResidencyOutcome::Failed(failure(
                InputCertainty::BeforeInput,
                "ui_task_finish",
                "收尾策略未声明目标界面",
            )),
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

/// 收尾等待粒度。与例程里的稳定观察轮询保持一致（100ms）。
const SETTLE_POLL: Duration = Duration::from_millis(100);
/// 入口稳定观察轮询。
const ENTRY_POLL: Duration = Duration::from_millis(100);
/// 单次收尾最多按键次数，防 Unknown↔MainMenu 振荡。
const MAX_SETTLE_ACTIONS: u32 = 6;
/// 派蒙菜单的证据规则名（`ui/state.rs` 的 `paimon_menu()`）。
const PAIMON_MENU_RULE: &str = "paimon_menu_panel_template";

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use image::DynamicImage;

    use super::*;
    use crate::runtime::ui::{
        UiDevice, UiRoutine, UiRoutineContext, UiRoutineFailure, UiStateClassification,
        UiStateClassifier, UiStateEvidence, UiStateKind, execute_routine_for_test,
        sealed::UiRoutineSealed,
    };

    // ---- 测试替身 ----

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RecordedAction {
        Capture,
        Key(Key),
        Activate,
    }

    #[derive(Default)]
    struct RecordingDevice {
        actions: Arc<Mutex<Vec<RecordedAction>>>,
    }

    impl RecordingDevice {
        fn actions(&self) -> Vec<RecordedAction> {
            self.actions.lock().unwrap().clone()
        }

        fn captures(&self) -> usize {
            self.actions()
                .iter()
                .filter(|action| matches!(action, RecordedAction::Capture))
                .count()
        }

        fn keys(&self) -> Vec<Key> {
            self.actions()
                .iter()
                .filter_map(|action| match action {
                    RecordedAction::Key(key) => Some(*key),
                    _ => None,
                })
                .collect()
        }
    }

    impl UiDevice for RecordingDevice {
        fn capture(&mut self) -> anyhow::Result<DynamicImage> {
            self.actions.lock().unwrap().push(RecordedAction::Capture);
            Ok(DynamicImage::new_rgba8(16, 16))
        }

        fn press_key(&mut self, key: Key) -> anyhow::Result<()> {
            self.actions.lock().unwrap().push(RecordedAction::Key(key));
            Ok(())
        }

        fn activate(&mut self, _after_activate_ms: u64) -> anyhow::Result<()> {
            self.actions.lock().unwrap().push(RecordedAction::Activate);
            Ok(())
        }
    }

    /// 按脚本逐次返回分类结果；脚本耗尽后重复最后一个。
    struct ScriptedClassifier {
        states: Vec<UiStateClassification>,
        index: usize,
    }

    impl ScriptedClassifier {
        fn new(states: Vec<UiStateClassification>) -> Self {
            assert!(!states.is_empty(), "classifier script must not be empty");
            Self { states, index: 0 }
        }
    }

    impl UiStateClassifier for ScriptedClassifier {
        fn classify(
            &mut self,
            _image: &DynamicImage,
        ) -> anyhow::Result<UiStateClassification> {
            let index = self.index.min(self.states.len() - 1);
            self.index += 1;
            Ok(self.states[index].clone())
        }
    }

    fn state(kind: UiStateKind, label: &str, final_rule: &str) -> UiStateClassification {
        UiStateClassification::with_evidence(
            kind,
            label,
            UiStateEvidence::new(Vec::new(), None, final_rule),
        )
    }

    fn primary() -> UiStateClassification {
        state(UiStateKind::Primary, "一级界面", "primary")
    }

    fn secondary() -> UiStateClassification {
        state(UiStateKind::Secondary, "二级聊天", "secondary_back_template")
    }

    fn main_menu() -> UiStateClassification {
        state(UiStateKind::MainMenu, "主菜单", "main_menu_template")
    }

    fn paimon_menu() -> UiStateClassification {
        state(UiStateKind::MainMenu, "派蒙菜单", PAIMON_MENU_RULE)
    }

    fn uncertain(count: usize) -> Vec<UiStateClassification> {
        vec![state(UiStateKind::Unknown, "无法识别的界面", "unknown"); count]
    }

    // ---- 内层例程 ----

    struct Noop;

    impl UiRoutineSealed for Noop {}

    impl UiRoutine for Noop {
        type Output = Result<(), UiRoutineFailure>;

        fn execute(self, _context: &mut UiRoutineContext<'_>) -> Self::Output {
            Ok(())
        }
    }

    struct FailWithoutInput;

    impl UiRoutineSealed for FailWithoutInput {}

    impl UiRoutine for FailWithoutInput {
        type Output = Result<(), UiRoutineFailure>;

        fn execute(self, _context: &mut UiRoutineContext<'_>) -> Self::Output {
            Err(failure(
                InputCertainty::BeforeInput,
                "test_inner",
                "内层例程在输入前失败",
            ))
        }
    }

    struct PressThenFail;

    impl UiRoutineSealed for PressThenFail {}

    impl UiRoutine for PressThenFail {
        type Output = Result<(), UiRoutineFailure>;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            context
                .device()
                .press_key(Key::Return)
                .map_err(|error| {
                    UiRoutineFailure::new(
                        InputCertainty::BeforeInput,
                        "test_inner",
                        format!("{error:#}"),
                    )
                })?;
            Err(failure(
                InputCertainty::AfterInputUnknown,
                "test_inner",
                "按下 Return 后结果未知",
            ))
        }
    }

    struct CancelThenOk {
        cancelled: Arc<AtomicBool>,
    }

    impl UiRoutineSealed for CancelThenOk {}

    impl UiRoutine for CancelThenOk {
        type Output = Result<(), UiRoutineFailure>;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            self.cancelled.store(true, Ordering::Release);
            // 收尾阶段必须仍能等待：这里用一小段等待模拟导航后的稳定等待。
            context.wait_cancellable(Duration::from_millis(10))?;
            Ok(())
        }
    }

    // ---- 测试驱动 ----

    fn run_task_with<D: UiDevice, R: UiRoutine>(
        device: D,
        script: Vec<UiStateClassification>,
        spec: UiTaskSpec,
        inner: R,
        cancelled: Arc<AtomicBool>,
    ) -> (UiTaskOutcome<R::Output>, D) {
        let execution = execute_routine_for_test(
            device,
            Some(Box::new(ScriptedClassifier::new(script))),
            2,
            cancelled,
            UiTask::new(spec, inner),
        );
        (execution.output, execution.device)
    }

    #[test]
    fn entry_is_observed_once_before_any_input() {
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            vec![primary()],
            UiTaskSpec::new(UiResidencyTarget::Primary, FinishPolicy::KeepCurrent),
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(outcome.entry.as_ref().unwrap().kind, UiStateKind::Primary);
        assert_eq!(outcome.input_epoch, 0);
        assert!(device.keys().is_empty());
        // 入口稳定判定 2 帧；KeepCurrent 收尾再观测 1 帧记录实际界面。
        // 关键是入口快照只采集**一次**，不会在目标检查和收尾里重复采入口。
        assert_eq!(device.captures(), 3);
    }

    #[test]
    fn primary_to_primary_has_no_actions() {
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            vec![primary()],
            UiTaskSpec::new(
                UiResidencyTarget::Primary,
                FinishPolicy::Fixed(UiResidencyTarget::Primary),
            ),
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(device.keys().is_empty(), "已在目标界面不应按键");
        assert!(outcome.finish.settled);
        assert_eq!(outcome.input_epoch, 0);
    }

    #[test]
    fn primary_to_secondary_uses_exactly_one_navigation() {
        // 入口 2 帧 + 收尾前观测 1 帧；按 Return 后 2 帧稳定为二级。
        let mut script = vec![primary(), primary(), primary()];
        script.extend(vec![secondary(), secondary()]);
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            script,
            UiTaskSpec::new(
                UiResidencyTarget::SecondaryCurrentHall,
                FinishPolicy::KeepCurrent,
            ),
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(device.keys(), vec![Key::Return]);
        assert_eq!(outcome.input_epoch, 1);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)
        ));
    }

    #[test]
    fn transient_unknown_is_waited_out_without_pressing_keys() {
        let mut script = uncertain(2);
        script.extend(vec![primary(), primary()]);
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            script,
            UiTaskSpec::new(UiResidencyTarget::Primary, FinishPolicy::KeepCurrent),
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(device.keys().is_empty(), "Unknown 瞬态不允许盲按 Esc");
        assert_eq!(outcome.entry.as_ref().unwrap().kind, UiStateKind::Primary);
    }

    #[test]
    fn main_menu_escape_restores_primary() {
        // 入口 2 帧 + 收尾前观测 1 帧；按 Esc 后 2 帧稳定回一级。
        let mut script = vec![main_menu(), main_menu(), main_menu()];
        script.extend(vec![primary(), primary()]);
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            script,
            UiTaskSpec::new(UiResidencyTarget::Primary, FinishPolicy::KeepCurrent),
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(outcome.finish.settled);
        assert_eq!(outcome.input_epoch, 1);
    }

    #[test]
    fn paimon_menu_is_rejected_without_pressing_keys() {
        let script = vec![paimon_menu()];
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            script,
            UiTaskSpec::new(UiResidencyTarget::Primary, FinishPolicy::KeepCurrent),
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(device.keys().is_empty());
        assert!(outcome.entry_failure.is_some());
        assert!(outcome.output.is_none());
        assert!(outcome.safe_retry());
    }

    #[test]
    fn loading_never_sends_input() {
        let script = vec![state(UiStateKind::Loading, "加载界面", "loading")];
        let spec = UiTaskSpec::new(
            UiResidencyTarget::Primary,
            FinishPolicy::Fixed(UiResidencyTarget::Primary),
        )
        .with_total_budget(Duration::from_millis(150));
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            script,
            spec,
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(device.keys().is_empty());
        assert!(outcome.output.is_none());
        assert!(!outcome.finish.settled);
    }

    #[test]
    fn keep_current_finish_has_no_actions() {
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            vec![secondary()],
            UiTaskSpec::new(
                UiResidencyTarget::SecondaryCurrentHall,
                FinishPolicy::KeepCurrent,
            ),
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(device.keys().is_empty());
        assert!(outcome.finish.settled);
        assert!(!outcome.finish.attempted);
    }

    #[test]
    fn partial_input_disables_safe_retry() {
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            vec![primary()],
            UiTaskSpec::new(UiResidencyTarget::Primary, FinishPolicy::KeepCurrent),
            PressThenFail,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(device.keys(), vec![Key::Return]);
        assert_eq!(outcome.input_epoch, 1);
        assert!(!outcome.safe_retry(), "发过输入后结果不确定，禁止自动重试");
    }

    #[test]
    fn failure_before_input_is_safe_to_retry() {
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            vec![primary()],
            UiTaskSpec::new(UiResidencyTarget::Primary, FinishPolicy::KeepCurrent),
            FailWithoutInput,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(device.keys().is_empty());
        assert_eq!(outcome.input_epoch, 0);
        assert!(outcome.safe_retry(), "没有发过输入时允许安全重试");
    }

    #[test]
    fn require_target_rejects_secondary_without_pressing_keys() {
        let mut script = vec![secondary(), secondary()];
        script.extend(vec![primary(), primary()]);
        let spec = UiTaskSpec::new(
            UiResidencyTarget::Primary,
            FinishPolicy::RestoreEntry {
                fallback: UiResidencyTarget::Primary,
            },
        )
        .with_entry(EntryPolicy::RequireTarget);
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            script,
            spec,
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(device.keys().is_empty(), "入口不满足时不允许任何按键");
        assert!(outcome.output.is_none());
        let failure = outcome.entry_failure.as_ref().expect("entry failure");
        assert_eq!(failure.certainty(), InputCertainty::BeforeInput);
        assert!(outcome.safe_retry());
    }

    #[test]
    fn passthrough_still_settles_a_fixed_finish_target() {
        // 邀请类任务：入口不观测/不导航，但结束后要统一落到 Primary（不回旧界面）。
        let mut script = vec![secondary(), secondary()];
        script.extend(vec![primary(), primary()]);
        let spec = UiTaskSpec::new(
            UiResidencyTarget::Primary,
            FinishPolicy::Fixed(UiResidencyTarget::Primary),
        )
        .with_entry(EntryPolicy::Passthrough);
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            script,
            spec,
            Noop,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(outcome.entry.is_none());
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn finish_still_runs_after_business_cancellation() {
        let cancelled = Arc::new(AtomicBool::new(false));
        // 入口 2 帧 + 收尾前观测 1 帧；按 Esc 后 2 帧稳定回一级。
        let mut script = vec![secondary(), secondary(), secondary()];
        script.extend(vec![primary(), primary()]);
        let spec = UiTaskSpec::new(
            UiResidencyTarget::SecondaryCurrentHall,
            FinishPolicy::Fixed(UiResidencyTarget::Primary),
        );
        let (outcome, device) = run_task_with(
            RecordingDevice::default(),
            script,
            spec,
            CancelThenOk {
                cancelled: Arc::clone(&cancelled),
            },
            Arc::clone(&cancelled),
        );
        assert!(cancelled.load(Ordering::Acquire));
        assert!(outcome.output.is_some(), "内层例程应已开始执行");
        assert!(outcome.finish.settled, "取消后收尾仍必须执行并完成");
        assert_eq!(device.keys(), vec![Key::Escape]);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }
}
