use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use image::imageops::FilterType;

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyTarget, before_input_failure, restore_residency,
};
use crate::adapters::windows::parse_key;
use crate::interfaces::ui_plan::{
    WorkflowOperation, WorkflowPixelStability, WorkflowPoint, WorkflowRect, WorkflowResidency,
};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure,
    UiRoutineProgressStage, UiRuntimeHandle, UiSubmitError, sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::change_detection::{change_stats, rect_chat_change_fingerprint};
use crate::ui::geometry::{Point, Rect, crop_canvas};
use crate::ui::template::{TemplateHit, best_template_hit};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CustomActionPlan {
    workflow: String,
    operations: Vec<WorkflowOperation>,
}

impl CustomActionPlan {
    pub(crate) fn new(workflow: impl Into<String>, operations: Vec<WorkflowOperation>) -> Self {
        Self {
            workflow: workflow.into(),
            operations,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CustomActionOutcome {
    completed: usize,
    failure: Option<UiRoutineFailure>,
}

impl CustomActionOutcome {
    pub(crate) fn completed(&self) -> usize {
        self.completed
    }

    pub(crate) fn failure(&self) -> Option<&UiRoutineFailure> {
        self.failure.as_ref()
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.failure.is_none()
    }
}

#[derive(Clone)]
pub(crate) struct CustomActionUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    running: Arc<AtomicBool>,
    canvas: CustomActionCanvas,
    residency: FriendDeliveryRoutineConfig,
}

impl CustomActionUi {
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        running: Arc<AtomicBool>,
        canvas_width: u32,
        canvas_height: u32,
        residency: FriendDeliveryRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            running,
            canvas: CustomActionCanvas {
                width: canvas_width,
                height: canvas_height,
            },
            residency,
        }
    }

    pub(crate) fn submit(
        &self,
        request: CustomActionPlan,
    ) -> Result<UiOperation<CustomActionOutcome>, UiSubmitError> {
        self.runtime.submit(CustomActionRoutine {
            request,
            ocr: self.ocr.clone(),
            running: Arc::clone(&self.running),
            canvas: self.canvas,
            residency: self.residency.clone(),
        })
    }
}

#[derive(Clone, Copy)]
struct CustomActionCanvas {
    width: u32,
    height: u32,
}

struct CustomActionRoutine {
    request: CustomActionPlan,
    ocr: OcrRuntimeHandle,
    running: Arc<AtomicBool>,
    canvas: CustomActionCanvas,
    residency: FriendDeliveryRoutineConfig,
}

impl sealed::UiRoutineSealed for CustomActionRoutine {}

impl UiRoutine for CustomActionRoutine {
    type Output = CustomActionOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let operation_count = self.request.operations.len();
        let mut completed = 0;
        let mut input_performed = false;
        for (index, operation) in self.request.operations.into_iter().enumerate() {
            context.publish_progress(UiRoutineProgressStage::ExecutingCustomAction {
                operation_index: index,
                operation_count,
            });
            match execute_operation(
                context,
                &self.ocr,
                &self.running,
                self.canvas,
                &self.residency,
                operation,
                input_performed,
            ) {
                Ok(sent_input) => {
                    completed += 1;
                    input_performed |= sent_input;
                }
                Err(failure) => {
                    log::error!(
                        "自定义动作计划失败: workflow={} completed={}/{} stage={} certainty={:?}",
                        self.request.workflow,
                        completed,
                        operation_count,
                        failure.stage(),
                        failure.certainty()
                    );
                    return CustomActionOutcome {
                        completed,
                        failure: Some(failure),
                    };
                }
            }
        }
        CustomActionOutcome {
            completed,
            failure: None,
        }
    }
}

