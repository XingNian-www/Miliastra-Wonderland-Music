use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use enigo::Key;
use image::DynamicImage;

#[cfg(test)]
use crate::config::AppConfig;
use crate::config::{
    FriendDeliveryConfig, InputTimingConfig, OcrConfig, OutputConfig, PointConfig, ScreenConfig,
    TemplateConfig,
};
use crate::observation::chat::{SECONDARY_TITLE_RECT, SecondaryChatIdentity, classify_title};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle, merge_ocr_lines};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure,
    UiRoutineProgressStage, UiRuntimeHandle, UiStateKind, UiStateObservation, UiSubmitError,
    sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
use crate::ui::geometry::{Point, Rect, crop_canvas};
use crate::ui::locator::secondary_hall_search_rect;
use crate::ui::template::best_template_hit;

use super::state_observation::{
    UiStateObservationConfig, capture_normalized_ui_state, wait_for_stable_ui_kind,
};

const FRIEND_ROW_Y_TOLERANCE: i32 = 6;
const MAX_FRIEND_LIST_DRAGS: usize = 2;
const FRIEND_AVATAR_TEXT_OFFSET_X: i32 = 40;
const FRIEND_DRAG_MAX_EDGE_INSET: i32 = 50;
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
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: FriendDeliveryRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
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

    pub(crate) fn observe(
        &self,
        target: UiResidencyTarget,
    ) -> std::result::Result<
        UiOperation<std::result::Result<DynamicImage, UiRoutineFailure>>,
        UiSubmitError,
    > {
        self.runtime.submit(ObserveResidencyRoutine {
            target,
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

struct ObserveResidencyRoutine {
    target: UiResidencyTarget,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

impl sealed::UiRoutineSealed for ObserveResidencyRoutine {}

impl UiRoutine for ObserveResidencyRoutine {
    type Output = std::result::Result<DynamicImage, UiRoutineFailure>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context.publish_progress(UiRoutineProgressStage::NormalizingStart);
        context
            .device()
            .ensure_ready(self.config.after_activate_ms)
            .map_err(|error| before_input_failure("prepare_residency_observation", error))?;
        restore_residency(context, &self.ocr, &self.config, self.target)?;
        capture_normalized(
            context,
            &self.config,
            "capture_residency_observation",
            InputCertainty::AfterInputUnknown,
        )
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
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: FriendDeliveryRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
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
    pub(crate) fn new(
        runtime: UiRuntimeHandle,
        ocr: OcrRuntimeHandle,
        config: FriendDeliveryRoutineConfig,
    ) -> Self {
        Self {
            runtime,
            ocr,
            config,
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
pub(crate) struct FriendDeliveryRoutineConfig {
    pub(super) screen: ScreenConfig,
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
    fast_match: bool,
    stable_mean_threshold: f32,
    stable_changed_ratio_threshold: f32,
    secondary_hall_template: PathBuf,
    template_threshold: f32,
}

pub(crate) struct FriendDeliveryRoutineConfigSource<'a> {
    pub(crate) screen: &'a ScreenConfig,
    pub(crate) templates: &'a TemplateConfig,
    pub(crate) ocr: &'a OcrConfig,
    pub(crate) output: &'a OutputConfig,
    pub(crate) input_timing: &'a InputTimingConfig,
    pub(crate) delivery: &'a FriendDeliveryConfig,
    pub(crate) friend_list_region: Rect,
    pub(crate) friend_chat_region: Rect,
    pub(crate) friend_step_ms: u64,
    pub(crate) timeout_ms: u64,
    pub(crate) poll_ms: u64,
    pub(crate) stable_count: u32,
}

impl FriendDeliveryRoutineConfig {
    pub(crate) fn resolve(source: FriendDeliveryRoutineConfigSource<'_>) -> Self {
        let hall_anchor: Rect = source.screen.secondary_hall_rect.into();
        let friend_list_region = source.friend_list_region;
        Self {
            screen: source.screen.clone(),
            friend_list_region,
            friend_chat_region: source.friend_chat_region,
            secondary_hall_search_region: secondary_hall_search_rect(
                hall_anchor,
                friend_list_region,
            ),
            chat_click: source.output.chat_click_2,
            send_enabled: source.output.send_enabled,
            after_activate_ms: source.input_timing.after_activate_ms,
            open_chat_ms: source.input_timing.open_chat_ms,
            click_ms: source.input_timing.click_ms,
            text_ms: source.input_timing.text_ms,
            send_ms: source.input_timing.send_ms,
            friend_step_ms: source.friend_step_ms,
            timeout_ms: source.timeout_ms,
            poll_ms: source.poll_ms.max(10),
            stable_count: source.stable_count,
            auto_retry_count: source.delivery.auto_retry_count,
            fast_match: source.delivery.fast_match,
            stable_mean_threshold: source.ocr.change_mean_threshold,
            stable_changed_ratio_threshold: source.ocr.change_pixel_threshold,
            secondary_hall_template: source.templates.secondary_hall.clone(),
            template_threshold: source.templates.marker_threshold,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_app(config: &AppConfig) -> Self {
        Self::resolve(FriendDeliveryRoutineConfigSource {
            screen: &config.screen,
            templates: &config.templates,
            ocr: &config.ocr,
            output: &config.output,
            input_timing: &config.timing.input,
            delivery: &config.friend_delivery,
            friend_list_region: config.invite.friend_list_region.into(),
            friend_chat_region: config.invite.friend_chat_region.into(),
            friend_step_ms: config.timing.invite.step_ms,
            timeout_ms: config.timing.workflow.default_timeout_ms,
            poll_ms: config.timing.workflow.default_poll_ms,
            stable_count: config.resolve_stability_count(config.invite.friend_name_stable_count),
        })
    }

    pub(super) fn state_observation(&self) -> UiStateObservationConfig {
        UiStateObservationConfig::new(
            self.screen.expected_width,
            self.screen.expected_height,
            self.poll_ms,
        )
    }

    pub(super) fn timeout_ms(&self) -> u64 {
        self.timeout_ms
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConversationIdentityEvidence {
    Title,
    ChatRegion,
    Missing,
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
    let state = wait_for_stable_ui_kind(
        context,
        config.state_observation(),
        None,
        config.timeout_ms,
        "observe_friend_delivery_start",
        InputCertainty::BeforeInput,
    )?;
    if state == UiStateKind::Secondary {
        return Ok(());
    }
    context
        .device()
        .press_key(Key::Return)
        .map_err(|error| before_input_failure("open_secondary_chat", error))?;
    sleep_ms(config.open_chat_ms);
    wait_for_stable_ui_kind(
        context,
        config.state_observation(),
        Some(UiStateKind::Secondary),
        config.timeout_ms,
        "confirm_secondary_chat",
        InputCertainty::AfterInputUnknown,
    )?;
    Ok(())
}

fn locate_stable_friend_row(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipient: &str,
) -> std::result::Result<Point, UiRoutineFailure> {
    let mut pages = Vec::<ChangeFingerprint>::new();
    for drag_index in 0..=MAX_FRIEND_LIST_DRAGS {
        match search_current_friend_page(context, ocr, config, recipient)? {
            PageSearch::Found(point) => return Ok(point),
            PageSearch::Missing => {}
        }
        let image = capture_normalized(
            context,
            config,
            "fingerprint_friend_list",
            InputCertainty::AfterInputUnknown,
        )?;
        let fingerprint = rect_chat_change_fingerprint(&image, config.friend_list_region)
            .map_err(|error| before_input_failure("fingerprint_friend_list", error))?;
        if pages
            .iter()
            .any(|page| page_matches(page, &fingerprint, config))
        {
            break;
        }
        pages.push(fingerprint);
        if drag_index == MAX_FRIEND_LIST_DRAGS {
            break;
        }
        let (from, to) = friend_list_drag_points(config.friend_list_region);
        context
            .device()
            .drag_point(from.x, from.y, to.x, to.y)
            .map_err(|error| before_input_failure("drag_friend_list", error))?;
        wait_friend_list_stable(context, config)?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "locate_friend",
        "no unique stable friend row was found within two list drags",
    ))
}

fn friend_list_drag_points(region: Rect) -> (Point, Point) {
    let avatar_x = region.x.saturating_sub(FRIEND_AVATAR_TEXT_OFFSET_X).max(0);
    let edge_inset = (region.height as i32 / 12).min(FRIEND_DRAG_MAX_EDGE_INSET);
    (
        Point::new(avatar_x, region.bottom() - edge_inset),
        Point::new(avatar_x, region.y + edge_inset),
    )
}

fn current_hall_restore_drag_points(region: Rect) -> (Point, Point) {
    let (bottom, top) = friend_list_drag_points(region);
    (top, bottom)
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
        let image = capture_normalized(
            context,
            config,
            "capture_friend_list",
            InputCertainty::AfterInputUnknown,
        )?;
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

fn confirm_friend_conversation(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipient: &str,
) -> std::result::Result<(), UiRoutineFailure> {
    context.publish_progress(UiRoutineProgressStage::ConfirmingUi);
    let target = normalize_lock_text(recipient);
    let attempts = confirmation_attempts(config);
    let mut stable_key = None;
    let mut streak = 0_u32;
    for attempt in 0..attempts {
        let image = capture_normalized(
            context,
            config,
            "confirm_friend_conversation",
            InputCertainty::AfterInputUnknown,
        )?;
        if let Some(key) = conversation_confirmation_key(ocr, &image, config, &target)? {
            if stable_key.as_deref() == Some(key.as_str()) {
                streak = streak.saturating_add(1);
            } else {
                stable_key = Some(key);
                streak = 1;
            }
            if streak >= config.stable_count {
                return Ok(());
            }
        } else {
            stable_key = None;
            streak = 0;
        }
        if attempt + 1 < attempts {
            sleep_ms(config.poll_ms);
        }
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "confirm_friend_conversation",
        if config.fast_match {
            "selected conversation title did not stably identify a private friend chat"
        } else {
            "selected conversation did not stably contain the complete friend name"
        },
    ))
}

fn conversation_confirmation_key(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    config: &FriendDeliveryRoutineConfig,
    normalized_target: &str,
) -> std::result::Result<Option<String>, UiRoutineFailure> {
    if config.fast_match {
        let title = merged_text(ocr, image, SECONDARY_TITLE_RECT)?;
        let normalized_title = normalize_lock_text(&title);
        return Ok(match classify_title(&title) {
            SecondaryChatIdentity::CurrentHall | SecondaryChatIdentity::PublicChannel => None,
            SecondaryChatIdentity::Friend(title) => {
                let title = normalize_lock_text(&title);
                (!title.is_empty()).then_some(title)
            }
            SecondaryChatIdentity::StrangerMessages => {
                (!normalized_title.is_empty()).then_some(normalized_title)
            }
            SecondaryChatIdentity::Unknown => None,
        });
    }
    Ok(
        (detect_conversation_identity(ocr, image, config, normalized_target)?
            != ConversationIdentityEvidence::Missing)
            .then(|| normalized_target.to_string()),
    )
}

fn detect_conversation_identity(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    config: &FriendDeliveryRoutineConfig,
    normalized_target: &str,
) -> std::result::Result<ConversationIdentityEvidence, UiRoutineFailure> {
    let title = merged_text(ocr, image, SECONDARY_TITLE_RECT)?;
    if text_contains_complete_target(&normalize_lock_text(&title), normalized_target) {
        return Ok(ConversationIdentityEvidence::Title);
    }
    if matching_text_rows(
        ocr,
        image,
        config.friend_chat_region,
        normalized_target,
        TextMatchMode::ContainsCompleteTarget,
    )?
    .is_empty()
    {
        Ok(ConversationIdentityEvidence::Missing)
    } else {
        Ok(ConversationIdentityEvidence::ChatRegion)
    }
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
    let state = wait_for_stable_ui_kind(
        context,
        config.state_observation(),
        None,
        config.timeout_ms,
        "observe_primary_residency",
        InputCertainty::ConfirmedFailure,
    )?;
    if state == UiStateKind::Primary {
        return Ok(());
    }
    context
        .device()
        .press_key(Key::Escape)
        .map_err(|error| before_input_failure("restore_primary_residency", error))?;
    confirm_primary_residency(context, config)
}

pub(super) fn confirm_primary_residency(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    wait_for_stable_ui_kind(
        context,
        config.state_observation(),
        Some(UiStateKind::Primary),
        config.timeout_ms,
        "confirm_primary_residency",
        InputCertainty::AfterInputUnknown,
    )?;
    confirm_primary_visual_stability(context, config)
}

fn confirm_primary_visual_stability(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    let started = std::time::Instant::now();
    let mut previous = None;
    let mut confirmed_primary = false;
    loop {
        let image = capture_normalized(
            context,
            config,
            "confirm_primary_residency",
            InputCertainty::AfterInputUnknown,
        )?;
        let observation = context.latest_ui_state().ok_or_else(|| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "confirm_primary_residency",
                "UI runtime has no configured template state classifier",
            )
        })?;
        match observation {
            UiStateObservation::Classified(state)
                if state.stable_kind() == Some(UiStateKind::Primary) =>
            {
                confirmed_primary = true;
                let current =
                    rect_chat_change_fingerprint(&image, config.screen.friend_rect.into())
                        .map_err(|error| {
                            UiRoutineFailure::new(
                                InputCertainty::AfterInputUnknown,
                                "confirm_primary_stability",
                                format!("{error:#}"),
                            )
                        })?;
                if previous
                    .as_ref()
                    .is_some_and(|previous| page_matches(previous, &current, config))
                {
                    return Ok(());
                }
                previous = Some(current);
            }
            UiStateObservation::Classified(_) => previous = None,
            UiStateObservation::Failed { reason, .. } => {
                return Err(UiRoutineFailure::new(
                    InputCertainty::AfterInputUnknown,
                    "confirm_primary_residency",
                    format!("template UI state classification failed: {reason}"),
                ));
            }
        }
        if started.elapsed() >= Duration::from_millis(PRIMARY_STABILITY_TIMEOUT_MS) {
            if !confirmed_primary {
                return Err(UiRoutineFailure::new(
                    InputCertainty::AfterInputUnknown,
                    "confirm_primary_residency",
                    "primary residency did not become stable",
                ));
            }
            log::warn!(
                "好友按钮区域持续变化 {}ms，按已确认的一级界面继续",
                PRIMARY_STABILITY_TIMEOUT_MS
            );
            return Ok(());
        }
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
    for drag_index in 0..=MAX_FRIEND_LIST_DRAGS {
        let image = capture_normalized(
            context,
            config,
            "locate_secondary_hall",
            InputCertainty::AfterInputUnknown,
        )?;
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
        if drag_index == MAX_FRIEND_LIST_DRAGS {
            break;
        }
        let (from, to) = current_hall_restore_drag_points(config.friend_list_region);
        context
            .device()
            .drag_point(from.x, from.y, to.x, to.y)
            .map_err(|error| before_input_failure("drag_to_secondary_hall", error))?;
        wait_friend_list_stable(context, config)?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::ConfirmedFailure,
        "restore_secondary_hall",
        "current-hall template was not found after two downward avatar drags",
    ))
}

fn current_title_is_hall(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<bool, UiRoutineFailure> {
    let image = capture_normalized(
        context,
        config,
        "observe_secondary_title",
        InputCertainty::AfterInputUnknown,
    )?;
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
    certainty: InputCertainty,
) -> std::result::Result<DynamicImage, UiRoutineFailure> {
    capture_normalized_ui_state(context, &config.state_observation(), stage, certainty)
}

pub(super) fn current_ui_is_primary(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    stage: &'static str,
) -> std::result::Result<bool, UiRoutineFailure> {
    Ok(wait_for_stable_ui_kind(
        context,
        config.state_observation(),
        None,
        config.timeout_ms,
        stage,
        InputCertainty::ConfirmedFailure,
    )? == UiStateKind::Primary)
}

fn wait_friend_list_stable(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    let mut previous = rect_chat_change_fingerprint(
        &capture_normalized(
            context,
            config,
            "observe_scrolled_friend_list",
            InputCertainty::AfterInputUnknown,
        )?,
        config.friend_list_region,
    )
    .map_err(|error| before_input_failure("observe_scrolled_friend_list", error))?;
    for _ in 0..confirmation_attempts(config) {
        sleep_ms(config.poll_ms);
        let current = rect_chat_change_fingerprint(
            &capture_normalized(
                context,
                config,
                "confirm_scrolled_friend_list",
                InputCertainty::AfterInputUnknown,
            )?,
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

    #[test]
    fn friend_list_drag_runs_from_the_lowest_avatar_to_the_highest_avatar() {
        let (from, to) = friend_list_drag_points(Rect::new(80, 280, 170, 600));

        assert_eq!((from.x, from.y), (40, 830));
        assert_eq!((to.x, to.y), (40, 330));
        assert!(from.y > to.y);
    }

    #[test]
    fn current_hall_restore_drags_the_highest_avatar_downward() {
        let (from, to) = current_hall_restore_drag_points(Rect::new(80, 280, 170, 600));

        assert_eq!((from.x, from.y), (40, 330));
        assert_eq!((to.x, to.y), (40, 830));
        assert!(from.y < to.y);
        assert_eq!(MAX_FRIEND_LIST_DRAGS, 2);
    }
    use crate::config::AppConfig;
    use crate::runtime::ocr::{OcrArgs, OcrDevice, OcrLine, OcrRuntime, ProductionOcrDevice};
    use crate::runtime::ui::{FrameDemand, FramePublication, UiDevice, UiRuntime};
    use crate::ui::geometry::Rect;
    use crate::ui::state::{TemplateUiStateClassifier, UiTemplateArgs};

    fn start_test_ui_runtime(
        device: impl UiDevice,
        config: &AppConfig,
        queue_capacity: usize,
    ) -> UiRuntime {
        UiRuntime::start_with_state_classifier(
            device,
            queue_capacity,
            TemplateUiStateClassifier::new(
                UiTemplateArgs::default().resolve(&config.templates, &config.ocr),
                config.screen.clone(),
            ),
            config.resolve_stability_count(0),
        )
        .unwrap()
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Conversation {
        Hall,
        Friend(String),
    }

    #[test]
    fn fixed_secondary_chat_fixture_proves_stable_friend_row_and_title_first_identity_fallback() {
        let config = AppConfig::load(Path::new("config.yaml")).expect("load default config");
        let args = OcrArgs::default().resolve(&config.ocr);
        let runtime = OcrRuntime::start(
            ProductionOcrDevice::new(args).expect("initialize OCR device"),
            1,
        )
        .expect("start OCR runtime");
        let image = image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg")
            .expect("open fixed secondary-chat screenshot");
        let region: Rect = config.invite.friend_list_region.into();

        let handle = runtime.handle();
        let samples = (0..2)
            .map(|_| {
                matching_text_rows(
                    &handle,
                    &image,
                    region,
                    &normalize_lock_text("破鹿子"),
                    TextMatchMode::Exact,
                )
                .expect("locate exact friend row")
            })
            .collect::<Vec<_>>();

        assert!(samples.iter().all(|hits| hits.len() == 1));
        assert_eq!(samples[0][0].y, samples[1][0].y);
        assert!((region.x..region.right()).contains(&samples[0][0].x));
        assert!((region.y..region.bottom()).contains(&samples[0][0].y));
        assert!(
            (320..=380).contains(&samples[0][0].y),
            "unexpected row: {:?}",
            samples[0][0]
        );

        let routine = FriendDeliveryRoutineConfig::from_app(&config);
        assert_eq!(
            detect_conversation_identity(&handle, &image, &routine, &normalize_lock_text("香菜"))
                .expect("recognize title identity"),
            ConversationIdentityEvidence::Title
        );
        assert_eq!(
            detect_conversation_identity(&handle, &image, &routine, &normalize_lock_text("星念"))
                .expect("recognize chat-region fallback identity"),
            ConversationIdentityEvidence::ChatRegion
        );
        let mut fast_routine = routine.clone();
        fast_routine.fast_match = true;
        assert_eq!(
            conversation_confirmation_key(
                &handle,
                &image,
                &fast_routine,
                &normalize_lock_text("不存在的好友")
            )
            .expect("accept any stable private-chat title in fast mode"),
            Some(normalize_lock_text("香菜"))
        );
        fast_routine.fast_match = false;
        assert_eq!(
            conversation_confirmation_key(
                &handle,
                &image,
                &fast_routine,
                &normalize_lock_text("不存在的好友")
            )
            .expect("strict mode still requires the target identity"),
            None
        );

        let state = Arc::new(Mutex::new(TestUiState {
            conversation: Conversation::Hall,
            pasted: Vec::new(),
            selected_friends: Vec::new(),
            hall_clicks: 0,
            scrolls: 0,
            send_attempts: 0,
            fail_send_attempt: None,
        }));
        let ui_runtime = start_test_ui_runtime(
            RecordingDevice {
                frame: image,
                state: Arc::clone(&state),
                friend_rows: Vec::new(),
                hall_point: (-1, -1),
            },
            &config,
            1,
        );
        let mut bounded_config = routine;
        bounded_config.poll_ms = 1;
        bounded_config.timeout_ms = 5;
        bounded_config.stable_count = 2;
        let failure = ui_runtime
            .handle()
            .submit(BoundedFixtureSearchRoutine {
                ocr: handle,
                config: bounded_config,
                recipient: "绝不会存在的好友".to_string(),
            })
            .expect("submit bounded friend traversal")
            .wait()
            .expect("wait for bounded friend traversal")
            .expect_err("missing friend must not be selected");
        assert_eq!(failure.stage(), "locate_friend");
        assert_eq!(failure.certainty(), InputCertainty::ConfirmedFailure);
        let scrolls = state.lock().unwrap().scrolls;
        assert!(scrolls > 0);
        assert!(scrolls <= MAX_FRIEND_LIST_DRAGS);
        ui_runtime
            .shutdown()
            .expect("shutdown fixed-fixture UI runtime");
        runtime.shutdown().expect("shutdown OCR runtime");
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

    struct BoundedFixtureSearchRoutine {
        ocr: OcrRuntimeHandle,
        config: FriendDeliveryRoutineConfig,
        recipient: String,
    }

    impl sealed::UiRoutineSealed for BoundedFixtureSearchRoutine {}

    impl UiRoutine for BoundedFixtureSearchRoutine {
        type Output = std::result::Result<Point, UiRoutineFailure>;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            locate_stable_friend_row(context, &self.ocr, &self.config, &self.recipient)
        }
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

        fn drag_point(&mut self, _from_x: i32, _from_y: i32, _to_x: i32, _to_y: i32) -> Result<()> {
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

    struct TitleFallbackOcrDevice {
        state: Arc<Mutex<TestUiState>>,
        friend_list_size: (u32, u32),
        title_size: (u32, u32),
        friend_chat_size: (u32, u32),
    }

    impl OcrDevice for TitleFallbackOcrDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            let size = (image.width(), image.height());
            if size == self.friend_list_size {
                return Ok(vec![OcrLine {
                    text: "萌萌".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(5, 70, 80, 30),
                }]);
            }
            if size == self.title_size {
                let title = match &self.state.lock().unwrap().conversation {
                    Conversation::Hall => "当前大厅",
                    Conversation::Friend(_) => "备注未显示昵称",
                };
                return Ok(vec![OcrLine {
                    text: title.to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(0, 0, image.width(), image.height()),
                }]);
            }
            if size == self.friend_chat_size
                && matches!(
                    self.state.lock().unwrap().conversation,
                    Conversation::Friend(_)
                )
            {
                return Ok(vec![OcrLine {
                    text: "原昵称(萌萌)".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(20, 80, 180, 30),
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
        let ui_runtime = start_test_ui_runtime(device, &config, 4);
        let friend_ui = FriendDeliveryUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            FriendDeliveryRoutineConfig::from_app(&config),
        );

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
    fn friend_conversation_falls_back_to_the_chat_region_when_title_has_no_name() {
        let mut config = AppConfig::load(Path::new("config.yaml")).unwrap();
        config.friend_delivery.fast_match = false;
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
        let friend_chat = config.invite.friend_chat_region;
        let hall_template = image::open(&config.templates.secondary_hall).unwrap();
        let hall_point = (
            config.screen.secondary_hall_rect.x + hall_template.width() as i32 / 2,
            config.screen.secondary_hall_rect.y + hall_template.height() as i32 / 2,
        );
        let ui_runtime = start_test_ui_runtime(
            RecordingDevice {
                frame: secondary_frame(&config),
                state: state.clone(),
                friend_rows: vec![(friend_list.y + 85, "萌萌".to_string())],
                hall_point,
            },
            &config,
            4,
        );
        let ocr_runtime = OcrRuntime::start(
            TitleFallbackOcrDevice {
                state: state.clone(),
                friend_list_size: (friend_list.width, friend_list.height),
                title_size: (title.width, title.height),
                friend_chat_size: (friend_chat.width, friend_chat.height),
            },
            4,
        )
        .unwrap();
        let friend_ui = FriendDeliveryUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            FriendDeliveryRoutineConfig::from_app(&config),
        );

        let outcome = friend_ui
            .submit(SendFriendDeliveries::new(
                vec![FriendDelivery::new("萌萌", ["报名成功"])],
                UiResidencyTarget::SecondaryCurrentHall,
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert!(outcome.is_complete());
        let state = state.lock().unwrap();
        assert_eq!(state.selected_friends, ["萌萌"]);
        assert_eq!(state.pasted, ["报名成功"]);
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
        let ui_runtime = start_test_ui_runtime(
            RecordingDevice {
                frame: secondary_frame(&config),
                state: state.clone(),
                friend_rows: Vec::new(),
                hall_point,
            },
            &config,
            4,
        );
        let ocr_runtime = OcrRuntime::start(
            FriendOcrDevice {
                state: state.clone(),
                friend_list_size: (friend_list.width, friend_list.height),
                title_size: (title.width, title.height),
            },
            4,
        )
        .unwrap();
        let hall_ui = HallBatchUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            FriendDeliveryRoutineConfig::from_app(&config),
        );

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
        let ui_runtime = start_test_ui_runtime(
            RecordingDevice {
                frame: secondary_frame(&config),
                state: state.clone(),
                friend_rows: vec![(friend_list.y + 85, "甲".to_string())],
                hall_point,
            },
            &config,
            4,
        );
        let ocr_runtime = OcrRuntime::start(
            SubstringFriendOcrDevice {
                state: state.clone(),
                friend_list_size: (friend_list.width, friend_list.height),
                title_size: (title.width, title.height),
            },
            4,
        )
        .unwrap();
        let friend_ui = FriendDeliveryUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            FriendDeliveryRoutineConfig::from_app(&config),
        );

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
        assert!(state.scrolls > 0);
        assert!(state.scrolls <= MAX_FRIEND_LIST_DRAGS);

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
        let ui_runtime = start_test_ui_runtime(
            RecordingDevice {
                frame: secondary_frame(&config),
                state: state.clone(),
                friend_rows: vec![(friend_list.y + 85, "甲".to_string())],
                hall_point,
            },
            &config,
            4,
        );
        let ocr_runtime = OcrRuntime::start(
            FriendOcrDevice {
                state: state.clone(),
                friend_list_size: (friend_list.width, friend_list.height),
                title_size: (title.width, title.height),
            },
            4,
        )
        .unwrap();
        let friend_ui = FriendDeliveryUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            FriendDeliveryRoutineConfig::from_app(&config),
        );
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

    struct RestorePrimaryRoutine {
        config: FriendDeliveryRoutineConfig,
    }

    impl sealed::UiRoutineSealed for RestorePrimaryRoutine {}

    impl UiRoutine for RestorePrimaryRoutine {
        type Output = std::result::Result<(), UiRoutineFailure>;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            restore_primary(context, &self.config)
        }
    }

    struct UnknownResidencyDevice {
        frame: DynamicImage,
        keys: Arc<Mutex<Vec<Key>>>,
    }

    impl UiDevice for UnknownResidencyDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(self.frame.clone())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            self.keys.lock().unwrap().push(key);
            Ok(())
        }
    }

    #[test]
    fn persistent_unknown_residency_never_authorizes_escape() {
        let config = AppConfig::load(Path::new("config.yaml")).unwrap();
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = start_test_ui_runtime(
            UnknownResidencyDevice {
                frame: DynamicImage::new_rgba8(
                    config.screen.expected_width,
                    config.screen.expected_height,
                ),
                keys: keys.clone(),
            },
            &config,
            2,
        );
        let mut routine_config = FriendDeliveryRoutineConfig::from_app(&config);
        routine_config.timeout_ms = 100;
        routine_config.poll_ms = 1;

        let failure = ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: routine_config,
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect_err("persistent unknown must time out without input");

        assert_eq!(failure.stage(), "observe_primary_residency");
        assert_eq!(failure.certainty(), InputCertainty::ConfirmedFailure);
        assert!(keys.lock().unwrap().is_empty());
        ui_runtime.shutdown().unwrap();
    }

    struct TransitionResidencyDevice {
        secondary: DynamicImage,
        primary: DynamicImage,
        keys: Arc<Mutex<Vec<Key>>>,
        escaped: bool,
        captures_after_escape: usize,
    }

    impl UiDevice for TransitionResidencyDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            if !self.escaped {
                return Ok(self.secondary.clone());
            }
            self.captures_after_escape += 1;
            if self.captures_after_escape <= 2 {
                return Ok(DynamicImage::new_rgba8(
                    self.primary.width(),
                    self.primary.height(),
                ));
            }
            Ok(self.primary.clone())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            self.keys.lock().unwrap().push(key);
            if key == Key::Escape {
                self.escaped = true;
            }
            Ok(())
        }
    }

    #[test]
    fn secondary_residency_escapes_once_then_waits_through_unknown_transition() {
        let config = AppConfig::load(Path::new("config.yaml")).unwrap();
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = start_test_ui_runtime(
            TransitionResidencyDevice {
                secondary: secondary_frame(&config),
                primary: primary_frame(&config),
                keys: keys.clone(),
                escaped: false,
                captures_after_escape: 0,
            },
            &config,
            2,
        );
        let mut routine_config = FriendDeliveryRoutineConfig::from_app(&config);
        routine_config.timeout_ms = 500;
        routine_config.poll_ms = 1;

        ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: routine_config,
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect("transition must settle back to primary");

        assert_eq!(*keys.lock().unwrap(), [Key::Escape]);
        ui_runtime.shutdown().unwrap();
    }

    #[derive(Default)]
    struct PreStabilizedRecoveryState {
        recovery_started: bool,
        captures_since_recovery: usize,
        captures_after_escape: usize,
        premature_escape: bool,
        escaped: bool,
        keys: Vec<Key>,
    }

    struct PreStabilizedRecoveryDevice {
        secondary: DynamicImage,
        primary: DynamicImage,
        state: Arc<Mutex<PreStabilizedRecoveryState>>,
    }

    impl UiDevice for PreStabilizedRecoveryDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            let mut state = self.state.lock().unwrap();
            if state.escaped {
                if state.premature_escape {
                    return Ok(self.secondary.clone());
                }
                state.captures_after_escape += 1;
                if state.captures_after_escape <= 2 {
                    return Ok(DynamicImage::new_rgba8(
                        self.primary.width(),
                        self.primary.height(),
                    ));
                }
                return Ok(self.primary.clone());
            }
            if state.recovery_started {
                state.captures_since_recovery += 1;
            }
            Ok(self.secondary.clone())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.keys.push(key);
            if key == Key::Escape {
                state.premature_escape = state.captures_since_recovery < 2;
                state.escaped = true;
            }
            Ok(())
        }
    }

    #[test]
    fn recovery_requires_fresh_stability_after_a_pre_stabilized_secondary_state() {
        let config = AppConfig::load(Path::new("config.yaml")).unwrap();
        let state = Arc::new(Mutex::new(PreStabilizedRecoveryState::default()));
        let ui_runtime = start_test_ui_runtime(
            PreStabilizedRecoveryDevice {
                secondary: secondary_frame(&config),
                primary: primary_frame(&config),
                state: Arc::clone(&state),
            },
            &config,
            4,
        );
        let handle = ui_runtime.handle();
        let subscription = handle
            .declare_frame_demand(FrameDemand::new(Duration::from_millis(1)).unwrap())
            .unwrap();
        loop {
            let publication = subscription.recv().unwrap();
            let FramePublication::Captured(frame) = publication else {
                panic!("pre-stabilization capture failed");
            };
            if frame
                .ui_state()
                .and_then(|observation| observation.classified())
                .and_then(|tracked| tracked.stable_kind())
                == Some(UiStateKind::Secondary)
            {
                break;
            }
        }
        subscription.cancel().unwrap();
        {
            let mut state = state.lock().unwrap();
            state.recovery_started = true;
            state.captures_since_recovery = 0;
        }

        let mut routine_config = FriendDeliveryRoutineConfig::from_app(&config);
        routine_config.timeout_ms = 300;
        routine_config.poll_ms = 1;
        handle
            .submit(RestorePrimaryRoutine {
                config: routine_config,
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect("recovery must wait for fresh stable evidence before Escape");

        let state = state.lock().unwrap();
        assert!(!state.premature_escape);
        assert!(state.captures_since_recovery >= 2);
        assert_eq!(state.keys, [Key::Escape]);
        drop(state);
        ui_runtime.shutdown().unwrap();
    }

    struct CaptureFailsAfterEscapeDevice {
        secondary: DynamicImage,
        escaped: bool,
    }

    impl UiDevice for CaptureFailsAfterEscapeDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            if self.escaped {
                bail!("capture unavailable after escape");
            }
            Ok(self.secondary.clone())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            if key == Key::Escape {
                self.escaped = true;
            }
            Ok(())
        }
    }

    #[test]
    fn capture_failure_after_escape_is_reported_as_input_result_unknown() {
        let config = AppConfig::load(Path::new("config.yaml")).unwrap();
        let ui_runtime = start_test_ui_runtime(
            CaptureFailsAfterEscapeDevice {
                secondary: secondary_frame(&config),
                escaped: false,
            },
            &config,
            4,
        );
        let mut routine_config = FriendDeliveryRoutineConfig::from_app(&config);
        routine_config.timeout_ms = 300;
        routine_config.poll_ms = 1;

        let failure = ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: routine_config,
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect_err("capture after Escape must remain result-unknown");

        assert_eq!(failure.stage(), "confirm_primary_residency");
        assert_eq!(failure.certainty(), InputCertainty::AfterInputUnknown);
        ui_runtime.shutdown().unwrap();
    }

    fn primary_frame(config: &AppConfig) -> DynamicImage {
        let mut frame =
            DynamicImage::new_rgba8(config.screen.expected_width, config.screen.expected_height);
        let friend = image::open(&config.templates.friend).unwrap();
        frame
            .copy_from(
                &friend,
                config.screen.friend_rect.x as u32,
                config.screen.friend_rect.y as u32,
            )
            .unwrap();
        frame
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
