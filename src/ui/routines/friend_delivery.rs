use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use enigo::Key;
use image::DynamicImage;
use image::imageops::FilterType;

use crate::config::{AppConfig, PointConfig, ScreenConfig};
use crate::observation::chat::{SECONDARY_TITLE_RECT, SecondaryChatIdentity, classify_title};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle, merge_ocr_lines};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure,
    UiRoutineProgressStage, UiRuntimeHandle, UiSubmitError, sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
use crate::ui::geometry::{Point, Rect, crop_canvas};
use crate::ui::locator::secondary_hall_search_rect;
use crate::ui::state::{ResolvedUiTemplateArgs, UiTemplateArgs, detect_ui_state};
use crate::ui::template::best_template_hit;

const FRIEND_ROW_Y_TOLERANCE: i32 = 6;
const MAX_UPWARD_SCROLLS: usize = 6;
const MAX_DOWNWARD_SCROLLS: usize = MAX_UPWARD_SCROLLS * 2;
const FRIEND_SCROLL_LENGTH: i32 = 8;
const PRIMARY_STABILITY_POLL_MS: u64 = 100;
const PRIMARY_STABILITY_TIMEOUT_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiResidencyTarget {
    Primary,
    SecondaryCurrentHall,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FriendDelivery {
    recipient: String,
    messages: Vec<String>,
}

impl FriendDelivery {
    pub(crate) fn new<R, I, M>(recipient: R, messages: I) -> Self
    where
        R: Into<String>,
        I: IntoIterator<Item = M>,
        M: Into<String>,
    {
        Self {
            recipient: recipient.into(),
            messages: messages.into_iter().map(Into::into).collect(),
        }
    }

    pub(crate) fn recipient(&self) -> &str {
        &self.recipient
    }

    pub(crate) fn messages(&self) -> &[String] {
        &self.messages
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SendFriendDeliveries {
    deliveries: Vec<FriendDelivery>,
    residency: UiResidencyTarget,
}

impl SendFriendDeliveries {
    pub(crate) fn new(deliveries: Vec<FriendDelivery>, residency: UiResidencyTarget) -> Self {
        Self {
            deliveries,
            residency,
        }
    }

    pub(crate) fn deliveries(&self) -> &[FriendDelivery] {
        &self.deliveries
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FriendDeliveryMessageStatus {
    NotAttempted,
    Sent,
    ResultUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FriendDeliveryOutcome {
    message_statuses: Vec<FriendDeliveryMessageStatus>,
    failure: Option<UiRoutineFailure>,
}

impl FriendDeliveryOutcome {
    pub(crate) fn message_statuses(&self) -> &[FriendDeliveryMessageStatus] {
        &self.message_statuses
    }

    pub(crate) fn failure(&self) -> Option<&UiRoutineFailure> {
        self.failure.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UiResidencyOutcome {
    Confirmed(UiResidencyTarget),
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SendFriendDeliveriesOutcome {
    deliveries: Vec<FriendDeliveryOutcome>,
    residency: UiResidencyOutcome,
}

impl SendFriendDeliveriesOutcome {
    pub(crate) fn deliveries(&self) -> &[FriendDeliveryOutcome] {
        &self.deliveries
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.deliveries.iter().all(|delivery| {
            delivery.failure.is_none()
                && delivery
                    .message_statuses
                    .iter()
                    .all(|status| *status == FriendDeliveryMessageStatus::Sent)
        }) && matches!(self.residency, UiResidencyOutcome::Confirmed(_))
    }

    pub(crate) fn safe_retry_request(
        &self,
        original: &SendFriendDeliveries,
    ) -> Option<SendFriendDeliveries> {
        if self.deliveries.len() != original.deliveries.len() {
            return None;
        }
        if original
            .deliveries
            .iter()
            .zip(&self.deliveries)
            .any(|(requested, outcome)| requested.messages.len() != outcome.message_statuses.len())
        {
            return None;
        }
        let deliveries = original
            .deliveries
            .iter()
            .zip(&self.deliveries)
            .filter_map(|(requested, outcome)| {
                let messages = requested
                    .messages
                    .iter()
                    .zip(&outcome.message_statuses)
                    .filter(|(_, status)| **status == FriendDeliveryMessageStatus::NotAttempted)
                    .map(|(message, _)| message.clone())
                    .collect::<Vec<_>>();
                (!messages.is_empty()).then_some(FriendDelivery {
                    recipient: requested.recipient.clone(),
                    messages,
                })
            })
            .collect::<Vec<_>>();
        (!deliveries.is_empty()).then_some(SendFriendDeliveries {
            deliveries,
            residency: original.residency,
        })
    }
}

#[derive(Clone)]
pub(crate) struct FriendDeliveryUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EstablishResidency {
    target: UiResidencyTarget,
}

impl EstablishResidency {
    pub(crate) const fn new(target: UiResidencyTarget) -> Self {
        Self { target }
    }
}

#[derive(Clone)]
pub(crate) struct ResidencyUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

impl ResidencyUi {
    pub(crate) fn new(runtime: UiRuntimeHandle, ocr: OcrRuntimeHandle, config: &AppConfig) -> Self {
        Self {
            runtime,
            ocr,
            config: FriendDeliveryRoutineConfig::from_app(config),
        }
    }

    pub(crate) fn submit(
        &self,
        request: EstablishResidency,
    ) -> std::result::Result<UiOperation<UiResidencyOutcome>, UiSubmitError> {
        self.runtime.submit(EstablishResidencyRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }
}

struct EstablishResidencyRoutine {
    request: EstablishResidency,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

impl sealed::UiRoutineSealed for EstablishResidencyRoutine {}

impl UiRoutine for EstablishResidencyRoutine {
    type Output = UiResidencyOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context.publish_progress(UiRoutineProgressStage::NormalizingStart);
        if let Err(error) = context.device().ensure_ready(self.config.after_activate_ms) {
            return UiResidencyOutcome::Failed(before_input_failure("prepare_residency", error));
        }
        match restore_residency(context, &self.ocr, &self.config, self.request.target) {
            Ok(()) => UiResidencyOutcome::Confirmed(self.request.target),
            Err(failure) => UiResidencyOutcome::Failed(failure),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SendHallBatch {
    messages: Vec<String>,
    residency: UiResidencyTarget,
    delay_ms: u64,
}

impl SendHallBatch {
    pub(crate) fn new(
        messages: impl IntoIterator<Item = impl Into<String>>,
        residency: UiResidencyTarget,
        delay_ms: u64,
    ) -> Self {
        Self {
            messages: messages.into_iter().map(Into::into).collect(),
            residency,
            delay_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HallBatchStatus {
    Complete,
    Failed(UiRoutineFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SendHallBatchOutcome {
    sent: usize,
    status: HallBatchStatus,
    residency: UiResidencyOutcome,
}

impl SendHallBatchOutcome {
    pub(crate) fn sent(&self) -> usize {
        self.sent
    }

    pub(crate) fn status(&self) -> &HallBatchStatus {
        &self.status
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }
}

#[derive(Clone)]
pub(crate) struct HallBatchUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

impl HallBatchUi {
    pub(crate) fn new(runtime: UiRuntimeHandle, ocr: OcrRuntimeHandle, config: &AppConfig) -> Self {
        Self {
            runtime,
            ocr,
            config: FriendDeliveryRoutineConfig::from_app(config),
        }
    }

    pub(crate) fn submit(
        &self,
        request: SendHallBatch,
    ) -> std::result::Result<UiOperation<SendHallBatchOutcome>, UiSubmitError> {
        self.runtime.submit(SendHallBatchRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }
}

struct SendHallBatchRoutine {
    request: SendHallBatch,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

impl sealed::UiRoutineSealed for SendHallBatchRoutine {}

impl UiRoutine for SendHallBatchRoutine {
    type Output = SendHallBatchOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        execute_hall_batch(context, self.request, &self.ocr, &self.config)
    }
}

impl FriendDeliveryUi {
    pub(crate) fn new(runtime: UiRuntimeHandle, ocr: OcrRuntimeHandle, config: &AppConfig) -> Self {
        Self {
            runtime,
            ocr,
            config: FriendDeliveryRoutineConfig::from_app(config),
        }
    }

    pub(crate) fn submit(
        &self,
        request: SendFriendDeliveries,
    ) -> std::result::Result<UiOperation<SendFriendDeliveriesOutcome>, UiSubmitError> {
        self.runtime.submit(SendFriendDeliveriesRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
        })
    }
}

#[derive(Clone)]
pub(super) struct FriendDeliveryRoutineConfig {
    pub(super) screen: ScreenConfig,
    templates: ResolvedUiTemplateArgs,
    canvas_width: u32,
    canvas_height: u32,
    friend_list_region: Rect,
    friend_chat_region: Rect,
    secondary_hall_search_region: Rect,
    chat_click: PointConfig,
    send_enabled: bool,
    pub(super) after_activate_ms: u64,
    open_chat_ms: u64,
    pub(super) click_ms: u64,
    text_ms: u64,
    send_ms: u64,
    friend_step_ms: u64,
    timeout_ms: u64,
    pub(super) poll_ms: u64,
    pub(super) stable_count: u32,
    auto_retry_count: u32,
    stable_mean_threshold: f32,
    stable_changed_ratio_threshold: f32,
    secondary_hall_template: PathBuf,
    template_threshold: f32,
}

impl FriendDeliveryRoutineConfig {
    pub(super) fn from_app(config: &AppConfig) -> Self {
        let hall_anchor: Rect = config.screen.secondary_hall_rect.into();
        let friend_list_region: Rect = config.invite.friend_list_region.into();
        Self {
            screen: config.screen.clone(),
            templates: UiTemplateArgs::default().resolve(config),
            canvas_width: config.screen.expected_width,
            canvas_height: config.screen.expected_height,
            friend_list_region,
            friend_chat_region: config.invite.friend_chat_region.into(),
            secondary_hall_search_region: secondary_hall_search_rect(
                hall_anchor,
                friend_list_region,
            ),
            chat_click: config.output.chat_click_2,
            send_enabled: config.output.send_enabled,
            after_activate_ms: config.timing.input.after_activate_ms,
            open_chat_ms: config.timing.input.open_chat_ms,
            click_ms: config.timing.input.click_ms,
            text_ms: config.timing.input.text_ms,
            send_ms: config.timing.input.send_ms,
            friend_step_ms: config.timing.invite.step_ms,
            timeout_ms: config.timing.workflow.default_timeout_ms,
            poll_ms: config.timing.workflow.default_poll_ms.max(10),
            stable_count: config.resolve_stability_count(config.invite.friend_name_stable_count),
            auto_retry_count: config.friend_delivery.auto_retry_count,
            stable_mean_threshold: config.ocr.change_mean_threshold,
            stable_changed_ratio_threshold: config.ocr.change_pixel_threshold,
            secondary_hall_template: config.templates.secondary_hall.clone(),
            template_threshold: config.templates.marker_threshold,
        }
    }
}

struct SendFriendDeliveriesRoutine {
    request: SendFriendDeliveries,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

impl sealed::UiRoutineSealed for SendFriendDeliveriesRoutine {}

impl UiRoutine for SendFriendDeliveriesRoutine {
    type Output = SendFriendDeliveriesOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        execute_friend_deliveries(context, self.request, &self.ocr, &self.config)
    }
}

struct DeliveryAttemptFailure {
    failure: UiRoutineFailure,
    unknown_message_index: Option<usize>,
}

enum PageSearch {
    Found(Point),
    Missing,
}

#[derive(Clone, Copy)]
enum TextMatchMode {
    Exact,
    ContainsCompleteTarget,
}

fn execute_friend_deliveries(
    context: &mut UiRoutineContext<'_>,
    request: SendFriendDeliveries,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
) -> SendFriendDeliveriesOutcome {
    let recipient_count = request.deliveries.len();
    let mut outcomes = request
        .deliveries
        .iter()
        .map(|delivery| FriendDeliveryOutcome {
            message_statuses: vec![
                FriendDeliveryMessageStatus::NotAttempted;
                delivery.messages.len()
            ],
            failure: None,
        })
        .collect::<Vec<_>>();

    let start = ensure_secondary_chat(context, config);
    if let Err(failure) = start {
        if let Some(first) = outcomes.first_mut() {
            first.failure = Some(failure);
        }
    } else {
        for (recipient_index, delivery) in request.deliveries.iter().enumerate() {
            let mut retry = 0_u32;
            loop {
                context.publish_progress(UiRoutineProgressStage::LocatingFriend {
                    recipient_index: recipient_index + 1,
                    recipient_count,
                });
                match deliver_remaining_messages(
                    context,
                    ocr,
                    config,
                    delivery,
                    &mut outcomes[recipient_index].message_statuses,
                    recipient_index,
                    recipient_count,
                ) {
                    Ok(()) => break,
                    Err(attempt)
                        if retry < config.auto_retry_count
                            && retryable_certainty(attempt.failure.certainty()) =>
                    {
                        retry = retry.saturating_add(1);
                        log::warn!(
                            "好友投递确认未发送，执行自动重试 {}/{} stage={}",
                            retry,
                            config.auto_retry_count,
                            attempt.failure.stage()
                        );
                    }
                    Err(attempt) => {
                        if let Some(index) = attempt.unknown_message_index {
                            outcomes[recipient_index].message_statuses[index] =
                                FriendDeliveryMessageStatus::ResultUnknown;
                        }
                        outcomes[recipient_index].failure = Some(attempt.failure);
                        break;
                    }
                }
            }
            if outcomes[recipient_index].failure.is_some() {
                break;
            }
        }
    }

    context.publish_progress(UiRoutineProgressStage::RecoveringResidency);
    let residency = match restore_residency(context, ocr, config, request.residency) {
        Ok(()) => UiResidencyOutcome::Confirmed(request.residency),
        Err(failure) => UiResidencyOutcome::Failed(failure),
    };
    SendFriendDeliveriesOutcome {
        deliveries: outcomes,
        residency,
    }
}

fn execute_hall_batch(
    context: &mut UiRoutineContext<'_>,
    request: SendHallBatch,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
) -> SendHallBatchOutcome {
    let mut sent = 0_usize;
    let mut status = HallBatchStatus::Complete;
    let mut primary_chat_opened = false;

    if !config.send_enabled || request.messages.is_empty() {
        sent = request.messages.len();
    } else if let Err(failure) = normalize_hall_batch_start(
        context,
        ocr,
        config,
        request.residency,
        &mut primary_chat_opened,
    ) {
        status = HallBatchStatus::Failed(failure);
    } else {
        for (index, message) in request.messages.iter().enumerate() {
            if index > 0 {
                sleep_ms(request.delay_ms);
            }
            match send_current_chat_message(context, config, message) {
                Ok(()) => sent += 1,
                Err(failure) => {
                    status = HallBatchStatus::Failed(failure);
                    break;
                }
            }
        }
    }

    let residency = match finish_hall_batch_residency(
        context,
        ocr,
        config,
        request.residency,
        primary_chat_opened,
    ) {
        Ok(()) => UiResidencyOutcome::Confirmed(request.residency),
        Err(failure) => UiResidencyOutcome::Failed(failure),
    };
    SendHallBatchOutcome {
        sent,
        status,
        residency,
    }
}

fn normalize_hall_batch_start(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    target: UiResidencyTarget,
    primary_chat_opened: &mut bool,
) -> std::result::Result<(), UiRoutineFailure> {
    context
        .device()
        .ensure_ready(config.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_hall_batch", error))?;
    match target {
        UiResidencyTarget::Primary => {
            restore_primary(context, config)?;
            context
                .device()
                .press_key(Key::Return)
                .map_err(|error| before_input_failure("open_primary_chat", error))?;
            *primary_chat_opened = true;
            sleep_ms(config.open_chat_ms);
            restore_secondary_hall(context, ocr, config)
        }
        UiResidencyTarget::SecondaryCurrentHall => restore_secondary_hall(context, ocr, config),
    }
}

fn finish_hall_batch_residency(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    target: UiResidencyTarget,
    primary_chat_opened: bool,
) -> std::result::Result<(), UiRoutineFailure> {
    if target == UiResidencyTarget::Primary && primary_chat_opened {
        context
            .device()
            .press_key(Key::Escape)
            .map_err(|error| before_input_failure("close_primary_chat", error))?;
        sleep_ms(config.click_ms);
    }
    restore_residency(context, ocr, config, target)
}

fn retryable_certainty(certainty: InputCertainty) -> bool {
    matches!(
        certainty,
        InputCertainty::BeforeInput | InputCertainty::ConfirmedFailure
    )
}

fn deliver_remaining_messages(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    delivery: &FriendDelivery,
    statuses: &mut [FriendDeliveryMessageStatus],
    recipient_index: usize,
    recipient_count: usize,
) -> std::result::Result<(), DeliveryAttemptFailure> {
    open_friend_conversation(context, ocr, config, &delivery.recipient)
        .map_err(safe_attempt_failure)?;

    for (message_index, (message, status)) in delivery
        .messages
        .iter()
        .zip(statuses.iter_mut())
        .enumerate()
    {
        if *status != FriendDeliveryMessageStatus::NotAttempted {
            continue;
        }
        context.publish_progress(UiRoutineProgressStage::SendingFriendMessage {
            recipient_index: recipient_index + 1,
            recipient_count,
            message_index: message_index + 1,
            message_count: delivery.messages.len(),
        });
        send_current_chat_message(context, config, message).map_err(|failure| {
            let unknown_message_index =
                (failure.certainty() == InputCertainty::AfterInputUnknown).then_some(message_index);
            DeliveryAttemptFailure {
                failure,
                unknown_message_index,
            }
        })?;
        *status = FriendDeliveryMessageStatus::Sent;
    }
    Ok(())
}

fn safe_attempt_failure(failure: UiRoutineFailure) -> DeliveryAttemptFailure {
    DeliveryAttemptFailure {
        failure,
        unknown_message_index: None,
    }
}

pub(super) fn open_friend_conversation(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipient: &str,
) -> std::result::Result<(), UiRoutineFailure> {
    ensure_secondary_chat(context, config)?;
    let point = locate_stable_friend_row(context, ocr, config, recipient)?;
    context
        .device()
        .click_point(point.x, point.y)
        .map_err(|error| before_input_failure("select_friend", error))?;
    sleep_ms(config.friend_step_ms);
    confirm_friend_conversation(context, ocr, config, recipient)
}

pub(super) fn send_current_chat_message(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    message: &str,
) -> std::result::Result<(), UiRoutineFailure> {
    if !config.send_enabled {
        return Ok(());
    }
    context
        .device()
        .click_point(config.chat_click.x, config.chat_click.y)
        .map_err(|error| before_input_failure("focus_friend_message", error))?;
    sleep_ms(config.click_ms);
    if let Err(paste_error) = context.device().paste_text(message, config.text_ms)
        && let Err(input_error) = context.device().input_text(message, config.text_ms)
    {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "input_friend_message",
            format!("paste failed: {paste_error:#}; text input failed: {input_error:#}"),
        ));
    }
    context.device().press_key(Key::Return).map_err(|error| {
        UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "send_friend_message",
            format!("{error:#}"),
        )
    })?;
    sleep_ms(config.send_ms);
    Ok(())
}

fn text_contains_complete_target(recognized: &str, target: &str) -> bool {
    !target.is_empty() && (recognized == target || recognized.contains(target))
}

fn ensure_secondary_chat(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    context.publish_progress(UiRoutineProgressStage::NormalizingStart);
    context
        .device()
        .ensure_ready(config.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_friend_delivery", error))?;
    for attempt in 0..2 {
        let image = capture_normalized(context, config, "observe_friend_delivery_start")?;
        let state = detect_ui_state(&image, &config.templates, &config.screen)
            .map_err(|error| before_input_failure("classify_friend_delivery_start", error))?;
        if state.is_secondary() {
            return Ok(());
        }
        if state.is_primary() {
            context
                .device()
                .press_key(Key::Return)
                .map_err(|error| before_input_failure("open_secondary_chat", error))?;
            sleep_ms(config.open_chat_ms);
            let opened = capture_normalized(context, config, "confirm_secondary_chat")?;
            let state = detect_ui_state(&opened, &config.templates, &config.screen)
                .map_err(|error| before_input_failure("confirm_secondary_chat", error))?;
            if state.is_secondary() {
                return Ok(());
            }
        }
        if attempt == 0 {
            sleep_ms(config.poll_ms);
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::BeforeInput,
        "normalize_friend_delivery_start",
        "current UI is not a supported stable primary or secondary state",
    ))
}

fn locate_stable_friend_row(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipient: &str,
) -> std::result::Result<Point, UiRoutineFailure> {
    for (scroll_length, max_scrolls) in [
        (-FRIEND_SCROLL_LENGTH, MAX_UPWARD_SCROLLS),
        (FRIEND_SCROLL_LENGTH, MAX_DOWNWARD_SCROLLS),
    ] {
        let mut pages = Vec::<ChangeFingerprint>::new();
        for scroll_index in 0..=max_scrolls {
            match search_current_friend_page(context, ocr, config, recipient)? {
                PageSearch::Found(point) => return Ok(point),
                PageSearch::Missing => {}
            }
            let image = capture_normalized(context, config, "fingerprint_friend_list")?;
            let fingerprint = rect_chat_change_fingerprint(&image, config.friend_list_region)
                .map_err(|error| before_input_failure("fingerprint_friend_list", error))?;
            if pages
                .iter()
                .any(|page| page_matches(page, &fingerprint, config))
            {
                break;
            }
            pages.push(fingerprint);
            if scroll_index == max_scrolls {
                break;
            }
            let point = config.friend_list_region.center();
            context
                .device()
                .scroll_point(point.x, point.y, scroll_length)
                .map_err(|error| before_input_failure("scroll_friend_list", error))?;
            wait_friend_list_stable(context, config)?;
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "locate_friend",
        "no unique stable friend row was found within the bounded list traversal",
    ))
}

fn search_current_friend_page(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipient: &str,
) -> std::result::Result<PageSearch, UiRoutineFailure> {
    let target = normalize_lock_text(recipient);
    if target.is_empty() {
        return Err(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "validate_friend_name",
            "normalized friend name is empty",
        ));
    }
    let mut stable_y = None;
    let mut streak = 0_u32;
    for sample in 0..config.stable_count {
        let image = capture_normalized(context, config, "capture_friend_list")?;
        let hits = matching_text_rows(
            ocr,
            &image,
            config.friend_list_region,
            &target,
            TextMatchMode::Exact,
        )?;
        match hits.as_slice() {
            [point] => {
                if stable_y
                    .is_some_and(|y: i32| y.abs_diff(point.y) <= FRIEND_ROW_Y_TOLERANCE as u32)
                {
                    streak = streak.saturating_add(1);
                } else {
                    stable_y = Some(point.y);
                    streak = 1;
                }
                if streak >= config.stable_count {
                    return Ok(PageSearch::Found(*point));
                }
            }
            [] => {
                stable_y = None;
                streak = 0;
            }
            _ => {
                return Err(UiRoutineFailure::new(
                    InputCertainty::ConfirmedFailure,
                    "locate_friend",
                    "multiple complete friend-name matches were visible in the strict list region",
                ));
            }
        }
        if sample + 1 < config.stable_count {
            sleep_ms(config.poll_ms);
        }
    }
    Ok(PageSearch::Missing)
}

pub(super) fn locate_stable_exact_text(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    region: Rect,
    expected: &str,
    stage: &'static str,
) -> std::result::Result<Point, UiRoutineFailure> {
    let target = normalize_lock_text(expected);
    if target.is_empty() {
        return Err(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            stage,
            "normalized target text is empty",
        ));
    }
    let mut stable_y = None;
    let mut streak = 0_u32;
    for attempt in 0..confirmation_attempts(config) {
        let image = capture_normalized(context, config, stage)?;
        let hits = matching_text_rows(ocr, &image, region, &target, TextMatchMode::Exact)?;
        match hits.as_slice() {
            [point] => {
                if stable_y
                    .is_some_and(|y: i32| y.abs_diff(point.y) <= FRIEND_ROW_Y_TOLERANCE as u32)
                {
                    streak = streak.saturating_add(1);
                } else {
                    stable_y = Some(point.y);
                    streak = 1;
                }
                if streak >= config.stable_count {
                    return Ok(*point);
                }
            }
            [] => {
                stable_y = None;
                streak = 0;
            }
            _ => {
                return Err(UiRoutineFailure::new(
                    InputCertainty::ConfirmedFailure,
                    stage,
                    "multiple exact text matches were visible in the requested region",
                ));
            }
        }
        if attempt + 1 < confirmation_attempts(config) {
            sleep_ms(config.poll_ms);
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        stage,
        "the exact text did not become unique and row-stable before timeout",
    ))
}

fn confirm_friend_conversation(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipient: &str,
) -> std::result::Result<(), UiRoutineFailure> {
    context.publish_progress(UiRoutineProgressStage::ConfirmingUi);
    let target = normalize_lock_text(recipient);
    let attempts = confirmation_attempts(config);
    let mut streak = 0_u32;
    for attempt in 0..attempts {
        let image = capture_normalized(context, config, "confirm_friend_conversation")?;
        let title = merged_text(ocr, &image, SECONDARY_TITLE_RECT)?;
        let title_matches = text_contains_complete_target(&normalize_lock_text(&title), &target);
        let found = if title_matches {
            true
        } else {
            !matching_text_rows(
                ocr,
                &image,
                config.friend_chat_region,
                &target,
                TextMatchMode::ContainsCompleteTarget,
            )?
            .is_empty()
        };
        if found {
            streak = streak.saturating_add(1);
            if streak >= config.stable_count {
                return Ok(());
            }
        } else {
            streak = 0;
        }
        if attempt + 1 < attempts {
            sleep_ms(config.poll_ms);
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "confirm_friend_conversation",
        "selected conversation did not stably contain the complete friend name",
    ))
}

pub(super) fn restore_residency(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    target: UiResidencyTarget,
) -> std::result::Result<(), UiRoutineFailure> {
    match target {
        UiResidencyTarget::Primary => restore_primary(context, config),
        UiResidencyTarget::SecondaryCurrentHall => restore_secondary_hall(context, ocr, config),
    }
}

fn restore_primary(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    let image = capture_normalized(context, config, "observe_primary_residency")?;
    let state = detect_ui_state(&image, &config.templates, &config.screen)
        .map_err(|error| before_input_failure("observe_primary_residency", error))?;
    if state.is_primary() {
        return Ok(());
    }
    if !state.is_secondary() {
        return Err(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "restore_primary_residency",
            "cannot recover primary residency from an unknown UI state",
        ));
    }
    context
        .device()
        .press_key(Key::Escape)
        .map_err(|error| before_input_failure("restore_primary_residency", error))?;
    sleep_ms(config.open_chat_ms);
    let started = std::time::Instant::now();
    let mut previous = None;
    loop {
        let image = capture_normalized(context, config, "confirm_primary_residency")?;
        let state = detect_ui_state(&image, &config.templates, &config.screen)
            .map_err(|error| before_input_failure("confirm_primary_residency", error))?;
        if !state.is_primary() {
            return Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "confirm_primary_residency",
                "primary residency did not become stable",
            ));
        }
        let current = rect_chat_change_fingerprint(&image, config.screen.friend_rect.into())
            .map_err(|error| before_input_failure("confirm_primary_stability", error))?;
        if previous
            .as_ref()
            .is_some_and(|previous| page_matches(previous, &current, config))
        {
            return Ok(());
        }
        if started.elapsed() >= Duration::from_millis(PRIMARY_STABILITY_TIMEOUT_MS) {
            log::warn!(
                "好友按钮区域持续变化 {}ms，按已确认的一级界面继续",
                PRIMARY_STABILITY_TIMEOUT_MS
            );
            return Ok(());
        }
        previous = Some(current);
        sleep_ms(PRIMARY_STABILITY_POLL_MS);
    }
}

fn restore_secondary_hall(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    ensure_secondary_chat(context, config)?;
    if current_title_is_hall(context, ocr, config)? {
        return confirm_current_hall(context, ocr, config);
    }
    let mut pages = Vec::<ChangeFingerprint>::new();
    for scroll_index in 0..=MAX_UPWARD_SCROLLS {
        let image = capture_normalized(context, config, "locate_secondary_hall")?;
        if let Some(hit) = best_template_hit(
            &image,
            Some(config.secondary_hall_search_region),
            &config.secondary_hall_template,
            config.template_threshold,
        )
        .map_err(|error| before_input_failure("locate_secondary_hall", error))?
        {
            let point = hit.center();
            context
                .device()
                .click_point(point.x, point.y)
                .map_err(|error| before_input_failure("select_secondary_hall", error))?;
            sleep_ms(config.click_ms);
            return confirm_current_hall(context, ocr, config);
        }
        let fingerprint = rect_chat_change_fingerprint(&image, config.friend_list_region)
            .map_err(|error| before_input_failure("fingerprint_secondary_hall_list", error))?;
        if pages
            .iter()
            .any(|page| page_matches(page, &fingerprint, config))
        {
            break;
        }
        pages.push(fingerprint);
        if scroll_index == MAX_UPWARD_SCROLLS {
            break;
        }
        let point = config.friend_list_region.center();
        context
            .device()
            .scroll_point(point.x, point.y, -FRIEND_SCROLL_LENGTH)
            .map_err(|error| before_input_failure("scroll_to_secondary_hall", error))?;
        wait_friend_list_stable(context, config)?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "restore_secondary_hall",
        "current-hall template was not found within the bounded list traversal",
    ))
}

fn current_title_is_hall(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<bool, UiRoutineFailure> {
    let image = capture_normalized(context, config, "observe_secondary_title")?;
    let title = merged_text(ocr, &image, SECONDARY_TITLE_RECT)?;
    Ok(matches!(
        classify_title(&title),
        SecondaryChatIdentity::CurrentHall
    ))
}

fn confirm_current_hall(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    let mut streak = 0_u32;
    for attempt in 0..confirmation_attempts(config) {
        if current_title_is_hall(context, ocr, config)? {
            streak = streak.saturating_add(1);
            if streak >= config.stable_count {
                return Ok(());
            }
        } else {
            streak = 0;
        }
        if attempt + 1 < confirmation_attempts(config) {
            sleep_ms(config.poll_ms);
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "confirm_secondary_hall",
        "current-hall title did not become stable",
    ))
}

fn matching_text_rows(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    region: Rect,
    normalized_target: &str,
    mode: TextMatchMode,
) -> std::result::Result<Vec<Point>, UiRoutineFailure> {
    let crop = crop_canvas(image, region)
        .map_err(|error| before_input_failure("crop_ocr_confirmation", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| before_input_failure("ocr_confirmation", error))?;
    Ok(lines
        .into_iter()
        .filter_map(|line| {
            let recognized = normalize_lock_text(&line.text);
            let matched = match mode {
                TextMatchMode::Exact => recognized == normalized_target,
                TextMatchMode::ContainsCompleteTarget => {
                    text_contains_complete_target(&recognized, normalized_target)
                }
            };
            matched.then(|| {
                Point::new(
                    region.x + line.bbox.center().x,
                    region.y + line.bbox.center().y,
                )
            })
        })
        .collect())
}

fn merged_text(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    region: Rect,
) -> std::result::Result<String, UiRoutineFailure> {
    let crop = crop_canvas(image, region)
        .map_err(|error| before_input_failure("crop_ocr_confirmation", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| before_input_failure("ocr_confirmation", error))?;
    Ok(merge_ocr_lines(lines, 12))
}

pub(super) fn capture_normalized(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    stage: &'static str,
) -> std::result::Result<DynamicImage, UiRoutineFailure> {
    let image = context
        .device()
        .capture()
        .map_err(|error| before_input_failure(stage, error))?;
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

pub(super) fn current_ui_is_primary(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    stage: &'static str,
) -> std::result::Result<bool, UiRoutineFailure> {
    let image = capture_normalized(context, config, stage)?;
    let state = detect_ui_state(&image, &config.templates, &config.screen)
        .map_err(|error| before_input_failure(stage, error))?;
    Ok(state.is_primary())
}

fn wait_friend_list_stable(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    let mut previous = rect_chat_change_fingerprint(
        &capture_normalized(context, config, "observe_scrolled_friend_list")?,
        config.friend_list_region,
    )
    .map_err(|error| before_input_failure("observe_scrolled_friend_list", error))?;
    for _ in 0..confirmation_attempts(config) {
        sleep_ms(config.poll_ms);
        let current = rect_chat_change_fingerprint(
            &capture_normalized(context, config, "confirm_scrolled_friend_list")?,
            config.friend_list_region,
        )
        .map_err(|error| before_input_failure("confirm_scrolled_friend_list", error))?;
        if page_matches(&previous, &current, config) {
            return Ok(());
        }
        previous = current;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::BeforeInput,
        "confirm_scrolled_friend_list",
        "friend list did not become stable after scrolling",
    ))
}

fn page_matches(
    left: &ChangeFingerprint,
    right: &ChangeFingerprint,
    config: &FriendDeliveryRoutineConfig,
) -> bool {
    let stats = change_stats(left, right);
    stats.mean_abs_diff <= config.stable_mean_threshold
        && stats.changed_ratio <= config.stable_changed_ratio_threshold
}

fn confirmation_attempts(config: &FriendDeliveryRoutineConfig) -> u32 {
    let polls = config.timeout_ms / config.poll_ms.max(1);
    polls.max(config.stable_count as u64).min(u32::MAX as u64) as u32
}

pub(super) fn before_input_failure(stage: &'static str, error: anyhow::Error) -> UiRoutineFailure {
    UiRoutineFailure::new(InputCertainty::BeforeInput, stage, format!("{error:#}"))
}

pub(super) fn sleep_ms(ms: u64) {
    if ms > 0 {
        sleep(Duration::from_millis(ms));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, bail};
    use enigo::Key;
    use image::{DynamicImage, GenericImage};

    use super::*;

    #[test]
    fn complete_target_does_not_accept_a_truncated_ocr_name() {
        assert!(!text_contains_complete_target("萌", "萌萌"));
        assert!(text_contains_complete_target("原昵称(萌萌)", "萌萌"));
    }
    use crate::config::AppConfig;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::ui::{UiDevice, UiRuntime};
    use crate::ui::geometry::Rect;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Conversation {
        Hall,
        Friend(String),
    }

    struct TestUiState {
        conversation: Conversation,
        pasted: Vec<String>,
        selected_friends: Vec<String>,
        hall_clicks: usize,
        scrolls: usize,
        send_attempts: usize,
        fail_send_attempt: Option<usize>,
    }

    struct RecordingDevice {
        frame: DynamicImage,
        state: Arc<Mutex<TestUiState>>,
        friend_rows: Vec<(i32, String)>,
        hall_point: (i32, i32),
    }

    impl UiDevice for RecordingDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(self.frame.clone())
        }

        fn ensure_ready(&mut self, _after_activate_ms: u64) -> Result<()> {
            Ok(())
        }

        fn ensure_foreground(&mut self) -> Result<()> {
            Ok(())
        }

        fn click_point(&mut self, x: i32, y: i32) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            if (x, y) == self.hall_point {
                state.conversation = Conversation::Hall;
                state.hall_clicks += 1;
                return Ok(());
            }
            if let Some((_, friend)) = self
                .friend_rows
                .iter()
                .find(|(row_y, _)| y.abs_diff(*row_y) <= 10)
            {
                state.conversation = Conversation::Friend(friend.clone());
                state.selected_friends.push(friend.clone());
            }
            Ok(())
        }

        fn paste_text(&mut self, text: &str, _clipboard_hold_ms: u64) -> Result<()> {
            self.state.lock().unwrap().pasted.push(text.to_string());
            Ok(())
        }

        fn input_text(&mut self, _text: &str, _input_settle_ms: u64) -> Result<()> {
            bail!("paste should succeed in this scenario")
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            if key == Key::Return {
                let mut state = self.state.lock().unwrap();
                state.send_attempts += 1;
                if state.fail_send_attempt == Some(state.send_attempts) {
                    bail!("simulated unknown send result")
                }
            }
            Ok(())
        }

        fn scroll_point(&mut self, _x: i32, _y: i32, _length: i32) -> Result<()> {
            self.state.lock().unwrap().scrolls += 1;
            Ok(())
        }
    }

    struct FriendOcrDevice {
        state: Arc<Mutex<TestUiState>>,
        friend_list_size: (u32, u32),
        title_size: (u32, u32),
    }

    impl OcrDevice for FriendOcrDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            let size = (image.width(), image.height());
            if size == self.friend_list_size {
                return Ok(vec![
                    OcrLine {
                        text: "甲".to_string(),
                        confidence: 1.0,
                        bbox: Rect::new(5, 70, 80, 30),
                    },
                    OcrLine {
                        text: "乙".to_string(),
                        confidence: 1.0,
                        bbox: Rect::new(5, 150, 80, 30),
                    },
                ]);
            }
            if size == self.title_size {
                let title = match &self.state.lock().unwrap().conversation {
                    Conversation::Hall => "当前大厅".to_string(),
                    Conversation::Friend(friend) => friend.clone(),
                };
                return Ok(vec![OcrLine {
                    text: title,
                    confidence: 1.0,
                    bbox: Rect::new(0, 0, image.width(), image.height()),
                }]);
            }
            Ok(Vec::new())
        }
    }

    #[test]
    fn ordered_batch_sends_every_message_before_restoring_secondary_hall_once() {
        let mut config = AppConfig::load(Path::new("config.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        config.timing.workflow.default_timeout_ms = 20;
        config.timing.workflow.default_poll_ms = 1;
        let state = Arc::new(Mutex::new(TestUiState {
            conversation: Conversation::Hall,
            pasted: Vec::new(),
            selected_friends: Vec::new(),
            hall_clicks: 0,
            scrolls: 0,
            send_attempts: 0,
            fail_send_attempt: None,
        }));
        let frame = secondary_frame(&config);
        let friend_list = config.invite.friend_list_region;
        let title = SECONDARY_TITLE_RECT;
        let hall_template = image::open(&config.templates.secondary_hall).unwrap();
        let hall_point = (
            config.screen.secondary_hall_rect.x + hall_template.width() as i32 / 2,
            config.screen.secondary_hall_rect.y + hall_template.height() as i32 / 2,
        );
        let device = RecordingDevice {
            frame,
            state: state.clone(),
            friend_rows: vec![
                (friend_list.y + 85, "甲".to_string()),
                (friend_list.y + 165, "乙".to_string()),
            ],
            hall_point,
        };
        let ocr_runtime = OcrRuntime::start(
            FriendOcrDevice {
                state: state.clone(),
                friend_list_size: (friend_list.width, friend_list.height),
                title_size: (title.width, title.height),
            },
            4,
        )
        .unwrap();
        let ui_runtime = UiRuntime::start(device, 4).unwrap();
        let friend_ui = FriendDeliveryUi::new(ui_runtime.handle(), ocr_runtime.handle(), &config);

        let operation = friend_ui
            .submit(SendFriendDeliveries::new(
                vec![
                    FriendDelivery::new("甲", ["甲一"]),
                    FriendDelivery::new("乙", ["乙一", "乙二"]),
                ],
                UiResidencyTarget::SecondaryCurrentHall,
            ))
            .unwrap();
        let result = operation.wait().unwrap();

        assert!(result.is_complete());
        assert_eq!(
            result.deliveries()[0].message_statuses(),
            &[FriendDeliveryMessageStatus::Sent]
        );
        assert_eq!(
            result.deliveries()[1].message_statuses(),
            &[
                FriendDeliveryMessageStatus::Sent,
                FriendDeliveryMessageStatus::Sent,
            ]
        );
        let state = state.lock().unwrap();
        assert_eq!(state.selected_friends, ["甲", "乙"]);
        assert_eq!(state.pasted, ["甲一", "乙一", "乙二"]);
        assert_eq!(state.hall_clicks, 1);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn hall_batch_sends_all_messages_and_confirms_secondary_residency() {
        let mut config = AppConfig::load(Path::new("config.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.workflow.default_timeout_ms = 20;
        config.timing.workflow.default_poll_ms = 1;
        let state = Arc::new(Mutex::new(TestUiState {
            conversation: Conversation::Hall,
            pasted: Vec::new(),
            selected_friends: Vec::new(),
            hall_clicks: 0,
            scrolls: 0,
            send_attempts: 0,
            fail_send_attempt: None,
        }));
        let friend_list = config.invite.friend_list_region;
        let title = SECONDARY_TITLE_RECT;
        let hall_template = image::open(&config.templates.secondary_hall).unwrap();
        let hall_point = (
            config.screen.secondary_hall_rect.x + hall_template.width() as i32 / 2,
            config.screen.secondary_hall_rect.y + hall_template.height() as i32 / 2,
        );
        let ui_runtime = UiRuntime::start(
            RecordingDevice {
                frame: secondary_frame(&config),
                state: state.clone(),
                friend_rows: Vec::new(),
                hall_point,
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(
            FriendOcrDevice {
                state: state.clone(),
                friend_list_size: (friend_list.width, friend_list.height),
                title_size: (title.width, title.height),
            },
            4,
        )
        .unwrap();
        let hall_ui = HallBatchUi::new(ui_runtime.handle(), ocr_runtime.handle(), &config);

        let outcome = hall_ui
            .submit(SendHallBatch::new(
                ["第一条", "第二条"],
                UiResidencyTarget::SecondaryCurrentHall,
                0,
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(outcome.sent(), 2);
        assert!(matches!(outcome.status(), HallBatchStatus::Complete));
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)
        ));
        assert_eq!(state.lock().unwrap().pasted, ["第一条", "第二条"]);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    struct SubstringFriendOcrDevice {
        state: Arc<Mutex<TestUiState>>,
        friend_list_size: (u32, u32),
        title_size: (u32, u32),
    }

    impl OcrDevice for SubstringFriendOcrDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            let size = (image.width(), image.height());
            if size == self.friend_list_size {
                return Ok(vec![OcrLine {
                    text: "路人甲".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(5, 70, 80, 30),
                }]);
            }
            if size == self.title_size {
                let title = match &self.state.lock().unwrap().conversation {
                    Conversation::Hall => "当前大厅".to_string(),
                    Conversation::Friend(friend) => friend.clone(),
                };
                return Ok(vec![OcrLine {
                    text: title,
                    confidence: 1.0,
                    bbox: Rect::new(0, 0, image.width(), image.height()),
                }]);
            }
            Ok(Vec::new())
        }
    }

    #[test]
    fn friend_list_does_not_treat_a_longer_name_as_the_requested_friend() {
        let mut config = AppConfig::load(Path::new("config.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        config.timing.workflow.default_timeout_ms = 20;
        config.timing.workflow.default_poll_ms = 1;
        let state = Arc::new(Mutex::new(TestUiState {
            conversation: Conversation::Hall,
            pasted: Vec::new(),
            selected_friends: Vec::new(),
            hall_clicks: 0,
            scrolls: 0,
            send_attempts: 0,
            fail_send_attempt: None,
        }));
        let friend_list = config.invite.friend_list_region;
        let title = SECONDARY_TITLE_RECT;
        let hall_template = image::open(&config.templates.secondary_hall).unwrap();
        let hall_point = (
            config.screen.secondary_hall_rect.x + hall_template.width() as i32 / 2,
            config.screen.secondary_hall_rect.y + hall_template.height() as i32 / 2,
        );
        let ui_runtime = UiRuntime::start(
            RecordingDevice {
                frame: secondary_frame(&config),
                state: state.clone(),
                friend_rows: vec![(friend_list.y + 85, "甲".to_string())],
                hall_point,
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(
            SubstringFriendOcrDevice {
                state: state.clone(),
                friend_list_size: (friend_list.width, friend_list.height),
                title_size: (title.width, title.height),
            },
            4,
        )
        .unwrap();
        let friend_ui = FriendDeliveryUi::new(ui_runtime.handle(), ocr_runtime.handle(), &config);

        let result = friend_ui
            .submit(SendFriendDeliveries::new(
                vec![FriendDelivery::new("甲", ["不应发送"])],
                UiResidencyTarget::SecondaryCurrentHall,
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(
            result.deliveries()[0].message_statuses(),
            &[FriendDeliveryMessageStatus::NotAttempted]
        );
        let state = state.lock().unwrap();
        assert!(state.selected_friends.is_empty());
        assert!(state.pasted.is_empty());

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn unknown_message_is_never_retried_and_only_unattempted_messages_remain_safe() {
        let mut config = AppConfig::load(Path::new("config.yaml")).unwrap();
        config.friend_delivery.auto_retry_count = 3;
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        config.timing.workflow.default_timeout_ms = 20;
        config.timing.workflow.default_poll_ms = 1;
        let state = Arc::new(Mutex::new(TestUiState {
            conversation: Conversation::Hall,
            pasted: Vec::new(),
            selected_friends: Vec::new(),
            hall_clicks: 0,
            scrolls: 0,
            send_attempts: 0,
            fail_send_attempt: Some(2),
        }));
        let friend_list = config.invite.friend_list_region;
        let title = SECONDARY_TITLE_RECT;
        let hall_template = image::open(&config.templates.secondary_hall).unwrap();
        let hall_point = (
            config.screen.secondary_hall_rect.x + hall_template.width() as i32 / 2,
            config.screen.secondary_hall_rect.y + hall_template.height() as i32 / 2,
        );
        let ui_runtime = UiRuntime::start(
            RecordingDevice {
                frame: secondary_frame(&config),
                state: state.clone(),
                friend_rows: vec![(friend_list.y + 85, "甲".to_string())],
                hall_point,
            },
            4,
        )
        .unwrap();
        let ocr_runtime = OcrRuntime::start(
            FriendOcrDevice {
                state: state.clone(),
                friend_list_size: (friend_list.width, friend_list.height),
                title_size: (title.width, title.height),
            },
            4,
        )
        .unwrap();
        let friend_ui = FriendDeliveryUi::new(ui_runtime.handle(), ocr_runtime.handle(), &config);
        let request = SendFriendDeliveries::new(
            vec![FriendDelivery::new("甲", ["第一", "第二", "第三"])],
            UiResidencyTarget::SecondaryCurrentHall,
        );

        let outcome = friend_ui.submit(request.clone()).unwrap().wait().unwrap();
        let retry = outcome
            .safe_retry_request(&request)
            .expect("the third message is confirmed unattempted");

        assert_eq!(
            outcome.deliveries()[0].message_statuses(),
            &[
                FriendDeliveryMessageStatus::Sent,
                FriendDeliveryMessageStatus::ResultUnknown,
                FriendDeliveryMessageStatus::NotAttempted,
            ]
        );
        assert_eq!(retry.deliveries()[0].recipient(), "甲");
        assert_eq!(retry.deliveries()[0].messages(), ["第三"]);
        let state = state.lock().unwrap();
        assert_eq!(state.send_attempts, 2);
        assert_eq!(state.pasted, ["第一", "第二"]);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn retry_request_is_rejected_when_any_delivery_shape_does_not_match() {
        let request = SendFriendDeliveries::new(
            vec![
                FriendDelivery::new("甲", ["甲一", "甲二"]),
                FriendDelivery::new("乙", ["乙一"]),
            ],
            UiResidencyTarget::Primary,
        );
        let outcome = SendFriendDeliveriesOutcome {
            deliveries: vec![
                FriendDeliveryOutcome {
                    message_statuses: vec![FriendDeliveryMessageStatus::NotAttempted],
                    failure: None,
                },
                FriendDeliveryOutcome {
                    message_statuses: vec![FriendDeliveryMessageStatus::NotAttempted],
                    failure: None,
                },
            ],
            residency: UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary),
        };

        assert!(outcome.safe_retry_request(&request).is_none());
    }

    fn secondary_frame(config: &AppConfig) -> DynamicImage {
        let mut frame =
            DynamicImage::new_rgba8(config.screen.expected_width, config.screen.expected_height);
        let back = image::open(&config.templates.secondary_back).unwrap();
        let hall = image::open(&config.templates.secondary_hall).unwrap();
        frame
            .copy_from(
                &back,
                config.screen.secondary_back_rect.x as u32,
                config.screen.secondary_back_rect.y as u32,
            )
            .unwrap();
        frame
            .copy_from(
                &hall,
                config.screen.secondary_hall_rect.x as u32,
                config.screen.secondary_hall_rect.y as u32,
            )
            .unwrap();
        frame
    }
}
