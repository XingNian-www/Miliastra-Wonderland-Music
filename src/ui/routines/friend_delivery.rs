#[cfg(test)]
use miliastra_kernel::clock::Delay;
use miliastra_kernel::clock::{Clock, SystemClock};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use enigo::Key;
use image::{DynamicImage, GenericImageView};

use crate::change_detection::{ChangeFingerprint, change_stats, rect_chat_change_fingerprint};
#[cfg(test)]
use crate::config::AppConfig;
use crate::config::{
    InputTimingConfig, OcrConfig, OutputConfig, PointConfig, ScreenConfig, TemplateConfig,
};
use crate::geometry::{Point, Rect, crop_canvas};
use crate::runtime::ocr::{OcrPriority, OcrRuntimeHandle};
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure,
    UiRoutineProgressStage, UiRuntimeHandle, UiStateKind, UiStateObservation, UiSubmitError,
    sealed,
};
use crate::text::normalize_comparison_text as normalize_lock_text;
use crate::ui::locator::secondary_hall_search_rect;
use crate::ui::template::best_template_hit;

use super::state_observation::{
    UiStateObservationConfig, capture_normalized_ui_state, wait_for_stable_ui_kind,
    wait_for_stable_ui_kind_or_unknown,
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
    /// 搜好友列表时可尝试的其他名字（可选）。
    ///
    /// 用于只有 UID 的场景：外部聊天源给的身份证是 UID，好友列表上显示的是
    /// 昵称或备注，拿 UID 搜必然落空。`recipient` 仍是主键（去重/日志/分组都用它），
    /// 这里只补充「还能拿哪些名字去找」。
    #[allow(dead_code)]
    search_names: Vec<String>,
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
            search_names: Vec::new(),
            messages: messages.into_iter().map(Into::into).collect(),
        }
    }

    /// 追加可搜索的候选名（UID 反查得到）。
    pub(crate) fn with_search_names<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.search_names = names.into_iter().map(Into::into).collect();
        self
    }

    pub(crate) fn recipient(&self) -> &str {
        &self.recipient
    }

    /// 实际拿去搜好友列表的名字：有候选就用候选，否则退回 `recipient`。
    pub(crate) fn search_recipients(&self) -> Vec<String> {
        if self.search_names.is_empty() {
            vec![self.recipient.clone()]
        } else {
            self.search_names.clone()
        }
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
                    search_names: requested.search_names.clone(),
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

#[derive(Clone, Debug)]
pub(crate) struct SendHallBatch {
    messages: Vec<String>,
    residency: UiResidencyTarget,
    delay_ms: u64,
    ready: Option<InviteReadyGate>,
}

#[derive(Clone, Debug)]
struct InviteReadyGate {
    running: Arc<AtomicBool>,
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
            ready: None,
        }
    }

    pub(crate) fn after_invite_primary_stability(mut self, running: Arc<AtomicBool>) -> Self {
        self.ready = Some(InviteReadyGate { running });
        self
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
    fuzzy_friend_match: bool,
    secondary_hall_search_region: Rect,
    chat_click: PointConfig,
    send_enabled: Arc<RwLock<bool>>,
    pub(super) after_activate_ms: u64,
    open_chat_ms: u64,
    pub(super) click_ms: u64,
    text_ms: u64,
    send_ms: u64,
    friend_step_ms: u64,
    timeout_ms: u64,
    pub(super) poll_ms: u64,
    pub(super) stable_count: u32,
    /// 热更新共享句柄（阶段 7）：保存 friend_delivery.auto_retry_count 后立即生效。
    auto_retry_count: Arc<RwLock<u32>>,
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
    /// 好友投递自动重试次数共享句柄（热更新）。
    pub(crate) auto_retry_count: Arc<RwLock<u32>>,
    /// 游戏内回复发送开关共享句柄（热更新）。
    pub(crate) send_enabled: Arc<RwLock<bool>>,
    pub(crate) friend_list_region: Rect,
    pub(crate) fuzzy_friend_match: bool,
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
            fuzzy_friend_match: source.fuzzy_friend_match,
            secondary_hall_search_region: secondary_hall_search_rect(
                hall_anchor,
                friend_list_region,
            ),
            chat_click: source.output.chat_click_2,
            send_enabled: source.send_enabled,
            after_activate_ms: source.input_timing.after_activate_ms,
            open_chat_ms: source.input_timing.open_chat_ms,
            click_ms: source.input_timing.click_ms,
            text_ms: source.input_timing.text_ms,
            send_ms: source.input_timing.send_ms,
            friend_step_ms: source.friend_step_ms,
            timeout_ms: source.timeout_ms,
            poll_ms: source.poll_ms.max(10),
            stable_count: source.stable_count,
            auto_retry_count: source.auto_retry_count,
            stable_mean_threshold: source.ocr.change_mean_threshold,
            stable_changed_ratio_threshold: source.ocr.change_pixel_threshold,
            secondary_hall_template: source.templates.secondary_hall.clone(),
            template_threshold: source.templates.marker_threshold,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_app(config: &AppConfig) -> Self {
        // 测试辅助：热更新共享值从给定配置初始化（与运行态 ApplicationRuntime 一致）。
        let live = crate::config::LiveConfigs::from_config(config);
        Self::resolve(FriendDeliveryRoutineConfigSource {
            screen: &config.screen,
            templates: &config.templates,
            ocr: &config.ocr,
            output: &config.output,
            input_timing: &config.timing.input,
            auto_retry_count: live.friend_delivery_auto_retry_count,
            send_enabled: live.output_send_enabled,
            friend_list_region: config.invite.friend_list_region.into(),
            fuzzy_friend_match: config.invite.fuzzy_friend_match,
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

    pub(super) fn friend_list_region(&self) -> Rect {
        self.friend_list_region
    }

    pub(super) fn friend_step_ms(&self) -> u64 {
        self.friend_step_ms
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

#[derive(Clone, Debug)]
struct FriendOcrRow {
    text: String,
    point: Point,
}

const FUZZY_FRIEND_MIN_SCORE: f32 = 0.55;
const FUZZY_FRIEND_MIN_MARGIN: f32 = 0.12;

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
                        if retry
                            < *config
                                .auto_retry_count
                                .read()
                                .expect("好友投递自动重试次数共享锁已中毒")
                            && retryable_certainty(attempt.failure.certainty()) =>
                    {
                        retry = retry.saturating_add(1);
                        // 首次重试只是 debug：这是流程内的正常兜底，玩家还没感知到失败。
                        // 反复重试才说明环境有问题，那时才值得进主日志（见下面升级逻辑）。
                        let limit = *config
                            .auto_retry_count
                            .read()
                            .expect("好友投递自动重试次数共享锁已中毒");
                        // 文案注意：这里**本次重试正要执行**，所以 retry==limit 时
                        // 说的是「最后一次自动重试」，不能说「已用尽」。
                        if retry < limit {
                            log::debug!(
                                "好友投递「{}」未送达，执行第 {}/{} 次自动重试（步骤：「{}」），本次失败可重试，流程继续：{}",
                                delivery.recipient(),
                                retry,
                                limit,
                                attempt.failure.stage_label(),
                                attempt.failure.reason()
                            );
                        } else {
                            log::warn!(
                                "好友投递「{}」已连续 {} 次未送达，第 {}/{} 次为最后一次自动重试（步骤：「{}」），若再失败本次投递将放弃：{}",
                                delivery.recipient(),
                                retry.saturating_sub(1),
                                retry,
                                limit,
                                attempt.failure.stage_label(),
                                attempt.failure.reason()
                            );
                        }
                    }
                    Err(attempt) => {
                        if let Some(index) = attempt.unknown_message_index {
                            outcomes[recipient_index].message_statuses[index] =
                                FriendDeliveryMessageStatus::ResultUnknown;
                        }
                        // 这里是**最终放弃**分支。以前完全没有日志：
                        // auto_retry_count 默认为 0 时根本进不了上面的重试分支，
                        // 于是投递失败在日志里彻底查不到（复核 P1-2 指出的漏报）。
                        log::warn!(
                            "好友投递放弃（步骤：「{}」）：收件人「{}」的这批消息本次未送达，不会自动重试，收件人需重新发起请求（{}）：{}",
                            attempt.failure.stage_label(),
                            delivery.recipient(),
                            attempt.failure.certainty().label(),
                            attempt.failure.reason()
                        );
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

    if !*config
        .send_enabled
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        || request.messages.is_empty()
    {
        sent = request.messages.len();
    } else if let Err(failure) = normalize_hall_batch_start(
        context,
        ocr,
        config,
        request.residency,
        request.ready.as_ref(),
        &mut primary_chat_opened,
    ) {
        status = HallBatchStatus::Failed(failure);
    } else {
        for (index, message) in request.messages.iter().enumerate() {
            if index > 0 {
                if let Err(failure) = wait_ms(
                    context,
                    request.delay_ms,
                    "wait_hall_batch",
                    InputCertainty::AfterInputUnknown,
                ) {
                    status = HallBatchStatus::Failed(failure);
                    break;
                }
            }
            match send_current_chat_message_guarded(
                context,
                config,
                message,
                request.ready.as_ref(),
            ) {
                Ok(()) => sent += 1,
                Err(failure) => {
                    status = HallBatchStatus::Failed(failure);
                    break;
                }
            }
        }
    }

    // 就绪闸门失败后不再按键恢复，避免取消后意外提交残留输入。
    let residency = if request.ready.is_some() && matches!(&status, HallBatchStatus::Failed(_)) {
        match &status {
            HallBatchStatus::Failed(failure) => UiResidencyOutcome::Failed(failure.clone()),
            HallBatchStatus::Complete => unreachable!(),
        }
    } else {
        match finish_hall_batch_residency(
            context,
            ocr,
            config,
            request.residency,
            primary_chat_opened,
        ) {
            Ok(()) => UiResidencyOutcome::Confirmed(request.residency),
            Err(failure) => UiResidencyOutcome::Failed(failure),
        }
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
    ready: Option<&InviteReadyGate>,
    primary_chat_opened: &mut bool,
) -> std::result::Result<(), UiRoutineFailure> {
    if let Some(gate) = ready {
        check_invite_ready_running(gate)?;
    }
    context
        .device()
        .ensure_ready(config.after_activate_ms)
        .map_err(|error| before_input_failure("prepare_hall_batch", error))?;
    if let Some(gate) = ready {
        check_invite_ready_running(gate)?;
        restore_primary(context, config)?;
        confirm_invite_primary_stability(context, config, gate)?;
    }
    match target {
        UiResidencyTarget::Primary => {
            if ready.is_none() {
                restore_primary(context, config)?;
            }
            if let Some(gate) = ready {
                check_invite_ready_running(gate)?;
            }
            context
                .device()
                .press_key(Key::Return)
                .map_err(|error| after_input_failure("open_primary_chat", error))?;
            *primary_chat_opened = true;
            wait_ms(
                context,
                config.open_chat_ms,
                "open_primary_chat",
                InputCertainty::AfterInputUnknown,
            )?;
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
            .map_err(|error| after_input_failure("close_primary_chat", error))?;
        wait_ms(
            context,
            config.click_ms,
            "close_primary_chat",
            InputCertainty::AfterInputUnknown,
        )?;
    }
    // 结束时的驻留恢复**必须保留**。
    //
    // 一度以为可以省掉：AI 聊天路径 ready=None，VerifySecondary 三步都是空操作，
    // 发送只做 Focus/Input/Submit，看起来不会离开二级大厅。
    // 但线上日志证明界面**确实会漂**：
    //   「二级值守当前不在二级聊天界面: primary:hall，保留逻辑模式并等待驻留恢复」
    // 省掉它会让本函数直接返回 Ok(())，对外报 Confirmed(SecondaryCurrentHall)——
    // 界面实际不在这里却宣称已确认，等于假确认。
    // MUST_READ 明确要求 UI 租约与安全协作等待必须保留，这里退回原实现。
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
    // 收件人可能只是 UID（外部源），真正能搜列表的名字在 search_names 里。
    let recipients = delivery.search_recipients();
    open_friend_conversation(context, ocr, config, &recipients).map_err(safe_attempt_failure)?;

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
    recipients: &[String],
) -> std::result::Result<(), UiRoutineFailure> {
    ensure_secondary_chat(context, config)?;
    let point = locate_stable_friend_row(context, ocr, config, recipients)?;
    context
        .device()
        .click_point(point.x, point.y)
        .map_err(|error| after_input_failure("select_friend", error))?;
    wait_ms(
        context,
        config.friend_step_ms,
        "select_friend",
        InputCertainty::AfterInputUnknown,
    )?;
    confirm_friend_conversation(context, ocr, config, recipients, point)
}

pub(super) fn send_current_chat_message(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    message: &str,
) -> std::result::Result<(), UiRoutineFailure> {
    send_current_chat_message_guarded(context, config, message, None)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatInputStep {
    VerifySecondary,
    Focus,
    Input,
    Submit,
}

fn run_verified_chat_input(
    mut action: impl FnMut(ChatInputStep) -> Result<(), UiRoutineFailure>,
) -> Result<(), UiRoutineFailure> {
    for step in [
        ChatInputStep::VerifySecondary,
        ChatInputStep::Focus,
        ChatInputStep::VerifySecondary,
        ChatInputStep::Input,
        ChatInputStep::VerifySecondary,
        ChatInputStep::Submit,
    ] {
        action(step)?;
    }
    Ok(())
}

fn send_current_chat_message_guarded(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    message: &str,
    ready: Option<&InviteReadyGate>,
) -> Result<(), UiRoutineFailure> {
    if !*config
        .send_enabled
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Ok(());
    }
    run_verified_chat_input(|step| {
        if let Some(gate) = ready {
            check_invite_ready_running(gate)?;
        }
        match step {
            ChatInputStep::VerifySecondary => {
                if ready.is_some() {
                    // 每次capture都会刷新模板分类，不能使用打开聊天前的旧状态。
                    capture_normalized_ui_state(
                        context,
                        &config.state_observation(),
                        "confirm_ready_secondary",
                        InputCertainty::ConfirmedFailure,
                    )?;
                    match context.latest_ui_state() {
                        Some(UiStateObservation::Classified(state))
                            if state.classification().kind() == UiStateKind::Secondary
                                && state.stable_kind() == Some(UiStateKind::Secondary) => {}
                        _ => {
                            return Err(UiRoutineFailure::new(
                                InputCertainty::ConfirmedFailure,
                                "confirm_ready_secondary",
                                "secondary chat is not freshly confirmed; readiness text will not be sent",
                            ));
                        }
                    }
                    check_invite_ready_running(ready.expect("checked above"))?;
                }
            }
            ChatInputStep::Focus => {
                context
                    .device()
                    .click_point(config.chat_click.x, config.chat_click.y)
                    .map_err(|error| after_input_failure("focus_friend_message", error))?;
                wait_ms(
                    context,
                    config.click_ms,
                    "focus_friend_message",
                    InputCertainty::AfterInputUnknown,
                )?;
            }
            ChatInputStep::Input => {
                if let Err(paste_error) = context.device().paste_text(message, config.text_ms) {
                    if ready.is_some() {
                        // 输入可能已经部分生效；就绪消息不能盲目键入重放，也不能继续按发送键。
                        return Err(UiRoutineFailure::new(
                            InputCertainty::AfterInputUnknown,
                            "input_invite_ready_message",
                            format!("readiness paste failed; do not replay: {paste_error:#}"),
                        ));
                    }
                    if let Err(input_error) = context.device().input_text(message, config.text_ms) {
                        return Err(UiRoutineFailure::new(
                            InputCertainty::AfterInputUnknown,
                            "input_friend_message",
                            format!(
                                "paste failed: {paste_error:#}; text input failed: {input_error:#}"
                            ),
                        ));
                    }
                }
            }
            ChatInputStep::Submit => {
                context.device().press_key(Key::Return).map_err(|error| {
                    UiRoutineFailure::new(
                        InputCertainty::AfterInputUnknown,
                        "send_friend_message",
                        format!("{error:#}"),
                    )
                })?;
                wait_ms(
                    context,
                    config.send_ms,
                    "send_friend_message",
                    InputCertainty::AfterInputUnknown,
                )?;
            }
        }
        Ok(())
    })
}

fn check_invite_ready_running(gate: &InviteReadyGate) -> Result<(), UiRoutineFailure> {
    if gate.running.load(Ordering::SeqCst) {
        Ok(())
    } else {
        Err(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "invite_ready_cancelled",
            "invitation readiness sending was cancelled",
        ))
    }
}

#[cfg(test)]
fn wait_for_invite_primary_stability(
    clock: &dyn Clock,
    delay: &dyn Delay,
    timeout: Duration,
    poll: Duration,
    running: &AtomicBool,
    mut observe: impl FnMut() -> Result<bool, UiRoutineFailure>,
) -> Result<(), UiRoutineFailure> {
    wait_for_invite_primary_stability_with_port(clock, timeout, poll, running, |wait| {
        if let Some(duration) = wait {
            delay.wait(duration);
            Ok(false)
        } else {
            observe()
        }
    })
}

fn wait_for_invite_primary_stability_with_port(
    clock: &dyn Clock,
    timeout: Duration,
    poll: Duration,
    running: &AtomicBool,
    mut port: impl FnMut(Option<Duration>) -> Result<bool, UiRoutineFailure>,
) -> Result<(), UiRoutineFailure> {
    let deadline = clock.now() + timeout;
    let mut stable_since = None;
    let mut last_sample = None;
    let poll = poll.clamp(Duration::from_millis(10), Duration::from_millis(100));
    loop {
        if !running.load(Ordering::SeqCst) {
            return Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "invite_ready_cancelled",
                "cancelled before readiness confirmation",
            ));
        }
        let primary = port(None).unwrap_or(false);
        let now = clock.now();
        if !running.load(Ordering::SeqCst) {
            return Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "invite_ready_cancelled",
                "cancelled during readiness observation",
            ));
        }
        // 长时间没有新观测也不能累计稳定时间。
        if last_sample
            .is_some_and(|last| now.saturating_duration_since(last) > Duration::from_secs(1))
        {
            stable_since = None;
        }
        last_sample = Some(now);
        if primary {
            let since = *stable_since.get_or_insert(now);
            if now <= deadline && now.saturating_duration_since(since) >= Duration::from_secs(2) {
                return Ok(());
            }
        } else {
            stable_since = None;
        }
        if now >= deadline {
            return Err(UiRoutineFailure::new(
                InputCertainty::ConfirmedFailure,
                "confirm_invite_primary_stability",
                "primary UI was not continuously confirmed for two seconds before timeout",
            ));
        }
        port(Some(poll.min(deadline.saturating_duration_since(now))))?;
    }
}

fn confirm_invite_primary_stability(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    gate: &InviteReadyGate,
) -> Result<(), UiRoutineFailure> {
    context.publish_progress(UiRoutineProgressStage::ConfirmingUi);
    wait_for_invite_primary_stability_with_port(
        &SystemClock,
        Duration::from_millis(config.timeout_ms).max(Duration::from_secs(3)),
        Duration::from_millis(config.poll_ms),
        &gate.running,
        |wait| {
            if let Some(duration) = wait {
                context.wait_cancellable(duration).map_err(|failure| {
                    UiRoutineFailure::new(
                        InputCertainty::ConfirmedFailure,
                        "confirm_invite_primary_stability",
                        failure.reason(),
                    )
                })?;
                return Ok(false);
            }
            capture_normalized_ui_state(
                context,
                &config.state_observation(),
                "confirm_invite_primary_stability",
                InputCertainty::ConfirmedFailure,
            )?;
            Ok(
                matches!(context.latest_ui_state(),Some(UiStateObservation::Classified(state)) if state.classification().kind()==UiStateKind::Primary && state.stable_kind()==Some(UiStateKind::Primary)),
            )
        },
    )
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
    let state = wait_for_stable_ui_kind_or_unknown(
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
    if matches!(state, UiStateKind::GameGate | UiStateKind::Overworld) {
        return Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "open_secondary_chat",
            "chat residency is unavailable at the game gate or in the overworld",
        ));
    }
    context
        .device()
        .press_key(Key::Return)
        .map_err(|error| after_input_failure("open_secondary_chat", error))?;
    wait_ms(
        context,
        config.open_chat_ms,
        "open_secondary_chat",
        InputCertainty::AfterInputUnknown,
    )?;
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
    recipients: &[String],
) -> std::result::Result<Point, UiRoutineFailure> {
    let mut pages = Vec::<ChangeFingerprint>::new();
    for drag_index in 0..=MAX_FRIEND_LIST_DRAGS {
        match search_current_friend_page(context, ocr, config, recipients)? {
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
            .map_err(|error| after_input_failure("fingerprint_friend_list", error))?;
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
            .map_err(|error| after_input_failure("drag_friend_list", error))?;
        wait_friend_list_stable(context, config)?;
    }
    if config.fuzzy_friend_match {
        return search_top_friend_candidate(context, ocr, config, recipients);
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "locate_friend",
        "no unique stable friend row was found within two list drags",
    ))
}

fn search_top_friend_candidate(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recipients: &[String],
) -> std::result::Result<Point, UiRoutineFailure> {
    let (from, to) = friend_list_drag_points(config.friend_list_region);
    context
        .device()
        .drag_point(to.x, to.y, from.x, from.y)
        .map_err(|error| after_input_failure("scroll_friend_list_to_top", error))?;
    wait_friend_list_stable(context, config)?;
    let image = capture_normalized(
        context,
        config,
        "fuzzy_friend_list",
        InputCertainty::AfterInputUnknown,
    )?;
    let rows = ocr_friend_rows(ocr, &image, config.friend_list_region)?;
    // 多候选：每个候选名各自打分，取所有候选里的最高分。
    let targets = normalize_friend_targets(recipients);
    let mut candidates = rows
        .into_iter()
        .map(|row| {
            let score = targets
                .iter()
                .map(|target| score_friend_candidate(target, &row.text))
                .fold(0.0_f32, f32::max);
            (score, row)
        })
        .filter(|(score, _)| *score >= FUZZY_FRIEND_MIN_SCORE)
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    let Some((best_score, best)) = candidates.first() else {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_friend_fuzzy",
            "no fuzzy friend candidate reached the score threshold",
        ));
    };
    if candidates
        .get(1)
        .is_some_and(|(score, _)| best_score - score < FUZZY_FRIEND_MIN_MARGIN)
    {
        return Err(UiRoutineFailure::new(
            InputCertainty::AfterInputUnknown,
            "locate_friend_fuzzy",
            "fuzzy friend candidates were not sufficiently distinct",
        ));
    }
    Ok(best.point)
}