fn execute_operation(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    running: &Arc<AtomicBool>,
    canvas: CustomActionCanvas,
    residency: &FriendDeliveryRoutineConfig,
    operation: WorkflowOperation,
    input_performed: bool,
) -> Result<bool, UiRoutineFailure> {
    if !running.load(Ordering::SeqCst) {
        return Err(cancelled_failure(input_performed));
    }
    match operation {
        WorkflowOperation::Wait { duration_ms } => {
            sleep(Duration::from_millis(duration_ms));
            Ok(false)
        }
        WorkflowOperation::PressKey { key } => {
            let key = parse_key(&key)
                .map_err(|error| observation_failure(input_performed, "parse_custom_key", error))?;
            context
                .device()
                .press_key(key)
                .map_err(|error| input_failure("press_custom_key", error))?;
            Ok(true)
        }
        WorkflowOperation::HoldKey {
            key,
            duration_seconds,
        } => {
            let key = parse_key(&key).map_err(|error| {
                observation_failure(input_performed, "parse_custom_hold_key", error)
            })?;
            context
                .device()
                .hold_key(
                    key,
                    Duration::from_secs(duration_seconds),
                    Arc::clone(running),
                )
                .map_err(|error| input_failure("hold_custom_key", error))?;
            Ok(true)
        }
        WorkflowOperation::ActivateGame { after_activate_ms } => {
            context
                .device()
                .activate(after_activate_ms)
                .map_err(|error| input_failure("activate_custom_game", error))?;
            Ok(true)
        }
        WorkflowOperation::FocusGame { after_activate_ms } => {
            context
                .device()
                .focus(after_activate_ms)
                .map_err(|error| input_failure("focus_custom_game", error))?;
            Ok(true)
        }
        WorkflowOperation::EnsureResidency { target } => {
            context
                .device()
                .ensure_ready(residency.after_activate_ms)
                .map_err(|error| before_input_failure("prepare_custom_residency", error))?;
            let target = match target {
                WorkflowResidency::Primary => UiResidencyTarget::Primary,
                WorkflowResidency::SecondaryCurrentHall => UiResidencyTarget::SecondaryCurrentHall,
            };
            restore_residency(context, ocr, residency, target)?;
            Ok(true)
        }
        WorkflowOperation::ClickPoint { point } => {
            click(context, point).map_err(|error| input_failure("click_custom_point", error))?;
            Ok(true)
        }
        WorkflowOperation::WaitTemplate {
            template,
            region,
            threshold,
            timeout_ms,
            poll_ms,
        } => {
            wait_template(
                context,
                running,
                canvas,
                &template,
                region,
                threshold,
                timeout_ms,
                poll_ms,
                input_performed,
            )?;
            Ok(false)
        }
        WorkflowOperation::ClickTemplate {
            template,
            region,
            threshold,
            timeout_ms,
            poll_ms,
            offset,
        } => {
            let hit = wait_template(
                context,
                running,
                canvas,
                &template,
                region,
                threshold,
                timeout_ms,
                poll_ms,
                input_performed,
            )?;
            let center = hit.center();
            context
                .device()
                .click_point(center.x + offset.x, center.y + offset.y)
                .map_err(|error| input_failure("click_custom_template", error))?;
            Ok(true)
        }
        WorkflowOperation::WaitTemplateAbsent {
            template,
            region,
            threshold,
            timeout_ms,
            poll_ms,
            stability,
        } => {
            wait_template_absent(
                context,
                running,
                canvas,
                &template,
                region,
                threshold,
                timeout_ms,
                poll_ms,
                input_performed,
            )?;
            if let Some(stability) = stability {
                wait_pixels_stable(
                    context,
                    running,
                    canvas,
                    region,
                    poll_ms,
                    stability,
                    input_performed,
                )?;
            }
            Ok(false)
        }
        WorkflowOperation::WaitPixelsStable {
            region,
            poll_ms,
            stability,
        } => {
            wait_pixels_stable(
                context,
                running,
                canvas,
                region,
                poll_ms,
                stability,
                input_performed,
            )?;
            Ok(false)
        }
        WorkflowOperation::WaitText {
            expected,
            region,
            timeout_ms,
            poll_ms,
        } => {
            wait_text(
                context,
                ocr,
                running,
                canvas,
                &expected,
                region,
                timeout_ms,
                poll_ms,
                input_performed,
            )?;
            Ok(false)
        }
        WorkflowOperation::ClickText {
            expected,
            region,
            timeout_ms,
            poll_ms,
            offset,
        } => {
            let point = wait_text(
                context,
                ocr,
                running,
                canvas,
                &expected,
                region,
                timeout_ms,
                poll_ms,
                input_performed,
            )?;
            context
                .device()
                .click_point(point.x + offset.x, point.y + offset.y)
                .map_err(|error| input_failure("click_custom_text", error))?;
            Ok(true)
        }
        WorkflowOperation::PasteText {
            text,
            clipboard_hold_ms,
        } => {
            context
                .device()
                .paste_text(&text, clipboard_hold_ms)
                .map_err(|error| input_failure("paste_custom_text", error))?;
            Ok(true)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn wait_template(
    context: &mut UiRoutineContext<'_>,
    running: &AtomicBool,
    canvas: CustomActionCanvas,
    template: &Path,
    region: WorkflowRect,
    threshold: f32,
    timeout_ms: u64,
    poll_ms: u64,
    input_performed: bool,
) -> Result<TemplateHit, UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        ensure_running(running, input_performed)?;
        let image = capture_normalized(context, canvas, input_performed)?;
        let hit = best_template_hit(&image, Some(rect(region)), template, threshold).map_err(
            |error| observation_failure(input_performed, "match_custom_template", error),
        )?;
        if let Some(hit) = hit {
            return Ok(hit);
        }
        if Instant::now() >= deadline {
            return Err(observation_reason(
                input_performed,
                "wait_custom_template",
                "template not found before timeout",
            ));
        }
        sleep(Duration::from_millis(poll_ms.max(50)));
    }
}

#[allow(clippy::too_many_arguments)]
fn wait_template_absent(
    context: &mut UiRoutineContext<'_>,
    running: &AtomicBool,
    canvas: CustomActionCanvas,
    template: &Path,
    region: WorkflowRect,
    threshold: f32,
    timeout_ms: u64,
    poll_ms: u64,
    input_performed: bool,
) -> Result<(), UiRoutineFailure> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        ensure_running(running, input_performed)?;
        let image = capture_normalized(context, canvas, input_performed)?;
        let hit = best_template_hit(&image, Some(rect(region)), template, threshold).map_err(
            |error| observation_failure(input_performed, "match_custom_template", error),
        )?;
        if hit.is_none() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(observation_reason(
                input_performed,
                "wait_custom_template_absent",
                "template remained visible before timeout",
            ));
        }
        sleep(Duration::from_millis(poll_ms.max(50)));
    }
}

