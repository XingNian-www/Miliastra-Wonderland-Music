use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, UiResidencyOutcome, UiResidencyTarget, after_input_failure,
    before_input_failure, capture_normalized, restore_residency, sleep_ms,
};
use crate::observation::chat::{
    FriendUnreadLayout, SECONDARY_TITLE_RECT, SecondaryChatIdentity, UnreadFriendHit,
    classify_title, find_unread_friend_hits, latest_incoming_bubble_rect,
    latest_incoming_fingerprint, unread_hit_still_visible,
};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle, merge_ocr_lines};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    UiSubmitError, sealed,
};
use crate::ui::geometry::{Rect, crop_canvas};

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
    Stale,
    NoMessage,
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
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: SecondaryUnreadRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
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
pub(crate) struct SecondaryUnreadRoutineConfig {
    residency: FriendDeliveryRoutineConfig,
    unread_layout: FriendUnreadLayout,
    same_line_y_tolerance: i32,
    bubble_poll_ms: Arc<RwLock<u64>>,
}

impl SecondaryUnreadRoutineConfig {
    pub(crate) fn resolve(
        residency: FriendDeliveryRoutineConfig,
        same_line_y_tolerance: i32,
        change_debounce_ms: Arc<RwLock<u64>>,
    ) -> Self {
        let unread_layout = FriendUnreadLayout::resolve(
            residency.screen.expected_width,
            residency.screen.expected_height,
            residency.friend_list_region(),
        );
        Self {
            residency,
            unread_layout,
            same_line_y_tolerance,
            bubble_poll_ms: change_debounce_ms,
        }
    }
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

    let Some(hit) = reacquire_unread_hit(context, config)? else {
        log::debug!(
            "二级好友未读任务执行前红点已失效: queued_y={}",
            request.hit.row_click.y
        );
        return Ok(SecondaryUnreadEffect::Stale);
    };
    if hit.row_click.y != request.hit.row_click.y {
        log::debug!(
            "二级好友未读任务已重新定位红点: queued_y={} current_y={}",
            request.hit.row_click.y,
            hit.row_click.y
        );
    }

    let mut opened = false;
    for _ in 0..2 {
        context
            .device()
            .click_point(hit.row_click.x, hit.row_click.y)
            .map_err(|error| after_input_failure("open_secondary_unread", error))?;
        sleep_ms(config.residency.click_ms);
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_secondary_unread_opened",
            InputCertainty::AfterInputUnknown,
        )?;
        if !unread_hit_still_visible(&image, hit, &config.unread_layout) {
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

    let deadline = Instant::now() + Duration::from_millis(config.residency.timeout_ms());
    loop {
        let (image, captured_at) = wait_bubble_stable(context, config, deadline)?;
        let title = merged_text(ocr, &image, SECONDARY_TITLE_RECT, config)?;
        let friend_name = match classify_title(&title) {
            SecondaryChatIdentity::Friend(name) => name,
            SecondaryChatIdentity::Unknown => "二级好友".to_string(),
            SecondaryChatIdentity::CurrentHall
            | SecondaryChatIdentity::PublicChannel
            | SecondaryChatIdentity::StrangerMessages => {
                if Instant::now() >= deadline {
                    return Err(unread_message_failure(
                        "conversation title did not become a friend before timeout",
                    ));
                }
                sleep_ms(bubble_poll_ms(config));
                continue;
            }
        };
        let rect = latest_incoming_bubble_rect(&image).ok_or_else(|| {
            unread_message_failure("stable incoming bubble disappeared before OCR")
        })?;
        let text = merged_text(ocr, &image, rect, config)?;
        if unread_text_is_usable(&text) {
            return Ok(SecondaryUnreadEffect::Message {
                captured_at,
                friend_name,
                text,
            });
        }
        if Instant::now() >= deadline {
            return Err(unread_message_failure(
                "incoming bubble OCR remained empty until timeout",
            ));
        }
        sleep_ms(bubble_poll_ms(config));
    }
}

fn reacquire_unread_hit(
    context: &mut UiRoutineContext<'_>,
    config: &SecondaryUnreadRoutineConfig,
) -> Result<Option<UnreadFriendHit>, UiRoutineFailure> {
    let first = capture_normalized(
        context,
        &config.residency,
        "reacquire_secondary_unread",
        InputCertainty::AfterInputUnknown,
    )?;
    let Some(candidate) = find_unread_friend_hits(&first, &config.unread_layout)
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    sleep_ms(bubble_poll_ms(config));
    let confirmed = capture_normalized(
        context,
        &config.residency,
        "confirm_secondary_unread_hit",
        InputCertainty::AfterInputUnknown,
    )?;
    Ok(find_unread_friend_hits(&confirmed, &config.unread_layout)
        .into_iter()
        .find(|hit| same_unread_hit(candidate, *hit)))
}

fn same_unread_hit(first: UnreadFriendHit, second: UnreadFriendHit) -> bool {
    first.indicator.x.abs_diff(second.indicator.x) <= 4
        && first.indicator.y.abs_diff(second.indicator.y) <= 4
}

fn wait_bubble_stable(
    context: &mut UiRoutineContext<'_>,
    config: &SecondaryUnreadRoutineConfig,
    deadline: Instant,
) -> Result<(image::DynamicImage, Instant), UiRoutineFailure> {
    let mut previous = None;
    let mut stable_since = None;
    let poll_ms = bubble_poll_ms(config);
    let required_stable = Duration::from_millis(bounded_bubble_stability_ms(
        config.residency.friend_step_ms(),
        config.residency.timeout_ms(),
        poll_ms,
    ));
    while Instant::now() < deadline {
        let image = capture_normalized(
            context,
            &config.residency,
            "confirm_secondary_bubble",
            InputCertainty::AfterInputUnknown,
        )?;
        let current = latest_incoming_fingerprint(&image)
            .map_err(|error| after_input_failure("confirm_secondary_bubble", error))?;
        let captured_at = Instant::now();
        let unchanged = stable_bubble_sample(previous.as_ref(), current.as_ref());
        if stable_window_elapsed(unchanged, stable_since, captured_at, required_stable) {
            return Ok((image, captured_at));
        }
        if !unchanged {
            stable_since = current.as_ref().map(|_| captured_at);
        }
        previous = current;
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        sleep_ms(next_bubble_sample_ms(
            poll_ms,
            stable_since,
            now,
            required_stable,
            deadline,
        ));
    }
    Err(unread_message_failure(
        "incoming bubble did not appear and stabilize before timeout",
    ))
}

fn bounded_bubble_stability_ms(step_ms: u64, timeout_ms: u64, poll_ms: u64) -> u64 {
    step_ms.min(timeout_ms.saturating_sub(poll_ms).max(1))
}

fn next_bubble_sample_ms(
    poll_ms: u64,
    stable_since: Option<Instant>,
    now: Instant,
    required_stable: Duration,
    deadline: Instant,
) -> u64 {
    let deadline_ms = duration_ceil_ms(deadline.saturating_duration_since(now));
    let stability_ms = stable_since
        .map(|since| {
            duration_ceil_ms(required_stable.saturating_sub(now.saturating_duration_since(since)))
        })
        .unwrap_or(u64::MAX);
    poll_ms.min(deadline_ms).min(stability_ms).max(1)
}

fn duration_ceil_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis())
        .unwrap_or(u64::MAX)
        .saturating_add(u64::from(
            !duration.subsec_nanos().is_multiple_of(1_000_000),
        ))
}