fn ocr_friend_rows(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    region: Rect,
) -> std::result::Result<Vec<FriendOcrRow>, UiRoutineFailure> {
    let crop = crop_canvas(image, region)
        .map_err(|error| after_input_failure("crop_friend_list", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| after_input_failure("ocr_friend_list", error.into()))?;
    Ok(lines
        .into_iter()
        .filter_map(|line| {
            let text = line.text.trim().to_string();
            (!text.is_empty()).then_some(FriendOcrRow {
                text,
                point: Point::new(
                    region.x + line.bbox.center().x,
                    region.y + line.bbox.center().y,
                ),
            })
        })
        .collect())
}

fn score_friend_candidate(target: &str, candidate: &str) -> f32 {
    let target = normalize_lock_text(target);
    let candidate = normalize_lock_text(candidate);
    if target.is_empty() || candidate.is_empty() {
        return 0.0;
    }
    if candidate == target {
        return 1.0;
    }
    let common = longest_common_subsequence(target.chars().collect(), candidate.chars().collect());
    common as f32 / target.chars().count().max(candidate.chars().count()) as f32
}

fn longest_common_subsequence(left: Vec<char>, right: Vec<char>) -> usize {
    let mut table = vec![vec![0; right.len() + 1]; left.len() + 1];
    for (i, left_char) in left.iter().enumerate() {
        for (j, right_char) in right.iter().enumerate() {
            table[i + 1][j + 1] = if left_char == right_char {
                table[i][j] + 1
            } else {
                table[i][j + 1].max(table[i + 1][j])
            };
        }
    }
    table[left.len()][right.len()]
}

pub(super) fn friend_list_drag_points(region: Rect) -> (Point, Point) {
    let avatar_x = region.x.saturating_sub(FRIEND_AVATAR_TEXT_OFFSET_X).max(0);
    let edge_inset = (region.height as i32 / 12).min(FRIEND_DRAG_MAX_EDGE_INSET);
    (
        Point::new(avatar_x, region.bottom() - edge_inset),
        Point::new(avatar_x, region.y + edge_inset),
    )
}

/// 定位“当前大厅”行时使用的拖拽方向：从列表最高头像往下拖（`to -> from`）。
/// 按住列表内容往下拖会让列表向顶部翻，把固定在列表上方的“当前大厅”行带回可视区；
/// 与逐页向下找好友的 `friend_list_drag_points` 方向相反，不要合并成一个。
pub(super) fn current_hall_restore_drag_points(region: Rect) -> (Point, Point) {
    let (bottom, top) = friend_list_drag_points(region);
    (top, bottom)
}

/// 归一化一组候选名：去空、去重，顺序保留。
///
/// 归一化必须与 `matching_text_rows` 里对 OCR 行做的处理一致，
/// 否则候选与识别结果永远不相等。
fn normalize_friend_targets(targets: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::with_capacity(targets.len());
    for target in targets {
        let value = normalize_lock_text(target);
        if value.is_empty() || normalized.iter().any(|existing| existing == &value) {
            continue;
        }
        normalized.push(value);
    }
    normalized
}
fn search_current_friend_page(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    targets: &[String],
) -> std::result::Result<PageSearch, UiRoutineFailure> {
    let targets = normalize_friend_targets(targets);
    if targets.is_empty() {
        return Err(UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "validate_friend_name",
            "no usable friend name to search",
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
        let hits = matching_text_rows(ocr, &image, config.friend_list_region, &targets)?;
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
                    InputCertainty::AfterInputUnknown,
                    "locate_friend",
                    "multiple complete friend-name matches were visible in the strict list region",
                ));
            }
        }
        if sample + 1 < config.stable_count {
            wait_ms(
                context,
                config.poll_ms,
                "locate_friend",
                InputCertainty::AfterInputUnknown,
            )?;
        }
    }
    Ok(PageSearch::Missing)
}