fn wait_pixels_stable(
    context: &mut UiRoutineContext<'_>,
    running: &AtomicBool,
    canvas: CustomActionCanvas,
    region: WorkflowRect,
    poll_ms: u64,
    stability: WorkflowPixelStability,
    input_performed: bool,
) -> Result<(), UiRoutineFailure> {
    let region = rect(region);
    let deadline = Instant::now() + Duration::from_millis(stability.timeout_ms);
    let first = capture_normalized(context, canvas, input_performed)?;
    let mut previous = rect_chat_change_fingerprint(&first, region).map_err(|error| {
        observation_failure(input_performed, "fingerprint_custom_region", error)
    })?;
    loop {
        ensure_running(running, input_performed)?;
        sleep(Duration::from_millis(poll_ms.max(50)));
        let current = capture_normalized(context, canvas, input_performed)?;
        let current = rect_chat_change_fingerprint(&current, region).map_err(|error| {
            observation_failure(input_performed, "fingerprint_custom_region", error)
        })?;
        let stats = change_stats(&previous, &current);
        if stats.mean_abs_diff <= stability.mean_threshold
            && stats.changed_ratio <= stability.changed_ratio_threshold
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(observation_reason(
                input_performed,
                "wait_custom_pixels_stable",
                "pixels did not stabilize before timeout",
            ));
        }
        previous = current;
    }
}

#[allow(clippy::too_many_arguments)]
fn wait_text(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    running: &AtomicBool,
    canvas: CustomActionCanvas,
    expected: &str,
    region: WorkflowRect,
    timeout_ms: u64,
    poll_ms: u64,
    input_performed: bool,
) -> Result<Point, UiRoutineFailure> {
    let target = normalize_lock_text(expected);
    let region = rect(region);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        ensure_running(running, input_performed)?;
        let image = capture_normalized(context, canvas, input_performed)?;
        let crop = crop_canvas(&image, region)
            .map_err(|error| observation_failure(input_performed, "crop_custom_text", error))?;
        let lines = ocr
            .recognize_lines(crop, OcrPriority::UiConfirmation)
            .map_err(|error| observation_failure(input_performed, "ocr_custom_text", error))?;
        let mut fallback = None;
        for line in lines {
            let normalized = normalize_lock_text(&line.text);
            if normalized.is_empty() {
                continue;
            }
            let point = Rect::new(
                region.x + line.bbox.x,
                region.y + line.bbox.y,
                line.bbox.width,
                line.bbox.height,
            )
            .center();
            if normalized == target {
                return Ok(point);
            }
            if fallback.is_none() && (normalized.contains(&target) || target.contains(&normalized))
            {
                fallback = Some(point);
            }
        }
        if let Some(point) = fallback {
            return Ok(point);
        }
        if Instant::now() >= deadline {
            return Err(observation_reason(
                input_performed,
                "wait_custom_text",
                "text not found before timeout",
            ));
        }
        sleep(Duration::from_millis(poll_ms.max(50)));
    }
}