fn bubble_poll_ms(config: &SecondaryUnreadRoutineConfig) -> u64 {
    config
        .bubble_poll_ms
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clamp(100, 200)
}

fn unread_message_failure(detail: impl Into<String>) -> UiRoutineFailure {
    UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "wait_secondary_unread_message",
        detail,
    )
}

fn stable_bubble_sample(
    previous: Option<&crate::ui::change_detection::ChangeFingerprint>,
    current: Option<&crate::ui::change_detection::ChangeFingerprint>,
) -> bool {
    previous.is_some() && current.is_some() && !optional_fingerprint_changed(previous, current)
}

fn stable_window_elapsed(
    unchanged: bool,
    stable_since: Option<Instant>,
    now: Instant,
    required: Duration,
) -> bool {
    unchanged && stable_since.is_some_and(|since| now.saturating_duration_since(since) >= required)
}

fn unread_text_is_usable(text: &str) -> bool {
    !text.trim().is_empty()
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
        (None, None) => true,
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
        .map_err(|error| after_input_failure("crop_secondary_unread", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::ChatObservation)
        .map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "ocr_secondary_unread",
                format!("{error:#}"),
            )
        })?;
    Ok(merge_ocr_lines(lines, config.same_line_y_tolerance))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::change_detection::ChangeFingerprint;
    use crate::ui::geometry::Point;

    fn fingerprint(value: u8) -> ChangeFingerprint {
        ChangeFingerprint {
            pixels: vec![value; 16],
            width: 4,
            height: 4,
        }
    }

    #[test]
    fn empty_bubble_frames_are_never_stable() {
        assert!(!stable_bubble_sample(None, None));
        assert!(optional_fingerprint_changed(None, None));
    }

    #[test]
    fn bubble_requires_two_present_matching_samples() {
        let first = fingerprint(20);
        let same = fingerprint(20);
        let changed = fingerprint(220);

        assert!(!stable_bubble_sample(None, Some(&first)));
        assert!(stable_bubble_sample(Some(&first), Some(&same)));
        assert!(!stable_bubble_sample(Some(&first), Some(&changed)));
    }

    #[test]
    fn bubble_must_remain_unchanged_for_the_settle_window() {
        let started = Instant::now();
        let required = Duration::from_millis(800);

        assert!(!stable_window_elapsed(
            true,
            Some(started),
            started + Duration::from_millis(799),
            required,
        ));
        assert!(stable_window_elapsed(
            true,
            Some(started),
            started + required,
            required,
        ));
        assert!(!stable_window_elapsed(
            false,
            Some(started),
            started + required,
            required,
        ));
    }

    #[test]
    fn bubble_stability_window_leaves_room_for_a_confirmation_sample() {
        assert_eq!(bounded_bubble_stability_ms(800, 5_000, 200), 800);
        assert_eq!(bounded_bubble_stability_ms(5_000, 5_000, 200), 4_800);
        assert_eq!(bounded_bubble_stability_ms(8_000, 5_000, 200), 4_800);

        let started = Instant::now();
        assert_eq!(
            next_bubble_sample_ms(
                200,
                Some(started),
                started + Duration::from_millis(799),
                Duration::from_millis(800),
                started + Duration::from_secs(5),
            ),
            1
        );
    }

    #[test]
    fn unread_message_text_must_not_be_blank() {
        assert!(!unread_text_is_usable("  \r\n "));
        assert!(unread_text_is_usable("@帮助"));
    }

    #[test]
    fn fresh_unread_hit_confirmation_allows_small_detection_drift() {
        let first = UnreadFriendHit {
            indicator: Point::new(64, 310),
            row_click: Point::new(150, 310),
        };
        let close = UnreadFriendHit {
            indicator: Point::new(67, 307),
            row_click: Point::new(150, 307),
        };
        let moved = UnreadFriendHit {
            indicator: Point::new(64, 330),
            row_click: Point::new(150, 330),
        };

        assert!(same_unread_hit(first, close));
        assert!(!same_unread_hit(first, moved));
    }
}