fn confirm_friend_conversation(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    targets: &[String],
    selected_point: Point,
) -> std::result::Result<(), UiRoutineFailure> {
    context.publish_progress(UiRoutineProgressStage::ConfirmingUi);
    let targets = normalize_friend_targets(targets);
    let image = capture_normalized(
        context,
        config,
        "confirm_friend_conversation",
        InputCertainty::AfterInputUnknown,
    )?;
    let highlighted = highlighted_row(&image, selected_row_region(config));
    let matches = matching_text_rows(ocr, &image, config.friend_list_region, &targets)?;
    let fuzzy_selected = config.fuzzy_friend_match
        && matches.is_empty()
        && ocr_friend_rows(ocr, &image, config.friend_list_region)?
            .into_iter()
            .any(|candidate| {
                candidate.point.y.abs_diff(selected_point.y) <= FRIEND_ROW_Y_TOLERANCE as u32
            });
    if highlighted.is_some_and(|row| {
        point_in_rect(selected_point, row)
            && (matches.iter().any(|point| point_in_rect(*point, row)) || fuzzy_selected)
    }) {
        return Ok(());
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "confirm_friend_conversation",
        "highlighted friend row did not contain the requested OCR name",
    ))
}

fn selected_row_region(config: &FriendDeliveryRoutineConfig) -> Rect {
    Rect::new(
        config.friend_list_region.x.saturating_sub(70),
        config.friend_list_region.y,
        config.friend_list_region.width.saturating_add(110),
        config.friend_list_region.height,
    )
}

