use std::time::{Duration, Instant};

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, before_input_failure,
    capture_normalized, restore_residency, sleep_ms,
};
use crate::config::AppConfig;
use crate::observation::chat::{
    SECONDARY_TITLE_RECT, SecondaryChatIdentity, UnreadFriendHit, classify_title,
    latest_incoming_bubble_rect, latest_incoming_fingerprint, unread_hit_still_visible,
};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle, merge_ocr_lines};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};
use crate::ui::geometry::{Rect, crop_canvas};

const BUBBLE_STABILITY_TIMEOUT_MS: u64 = 500;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessSecondaryUnread {
    hit: UnreadFriendHit,
    discard_only: bool,
}

impl ProcessSecondaryUnread {
    pub(crate) const fn new(hit: UnreadFriendHit, discard_only: bool) -> Self {
        Self { hit, discard_only }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum SecondaryUnreadEffect {
    Message {
        captured_at: Instant,
        friend_name: String,
        text: String,
    },
    Discarded,
    NoMessage,
    SkippedNonFriend,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug)]
pub(crate) struct ProcessSecondaryUnreadOutcome {
    effect: SecondaryUnreadEffect,
    residency: UiResidencyOutcome,
}

impl ProcessSecondaryUnreadOutcome {
    pub(crate) fn effect(&self) -> &SecondaryUnreadEffect {
        &self.effect
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone)]
pub(crate) struct SecondaryUnreadUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: SecondaryUnreadRoutineConfig,
}

impl SecondaryUnreadUi {
    pub(crate) fn new(runtime: UiRuntimeHandle, ocr: OcrRuntimeHandle, config: &AppConfig) -> Self {
        Self {
            runtime,
            ocr,
            config: SecondaryUnreadRoutineConfig {
                residency: FriendDeliveryRoutineConfig::from_app(config),
                same_line_y_tolerance: config.ocr.same_line_y_tolerance,
                bubble_poll_ms: config.timing.chat_scan.change_debounce_ms.clamp(100, 200),
            },
        }
    }

    pub(crate) fn submit(
        &self,
        request: ProcessSecondaryUnread,
    ) -> Result<UiOperation<ProcessSecondaryUnreadOutcome>, UiSubmitError> {
        self.runtime.submit(ProcessSecondaryUnreadRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }
}

#[derive(Clone)]
struct SecondaryUnreadRoutineConfig {
    residency: FriendDeliveryRoutineConfig,
    same_line_y_tolerance: i32,
    bubble_poll_ms: u64,
}

struct ProcessSecondaryUnreadRoutine {
    request: ProcessSecondaryUnread,
    ocr: OcrRuntimeHandle,
    config: SecondaryUnreadRoutineConfig,
}

impl sealed::UiRoutineSealed for ProcessSecondaryUnreadRoutine {}

impl UiRoutine for ProcessSecondaryUnreadRoutine {
    type Output = ProcessSecondaryUnreadOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let effect = match process_unread(context, &self.request, &self.ocr, &self.config) {
            Ok(effect) => effect,
            Err(failure) => SecondaryUnreadEffect::Failed(failure),
        };
        let residency = match restore_residency(
            context,
            &self.ocr,
            &self.config.residency,
            UiResidencyTarget::SecondaryCurrentHall,
        ) {
            Ok(()) => UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall),
            Err(failure) => UiResidencyOutcome::Failed(failure),
        };
        ProcessSecondaryUnreadOutcome { effect, residency }
    }
}

fn process_unread(
    context: &mut UiRoutineContext<'_>,
    request: &ProcessSecondaryUnread,
    ocr: &OcrRuntimeHandle,
    config: &SecondaryUnreadRoutineConfig,
) -> Result<SecondaryUnreadEffect, UiRoutineFailure> {
    context
        .device()
        .ensure_ready(config.residency.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_secondary_unread", error))?;
    restore_residency(
        context,
        ocr,
        &config.residency,
        UiResidencyTarget::SecondaryCurrentHall,
    )?;

    let mut opened = false;
    for _ in 0..2 {
        context
            .device()
            .click_point(request.hit.row_click.x, request.hit.row_click.y)
            .map_err(|error| before_input_failure("open_secondary_unread", error))?;
        sleep_ms(config.residency.click_ms);
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_secondary_unread_opened",
        )?;
        if !unread_hit_still_visible(&image, request.hit) {
            opened = true;
            break;
        }
    }
    if !opened {
        return Ok(SecondaryUnreadEffect::NoMessage);
    }
    if request.discard_only {
        return Ok(SecondaryUnreadEffect::Discarded);
    }

    let (image, captured_at) = wait_bubble_stable(context, config)?;
    let title = merged_text(ocr, &image, SECONDARY_TITLE_RECT, config)?;
    let friend_name = match classify_title(&title) {
        SecondaryChatIdentity::Friend(name) => name,
        SecondaryChatIdentity::Unknown => "二级好友".to_string(),
        SecondaryChatIdentity::CurrentHall
        | SecondaryChatIdentity::PublicChannel
        | SecondaryChatIdentity::StrangerMessages => {
            return Ok(SecondaryUnreadEffect::SkippedNonFriend);
        }
    };
    let Some(rect) = latest_incoming_bubble_rect(&image) else {
        return Ok(SecondaryUnreadEffect::NoMessage);
    };
    let text = merged_text(ocr, &image, rect, config)?;
    Ok(SecondaryUnreadEffect::Message {
        captured_at,
        friend_name,
        text,
    })
}

fn wait_bubble_stable(
    context: &mut UiRoutineContext<'_>,
    config: &SecondaryUnreadRoutineConfig,
) -> Result<(image::DynamicImage, Instant), UiRoutineFailure> {
    let first = capture_normalized(context, &config.residency, "observe_secondary_bubble")?;
    let mut previous = latest_incoming_fingerprint(&first)
        .map_err(|error| before_input_failure("observe_secondary_bubble", error))?;
    let mut latest = first;
    let mut captured_at = Instant::now();
    let deadline = Instant::now() + Duration::from_millis(BUBBLE_STABILITY_TIMEOUT_MS);
    while Instant::now() < deadline {
        sleep_ms(config.bubble_poll_ms);
        let image = capture_normalized(context, &config.residency, "confirm_secondary_bubble")?;
        let current = latest_incoming_fingerprint(&image)
            .map_err(|error| before_input_failure("confirm_secondary_bubble", error))?;
        captured_at = Instant::now();
        if !optional_fingerprint_changed(previous.as_ref(), current.as_ref()) {
            return Ok((image, captured_at));
        }
        previous = current;
        latest = image;
    }
    Ok((latest, captured_at))
}

fn optional_fingerprint_changed(
    previous: Option<&crate::ui::change_detection::ChangeFingerprint>,
    current: Option<&crate::ui::change_detection::ChangeFingerprint>,
) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => {
            let stats = crate::ui::change_detection::change_stats(previous, current);
            stats.mean_abs_diff >= 0.8 || stats.changed_ratio >= 0.01
        }
        (None, None) => false,
        _ => true,
    }
}

fn merged_text(
    ocr: &OcrRuntimeHandle,
    image: &image::DynamicImage,
    region: Rect,
    config: &SecondaryUnreadRoutineConfig,
) -> Result<String, UiRoutineFailure> {
    let crop = crop_canvas(image, region)
        .map_err(|error| before_input_failure("crop_secondary_unread", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::ChatObservation)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "ocr_secondary_unread",
                format!("{error:#}"),
            )
        })?;
    Ok(merge_ocr_lines(lines, config.same_line_y_tolerance))
}
