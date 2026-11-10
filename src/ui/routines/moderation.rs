use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use enigo::Key;

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, after_input_failure,
    before_input_failure, capture_normalized, wait_ms,
};
use super::transaction::{EntryPolicy, FinishPolicy, UiTask, UiTaskOutcome, UiTaskSpec};
use crate::geometry::{Point, Rect};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiStateObservation, UiSubmitError, sealed,
};
use crate::ui::template::{best_template_hit, template_exists};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModerationUiAction {
    Blacklist,
    BlockChat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecuteModeration {
    action: ModerationUiAction,
    uid: String,
}

impl ExecuteModeration {
    pub(crate) fn new(action: ModerationUiAction, uid: impl Into<String>) -> Self {
        Self {
            action,
            uid: uid.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModerationEffect {
    Applied,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecuteModerationOutcome {
    effect: ModerationEffect,
    residency: UiResidencyOutcome,
}

impl ExecuteModerationOutcome {
    pub(crate) fn effect(&self) -> &ModerationEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone)]
pub(crate) struct ModerationUi {
    runtime: UiRuntimeHandle,
    config: ModerationRoutineConfig,
}

impl ModerationUi {
    pub(crate) fn new(runtime: UiRuntimeHandle, config: ModerationRoutineConfig) -> Self {
        Self { runtime, config }
    }

    pub(crate) fn submit(
        &self,
        request: ExecuteModeration,
    ) -> Result<UiOperation<ExecuteModerationOutcome>, UiSubmitError> {
        self.runtime.submit(ExecuteModerationRoutine {
            request,
            config: self.config.clone(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct ModerationRoutineConfig {
    residency: FriendDeliveryRoutineConfig,
    friend_panel_template: PathBuf,
    search_panel_template: PathBuf,
    more_settings_template: PathBuf,
    blacklist_template: PathBuf,
    block_chat_template: PathBuf,
    confirm_template: PathBuf,
    friend_panel_region: Rect,
    search_panel_region: Rect,
    more_settings_region: Rect,
    blacklist_region: Rect,
    block_chat_region: Rect,
    confirm_region: Rect,
    search_input: Point,
    search_button: Point,
    marker_threshold: f32,
    ui_timeout_ms: u64,
    search_timeout_ms: u64,
    confirm_wait_ms: u64,
    step_ms: u64,
    text_ms: u64,
    return_retry_ms: u64,
}

pub(crate) struct ModerationRoutineConfigSource {
    pub(crate) residency: FriendDeliveryRoutineConfig,
    pub(crate) friend_panel_template: PathBuf,
    pub(crate) search_panel_template: PathBuf,
    pub(crate) more_settings_template: PathBuf,
    pub(crate) blacklist_template: PathBuf,
    pub(crate) block_chat_template: PathBuf,
    pub(crate) confirm_template: PathBuf,
    pub(crate) friend_panel_region: Rect,
    pub(crate) search_panel_region: Rect,
    pub(crate) more_settings_region: Rect,
    pub(crate) blacklist_region: Rect,
    pub(crate) block_chat_region: Rect,
    pub(crate) confirm_region: Rect,
    pub(crate) search_input: Point,
    pub(crate) search_button: Point,
    pub(crate) marker_threshold: f32,
    pub(crate) ui_timeout_ms: u64,
    pub(crate) search_timeout_ms: u64,
    pub(crate) confirm_wait_ms: u64,
    pub(crate) step_ms: u64,
    pub(crate) text_ms: u64,
    pub(crate) return_retry_ms: u64,
}

impl ModerationRoutineConfig {
    pub(crate) fn resolve(source: ModerationRoutineConfigSource) -> Self {
        Self {
            residency: source.residency,
            friend_panel_template: source.friend_panel_template,
            search_panel_template: source.search_panel_template,
            more_settings_template: source.more_settings_template,
            blacklist_template: source.blacklist_template,
            block_chat_template: source.block_chat_template,
            confirm_template: source.confirm_template,
            friend_panel_region: source.friend_panel_region,
            search_panel_region: source.search_panel_region,
            more_settings_region: source.more_settings_region,
            blacklist_region: source.blacklist_region,
            block_chat_region: source.block_chat_region,
            confirm_region: source.confirm_region,
            search_input: source.search_input,
            search_button: source.search_button,
            marker_threshold: source.marker_threshold,
            ui_timeout_ms: source.ui_timeout_ms,
            search_timeout_ms: source.search_timeout_ms,
            confirm_wait_ms: source.confirm_wait_ms,
            step_ms: source.step_ms,
            text_ms: source.text_ms,
            return_retry_ms: source.return_retry_ms,
        }
    }
}

struct ExecuteModerationRoutine {
    request: ExecuteModeration,
    config: ModerationRoutineConfig,
}

impl sealed::UiRoutineSealed for ExecuteModerationRoutine {}

impl UiRoutine for ExecuteModerationRoutine {
    type Output = ExecuteModerationOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        // 统一事务：入口必须是稳定 Primary（RequireTarget：不满足时零按键拒绝），
        // 收尾由 settle_to 吸收，面板/弹窗的有界 Esc 不再由例程自己 recover。
        let inner = ModerationInner {
            request: self.request,
            config: self.config,
        };
        let outcome: UiTaskOutcome<ModerationEffect> =
            UiRoutine::execute(UiTask::new(moderation_task_spec(), inner), context);
        let effect = outcome.output.unwrap_or_else(|| {
            ModerationEffect::Failed(outcome.entry_failure.clone().unwrap_or_else(|| {
                UiRoutineFailure::new(
                    InputCertainty::BeforeInput,
                    "ui_task_entry",
                    "管理任务未开始执行",
                )
            }))
        });
        ExecuteModerationOutcome {
            effect,
            residency: outcome.residency,
        }
    }
}

/// 管理任务事务声明：
/// - 目标 Primary；收尾回入口（入口本来就强制 Primary，因此是恒等收尾，面板由 settle_to 有界 Esc 关闭）；
/// - `RequireTarget` 保证“入口必须稳定 Primary”：Secondary/Unknown 等直接 BeforeInput 拒绝，零按键；
/// - 总预算覆盖搜索/确认/收尾（收尾预留一个 `ui_timeout`）。
fn moderation_task_spec() -> UiTaskSpec {
    UiTaskSpec::new(
        UiResidencyTarget::Primary,
        FinishPolicy::RestoreEntry {
            fallback: UiResidencyTarget::Primary,
        },
    )
    .with_entry(EntryPolicy::RequireTarget)
    .with_total_budget(Duration::from_secs(60))
}

struct ModerationInner {
    request: ExecuteModeration,
    config: ModerationRoutineConfig,
}

impl sealed::UiRoutineSealed for ModerationInner {}

impl UiRoutine for ModerationInner {
    type Output = ModerationEffect;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        match execute_moderation(context, &self.request, &self.config) {
            Ok(()) => ModerationEffect::Applied,
            Err(failure) => ModerationEffect::Failed(failure),
        }
    }
}

fn execute_moderation(
    context: &mut UiRoutineContext<'_>,
    request: &ExecuteModeration,
    config: &ModerationRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    normalize_primary(context, config)?;
    context
        .device()
        .press_key(Key::Unicode('o'))
        .map_err(|error| after_input_failure("open_friend_panel", error))?;
    wait_template(
        context,
        config,
        &config.friend_panel_template,
        config.friend_panel_region,
        config.ui_timeout_ms,
        false,
        "confirm_friend_panel",
    )?;

    context
        .device()
        .press_key(Key::Unicode('e'))
        .map_err(|error| after_input_failure("open_friend_search", error))?;
    wait_ms(
        context,
        config.step_ms,
        "open_friend_search",
        InputCertainty::AfterInputUnknown,
    )?;
    context
        .device()
        .press_key(Key::Unicode('e'))
        .map_err(|error| after_input_failure("open_friend_search", error))?;
    wait_template(
        context,
        config,
        &config.search_panel_template,
        config.search_panel_region,
        config.ui_timeout_ms,
        false,
        "confirm_friend_search",
    )?;

    context
        .device()
        .click_point(config.search_input.x, config.search_input.y)
        .map_err(|error| after_input_failure("focus_uid_input", error))?;
    wait_ms(
        context,
        config.residency.click_ms,
        "focus_uid_input",
        InputCertainty::AfterInputUnknown,
    )?;
    context
        .device()
        .paste_text(&request.uid, config.text_ms)
        .map_err(|error| after_input_failure("input_uid", error))?;
    context
        .device()
        .click_point(config.search_button.x, config.search_button.y)
        .map_err(|error| after_input_failure("submit_uid_search", error))?;
    wait_template(
        context,
        config,
        &config.more_settings_template,
        config.more_settings_region,
        config.search_timeout_ms,
        true,
        "open_more_settings",
    )?;

    let (action_template, action_region) = match request.action {
        ModerationUiAction::Blacklist => (&config.blacklist_template, config.blacklist_region),
        ModerationUiAction::BlockChat => (&config.block_chat_template, config.block_chat_region),
    };
    wait_template(
        context,
        config,
        action_template,
        action_region,
        config.ui_timeout_ms,
        true,
        "select_moderation_action",
    )?;
    wait_template(
        context,
        config,
        &config.confirm_template,
        config.confirm_region,
        config.ui_timeout_ms,
        true,
        "confirm_moderation_action",
    )?;
    confirm_template_absent(context, config)
}

/// 入口“必须是稳定 Primary”由 `UiTaskSpec::RequireTarget` 在事务层保证；
/// 这里只做真实的输入准备（激活/前台），不再重复采样状态。
fn normalize_primary(
    context: &mut UiRoutineContext<'_>,
    config: &ModerationRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    context
        .device()
        .ensure_ready(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_moderation", error))
}

#[allow(clippy::too_many_arguments)]
fn wait_template(
    context: &mut UiRoutineContext<'_>,
    config: &ModerationRoutineConfig,
    template: &Path,
    region: Rect,
    timeout_ms: u64,
    click: bool,
    stage: &'static str,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let image = capture_normalized(
            context,
            &config.residency,
            stage,
            InputCertainty::AfterInputUnknown,
        )?;
        if let Some(hit) =
            best_template_hit(&image, Some(region), template, config.marker_threshold)
                .map_err(|error| after_input_failure(stage, error))?
        {
            if click {
                let point = hit.center();
                context
                    .device()
                    .click_point(point.x, point.y)
                    .map_err(|error| after_input_failure(stage, error))?;
                wait_ms(
                    context,
                    config.residency.click_ms,
                    stage,
                    InputCertainty::AfterInputUnknown,
                )?;
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                stage,
                "required moderation template was not found before timeout",
            ));
        }
        wait_ms(
            context,
            config.residency.poll_ms,
            stage,
            InputCertainty::AfterInputUnknown,
        )?;
    }
}

fn confirm_template_absent(
    context: &mut UiRoutineContext<'_>,
    config: &ModerationRoutineConfig,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(config.confirm_wait_ms);
    let mut absent_streak = 0_u32;
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_action_applied",
            InputCertainty::AfterInputUnknown,
        )?;
        let present = template_exists(
            &image,
            Some(config.confirm_region),
            &config.confirm_template,
            config.marker_threshold,
        )
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "confirm_action_applied",
                format!("{error:#}"),
            )
        })?;
        if present {
            absent_streak = 0;
        } else {
            absent_streak = absent_streak.saturating_add(1);
            if absent_streak >= config.residency.stable_count {
                return Ok(());
            }
        }
        wait_ms(
            context,
            config.residency.poll_ms,
            "confirm_action_applied",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_action_applied",
        "confirmation template did not stably disappear",
    ))
}