fn point_in_rect(point: Point, rect: Rect) -> bool {
    (rect.x..rect.right()).contains(&point.x) && (rect.y..rect.bottom()).contains(&point.y)
}

fn highlighted_row(image: &DynamicImage, region: Rect) -> Option<Rect> {
    highlighted_rows(image, region)
        .into_iter()
        .max_by_key(|row| row.height)
}

/// Read-only counterpart of active friend confirmation: the current frame must
/// contain exactly one selected sidebar row. No recipient or click point is assumed.
pub(crate) fn highlighted_conversation_row(image: &DynamicImage, region: Rect) -> Option<Rect> {
    let rows = highlighted_rows(image, region);
    match rows.as_slice() {
        [row] => Some(*row),
        _ => None,
    }
}

fn highlighted_rows(image: &DynamicImage, region: Rect) -> Vec<Rect> {
    let scan_region = region;
    let start = scan_region.y.max(0);
    let end = scan_region.bottom().min(image.height() as i32);
    let mut runs = Vec::new();
    let mut run_start = None;
    for y in start..end {
        let bright = (scan_region.x.max(0)..scan_region.right().min(image.width() as i32))
            .filter(|x| {
                let pixel = image.get_pixel(*x as u32, y as u32).0;
                let luminance =
                    (u16::from(pixel[0]) + u16::from(pixel[1]) + u16::from(pixel[2])) / 3;
                luminance >= 50
            })
            .count();
        let width =
            (scan_region.right().min(image.width() as i32) - scan_region.x.max(0)).max(1) as usize;
        if bright * 2 >= width {
            run_start.get_or_insert(y);
        } else if let Some(top) = run_start.take()
            && y - top >= 24
        {
            runs.push((top, y));
        }
    }
    if let Some(top) = run_start
        && end - top >= 24
    {
        runs.push((top, end));
    }
    runs.into_iter()
        .map(|(top, bottom)| {
            Rect::new(
                scan_region.x.max(0),
                top,
                (scan_region.right().min(image.width() as i32) - scan_region.x.max(0)).max(0)
                    as u32,
                (bottom - top) as u32,
            )
        })
        .collect()
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
    let state = wait_for_stable_ui_kind_or_unknown(
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
    if matches!(state, UiStateKind::GameGate | UiStateKind::Overworld) {
        return Err(UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "restore_primary_residency",
            "chat residency is unavailable at the game gate or in the overworld",
        ));
    }
    if state == UiStateKind::Unknown {
        // 到这里说明已经等待过稳定窗口、仍未得到可识别的一级界面。
        // 需要写清「此前尝试过什么」，否则维护者无法判断该 Esc 是第几次兜底。
        log::warn!(
            "一级界面驻留未就绪（步骤：恢复一级界面驻留）：已等待界面稳定超时，状态仍为无法识别，说明可能有残留面板遮挡；本次按 Esc 尝试关闭残留面板后重新确认一级界面，若仍失败则后续投递无法继续"
        );
    }
    context
        .device()
        .press_key(Key::Escape)
        .map_err(|error| after_input_failure("restore_primary_residency", error))?;
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
                "一级界面驻留确认超时（步骤：确认一级界面驻留）：好友按钮区域持续变化 {}ms 未稳定，但此前已确认过一级界面，因此按已确认结果继续，本次投递不受影响",
                PRIMARY_STABILITY_TIMEOUT_MS
            );
            return Ok(());
        }
        wait_ms(
            context,
            PRIMARY_STABILITY_POLL_MS,
            "confirm_primary_residency",
            InputCertainty::AfterInputUnknown,
        )?;
    }
}

