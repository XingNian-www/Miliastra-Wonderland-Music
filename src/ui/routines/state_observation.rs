use std::time::{Duration, Instant};

use image::DynamicImage;

use crate::runtime::ui::{
    InputCertainty, UiRoutineContext, UiRoutineFailure, UiStateKind, UiStateObservation,
};
use crate::ui::frame::{Canvas, resize_to_canvas};

#[derive(Clone, Copy)]
pub(super) struct UiStateObservationConfig {
    canvas_width: u32,
    canvas_height: u32,
    poll_ms: u64,
}

impl UiStateObservationConfig {
    pub(super) fn new(canvas_width: u32, canvas_height: u32, poll_ms: u64) -> Self {
        Self {
            canvas_width,
            canvas_height,
            poll_ms: poll_ms.max(10),
        }
    }
}

pub(super) fn wait_ms(
    context: &mut UiRoutineContext<'_>,
    ms: u64,
    stage: &'static str,
    certainty: InputCertainty,
) -> Result<(), UiRoutineFailure> {
    context
        .wait_cancellable(Duration::from_millis(ms))
        .map_err(|failure| UiRoutineFailure::new(certainty, stage, failure.reason()))
}

pub(super) fn capture_normalized_ui_state(
    context: &mut UiRoutineContext<'_>,
    config: &UiStateObservationConfig,
    stage: &'static str,
    certainty: InputCertainty,
) -> Result<DynamicImage, UiRoutineFailure> {
    let image = context
        .capture_frame()
        .map_err(|error| UiRoutineFailure::new(certainty, stage, format!("{error:#}")))?;
    let image = image.into_image();
    let canvas = Canvas {
        width: config.canvas_width,
        height: config.canvas_height,
        resize: true,
    };
    let normalized = resize_to_canvas(&image, &canvas, None);
    Ok(normalized.unwrap_or(image))
}

pub(super) fn wait_for_stable_ui_kind(
    context: &mut UiRoutineContext<'_>,
    config: UiStateObservationConfig,
    expected: Option<UiStateKind>,
    timeout_ms: u64,
    stage: &'static str,
    certainty: InputCertainty,
) -> Result<UiStateKind, UiRoutineFailure> {
    wait_for_stable_ui_kind_internal(
        context, config, expected, timeout_ms, stage, certainty, false,
    )
}

pub(super) fn wait_for_stable_ui_kind_or_unknown(
    context: &mut UiRoutineContext<'_>,
    config: UiStateObservationConfig,
    expected: Option<UiStateKind>,
    timeout_ms: u64,
    stage: &'static str,
    certainty: InputCertainty,
) -> Result<UiStateKind, UiRoutineFailure> {
    wait_for_stable_ui_kind_internal(
        context, config, expected, timeout_ms, stage, certainty, true,
    )
}

fn wait_for_stable_ui_kind_internal(
    context: &mut UiRoutineContext<'_>,
    config: UiStateObservationConfig,
    expected: Option<UiStateKind>,
    timeout_ms: u64,
    stage: &'static str,
    certainty: InputCertainty,
    allow_stable_unknown: bool,
) -> Result<UiStateKind, UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut fresh_candidate: Option<(UiStateKind, String)> = None;
    let mut fresh_samples = 0_u32;
    loop {
        capture_normalized_ui_state(context, &config, stage, certainty)?;
        let observation = context.latest_ui_state().ok_or_else(|| {
            UiRoutineFailure::new(
                certainty,
                stage,
                "UI runtime has no configured template state classifier",
            )
        })?;
        match &observation {
            UiStateObservation::Classified(state) => {
                let kind = state.classification().kind();
                let expected_mismatch = expected.is_some_and(|expected| expected != kind);
                if expected_mismatch || (kind == UiStateKind::Unknown && !allow_stable_unknown) {
                    fresh_candidate = None;
                    fresh_samples = 0;
                } else {
                    let candidate = (kind, state.classification().label().to_string());
                    if fresh_candidate.as_ref() == Some(&candidate) {
                        fresh_samples = fresh_samples.saturating_add(1);
                    } else {
                        fresh_candidate = Some(candidate);
                        fresh_samples = 1;
                    }
                    let runtime_stable = if kind == UiStateKind::Unknown {
                        allow_stable_unknown
                    } else {
                        state.stable_kind() == Some(kind)
                    };
                    if runtime_stable && fresh_samples >= state.required_count() {
                        return Ok(kind);
                    }
                }
            }
            UiStateObservation::Failed { reason, .. } => {
                return Err(UiRoutineFailure::new(
                    certainty,
                    stage,
                    format!("template UI state classification failed: {reason}"),
                ));
            }
        }
        let last_observation = observation.diagnostic();
        if Instant::now() >= deadline {
            return Err(UiRoutineFailure::new(
                certainty,
                stage,
                format!(
                    "stable template UI state was not observed before timeout; last={}",
                    last_observation
                ),
            ));
        }
        wait_ms(context, config.poll_ms, stage, certainty)?;
    }
}