fn capture_normalized(
    context: &mut UiRoutineContext<'_>,
    canvas: CustomActionCanvas,
    input_performed: bool,
) -> Result<image::DynamicImage, UiRoutineFailure> {
    let image = context
        .device()
        .capture()
        .map_err(|error| observation_failure(input_performed, "capture_custom_action", error))?;
    if image.width() == canvas.width && image.height() == canvas.height {
        Ok(image)
    } else {
        Ok(image.resize_exact(canvas.width, canvas.height, FilterType::Triangle))
    }
}

fn click(context: &mut UiRoutineContext<'_>, point: WorkflowPoint) -> anyhow::Result<()> {
    context.device().click_point(point.x, point.y)
}

fn rect(value: WorkflowRect) -> Rect {
    Rect::new(value.x, value.y, value.width, value.height)
}

fn ensure_running(running: &AtomicBool, input_performed: bool) -> Result<(), UiRoutineFailure> {
    running
        .load(Ordering::SeqCst)
        .then_some(())
        .ok_or_else(|| cancelled_failure(input_performed))
}

fn cancelled_failure(input_performed: bool) -> UiRoutineFailure {
    UiRoutineFailure::new(
        if input_performed {
            InputCertainty::AfterInputUnknown
        } else {
            InputCertainty::Cancelled
        },
        "custom_action_cancelled",
        "program is stopping",
    )
}

fn observation_failure(
    input_performed: bool,
    stage: &'static str,
    error: anyhow::Error,
) -> UiRoutineFailure {
    observation_reason(input_performed, stage, format!("{error:#}"))
}

fn observation_reason(
    input_performed: bool,
    stage: &'static str,
    reason: impl Into<String>,
) -> UiRoutineFailure {
    UiRoutineFailure::new(
        if input_performed {
            InputCertainty::AfterInputUnknown
        } else {
            InputCertainty::BeforeInput
        },
        stage,
        reason,
    )
}

fn input_failure(stage: &'static str, error: anyhow::Error) -> UiRoutineFailure {
    UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        stage,
        format!("{error:#}"),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;

    use anyhow::Result;
    use enigo::Key;
    use image::DynamicImage;

    use super::*;
    use crate::config::AppConfig;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::ui::{UiDevice, UiRuntime};

    struct EmptyOcr;

    impl OcrDevice for EmptyOcr {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Ok(Vec::new())
        }
    }

    struct RecordingDevice {
        keys: Arc<Mutex<Vec<Key>>>,
    }

    impl UiDevice for RecordingDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(1920, 1080))
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            self.keys.lock().unwrap().push(key);
            Ok(())
        }
    }

    struct MarkerRoutine;

    impl sealed::UiRoutineSealed for MarkerRoutine {}

    impl UiRoutine for MarkerRoutine {
        type Output = Result<(), UiRoutineFailure>;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            context
                .device()
                .press_key(Key::Unicode('c'))
                .map_err(|error| {
                    UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "marker",
                        format!("{error:#}"),
                    )
                })
        }
    }

    #[test]
    fn action_plan_finishes_before_the_next_ui_job_can_run() {
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = UiRuntime::start(RecordingDevice { keys: keys.clone() }, 4).unwrap();
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let config = AppConfig::load(Path::new("config.yaml")).unwrap();
        let action_ui = CustomActionUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            Arc::new(AtomicBool::new(true)),
            config.screen.expected_width,
            config.screen.expected_height,
            FriendDeliveryRoutineConfig::from_app(&config),
        );

        let action = action_ui
            .submit(CustomActionPlan::new(
                "atomic",
                vec![
                    WorkflowOperation::PressKey {
                        key: "a".to_string(),
                    },
                    WorkflowOperation::Wait { duration_ms: 1 },
                    WorkflowOperation::PressKey {
                        key: "b".to_string(),
                    },
                ],
            ))
            .unwrap();
        let marker = ui_runtime.handle().submit(MarkerRoutine).unwrap();

        let outcome = action.wait().unwrap();
        marker.wait().unwrap().unwrap();

        assert!(outcome.is_complete());
        assert_eq!(outcome.completed(), 3);
        assert_eq!(
            *keys.lock().unwrap(),
            vec![Key::Unicode('a'), Key::Unicode('b'), Key::Unicode('c')]
        );
        ocr_runtime.shutdown().unwrap();
        ui_runtime.shutdown().unwrap();
    }
}