fn restore_secondary_hall(
    context: &mut UiRoutineContext<'_>,
    _ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    ensure_secondary_chat(context, config)?;
    // “当前大厅”固定在会话列表上方。模板命中后只点击一次，不用好友高亮和 OCR 做二次确认。
    // 未命中时向列表顶部拖动（从最高头像往下拖），避免当前选中的好友私聊目标被误当成大厅。
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
        .map_err(|error| after_input_failure("locate_secondary_hall", error))?
        {
            let point = hit.center();
            context
                .device()
                .click_point(point.x, point.y)
                .map_err(|error| after_input_failure("select_secondary_hall", error))?;
            wait_ms(
                context,
                config.click_ms,
                "select_secondary_hall",
                InputCertainty::AfterInputUnknown,
            )?;
            // 点击大厅条目不是进入二级的成功凭据，发送前重新确认。
            wait_for_stable_ui_kind(
                context,
                config.state_observation(),
                Some(UiStateKind::Secondary),
                config.timeout_ms,
                "confirm_secondary_hall_selection",
                InputCertainty::AfterInputUnknown,
            )?;
            return Ok(());
        }
        let fingerprint = rect_chat_change_fingerprint(&image, config.friend_list_region)
            .map_err(|error| after_input_failure("fingerprint_secondary_hall_list", error))?;
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
            .map_err(|error| after_input_failure("drag_to_secondary_hall", error))?;
        wait_friend_list_stable(context, config)?;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        "restore_secondary_hall",
        "current-hall template was not found after two drags toward the list top",
    ))
}