/// 一次分类后的只读快照：恢复决策与失败诊断所需的最小事实集。
///
/// 只读取 `UiStateClassification` 已公开的字段，不新增运行时接口，
/// 因此可用于 friend_delivery 的状态驱动恢复，而不改 `runtime/ui.rs`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct UiStateSnapshot {
    pub(super) kind: UiStateKind,
    /// 分类标签（如 `main_menu`、`secondary:chat`、`primary:hall`）。
    pub(super) label: String,
    /// 判据最终规则名（区分千星主菜单 `main_menu_template` 与
    /// 派蒙菜单 `paimon_menu_panel_template` 的唯一依据）。
    pub(super) final_rule: String,
    /// 世界判据结论（`teyvat` / `qianxing` / `unknown` / `not_applicable`…）。
    pub(super) world_guess: String,
    /// 运行时跟踪器给出的"最后一次稳定类型"，用于诊断瞬失。
    pub(super) last_stable_kind: Option<UiStateKind>,
    /// 本帧是否已被跟踪器判为稳定（连续 required_count 帧一致）。
    pub(super) runtime_stable: bool,
    pub(super) candidate_count: u32,
    pub(super) required_count: u32,
}

impl UiStateSnapshot {
    /// 状态身份：动作前后比较"是否有进展"用（kind + label，不含证据）。
    pub(super) fn identity(&self) -> String {
        format!("{}|{}", self.kind.name(), self.label)
    }

    /// 失败诊断用的一行事实串（含 rule / world / last_stable）。
    pub(super) fn describe(&self) -> String {
        format!(
            "kind={} label={} rule={} world={} last_stable={:?} stable={} samples={}/{}",
            self.kind.name(),
            self.label,
            self.final_rule,
            self.world_guess,
            self.last_stable_kind,
            self.runtime_stable,
            self.candidate_count,
            self.required_count
        )
    }

    /// 是否为千星奇域主菜单（由「返回提瓦特」模板判出）。
    pub(super) fn is_wonderland_main_menu(&self) -> bool {
        self.kind == UiStateKind::MainMenu && self.final_rule == "main_menu_template"
    }

    /// 是否为派蒙菜单（ESC 系统菜单）：它复用主菜单状态，但绝不是大厅菜单。
    pub(super) fn is_paimon_menu(&self) -> bool {
        self.kind == UiStateKind::MainMenu && self.final_rule == "paimon_menu_panel_template"
    }
}

/// 采一帧并返回分类快照；分类失败也如实返回错误（带 last_stable 诊断）。
///
/// 与 [`wait_for_stable_ui_kind`] 的区别：它只采样一次、把完整证据交回调用方，
/// 由调用方按状态机决策下一步动作（恢复入口 `settle_to` 用）。
pub(super) fn observe_ui_state_once(
    context: &mut UiRoutineContext<'_>,
    config: UiStateObservationConfig,
    stage: &'static str,
    certainty: InputCertainty,
) -> Result<UiStateSnapshot, UiRoutineFailure> {
    capture_normalized_ui_state(context, &config, stage, certainty)?;
    let observation = context.latest_ui_state().ok_or_else(|| {
        UiRoutineFailure::new(
            certainty,
            stage,
            "UI runtime has no configured template state classifier",
        )
    })?;
    match &observation {
        UiStateObservation::Classified(state) => {
            let classification = state.classification();
            Ok(UiStateSnapshot {
                kind: classification.kind(),
                label: classification.label().to_string(),
                final_rule: classification.evidence().final_rule().to_string(),
                world_guess: classification
                    .evidence()
                    .world_probe()
                    .map(|probe| probe.guess().to_string())
                    .unwrap_or_else(|| "none".to_string()),
                last_stable_kind: state.last_stable_kind(),
                runtime_stable: state.stable_kind() == Some(classification.kind()),
                candidate_count: state.candidate_count(),
                required_count: state.required_count(),
            })
        }
        UiStateObservation::Failed {
            reason,
            last_stable_kind,
            ..
        } => Err(UiRoutineFailure::new(
            certainty,
            stage,
            format!(
                "template UI state classification failed: {reason}; last_stable={last_stable_kind:?}"
            ),
        )),
    }
}