/// 面板有界 Esc 的稳定性判定。
///
/// 收尾恢复已统一到 `transaction::UiTask`（按稳定 kind 决定是否动作）；
/// 这里保留判定与模板清单，供后续“会话级/面板级恢复”复用与单测，不在本任务删除。
#[allow(dead_code)]
#[derive(Default)]
struct PanelStability {
    candidate: Option<usize>,
    samples: u32,
}

impl PanelStability {
    #[allow(dead_code)]
    fn observe(&mut self, candidate: Option<usize>, required_samples: u32) -> bool {
        let Some(candidate) = candidate else {
            self.candidate = None;
            self.samples = 0;
            return false;
        };
        if self.candidate == Some(candidate) {
            self.samples = self.samples.saturating_add(1);
        } else {
            self.candidate = Some(candidate);
            self.samples = 1;
        }
        self.samples >= required_samples.max(2)
    }
}

/// 面板模板清单：保留“哪些面板算可 Esc 恢复”的单一来源，后续会话级恢复复用。
#[allow(dead_code)]
fn recoverable_panel_candidate(
    image: &image::DynamicImage,
    config: &ModerationRoutineConfig,
) -> Result<Option<usize>, UiRoutineFailure> {
    for (index, (template, region)) in [
        (&config.friend_panel_template, config.friend_panel_region),
        (&config.search_panel_template, config.search_panel_region),
        (&config.more_settings_template, config.more_settings_region),
        (&config.blacklist_template, config.blacklist_region),
        (&config.block_chat_template, config.block_chat_region),
        (&config.confirm_template, config.confirm_region),
    ]
    .into_iter()
    .enumerate()
    {
        if template_exists(image, Some(region), template, config.marker_threshold)
            .map_err(|error| after_input_failure("recover_moderation", error))?
        {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;

    use image::DynamicImage;

    use super::*;
    use crate::runtime::ui::{
        UiDevice, UiRoutine, UiRoutineContext, UiRoutineFailure, UiStateClassification,
        UiStateClassifier, UiStateEvidence, UiStateKind, execute_routine_for_test,
        sealed::UiRoutineSealed,
    };
    use crate::ui::routines::transaction::{FinishPolicy, UiTask, UiTaskOutcome};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum RecordedKey {
        Capture,
        Key(enigo::Key),
    }

    #[derive(Default)]
    struct RecordingDevice {
        actions: Arc<Mutex<Vec<RecordedKey>>>,
    }

    impl RecordingDevice {
        fn keys(&self) -> Vec<enigo::Key> {
            self.actions
                .lock()
                .unwrap()
                .iter()
                .filter_map(|action| match action {
                    RecordedKey::Key(key) => Some(*key),
                    RecordedKey::Capture => None,
                })
                .collect()
        }
    }

    impl UiDevice for RecordingDevice {
        fn capture(&mut self) -> anyhow::Result<DynamicImage> {
            self.actions.lock().unwrap().push(RecordedKey::Capture);
            Ok(DynamicImage::new_rgba8(16, 16))
        }

        fn press_key(&mut self, key: enigo::Key) -> anyhow::Result<()> {
            self.actions.lock().unwrap().push(RecordedKey::Key(key));
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
        fn classify(
            &mut self,
            _image: &DynamicImage,
        ) -> anyhow::Result<UiStateClassification> {
            let index = self.index.min(self.states.len() - 1);
            self.index += 1;
            Ok(self.states[index].clone())
        }
    }

    fn state(kind: UiStateKind) -> UiStateClassification {
        UiStateClassification::with_evidence(
            kind,
            kind.label(),
            UiStateEvidence::new(Vec::new(), None, kind.name()),
        )
    }

    struct AppliedNoop;

    impl UiRoutineSealed for AppliedNoop {}

    impl UiRoutine for AppliedNoop {
        type Output = ModerationEffect;

        fn execute(self, _context: &mut UiRoutineContext<'_>) -> Self::Output {
            ModerationEffect::Applied
        }
    }

    fn run_task(
        script: Vec<UiStateClassification>,
    ) -> (UiTaskOutcome<ModerationEffect>, RecordingDevice) {
        let spec = moderation_task_spec();
        let execution = execute_routine_for_test(
            RecordingDevice::default(),
            Some(Box::new(ScriptedClassifier::new(script))),
            2,
            Arc::new(AtomicBool::new(false)),
            UiTask::new(spec, AppliedNoop),
        );
        (execution.output, execution.device)
    }

    #[test]
    fn panel_stack_uses_bounded_escape() {
        // 面板一直关不掉时，只允许有界 Esc，不能无限按键（更不能把玩家会话当地板刷）。
        let script = vec![state(UiStateKind::Primary), state(UiStateKind::Primary)]
            .into_iter()
            .chain(std::iter::repeat(state(UiStateKind::Panel)).take(20))
            .collect();
        let (outcome, device) = run_task(script);
        assert!(device.keys().len() <= 2, "Esc 必须是有界的");
        assert!(!outcome.finish.settled);
        assert!(!device.keys().is_empty());
    }

    #[test]
    fn entry_secondary_is_rejected_before_any_input() {
        // 管理操作只允许从一级界面开始；二级（玩家会话）上必须零按键拒绝。
        let script = vec![state(UiStateKind::Secondary); 4];
        let (outcome, device) = run_task(script);
        assert!(device.keys().is_empty(), "入口不满足时不允许按 Esc 退出玩家会话");
        assert!(outcome.output.is_none());
        let failure = outcome.entry_failure.as_ref().expect("entry failure");
        assert_eq!(failure.certainty(), InputCertainty::BeforeInput);
        assert!(outcome.safe_retry());
    }

    #[test]
    fn entry_primary_with_panels_closed_returns_primary_without_extra_input() {
        let script = vec![state(UiStateKind::Primary); 6];
        let (outcome, device) = run_task(script);
        assert!(device.keys().is_empty());
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn panel_closes_then_returns_primary() {
        let mut script = vec![state(UiStateKind::Primary), state(UiStateKind::Primary)];
        script.extend(vec![state(UiStateKind::Panel), state(UiStateKind::Panel)]);
        script.extend(vec![state(UiStateKind::Primary), state(UiStateKind::Primary)]);
        let (outcome, device) = run_task(script);
        assert_eq!(device.keys(), vec![enigo::Key::Escape]);
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
    }

    #[test]
    fn recoverable_panel_requires_consecutive_matching_samples_before_escape() {
        let mut stability = PanelStability::default();

        assert!(!stability.observe(Some(1), 2));
        assert!(!stability.observe(Some(2), 2));
        assert!(!stability.observe(None, 2));
        assert!(!stability.observe(Some(2), 2));
        assert!(stability.observe(Some(2), 2));
    }
}