/// 在一屏好友列表里找**任意一个**候选名，返回所有命中行。
///
/// 为什么要多候选：外部聊天源的发起人只有 UID。UID 反查出来可能是「备注」
/// 也可能是「游戏内昵称」（自动建身份把 API 昵称放 note、随机串放 nickname；
/// 手工映射反过来），没法预先知道好友列表上显示的是哪一个。两个都试。
///
/// 同一屏命中多个**不同**候选时仍返回全部命中，由调用方判歧义——
/// 悄悄挑一个等于赌运气点错人。
fn matching_text_rows(
    ocr: &OcrRuntimeHandle,
    image: &DynamicImage,
    region: Rect,
    normalized_targets: &[String],
) -> std::result::Result<Vec<Point>, UiRoutineFailure> {
    let crop = crop_canvas(image, region)
        .map_err(|error| after_input_failure("crop_ocr_confirmation", error))?;
    let lines = ocr
        .recognize_lines(crop, OcrPriority::UiConfirmation)
        .map_err(|error| after_input_failure("ocr_confirmation", error.into()))?;
    Ok(lines
        .into_iter()
        .filter_map(|line| {
            let recognized = normalize_lock_text(&line.text);
            let matched = normalized_targets
                .iter()
                .any(|target| recognized == *target);
            matched.then(|| {
                Point::new(
                    region.x + line.bbox.center().x,
                    region.y + line.bbox.center().y,
                )
            })
        })
        .collect())
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
    certainty: InputCertainty,
) -> std::result::Result<bool, UiRoutineFailure> {
    Ok(wait_for_stable_ui_kind(
        context,
        config.state_observation(),
        None,
        config.timeout_ms,
        stage,
        certainty,
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
    .map_err(|error| after_input_failure("observe_scrolled_friend_list", error))?;
    for _ in 0..confirmation_attempts(config) {
        wait_ms(
            context,
            config.poll_ms,
            "confirm_scrolled_friend_list",
            InputCertainty::AfterInputUnknown,
        )?;
        let current = rect_chat_change_fingerprint(
            &capture_normalized(
                context,
                config,
                "confirm_scrolled_friend_list",
                InputCertainty::AfterInputUnknown,
            )?,
            config.friend_list_region,
        )
        .map_err(|error| after_input_failure("confirm_scrolled_friend_list", error))?;
        if page_matches(&previous, &current, config) {
            return Ok(());
        }
        previous = current;
    }
    Err(UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
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

pub(super) fn after_input_failure(stage: &'static str, error: anyhow::Error) -> UiRoutineFailure {
    UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        stage,
        format!("{error:#}"),
    )
}

pub(super) use super::state_observation::wait_ms;

#[cfg(test)]
mod tests {
    use miliastra_kernel::clock::ManualClock;

    fn ready_failure() -> UiRoutineFailure {
        UiRoutineFailure::new(
            InputCertainty::ConfirmedFailure,
            "test_ready",
            "not confirmed",
        )
    }

    #[test]
    fn invite_ready_propagates_cooperative_wait_failure() {
        let clock = ManualClock::new(std::time::Instant::now());
        let running = AtomicBool::new(true);
        let mut observations = 0;
        let failure = wait_for_invite_primary_stability_with_port(
            &clock,
            Duration::from_secs(3),
            Duration::from_millis(100),
            &running,
            |wait| {
                if wait.is_some() {
                    Err(UiRoutineFailure::new(
                        InputCertainty::ConfirmedFailure,
                        "confirm_invite_primary_stability",
                        "cooperative wait cancelled",
                    ))
                } else {
                    observations += 1;
                    Ok(true)
                }
            },
        )
        .expect_err("a failed wait must not count as another observation");
        assert_eq!(observations, 1);
        assert_eq!(failure.stage(), "confirm_invite_primary_stability");
        assert_eq!(failure.certainty(), InputCertainty::ConfirmedFailure);
    }

    #[test]
    fn invite_ready_requires_full_two_seconds_of_primary_observations() {
        for (timeout, success) in [(1900, false), (2000, true)] {
            let clock = ManualClock::new(std::time::Instant::now());
            let start = clock.now();
            let running = AtomicBool::new(true);
            let mut samples = 0;
            let result = wait_for_invite_primary_stability(
                &clock,
                &clock,
                Duration::from_millis(timeout),
                Duration::from_millis(100),
                &running,
                || {
                    samples += 1;
                    Ok(true)
                },
            );
            assert_eq!(result.is_ok(), success);
            assert_eq!(
                clock.now().duration_since(start),
                Duration::from_millis(timeout)
            );
            assert!(samples >= 20);
        }
    }

    #[test]
    fn invite_ready_resets_stability_after_non_primary_or_observation_failure() {
        for failed_capture in [false, true] {
            let clock = ManualClock::new(std::time::Instant::now());
            let start = clock.now();
            let running = AtomicBool::new(true);
            wait_for_invite_primary_stability(
                &clock,
                &clock,
                Duration::from_secs(5),
                Duration::from_millis(100),
                &running,
                || {
                    if clock.now().duration_since(start) == Duration::from_millis(1500) {
                        if failed_capture {
                            Err(ready_failure())
                        } else {
                            Ok(false)
                        }
                    } else {
                        Ok(true)
                    }
                },
            )
            .unwrap();
            assert_eq!(
                clock.now().duration_since(start),
                Duration::from_millis(3600)
            );
        }
    }

    #[test]
    fn invite_ready_never_accepts_flickering_primary_or_unknown() {
        let clock = ManualClock::new(std::time::Instant::now());
        let running = AtomicBool::new(true);
        let mut samples = 0;
        assert!(
            wait_for_invite_primary_stability(
                &clock,
                &clock,
                Duration::from_secs(4),
                Duration::from_millis(100),
                &running,
                || {
                    samples += 1;
                    Ok(samples % 10 != 0)
                }
            )
            .is_err()
        );
    }

    #[test]
    fn invite_ready_does_not_count_a_stale_observation_gap() {
        let clock = ManualClock::new(std::time::Instant::now());
        let start = clock.now();
        let running = AtomicBool::new(true);
        let mut samples = 0;
        wait_for_invite_primary_stability(
            &clock,
            &clock,
            Duration::from_secs(6),
            Duration::from_millis(100),
            &running,
            || {
                samples += 1;
                if samples == 2 {
                    clock.advance(Duration::from_secs(2)).unwrap();
                }
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(
            clock.now().duration_since(start),
            Duration::from_millis(4100)
        );
    }

    #[test]
    fn invite_ready_cancellation_before_and_during_observation_prevents_success() {
        let clock = ManualClock::new(std::time::Instant::now());
        let running = AtomicBool::new(false);
        assert!(
            wait_for_invite_primary_stability(
                &clock,
                &clock,
                Duration::from_secs(4),
                Duration::from_millis(100),
                &running,
                || panic!("cancelled must not capture")
            )
            .is_err()
        );
        running.store(true, Ordering::SeqCst);
        assert!(
            wait_for_invite_primary_stability(
                &clock,
                &clock,
                Duration::from_secs(4),
                Duration::from_millis(100),
                &running,
                || {
                    running.store(false, Ordering::SeqCst);
                    Ok(true)
                }
            )
            .is_err()
        );
        assert!(
            check_invite_ready_running(&InviteReadyGate {
                running: Arc::new(AtomicBool::new(false))
            })
            .is_err()
        );
    }

    #[test]
    fn ready_message_requires_secondary_before_focus_input_and_submit() {
        for failed_check in 1..=3 {
            let mut calls = Vec::new();
            let mut checks = 0;
            let result = run_verified_chat_input(|step| {
                calls.push(step);
                if step == ChatInputStep::VerifySecondary {
                    checks += 1;
                    if checks == failed_check {
                        return Err(ready_failure());
                    }
                }
                Ok(())
            });
            assert!(result.is_err());
            assert!(!calls.contains(&ChatInputStep::Submit));
            if failed_check <= 2 {
                assert!(!calls.contains(&ChatInputStep::Input));
            }
        }
    }

    #[test]
    fn ready_message_submits_once_and_never_replays_an_unknown_send() {
        for fail_send in [false, true] {
            let mut calls = Vec::new();
            let result = run_verified_chat_input(|step| {
                calls.push(step);
                if step == ChatInputStep::Submit && fail_send {
                    return Err(ready_failure());
                }
                Ok(())
            });
            assert_eq!(result.is_ok(), !fail_send);
            assert_eq!(
                calls,
                [
                    ChatInputStep::VerifySecondary,
                    ChatInputStep::Focus,
                    ChatInputStep::VerifySecondary,
                    ChatInputStep::Input,
                    ChatInputStep::VerifySecondary,
                    ChatInputStep::Submit
                ]
            );
        }
    }

    #[test]
    fn ready_input_failure_does_not_press_enter() {
        let mut sent = 0;
        assert!(
            run_verified_chat_input(|step| {
                if step == ChatInputStep::Input {
                    return Err(ready_failure());
                }
                if step == ChatInputStep::Submit {
                    sent += 1;
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(sent, 0);
    }

    #[test]
    fn readiness_is_an_opt_in_standard_batch_requirement() {
        let request = SendHallBatch::new(["ready"], UiResidencyTarget::Primary, 0);
        assert!(request.ready.is_none());
        let running = Arc::new(AtomicBool::new(true));
        let request = request.after_invite_primary_stability(running.clone());
        assert_eq!(request.messages, ["ready"]);
        assert_eq!(request.residency, UiResidencyTarget::Primary);
        assert!(Arc::ptr_eq(&request.ready.unwrap().running, &running));
    }

    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use anyhow::{Result, bail};
    use enigo::Key;
    use image::{DynamicImage, GenericImage};

    use super::*;
    use crate::observation::chat::SECONDARY_TITLE_RECT;

    #[test]
    fn fuzzy_friend_candidate_score_accepts_lost_special_characters() {
        assert_eq!(score_friend_candidate("摩羯233★", "摩羯233"), 5.0 / 6.0);
        assert!(score_friend_candidate("摩羯233★", "摩羯233") >= FUZZY_FRIEND_MIN_SCORE);
        assert!(score_friend_candidate("摩羯233★", "天秤233") < FUZZY_FRIEND_MIN_SCORE);
    }

    #[test]
    fn fuzzy_friend_candidate_requires_a_clear_leader() {
        let candidates = [
            score_friend_candidate("小明★", "小明"),
            score_friend_candidate("小明★", "小红"),
        ];
        assert!(candidates[0] - candidates[1] >= FUZZY_FRIEND_MIN_MARGIN);
        assert!(
            score_friend_candidate("小明★", "小红") - score_friend_candidate("小明★", "小朋")
                < FUZZY_FRIEND_MIN_MARGIN
        );
    }

    #[test]
    fn fuzzy_friend_matching_is_disabled_by_default() {
        let config = AppConfig::default();
        assert!(!config.invite.fuzzy_friend_match);
    }

    #[test]
    fn friend_list_drag_runs_from_the_lowest_avatar_to_the_highest_avatar() {
        let (from, to) = friend_list_drag_points(Rect::new(80, 280, 170, 600));

        assert_eq!((from.x, from.y), (40, 830));
        assert_eq!((to.x, to.y), (40, 330));
        assert!(from.y > to.y);
    }

    #[test]
    fn current_hall_restore_drags_from_the_highest_avatar_downward() {
        let (from, to) = current_hall_restore_drag_points(Rect::new(80, 280, 170, 600));

        assert_eq!((from.x, from.y), (40, 330));
        assert_eq!((to.x, to.y), (40, 830));
        assert!(from.y < to.y);
    }

    use crate::config::AppConfig;
    use crate::geometry::Rect;
    #[cfg(feature = "ocr-mnn")]
    use crate::runtime::ocr::{OcrArgs, ProductionOcrDevice};
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};
    use crate::runtime::ui::{FrameDemand, FramePublication, UiDevice, UiRuntime};
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

    /// 读取固定截图里应当出现的好友名（每行一个）。
    ///
    /// 夹具图与真实昵称都不进公开仓库，所以期望值只能放在本地声明文件里。
    /// 返回 None 表示没有本地声明，调用方跳过断言而不是报假红。
    fn fixture_expected_friend_names() -> Option<Vec<String>> {
        let text = std::fs::read_to_string("tests/fixtures/ui/secondary-chat-friends.txt").ok()?;
        let names = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        (!names.is_empty()).then_some(names)
    }

    #[test]
    #[cfg(feature = "ocr-mnn")]
    fn fixed_secondary_chat_fixture_proves_stable_friend_row_and_title_first_identity_fallback() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml"))
            .expect("load default config");
        let args = OcrArgs::default().resolve(&config.ocr);
        let runtime = OcrRuntime::start(
            ProductionOcrDevice::new(args).expect("initialize OCR device"),
            1,
        )
        .expect("start OCR runtime");
        let image = image::open("tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg")
            .expect("open fixed secondary-chat screenshot");
        let region: Rect = config.invite.friend_list_region.into();

        // 夹具图与真实昵称都不进公开仓库（.gitignore 的 /* 覆盖 tests/），
        // 期望好友名改为读本地声明文件；没有声明就跳过，避免报假红。
        let Some(expected) = fixture_expected_friend_names() else {
            eprintln!(
                "跳过稳定好友行断言：缺少本地夹具声明 tests/fixtures/ui/secondary-chat-friends.txt"
            );
            return;
        };
        let handle = runtime.handle();
        let samples = (0..2)
            .map(|_| {
                // 只取第一行做「唯一命中」目标：多为名字时命中数必然 >1，
                // 这条用例验证的是同一行稳定定位。
                matching_text_rows(
                    &handle,
                    &image,
                    region,
                    &[normalize_lock_text(&expected[0])],
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
        assert!(
            !highlighted_row(&image, selected_row_region(&routine))
                .is_some_and(|row| { samples[0].iter().any(|point| point_in_rect(*point, row)) })
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
        assert_eq!(failure.certainty(), InputCertainty::AfterInputUnknown);
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

    #[cfg(feature = "ocr-mnn")]
    struct BoundedFixtureSearchRoutine {
        ocr: OcrRuntimeHandle,
        config: FriendDeliveryRoutineConfig,
        recipient: String,
    }

    #[cfg(feature = "ocr-mnn")]
    impl sealed::UiRoutineSealed for BoundedFixtureSearchRoutine {}

    #[cfg(feature = "ocr-mnn")]
    impl UiRoutine for BoundedFixtureSearchRoutine {
        type Output = std::result::Result<Point, UiRoutineFailure>;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            locate_stable_friend_row(
                context,
                &self.ocr,
                &self.config,
                std::slice::from_ref(&self.recipient),
            )
        }
    }

    impl UiDevice for RecordingDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            let mut frame = self.frame.clone();
            let selected_y = {
                let state = self.state.lock().unwrap();
                match &state.conversation {
                    Conversation::Hall => Some(365),
                    Conversation::Friend(friend) => self
                        .friend_rows
                        .iter()
                        .find(|(_, name)| name == friend)
                        .map(|(y, _)| *y),
                }
            };
            if let Some(y) = selected_y {
                for row_y in (y - 35).max(0)..(y + 35) {
                    for x in 10..290 {
                        frame.put_pixel(x, row_y as u32, image::Rgba([65, 73, 86, 255]));
                    }
                }
            }
            Ok(frame)
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
            if size.0 > self.friend_list_size.0 && size.1 <= self.friend_list_size.1 {
                let title = match &self.state.lock().unwrap().conversation {
                    Conversation::Hall => "当前大厅".to_string(),
                    Conversation::Friend(friend) => friend.clone(),
                };
                return Ok(vec![OcrLine {
                    text: title,
                    confidence: 1.0,
                    bbox: Rect::new(75, 35, 120, 30),
                }]);
            }
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
            if size.0 > self.friend_list_size.0 && size.1 <= self.friend_list_size.1 {
                return Ok(vec![OcrLine {
                    text: "萌萌".to_string(),
                    confidence: 1.0,
                    bbox: Rect::new(75, 35, 120, 30),
                }]);
            }
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
        let mut config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        // 真帧下每帧分类在 debug 里要数百毫秒，200ms 的观测预算会随并发负载抖动；
        // 放宽到 2s（与 invite 用例一致），等待逻辑本身仍由稳定性计数收敛。
        config.timing.workflow.default_timeout_ms = 2_000;
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
        let frame = secondary_hall_row_frame(&config);
        let friend_list = config.invite.friend_list_region;
        let title = SECONDARY_TITLE_RECT;
        let hall_point = hall_row_click_point(&frame, &config);
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
        assert_eq!(state.conversation, Conversation::Hall);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn friend_conversation_confirms_from_the_highlighted_row_when_title_has_no_name() {
        let mut config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        // 真帧下每帧分类在 debug 里要数百毫秒，200ms 的观测预算会随并发负载抖动；
        // 放宽到 2s（与 invite 用例一致），等待逻辑本身仍由稳定性计数收敛。
        config.timing.workflow.default_timeout_ms = 2_000;
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
        let frame = secondary_hall_row_frame(&config);
        let hall_point = hall_row_click_point(&frame, &config);
        let ui_runtime = start_test_ui_runtime(
            RecordingDevice {
                frame,
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
        assert_eq!(state.conversation, Conversation::Hall);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn hall_batch_sends_all_messages_and_confirms_secondary_residency() {
        let mut config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        // 真帧下每帧分类在 debug 里要数百毫秒，200ms 的观测预算会随并发负载抖动；
        // 放宽到 2s（与 invite 用例一致），等待逻辑本身仍由稳定性计数收敛。
        config.timing.workflow.default_timeout_ms = 2_000;
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
        let frame = secondary_hall_row_frame(&config);
        let hall_point = hall_row_click_point(&frame, &config);
        let ui_runtime = start_test_ui_runtime(
            RecordingDevice {
                frame,
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
        let mut config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        // 真帧下每帧分类在 debug 里要数百毫秒，200ms 的观测预算会随并发负载抖动；
        // 放宽到 2s（与 invite 用例一致），等待逻辑本身仍由稳定性计数收敛。
        config.timing.workflow.default_timeout_ms = 2_000;
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
        let mut config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        config.friend_delivery.auto_retry_count = 3;
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        // 真帧下每帧分类在 debug 里要数百毫秒，200ms 的观测预算会随并发负载抖动；
        // 放宽到 2s（与 invite 用例一致），等待逻辑本身仍由稳定性计数收敛。
        config.timing.workflow.default_timeout_ms = 2_000;
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
    fn persistent_unknown_residency_escapes_once_then_reports_primary_failure() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = start_test_ui_runtime(
            UnknownResidencyDevice {
                frame: unknown_frame(&config),
                keys: keys.clone(),
            },
            &config,
            2,
        );
        let mut routine_config = FriendDeliveryRoutineConfig::from_app(&config);
        // Classification of real 1920x1080 frames must fit before testing the post-Esc timeout.
        // Use the configured workflow budget so parallel test load cannot skip the input stage.
        routine_config.poll_ms = 1;

        let failure = ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: routine_config,
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect_err("persistent unknown cannot confirm primary after escape");

        assert_eq!(failure.stage(), "confirm_primary_residency");
        assert_eq!(failure.certainty(), InputCertainty::AfterInputUnknown);
        assert_eq!(*keys.lock().unwrap(), [Key::Escape]);
        ui_runtime.shutdown().unwrap();
    }

    struct StableUnknownThenPrimaryDevice {
        unknown: DynamicImage,
        primary: DynamicImage,
        keys: Arc<Mutex<Vec<Key>>>,
        escaped: bool,
        captures_after_escape: usize,
    }

    impl UiDevice for StableUnknownThenPrimaryDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            if !self.escaped {
                return Ok(self.unknown.clone());
            }
            self.captures_after_escape += 1;
            if self.captures_after_escape <= 2 {
                return Ok(self.unknown.clone());
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
    fn stable_unknown_residency_escapes_then_confirms_primary() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = start_test_ui_runtime(
            StableUnknownThenPrimaryDevice {
                unknown: unknown_frame(&config),
                primary: primary_frame(&config),
                keys: keys.clone(),
                escaped: false,
                captures_after_escape: 0,
            },
            &config,
            2,
        );
        let mut routine_config = FriendDeliveryRoutineConfig::from_app(&config);
        routine_config.timeout_ms = 5_000;
        routine_config.poll_ms = 1;

        ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: routine_config,
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect("stable unknown should authorize one escape");

        assert_eq!(*keys.lock().unwrap(), [Key::Escape]);
        ui_runtime.shutdown().unwrap();
    }

    struct TransitionResidencyDevice {
        secondary: DynamicImage,
        primary: DynamicImage,
        unknown: DynamicImage,
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
                return Ok(self.unknown.clone());
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
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = start_test_ui_runtime(
            TransitionResidencyDevice {
                secondary: secondary_frame(&config),
                primary: primary_frame(&config),
                unknown: unknown_frame(&config),
                keys: keys.clone(),
                escaped: false,
                captures_after_escape: 0,
            },
            &config,
            2,
        );
        let mut routine_config = FriendDeliveryRoutineConfig::from_app(&config);
        // 真实截图需完成两帧 Unknown 和连续 Primary 分类，沿用 fixture 的超时预算。
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
        unknown: DynamicImage,
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
                    return Ok(self.unknown.clone());
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
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let state = Arc::new(Mutex::new(PreStabilizedRecoveryState::default()));
        let ui_runtime = start_test_ui_runtime(
            PreStabilizedRecoveryDevice {
                secondary: secondary_frame(&config),
                primary: primary_frame(&config),
                unknown: unknown_frame(&config),
                state: Arc::clone(&state),
            },
            &config,
            4,
        );
        let handle = ui_runtime.handle();
        let subscription = handle
            .declare_frame_demand(FrameDemand::new(Duration::from_millis(1)).unwrap())
            .unwrap();
        // 有界等待：状态机若不再对真实二级帧给出稳定 Secondary，直接失败而不是永久空转。
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
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
            if std::time::Instant::now() >= deadline {
                panic!("预稳定阶段未在 5s 内观察到稳定 Secondary");
            }
        }
        subscription.cancel().unwrap();
        {
            let mut state = state.lock().unwrap();
            state.recovery_started = true;
            state.captures_since_recovery = 0;
        }

        let mut routine_config = FriendDeliveryRoutineConfig::from_app(&config);
        routine_config.timeout_ms = 2_000;
        routine_config.poll_ms = 10;
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
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let ui_runtime = start_test_ui_runtime(
            CaptureFailsAfterEscapeDevice {
                secondary: secondary_frame(&config),
                escaped: false,
            },
            &config,
            4,
        );
        let mut routine_config = FriendDeliveryRoutineConfig::from_app(&config);
        // 这条用例验证的是「按 Esc 后截图必然失败 → 结论必须是结果未知」，
        // 而 CaptureFailsAfterEscapeDevice 一旦 escaped 就恒返回 Err，所以失败本身是确定的。
        // timeout_ms 只决定「等状态机走完」的预算：原先 300ms 在并发跑全量测试时
        // 会被真实 UI 线程的调度延迟吃掉（实测 3 次全量跑偶发 1 次红），
        // 与同文件其它用例的 2000ms 口径对齐，不改断言语义。
        routine_config.timeout_ms = 2_000;
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

    /// 真实一级（千星大厅）截图。状态机的 `loading` 判据只认整屏极端色调的真实帧，
    /// 合成黑底帧会被判成 loading（`src/ui/state.rs` 第 55 节），代表不了真实执行路径；
    /// 这里统一改用实际运行路径上的真帧，让状态机按真实界面归类。
    fn primary_frame(config: &AppConfig) -> DynamicImage {
        load_real_frame("tests/fixtures/ui/hall-primary-1920x1080.jpg", config)
    }

    /// 真实二级（私聊）截图，列表**已滚动、无大厅行**（多数好友投递用例的默认帧）。
    fn secondary_frame(config: &AppConfig) -> DynamicImage {
        load_real_frame(
            "tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg",
            config,
        )
    }

    /// 真实二级（私聊）截图，左侧会话列表**含"大厅"那一行**：需要定位大厅行的用例用它。
    fn secondary_hall_row_frame(config: &AppConfig) -> DynamicImage {
        load_real_frame(
            "tests/fixtures/ui/secondary-with-hall-row-1920x1080.png",
            config,
        )
    }

    /// 真实残留面板（大厅信息页）截图：状态机判为 Unknown，代表“非一级/二级”的残留界面。
    fn unknown_frame(config: &AppConfig) -> DynamicImage {
        load_real_frame("tests/fixtures/ui/unknown-panel-1920x1080.png", config)
    }

    fn load_real_frame(path: &str, config: &AppConfig) -> DynamicImage {
        let frame =
            image::open(path).unwrap_or_else(|error| panic!("打不开真实帧 {path}: {error}"));
        assert_eq!(
            (frame.width(), frame.height()),
            (config.screen.expected_width, config.screen.expected_height),
            "{path} 尺寸必须与配置画布一致"
        );
        frame
    }

    #[test]
    fn tmp_time_classification() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        for path in [
            "tests/fixtures/ui/hall-primary-1920x1080.jpg",
            "tests/fixtures/ui/secondary-with-hall-row-1920x1080.png",
            "technical-docs/ui-state-evidence/hall-page.png",
        ] {
            let image = image::open(path).unwrap();
            let templates = UiTemplateArgs::default().resolve(&config.templates, &config.ocr);
            let mut classifier = TemplateUiStateClassifier::new(templates, config.screen.clone());
            let n = 8;
            let start = std::time::Instant::now();
            let mut kind = None;
            for _ in 0..n {
                kind = Some(
                    classifier
                        .classify_at(&image, std::time::Instant::now())
                        .unwrap()
                        .kind(),
                );
            }
            println!(
                "TIMING {path} => {kind:?} avg={:.1}ms",
                start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
            );
        }
    }

    /// 用与例程 `restore_secondary_hall` **完全相同**的匹配，算出真实帧里"大厅行"的点击点。
    /// 之前用例按配置 rect 中心硬算固定点，换成真实帧后与例程实际命中位置不符，`hall_clicks` 收不到。
    fn hall_row_click_point(frame: &DynamicImage, config: &AppConfig) -> (i32, i32) {
        let routine = FriendDeliveryRoutineConfig::from_app(config);
        let hit = best_template_hit(
            frame,
            Some(routine.secondary_hall_search_region),
            &routine.secondary_hall_template,
            routine.template_threshold,
        )
        .unwrap()
        .expect("真实二级帧里应能定位到大厅行");
        let center = hit.center();
        (center.x, center.y)
    }

    #[test]
    #[ignore = "local screenshot timing probe; requires dist/ui-flow/frames fixtures"]
    fn tmp_panel_probe_cost() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let threshold = config.templates.marker_threshold;
        let cases: Vec<(&str, std::path::PathBuf, Rect)> = vec![
            (
                "friend-panel",
                config.templates.friend_panel.clone(),
                config.moderation.friend_panel_region.into(),
            ),
            (
                "friend-search-panel",
                config.templates.friend_search_panel.clone(),
                config.moderation.search_panel_region.into(),
            ),
            (
                "friend-more-settings",
                config.templates.friend_more_settings.clone(),
                config.moderation.more_settings_region.into(),
            ),
            (
                "friend-block-chat",
                config.templates.friend_block_chat.clone(),
                config.moderation.block_chat_region.into(),
            ),
            (
                "friend-confirm",
                config.templates.friend_confirm.clone(),
                config.moderation.confirm_region.into(),
            ),
            (
                "invite-view-star",
                config.templates.invite_view_star.clone(),
                config.invite.view_star_region.into(),
            ),
            (
                "invite-goto-hall",
                config.templates.invite_goto_hall.clone(),
                config.invite.goto_hall_region.into(),
            ),
            (
                "invite-enter-hall",
                config.templates.invite_enter_hall.clone(),
                config.invite.enter_hall_region.into(),
            ),
        ];
        let frames = [
            "dist/ui-flow/frames/t1.jpg",
            "dist/ui-flow/frames/u1.jpg",
            "dist/ui-flow/frames/w1.jpg",
            "dist/ui-flow/frames/x1.jpg",
            "dist/ui-flow/frames/h1.jpg",
            "dist/ui-flow/frames/o1.jpg",
            "dist/ui-flow/frames/p1.jpg",
            "dist/ui-flow/frames/m1.jpg",
        ];
        for frame in frames {
            let image = image::open(frame).unwrap();
            let rounds = 1;
            println!("FRAME {frame}");
            for (name, path, region) in &cases {
                let start = std::time::Instant::now();
                let mut best: Option<(f32, i32, i32, u32, u32)> = None;
                for _ in 0..rounds {
                    let hit = best_template_hit(&image, Some(*region), path, threshold).unwrap();
                    best = hit.map(|hit| (hit.score, hit.x, hit.y, hit.width, hit.height));
                }
                println!(
                    "  {name} region={:?} hit={:?} avg={:.1}ms",
                    region,
                    best,
                    start.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds)
                );
            }
        }
    }
}
