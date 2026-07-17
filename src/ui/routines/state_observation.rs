use std::time::{Duration, Instant};

use image::DynamicImage;
use image::imageops::FilterType;

use crate::runtime::ui::{
    InputCertainty, UiRoutineContext, UiRoutineFailure, UiStateKind, UiStateObservation,
};

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
    if image.width() == config.canvas_width && image.height() == config.canvas_height {
        Ok(image)
    } else {
        Ok(image.resize_exact(
            config.canvas_width,
            config.canvas_height,
            FilterType::Triangle,
        ))
    }
}

pub(super) fn wait_for_stable_ui_kind(
    context: &mut UiRoutineContext<'_>,
    config: UiStateObservationConfig,
    expected: Option<UiStateKind>,
    timeout_ms: u64,
    stage: &'static str,
    certainty: InputCertainty,
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
                if kind == UiStateKind::Unknown || expected.is_some_and(|expected| expected != kind)
                {
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
                    if state.stable_kind() == Some(kind) && fresh_samples >= state.required_count()
                    {
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
        std::thread::sleep(Duration::from_millis(config.poll_ms));
    }
}
