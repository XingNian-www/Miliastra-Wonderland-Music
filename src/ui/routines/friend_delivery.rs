#[cfg(test)]
use miliastra_kernel::clock::Delay;
use miliastra_kernel::clock::{Clock, SystemClock};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

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
    UiStateObservationConfig, UiStateSnapshot, capture_normalized_ui_state, observe_ui_state_once,
    wait_for_stable_ui_kind, wait_for_stable_ui_kind_or_unknown,
};
use super::transaction::{EntryPolicy, FinishPolicy, UiTask, UiTaskSpec};

const FRIEND_ROW_Y_TOLERANCE: i32 = 6;
const MAX_FRIEND_LIST_DRAGS: usize = 2;
const FRIEND_AVATAR_TEXT_OFFSET_X: i32 = 40;
const FRIEND_DRAG_MAX_EDGE_INSET: i32 = 50;
const PRIMARY_STABILITY_POLL_MS: u64 = 100;
const PRIMARY_STABILITY_TIMEOUT_MS: u64 = 1_000;
/// 状态驱动恢复：单次恢复最多按 Esc 的次数（超过即失败，不再继续试探）。
const RECOVERY_MAX_ESC: u32 = 3;
/// 无进展熔断：动作后连续两次观察到同一 (kind,label)，判定为没有进展。
const RECOVERY_NO_PROGRESS_LIMIT: u32 = 2;
/// 恢复循环的最大采样步数（防止异常状态机空转）。
const RECOVERY_MAX_STEPS: u32 = 24;
/// 诊断历史保留条数（失败原因里附带，便于现场回放）。
const RECOVERY_HISTORY_LIMIT: usize = 12;
/// 连续 Unknown 至少采样这么多次、且持续这么久，才允许判定"无证据"。
const RECOVERY_UNKNOWN_MIN_SAMPLES: u32 = 3;
const RECOVERY_UNKNOWN_WINDOW_MS: u64 = 600;
/// Panel 必须连续确认这么多次（仍是 Panel）才允许按 Esc。
const RECOVERY_PANEL_CONFIRM_SAMPLES: u32 = 2;
/// Primary↔MainMenu 振荡：连续来回这么多次即熔断。
const RECOVERY_OSCILLATION_LIMIT: u32 = 2;

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

    /// 实际拿去搜好友列表的名字。
    ///
    /// 候选由装配层统一用 identity.friend_search_names 生成，这里只做兜底：
    /// 没给候选时用收件人本身，但**内部键（UUID）除外**——拿它去搜好友列表
    /// 只会白跑一轮，还会把 UUID 写进日志。返回空表时上层直接报「没有可搜索的
    /// 好友名」，不会盲点。
    pub(crate) fn search_recipients(&self) -> Vec<String> {
        if !self.search_names.is_empty() {
            return self.search_names.clone();
        }
        let recipient = self.recipient.trim();
        if recipient.is_empty() || uuid::Uuid::parse_str(recipient).is_ok() {
            return Vec::new();
        }
        vec![self.recipient.clone()]
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
    /// 会话级恢复能力边界：入口是某个二级会话、收尾只回到“二级界面类型”时置真。
    /// 当前包装器没有会话锚点，无法保证回到入口那个具体会话。
    session_restore_degraded: bool,
}

impl SendFriendDeliveriesOutcome {
    pub(crate) fn deliveries(&self) -> &[FriendDeliveryOutcome] {
        &self.deliveries
    }

    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        &self.residency
    }

    /// 是否发生了“只回到二级类型、未回到入口具体会话”的降级。
    pub(crate) fn session_restore_degraded(&self) -> bool {
        self.session_restore_degraded
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

    /// 抓一张一级界面整帧：当前在二级时先导航到一级，截完图再回到当前大厅二级。
    ///
    /// 返回值语义见 [`CapturedPrimaryOutcome`]；截图失败或返回驻守失败都会如实
    /// 写进产物，不会让调用方误以为成功。
    pub(crate) fn capture_primary_and_restore(
        &self,
    ) -> std::result::Result<UiOperation<CapturedPrimaryOutcome>, UiSubmitError> {
        self.runtime.submit(CapturePrimaryAndRestoreRoutine {
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
        let target = self.request.target;
        // 它本身就是恢复器：包装器不导航、不重复收尾（AssumeTarget + KeepCurrent）。
        let spec = UiTaskSpec::new(target, FinishPolicy::KeepCurrent)
            .with_entry(EntryPolicy::AssumeTarget);
        let task = UiTask::new(
            spec,
            EstablishResidencyExecution {
                target,
                ocr: self.ocr,
                config: self.config,
            },
        );
        let outcome = task.execute(context);
        match outcome.output {
            Some(Ok(())) => UiResidencyOutcome::Confirmed(target),
            Some(Err(failure)) => UiResidencyOutcome::Failed(failure),
            None => UiResidencyOutcome::Failed(outcome.entry_failure.clone().unwrap_or_else(|| {
                UiRoutineFailure::new(
                    InputCertainty::BeforeInput,
                    "prepare_residency",
                    "驻留恢复任务没有开始",
                )
            })),
        }
    }
}

/// 驻留恢复内层：保留原有恢复算法（它本身就是恢复器，不应被包装器再导航）。
struct EstablishResidencyExecution {
    target: UiResidencyTarget,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

impl sealed::UiRoutineSealed for EstablishResidencyExecution {}

impl UiRoutine for EstablishResidencyExecution {
    type Output = std::result::Result<(), UiRoutineFailure>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context.publish_progress(UiRoutineProgressStage::NormalizingStart);
        context
            .device()
            .ensure_ready(self.config.after_activate_ms)
            .map_err(|error| before_input_failure("prepare_residency", error))?;
        restore_residency(context, &self.ocr, &self.config, self.target)
    }
}

/// 一级界面截图产物：图像 + 是否发生过导航 + 截图之后实际所在的驻留界面。
///
/// 消费约定（Task-62 的 AI 聊天按此接入）：
/// - `Captured`：已拿到一级整帧。`residency` 说明返回驻守界面的结果：
///   本来就在一级时为 `Confirmed(Primary)`（`navigated == false`）；
///   从二级导航过去时为 `Confirmed(SecondaryCurrentHall)`（`navigated == true`）；
///   若返回驻守失败，`residency == Failed`，但**图像仍然可用**——调用方必须
///   把驻留异常如实报给用户，不能当作完全成功。
/// - `Failed`：没有拿到图像（观察/导航/截图失败）。`residency` 仍如实：
///   导航前失败时与 `failure` 相同；导航后失败时记录返回驻守是否成功。
#[derive(Clone, Debug)]
pub(crate) enum CapturedPrimaryOutcome {
    Captured {
        image: DynamicImage,
        navigated: bool,
        residency: UiResidencyOutcome,
    },
    Failed {
        failure: UiRoutineFailure,
        navigated: bool,
        residency: UiResidencyOutcome,
    },
}

impl CapturedPrimaryOutcome {
    /// 截图图像；只有 `Captured` 变体有值。
    pub(crate) fn image(&self) -> Option<&DynamicImage> {
        match self {
            Self::Captured { image, .. } => Some(image),
            Self::Failed { .. } => None,
        }
    }

    /// 截图/导航失败原因；只有 `Failed` 变体有值。
    pub(crate) fn failure(&self) -> Option<&UiRoutineFailure> {
        match self {
            Self::Failed { failure, .. } => Some(failure),
            Self::Captured { .. } => None,
        }
    }

    /// 本次是否为了截图离开过驻守界面（当前为二级恒为 true）。
    pub(crate) fn navigated(&self) -> bool {
        match self {
            Self::Captured { navigated, .. } | Self::Failed { navigated, .. } => *navigated,
        }
    }

    /// 截图之后实际驻留的界面（如实反映返回驻守的结果）。
    pub(crate) fn residency(&self) -> &UiResidencyOutcome {
        match self {
            Self::Captured { residency, .. } | Self::Failed { residency, .. } => residency,
        }
    }
}

/// 是否允许为了截图离开当前界面。
///
/// 只有二级聊天才需要导航到一级；已经在一级时直接截图。其余状态
/// （登录入口/大世界/主菜单/加载/面板/无法识别）一律不盲发输入：
/// 在这些界面上按 Escape 可能关掉玩家的东西，也可能把状态推得更远。
fn capture_navigation_is_allowed(current: UiStateKind) -> bool {
    current == UiStateKind::Secondary
}

/// 一级截图后自动回到驻守界面。
///
/// 时序：观察稳定界面 →（二级时）导航到一级并确认 → 截图 → **无论截图成败**
/// 都尽力返回当前大厅二级。任何一步失败都写进返回值，不假装成功。
struct CapturePrimaryAndRestoreRoutine {
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

impl sealed::UiRoutineSealed for CapturePrimaryAndRestoreRoutine {}

impl UiRoutine for CapturePrimaryAndRestoreRoutine {
    type Output = CapturedPrimaryOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let config = self.config;
        capture_primary_and_restore_with(context, &self.ocr, &config, capture_primary_frame)
    }
}

/// 生产截图注入点；与测试注入保持同一函数签名。
fn capture_primary_frame(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    navigated: bool,
) -> std::result::Result<DynamicImage, UiRoutineFailure> {
    let certainty = if navigated {
        InputCertainty::AfterInputUnknown
    } else {
        InputCertainty::BeforeInput
    };
    capture_normalized(context, config, "capture_primary_frame", certainty)
}

/// 截图内层：只抓一帧，导航与收尾都交给 [`UiTask`]。
struct CapturePrimaryFrameTask {
    config: FriendDeliveryRoutineConfig,
    navigated: bool,
    capture: fn(
        &mut UiRoutineContext<'_>,
        &FriendDeliveryRoutineConfig,
        bool,
    ) -> std::result::Result<DynamicImage, UiRoutineFailure>,
}

impl sealed::UiRoutineSealed for CapturePrimaryFrameTask {}

impl UiRoutine for CapturePrimaryFrameTask {
    type Output = std::result::Result<DynamicImage, UiRoutineFailure>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        (self.capture)(context, &self.config, self.navigated)
    }
}

/// `CapturePrimaryAndRestoreRoutine` 的可测试主体；`capture` 是截图注入点，
/// 生产走 [`capture_primary_frame`]，测试用它验证「截图失败也必须返回驻守界面」。
///
/// 迁移（task-94）：导航与收尾改由 [`UiTask`] 统一负责，本函数只保留
/// **安全前置**（非一级/二级一律零输入失败）与结果整形，不再自己
/// `restore_primary` / `restore_secondary_hall` 两段导航。
fn capture_primary_and_restore_with(
    context: &mut UiRoutineContext<'_>,
    _ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    capture: fn(
        &mut UiRoutineContext<'_>,
        &FriendDeliveryRoutineConfig,
        bool,
    ) -> std::result::Result<DynamicImage, UiRoutineFailure>,
) -> CapturedPrimaryOutcome {
    context.publish_progress(UiRoutineProgressStage::NormalizingStart);
    if let Err(error) = context.device().ensure_ready(config.after_activate_ms) {
        let failure = before_input_failure("prepare_capture_primary", error);
        return CapturedPrimaryOutcome::Failed {
            residency: UiResidencyOutcome::Failed(failure.clone()),
            failure,
            navigated: false,
        };
    }
    // 安全前置（事故后保留）：只允许一级/二级入口，其余状态一律不发送导航输入。
    let entry = match observe_ui_state_once(
        context,
        config.state_observation(),
        "observe_capture_residency",
        InputCertainty::BeforeInput,
    ) {
        Ok(snapshot) => snapshot,
        Err(failure) => {
            return CapturedPrimaryOutcome::Failed {
                residency: UiResidencyOutcome::Failed(failure.clone()),
                failure,
                navigated: false,
            };
        }
    };
    if !capture_navigation_is_allowed(entry.kind) && entry.kind != UiStateKind::Primary {
        let failure = UiRoutineFailure::new(
            InputCertainty::BeforeInput,
            "observe_capture_residency",
            format!(
                "一级截图只支持一级或二级聊天界面；当前为{}（{}），不发送任何导航输入",
                entry.kind.label(),
                entry.kind.name()
            ),
        );
        return CapturedPrimaryOutcome::Failed {
            residency: UiResidencyOutcome::Failed(failure.clone()),
            failure,
            navigated: false,
        };
    }
    let navigated = entry.kind == UiStateKind::Secondary;
    // 目标一级；收尾回到入口（入口二级回二级，入口一级回一级；入口不可重建时退二级）。
    let spec = UiTaskSpec::new(
        UiResidencyTarget::Primary,
        FinishPolicy::RestoreEntry {
            fallback: UiResidencyTarget::SecondaryCurrentHall,
        },
    );
    let task = CapturePrimaryFrameTask {
        config: config.clone(),
        navigated,
        capture,
    };
    let outcome = UiTask::new(spec, task).execute(context);
    let mut residency = outcome.residency.clone();
    // 入口是二级会话时，包装器只保证回到"二级类型"；按固定 fallback 回到当前大厅
    // （确定性、不点好友行），与旧 restore_secondary_hall 语义一致。
    if navigated && matches!(residency, UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)) {
        if let Err(failure) = restore_secondary_hall(context, _ocr, config) {
            residency = UiResidencyOutcome::Failed(failure);
        }
    }
    match outcome.output {
        Some(Ok(image)) => CapturedPrimaryOutcome::Captured {
            image,
            navigated,
            residency,
        },
        Some(Err(failure)) => CapturedPrimaryOutcome::Failed {
            failure,
            navigated,
            residency,
        },
        None => CapturedPrimaryOutcome::Failed {
            failure: outcome.entry_failure.clone().unwrap_or_else(|| {
                UiRoutineFailure::new(
                    InputCertainty::AfterInputUnknown,
                    "capture_primary_frame",
                    "截图任务没有开始：入口观测或导航失败",
                )
            }),
            navigated,
            residency,
        },
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
        let request = self.request;
        let ready_gated = request.ready.is_some();
        let spec = hall_batch_task_spec(&request);
        let task = UiTask::new(
            spec,
            HallBatchExecution {
                request,
                ocr: self.ocr,
                config: self.config,
            },
        );
        let outcome = task.execute(context);

        let (sent, status, primary_chat_opened) = match outcome.output.as_ref() {
            Some(inner) => (
                inner.sent,
                inner.status.clone(),
                inner.primary_chat_opened,
            ),
            None => {
                let failure = outcome.entry_failure.clone().unwrap_or_else(|| {
                    UiRoutineFailure::new(
                        InputCertainty::BeforeInput,
                        "execute_hall_batch",
                        "大厅批量发送没有开始：入口观测或导航失败",
                    )
                });
                (0, HallBatchStatus::Failed(failure), false)
            }
        };
        if primary_chat_opened {
            // 业务状态保留：本次发送曾主动从一级大厅打开聊天。
            log::debug!("大厅批量发送曾在打开聊天后发送（primary_chat_opened=true）");
        }
        // 就绪闸门取消/失败时不再发输入，驻留结论就是闸门失败本身（旧语义）。
        let residency = if ready_gated && matches!(status, HallBatchStatus::Failed(_)) {
            match &status {
                HallBatchStatus::Failed(failure) => UiResidencyOutcome::Failed(failure.clone()),
                HallBatchStatus::Complete => unreachable!(),
            }
        } else {
            outcome.residency.clone()
        };
        SendHallBatchOutcome {
            sent,
            status,
            residency,
        }
    }
}

/// 大厅批量发送的事务声明。
///
/// - 普通发送：发送发生在二级聊天，包装器负责入口→二级的一次导航，
///   收尾回到入口（入口不可重建时退逻辑驻留）——不再自做
///   `finish_hall_batch_residency` 的 "Esc + restore_residency" 双重恢复。
/// - 邀请就绪（ready 闸门）：闸门与一级稳定确认必须先于任何导航，
///   所以用 `AssumeTarget` 让包装器不导航，由内层按邀请流程准备发送位置；
///   收尾仍交给包装器，取消时零输入。
fn hall_batch_task_spec(request: &SendHallBatch) -> UiTaskSpec {
    if request.ready.is_some() {
        UiTaskSpec::new(
            request.residency,
            FinishPolicy::RestoreEntry {
                fallback: request.residency,
            },
        )
        .with_entry(EntryPolicy::AssumeTarget)
    } else {
        UiTaskSpec::new(
            UiResidencyTarget::SecondaryCurrentHall,
            FinishPolicy::RestoreEntry {
                fallback: request.residency,
            },
        )
    }
}

struct HallBatchExecution {
    request: SendHallBatch,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

struct HallBatchInner {
    sent: usize,
    status: HallBatchStatus,
    /// 本次是否主动从一级大厅打开了聊天（业务状态；收尾由包装器负责）。
    primary_chat_opened: bool,
}

impl sealed::UiRoutineSealed for HallBatchExecution {}

impl UiRoutine for HallBatchExecution {
    type Output = HallBatchInner;

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

/// 逻辑驻留是否为"当前大厅二级"（决定会话降级时是否回到大厅）。
fn target_wants_current_hall(request: &SendFriendDeliveries) -> bool {
    request.residency == UiResidencyTarget::SecondaryCurrentHall
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
        let request = self.request;
        let request_for_failure = request.clone();
        let ocr = self.ocr.clone();
        let config = self.config.clone();
        // 好友投递在二级聊天里完成；收尾回到入口（入口不可重建时退逻辑驻留）。
        let spec = UiTaskSpec::new(
            UiResidencyTarget::SecondaryCurrentHall,
            FinishPolicy::RestoreEntry {
                fallback: request.residency,
            },
        );
        let task = UiTask::new(
            spec,
            FriendDeliveriesExecution {
                request,
                ocr: self.ocr,
                config: self.config,
            },
        );
        let outcome = task.execute(context);

        let entry_was_secondary = outcome
            .entry
            .as_ref()
            .is_some_and(|entry| entry.kind == UiStateKind::Secondary);
        let residency = outcome.residency.clone();
        let deliveries = match outcome.output {
            Some(inner) => inner.outcomes,
            None => {
                // 入口观测/导航失败：沿用旧语义，把失败记在第一个收件人上。
                let mut outcomes = request_for_failure
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
                if let Some(first) = outcomes.first_mut() {
                    first.failure = outcome.entry_failure.clone();
                }
                outcomes
            }
        };
        // 会话级恢复能力边界：包装器只保证 "在二级界面类型"，没有会话锚点。
        // 入口是某个二级会话时，用**确定性的当前大厅**作为显式 fallback
        // （不猜测、不点好友行），并标记 session_restore_degraded。
        let mut residency = residency;
        let session_restore_degraded = entry_was_secondary
            && matches!(
                residency,
                UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)
            );
        if session_restore_degraded {
            log::warn!(
                "好友投递收尾只回到二级聊天界面类型，未确认回到入口的具体会话（会话级恢复尚无锚点），按固定 fallback 回到当前大厅"
            );
            if target_wants_current_hall(&request_for_failure) {
                if let Err(failure) = restore_secondary_hall(context, &ocr, &config) {
                    residency = UiResidencyOutcome::Failed(failure);
                }
            }
        }
        SendFriendDeliveriesOutcome {
            deliveries,
            residency,
            session_restore_degraded,
        }
    }
}

/// 好友投递内层：只在二级聊天内做业务（导航与收尾由 [`UiTask`] 负责）。
struct FriendDeliveriesExecution {
    request: SendFriendDeliveries,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
}

struct FriendDeliveriesInner {
    outcomes: Vec<FriendDeliveryOutcome>,
}

impl sealed::UiRoutineSealed for FriendDeliveriesExecution {}

impl UiRoutine for FriendDeliveriesExecution {
    type Output = FriendDeliveriesInner;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        FriendDeliveriesInner {
            outcomes: execute_friend_deliveries(context, self.request, &self.ocr, &self.config),
        }
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
) -> Vec<FriendDeliveryOutcome> {
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
    // 收尾（回到入口/逻辑驻留）由外层 `UiTask` 统一完成，内层不再第二次导航。
    outcomes
}

fn execute_hall_batch(
    context: &mut UiRoutineContext<'_>,
    request: SendHallBatch,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
) -> HallBatchInner {
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
    } else if let Err(failure) = prepare_hall_batch_send_location(
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

    HallBatchInner {
        sent,
        status,
        primary_chat_opened,
    }
}

/// 把界面准备到"可发送的二级聊天（当前大厅聊天）"。
///
/// 这是发送位置的会话级前置，不是任务收尾：收尾统一由包装器完成。
fn prepare_hall_batch_send_location(
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
        // 邀请就绪：闸门与一级稳定必须先于导航（包装器已用 AssumeTarget 跳过导航）。
        check_invite_ready_running(gate)?;
        restore_primary(context, config)?;
        confirm_invite_primary_stability(context, config, gate)?;
        if target == UiResidencyTarget::Primary {
            check_invite_ready_running(gate)?;
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
        }
    }
    // 发送位置固定为"二级聊天的当前大厅"：索引/点击大厅行是会话级动作，
    // 包装器的 kind 级导航不替代它（设计 §4 会话锚点尚未实现）。
    restore_secondary_hall(context, ocr, config)
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
    settle_ui(context, config, UiResidencyTarget::SecondaryCurrentHall)?;
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

/// 状态驱动恢复允许的下一步动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryAction {
    /// 已到达目标稳定界面。
    Done,
    /// 只等待/重新采样，不发送任何输入。
    Wait,
    /// 按 Esc：仅用于已确认的二级、千星主菜单或稳定 Panel。
    Escape,
    /// 按 Return 打开二级聊天（目标为二级时）。
    OpenChat,
    /// 拒绝恢复：不给游戏发任何输入。
    Reject(&'static str),
}

/// 恢复结果：成功到达目标时返回，供调用方与测试断言。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RecoveryReport {
    pub(super) target: UiResidencyTarget,
    pub(super) settled_as: UiStateKind,
    pub(super) esc_presses: u32,
    pub(super) open_chat_presses: u32,
    pub(super) resamples: u32,
    pub(super) history: Vec<String>,
}

/// 恢复过程的累计状态（纯数据；决策函数可在无 UI 设备的单测里驱动）。
#[derive(Default)]
struct RecoveryProgress {
    esc_presses: u32,
    open_chat_presses: u32,
    resamples: u32,
    fresh_identity: Option<String>,
    fresh_samples: u32,
    unknown_samples: u32,
    unknown_since: Option<Instant>,
    loading_samples: u32,
    loading_since: Option<Instant>,
    panel_samples: u32,
    pending_action_identity: Option<String>,
    no_progress_streak: u32,
    kind_tail: Vec<UiStateKind>,
    history: Vec<String>,
}

impl RecoveryProgress {
    fn record(&mut self, snapshot: &UiStateSnapshot, now: Instant) {
        self.resamples = self.resamples.saturating_add(1);
        self.history.push(snapshot.describe());
        if self.history.len() > RECOVERY_HISTORY_LIMIT {
            self.history.remove(0);
        }

        let identity = snapshot.identity();
        let same_fresh = self.fresh_identity.as_deref() == Some(identity.as_str());
        if same_fresh {
            self.fresh_samples = self.fresh_samples.saturating_add(1);
        } else {
            self.fresh_identity = Some(identity.clone());
            self.fresh_samples = 1;
        }

        // 动作后验证：同一 (kind,label) 反复出现即"无进展"。
        if let Some(pending) = self.pending_action_identity.as_deref() {
            if pending == identity.as_str() {
                self.no_progress_streak = self.no_progress_streak.saturating_add(1);
            } else {
                self.no_progress_streak = 0;
            }
        }

        match snapshot.kind {
            UiStateKind::Unknown => {
                self.unknown_samples = self.unknown_samples.saturating_add(1);
                self.unknown_since.get_or_insert(now);
            }
            _ => {
                self.unknown_samples = 0;
                self.unknown_since = None;
            }
        }
        match snapshot.kind {
            UiStateKind::Loading => {
                self.loading_samples = self.loading_samples.saturating_add(1);
                self.loading_since.get_or_insert(now);
            }
            _ => {
                self.loading_samples = 0;
                self.loading_since = None;
            }
        }
        if snapshot.kind == UiStateKind::Panel {
            self.panel_samples = self.panel_samples.saturating_add(1);
        } else {
            self.panel_samples = 0;
        }

        if self.kind_tail.last().copied() != Some(snapshot.kind) {
            self.kind_tail.push(snapshot.kind);
            if self.kind_tail.len() > 6 {
                self.kind_tail.remove(0);
            }
        }
    }

    /// 运行时与本次恢复的"新鲜稳定"双重确认：既要求跟踪器判稳定，
    /// 也要求本次恢复自己连续采到 required_count 帧同一状态。
    fn settled(&self, snapshot: &UiStateSnapshot) -> bool {
        snapshot.runtime_stable && self.fresh_samples >= snapshot.required_count.max(1)
    }

    fn primary_main_menu_oscillations(&self) -> u32 {
        let mut count = 0;
        for pair in self.kind_tail.windows(2) {
            if matches!(
                (pair[0], pair[1]),
                (UiStateKind::Primary, UiStateKind::MainMenu)
                    | (UiStateKind::MainMenu, UiStateKind::Primary)
            ) {
                count += 1;
            }
        }
        count
    }
}

/// 纯决策：根据一次分类快照与累计进度，决定下一步动作。
///
/// 顺序与约束（本事故的核心）：
/// - Primary 稳定即成功；Primary 未稳定只等，不按任何键；
/// - Secondary 仅在目标为 Primary 时 Esc，且必须已确认稳定；
/// - MainMenu 按 `final_rule` 区分：千星主菜单可 Esc，派蒙菜单**拒绝**；
/// - Panel 需连续确认仍是 Panel 才 Esc；
/// - Loading 只等待；
/// - Unknown 先等待/重采样，连续无证据才失败，**绝不立刻盲按 Esc**；
/// - GameGate / Overworld 直接拒绝。
fn decide_recovery_step(
    snapshot: &UiStateSnapshot,
    target: UiResidencyTarget,
    progress: &RecoveryProgress,
) -> RecoveryAction {
    if progress.no_progress_streak >= RECOVERY_NO_PROGRESS_LIMIT {
        return RecoveryAction::Reject("恢复动作没有产生任何状态变化（无进展熔断）");
    }
    if progress.primary_main_menu_oscillations() >= RECOVERY_OSCILLATION_LIMIT {
        return RecoveryAction::Reject("一级界面与主菜单反复来回（振荡熔断）");
    }

    match snapshot.kind {
        UiStateKind::Primary => {
            if !progress.settled(snapshot) {
                return RecoveryAction::Wait;
            }
            match target {
                UiResidencyTarget::Primary => RecoveryAction::Done,
                UiResidencyTarget::SecondaryCurrentHall => RecoveryAction::OpenChat,
            }
        }
        UiStateKind::Secondary => {
            if !progress.settled(snapshot) {
                return RecoveryAction::Wait;
            }
            match target {
                UiResidencyTarget::SecondaryCurrentHall => RecoveryAction::Done,
                UiResidencyTarget::Primary => {
                    if progress.esc_presses < RECOVERY_MAX_ESC {
                        RecoveryAction::Escape
                    } else {
                        RecoveryAction::Reject("按 Esc 次数已达上限，二级聊天仍未退出")
                    }
                }
            }
        }
        UiStateKind::MainMenu => {
            if snapshot.is_paimon_menu() {
                // 派蒙菜单是提瓦特 ESC 系统菜单，不是千星大厅主菜单：
                // 在这里按 Esc / Return 会把玩家推得更远，明确拒绝。
                return RecoveryAction::Reject(
                    "当前是派蒙菜单（ESC 系统菜单），不是千星大厅主菜单，拒绝发送导航输入",
                );
            }
            if !progress.settled(snapshot) {
                return RecoveryAction::Wait;
            }
            if progress.esc_presses < RECOVERY_MAX_ESC {
                // 千星奇域主菜单：Esc 关闭浮层回到大厅（一级）。
                RecoveryAction::Escape
            } else {
                RecoveryAction::Reject("按 Esc 次数已达上限，主菜单仍未关闭")
            }
        }
        UiStateKind::Panel => {
            // 面板抖动常见：必须连续确认仍是 Panel，才允许 Esc 关闭。
            if progress.panel_samples < RECOVERY_PANEL_CONFIRM_SAMPLES
                || !progress.settled(snapshot)
            {
                return RecoveryAction::Wait;
            }
            if progress.esc_presses < RECOVERY_MAX_ESC {
                RecoveryAction::Escape
            } else {
                RecoveryAction::Reject("按 Esc 次数已达上限，残留面板仍未关闭")
            }
        }
        UiStateKind::Loading => RecoveryAction::Wait,
        UiStateKind::Unknown => {
            let sustained = progress.unknown_samples >= RECOVERY_UNKNOWN_MIN_SAMPLES
                && progress
                    .unknown_since
                    .is_some_and(|since| since.elapsed() >= Duration::from_millis(RECOVERY_UNKNOWN_WINDOW_MS));
            if sustained {
                RecoveryAction::Reject("界面持续无法识别且没有任何稳定证据，拒绝盲按 Esc")
            } else {
                RecoveryAction::Wait
            }
        }
        UiStateKind::GameGate | UiStateKind::Overworld => {
            RecoveryAction::Reject("登录入口或大世界不可恢复聊天驻留")
        }
    }
}

/// 恢复失败时的统一诊断：包含当前事实、动作次数与历史。
fn recovery_failure(
    stage: &'static str,
    certainty: InputCertainty,
    target: UiResidencyTarget,
    snapshot: &UiStateSnapshot,
    progress: &RecoveryProgress,
    reason: &str,
) -> UiRoutineFailure {
    UiRoutineFailure::new(
        certainty,
        stage,
        format!(
            "状态驱动恢复中止（{reason}）：目标={target:?}；当前={}；Esc={} Return={} 采样={}；历史={}",
            snapshot.describe(),
            progress.esc_presses,
            progress.open_chat_presses,
            progress.resamples,
            progress.history.join(" <- ")
        ),
    )
}

/// 统一状态驱动恢复入口：把界面安全地驱动到目标驻留界面。
///
/// 供 `restore_primary` / `ensure_secondary_chat` / `restore_secondary_hall` 复用。
/// 原则：**只有已确认的状态才允许输入**；Unknown/Loading 只等待；主菜单按证据区分
/// 千星（可 Esc）与派蒙（拒绝）；无进展或振荡立即熔断。
/// 统一状态驱动恢复入口：把界面安全地驱动到目标驻留界面。
///
/// 供 `restore_primary` / `ensure_secondary_chat` / `restore_secondary_hall` 复用。
/// 原则：**只有已确认的状态才允许输入**；Unknown/Loading 只等待；主菜单按证据区分
/// 千星（可 Esc）与派蒙（拒绝）；无进展或振荡立即熔断。
///
/// `ocr` 目前不参与恢复决策（决策只依赖 UI 状态证据），保留参数是为了与
/// 驻留恢复的统一入口签名一致，后续需要 OCR 佐证时无需再改调用方。
pub(super) fn settle_to(
    context: &mut UiRoutineContext<'_>,
    _ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    target: UiResidencyTarget,
) -> std::result::Result<RecoveryReport, UiRoutineFailure> {
    settle_ui(context, config, target)
}

fn settle_ui(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    target: UiResidencyTarget,
) -> std::result::Result<RecoveryReport, UiRoutineFailure> {
    let observation_config = config.state_observation();
    let deadline = Instant::now() + Duration::from_millis(config.timeout_ms.max(1));
    let mut progress = RecoveryProgress::default();
    let mut action_taken = false;
    let mut steps = 0_u32;

    loop {
        if steps >= RECOVERY_MAX_STEPS {
            return Err(UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                match target {
                    UiResidencyTarget::Primary => "confirm_primary_residency",
                    UiResidencyTarget::SecondaryCurrentHall => "confirm_secondary_chat",
                },
                format!(
                    "状态驱动恢复超过最大动作步数（{}）：目标={target:?}；Esc={} Return={}；历史={}",
                    RECOVERY_MAX_STEPS,
                    progress.esc_presses,
                    progress.open_chat_presses,
                    progress.history.join(" <- ")
                ),
            ));
        }
        let now = Instant::now();
        // 输入前失败可安全重试；一旦按过键，结论只能是"动作后状态未知"。
        let (stage, certainty) = if action_taken {
            match target {
                UiResidencyTarget::Primary => (
                    "confirm_primary_residency",
                    InputCertainty::AfterInputUnknown,
                ),
                UiResidencyTarget::SecondaryCurrentHall => (
                    "confirm_secondary_chat",
                    InputCertainty::AfterInputUnknown,
                ),
            }
        } else {
            match target {
                UiResidencyTarget::Primary => {
                    ("observe_primary_residency", InputCertainty::BeforeInput)
                }
                UiResidencyTarget::SecondaryCurrentHall => {
                    ("observe_friend_delivery_start", InputCertainty::BeforeInput)
                }
            }
        };

        let snapshot = observe_ui_state_once(context, observation_config, stage, certainty)?;
        progress.record(&snapshot, now);

        // 超出本次恢复预算：按当前界面给出可操作的诊断；期间不按任何键。
        if now >= deadline {
            let reason = match snapshot.kind {
                UiStateKind::Loading => "加载界面持续超出恢复预算",
                UiStateKind::Unknown => "界面持续无法识别且没有任何稳定证据，拒绝盲按 Esc",
                _ => "恢复未在时间预算内到达目标界面",
            };
            return Err(recovery_failure(
                stage,
                certainty,
                target,
                &snapshot,
                &progress,
                reason,
            ));
        }

        match decide_recovery_step(&snapshot, target, &progress) {
            RecoveryAction::Done => {
                return Ok(RecoveryReport {
                    target,
                    settled_as: snapshot.kind,
                    esc_presses: progress.esc_presses,
                    open_chat_presses: progress.open_chat_presses,
                    resamples: progress.resamples,
                    history: progress.history,
                });
            }
            RecoveryAction::Wait => {}
            RecoveryAction::Escape => {
                action_taken = true;
                steps = steps.saturating_add(1);
                let pre_action_identity = snapshot.identity();
                context
                    .device()
                    .press_key(Key::Escape)
                    .map_err(|error| after_input_failure("restore_residency", error))?;
                progress.esc_presses = progress.esc_presses.saturating_add(1);
                progress.pending_action_identity = Some(pre_action_identity);
                wait_ms(
                    context,
                    config.poll_ms.max(10),
                    "restore_residency_wait",
                    InputCertainty::AfterInputUnknown,
                )?;
            }
            RecoveryAction::OpenChat => {
                action_taken = true;
                steps = steps.saturating_add(1);
                let pre_action_identity = snapshot.identity();
                context
                    .device()
                    .press_key(Key::Return)
                    .map_err(|error| after_input_failure("open_secondary_chat", error))?;
                progress.open_chat_presses = progress.open_chat_presses.saturating_add(1);
                progress.pending_action_identity = Some(pre_action_identity);
                wait_ms(
                    context,
                    config.open_chat_ms,
                    "open_secondary_chat",
                    InputCertainty::AfterInputUnknown,
                )?;
            }
            RecoveryAction::Reject(reason) => {
                return Err(recovery_failure(
                    stage,
                    certainty,
                    target,
                    &snapshot,
                    &progress,
                    reason,
                ));
            }
        }

        wait_ms(
            context,
            config.poll_ms.max(10),
            stage,
            if action_taken {
                InputCertainty::AfterInputUnknown
            } else {
                InputCertainty::BeforeInput
            },
        )?;
    }
}

pub(super) fn restore_residency(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    target: UiResidencyTarget,
) -> std::result::Result<(), UiRoutineFailure> {
    match target {
        UiResidencyTarget::Primary => {
            settle_to(context, ocr, config, target)?;
            confirm_primary_residency(context, config)
        }
        UiResidencyTarget::SecondaryCurrentHall => restore_secondary_hall(context, ocr, config),
    }
}

fn restore_primary(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<(), UiRoutineFailure> {
    // 状态驱动：只对已确认的二级/千星主菜单/稳定面板按 Esc；
    // Unknown/Loading 只等待，主菜单按 final_rule 区分千星与派蒙。
    settle_ui(context, config, UiResidencyTarget::Primary)?;
    // 保留原有的"视觉驻留稳定"二次确认（好友按钮区域指纹不抖动）。
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
/// 为什么要多候选：好友列表上显示的是「备注串」，没有备注时显示玩家自设昵称；
/// 两者都可能出现，没法预先知道是哪一种。候选由装配层统一用
/// identity.friend_search_names 生成（备注串 → 真实昵称），这里只负责逐个匹配。
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

    /// 与生产装配一致地注入面板判据区域：只有注入后残留面板才会判成 `Panel`，
    /// 注入前后的差异本身也是"面板判据需装配层开启"的回归保护。
    fn start_test_ui_runtime_with_panel_regions(
        device: impl UiDevice,
        config: &AppConfig,
        queue_capacity: usize,
    ) -> UiRuntime {
        let panels = crate::ui::state::PanelRegions {
            friend_panel: config.moderation.friend_panel_region.into(),
            friend_search_panel: config.moderation.search_panel_region.into(),
            friend_block_chat: config.moderation.block_chat_region.into(),
            friend_confirm: config.moderation.confirm_region.into(),
            invite_view_star: config.invite.view_star_region.into(),
            invite_goto_hall: config.invite.goto_hall_region.into(),
            invite_enter_hall: config.invite.enter_hall_region.into(),
        };
        UiRuntime::start_with_state_classifier(
            device,
            queue_capacity,
            TemplateUiStateClassifier::new(
                UiTemplateArgs::default().resolve(&config.templates, &config.ocr),
                config.screen.clone(),
            )
            .with_panel_regions(panels),
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
        config.timing.workflow.default_timeout_ms = 5_000;
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
        config.timing.workflow.default_timeout_ms = 5_000;
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
        config.timing.workflow.default_timeout_ms = 5_000;
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
        config.timing.workflow.default_timeout_ms = 5_000;
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
        config.timing.workflow.default_timeout_ms = 5_000;
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
            session_restore_degraded: false,
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

    /// 按"状态机"出帧的设备：同一状态重复出帧，直到收到指定按键；
    /// `auto_advance` 模拟"还没等到输入就自己消失"的瞬态帧（事故回放的 Unknown 瞬失）。
    #[derive(Default)]
    struct ScriptedStateDeviceState {
        state: usize,
        keys: Vec<Key>,
        clicks: Vec<(i32, i32)>,
        pasted: Vec<String>,
        captures_in_state: Vec<usize>,
        captures_total: usize,
        first_escape_after: Option<usize>,
    }

    struct ScriptedStateDevice {
        frames: Vec<DynamicImage>,
        transitions: Vec<(usize, Key, usize)>,
        auto_advance: std::collections::HashMap<usize, usize>,
        shared: Arc<Mutex<ScriptedStateDeviceState>>,
    }

    impl ScriptedStateDevice {
        fn new(frames: Vec<DynamicImage>) -> (Self, Arc<Mutex<ScriptedStateDeviceState>>) {
            let shared = Arc::new(Mutex::new(ScriptedStateDeviceState {
                captures_in_state: vec![0; frames.len()],
                ..ScriptedStateDeviceState::default()
            }));
            (
                Self {
                    frames,
                    transitions: Vec::new(),
                    auto_advance: std::collections::HashMap::new(),
                    shared: Arc::clone(&shared),
                },
                shared,
            )
        }

        fn auto_advance(mut self, from: usize, to: usize) -> Self {
            self.auto_advance.insert(from, to);
            self
        }

        fn on_key(mut self, from: usize, key: Key, to: usize) -> Self {
            self.transitions.push((from, key, to));
            self
        }
    }

    impl UiDevice for ScriptedStateDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            let mut state = self.shared.lock().unwrap();
            let current = state.state;
            state.captures_total += 1;
            if let Some(count) = state.captures_in_state.get_mut(current) {
                *count += 1;
            }
            let frame = self.frames[current].clone();
            if let Some(next) = self.auto_advance.get(&current) {
                state.state = *next;
            }
            Ok(frame)
        }

        fn ensure_ready(&mut self, _after_activate_ms: u64) -> Result<()> {
            Ok(())
        }

        fn click_point(&mut self, x: i32, y: i32) -> Result<()> {
            self.shared.lock().unwrap().clicks.push((x, y));
            Ok(())
        }

        fn paste_text(&mut self, text: &str, _clipboard_hold_ms: u64) -> Result<()> {
            self.shared.lock().unwrap().pasted.push(text.to_string());
            Ok(())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            let mut state = self.shared.lock().unwrap();
            state.keys.push(key);
            if key == Key::Escape && state.first_escape_after.is_none() {
                state.first_escape_after = Some(state.captures_total);
            }
            if let Some((_, _, to)) = self
                .transitions
                .iter()
                .find(|(from, bound, _)| *from == state.state && *bound == key)
            {
                state.state = *to;
            }
            Ok(())
        }
    }

    struct EnsureSecondaryChatRoutine {
        config: FriendDeliveryRoutineConfig,
    }

    impl sealed::UiRoutineSealed for EnsureSecondaryChatRoutine {}

    impl UiRoutine for EnsureSecondaryChatRoutine {
        type Output = std::result::Result<(), UiRoutineFailure>;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            ensure_secondary_chat(context, &self.config)
        }
    }

    fn main_menu_frame(config: &AppConfig) -> DynamicImage {
        load_real_frame("tests/fixtures/ui/main-menu-wonderland-1920x1080.png", config)
    }

    fn paimon_menu_frame(config: &AppConfig) -> DynamicImage {
        load_real_frame("tests/fixtures/ui/menu-paimon-1920x1080.jpg", config)
    }

    fn loading_frame(config: &AppConfig) -> DynamicImage {
        load_real_frame("tests/fixtures/ui/loading-dark-1920x1080.jpg", config)
    }

    fn overworld_frame(config: &AppConfig) -> DynamicImage {
        load_real_frame("tests/fixtures/ui/overworld-teyvat-1920x1080.jpg", config)
    }

    fn panel_frame(config: &AppConfig) -> DynamicImage {
        load_real_frame("tests/fixtures/ui/panel-hall-card-1920x1080.jpg", config)
    }

    fn recovery_config(config: &AppConfig, timeout_ms: u64) -> FriendDeliveryRoutineConfig {
        let mut routine_config = FriendDeliveryRoutineConfig::from_app(config);
        routine_config.timeout_ms = timeout_ms;
        routine_config.poll_ms = 1;
        routine_config
    }

    /// 事故回放：Unknown 只闪一帧就恢复成一级界面。旧实现会立刻按 Esc；
    /// 新实现必须先等待/重采样，确认 Primary 后零按键成功。
    #[test]
    fn unknown_blip_is_waited_out_without_any_input() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) = ScriptedStateDevice::new(vec![
            unknown_frame(&config),
            primary_frame(&config),
        ]);
        let device = device.auto_advance(0, 1);
        let ui_runtime = start_test_ui_runtime(device, &config, 2);

        ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: recovery_config(&config, 5_000),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect("Unknown 瞬失必须等待并恢复一级界面");

        let state = state.lock().unwrap();
        assert!(state.keys.is_empty(), "Unknown 瞬失不允许任何按键：{:?}", state.keys);
        ui_runtime.shutdown().unwrap();
    }

    /// 持续 Unknown：只重新采样，绝不盲按 Esc；失败诊断要带 rule/world/last_stable。
    #[test]
    fn persistent_unknown_fails_with_diagnostics_and_no_keys() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) =
            ScriptedStateDevice::new(vec![unknown_frame(&config)]);
        let ui_runtime = start_test_ui_runtime(device, &config, 2);

        let failure = ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: recovery_config(&config, 2_000),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect_err("持续 Unknown 必须失败");

        assert!(state.lock().unwrap().keys.is_empty(), "不允许盲按 Esc");
        let reason = failure.reason().to_string();
        assert!(
            reason.contains("无法识别") || reason.contains("加载界面"),
            "失败原因必须说明持续不可识别：{reason}"
        );
        assert!(reason.contains("rule="), "诊断必须带 final_rule：{reason}");
        assert!(reason.contains("last_stable="), "诊断必须带 last_stable：{reason}");
        ui_runtime.shutdown().unwrap();
    }

    /// 千星主菜单：Esc 关闭浮层回到一级界面。
    #[test]
    fn wonderland_main_menu_is_escaped_back_to_primary() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) = ScriptedStateDevice::new(vec![
            main_menu_frame(&config),
            primary_frame(&config),
        ]);
        let device = device.on_key(0, Key::Escape, 1);
        let ui_runtime = start_test_ui_runtime(device, &config, 2);

        ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: recovery_config(&config, 5_000),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect("千星主菜单必须能通过 Esc 回到一级界面");

        assert_eq!(*state.lock().unwrap().keys, [Key::Escape].to_vec());
        ui_runtime.shutdown().unwrap();
    }

    /// 派蒙菜单不是大厅菜单：明确拒绝，零输入。
    #[test]
    fn paimon_menu_is_rejected_without_input() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) =
            ScriptedStateDevice::new(vec![paimon_menu_frame(&config)]);
        let ui_runtime = start_test_ui_runtime(device, &config, 2);

        let failure = ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: recovery_config(&config, 2_000),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect_err("派蒙菜单必须被拒绝");

        assert!(state.lock().unwrap().keys.is_empty(), "派蒙菜单不允许任何输入");
        assert!(
            failure.reason().contains("派蒙"),
            "必须说明是派蒙菜单：{}",
            failure.reason()
        );
        ui_runtime.shutdown().unwrap();
    }

    /// Panel 需要连续确认仍是 Panel 才允许 Esc（防止一帧抖动就按键）。
    #[test]
    fn panel_is_confirmed_twice_before_escape() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) =
            ScriptedStateDevice::new(vec![panel_frame(&config), primary_frame(&config)]);
        let device = device.on_key(0, Key::Escape, 1);
        let ui_runtime = start_test_ui_runtime_with_panel_regions(device, &config, 2);

        ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: recovery_config(&config, 5_000),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect("稳定 Panel 必须能通过 Esc 关闭");

        let state = state.lock().unwrap();
        assert_eq!(state.keys, [Key::Escape].to_vec());
        assert!(
            state.captures_in_state[0] >= 2,
            "Panel 必须先被重采样确认，实际只采了 {} 帧",
            state.captures_in_state[0]
        );
        ui_runtime.shutdown().unwrap();
    }

    /// Loading：只等待，不发任何按键；超预算后如实失败。
    #[test]
    fn loading_waits_without_input_and_times_out() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) = ScriptedStateDevice::new(vec![loading_frame(&config)]);
        let ui_runtime = start_test_ui_runtime(device, &config, 2);

        let failure = ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: recovery_config(&config, 1_200),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect_err("持续 Loading 必须失败");

        assert!(state.lock().unwrap().keys.is_empty(), "Loading 不允许按键");
        assert!(
            failure.reason().contains("加载"),
            "失败原因必须说明加载界面：{}",
            failure.reason()
        );
        ui_runtime.shutdown().unwrap();
    }

    /// 大世界：拒绝恢复，零输入。
    #[test]
    fn overworld_is_rejected_without_input() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) =
            ScriptedStateDevice::new(vec![overworld_frame(&config)]);
        let ui_runtime = start_test_ui_runtime(device, &config, 2);

        ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: recovery_config(&config, 2_000),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect_err("大世界必须拒绝恢复");

        assert!(state.lock().unwrap().keys.is_empty(), "大世界不允许输入");
        ui_runtime.shutdown().unwrap();
    }

    /// 事故完整回放：二级 → Esc → Unknown（不得再 Esc）→ 千星主菜单 → Esc → 一级。
    #[test]
    fn unknown_after_escape_then_main_menu_then_escape_reaches_primary() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) = ScriptedStateDevice::new(vec![
            secondary_frame(&config),
            unknown_frame(&config),
            main_menu_frame(&config),
            primary_frame(&config),
        ]);
        let device = device
            .on_key(0, Key::Escape, 1)
            .auto_advance(1, 2)
            .on_key(2, Key::Escape, 3);
        let ui_runtime = start_test_ui_runtime(device, &config, 2);

        ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: recovery_config(&config, 8_000),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect("Unknown 过渡后必须靠主菜单 Esc 回到一级界面");

        assert_eq!(
            *state.lock().unwrap().keys,
            [Key::Escape, Key::Escape].to_vec(),
            "只允许两次 Esc：一次退出二级、一次关闭千星主菜单"
        );
        ui_runtime.shutdown().unwrap();
    }

    /// 千星主菜单按 Esc 没反应（无进展）时，必须在 2 次后熔断，不能无限按。
    #[test]
    fn main_menu_without_progress_trips_the_breaker() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) =
            ScriptedStateDevice::new(vec![main_menu_frame(&config)]);
        let ui_runtime = start_test_ui_runtime(device, &config, 2);

        let failure = ui_runtime
            .handle()
            .submit(RestorePrimaryRoutine {
                config: recovery_config(&config, 8_000),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect_err("无进展必须熔断");

        let keys = state.lock().unwrap().keys.clone();
        assert!(
            keys.len() <= RECOVERY_NO_PROGRESS_LIMIT as usize,
            "无进展熔断前最多按 {} 次 Esc，实际 {} 次",
            RECOVERY_NO_PROGRESS_LIMIT,
            keys.len()
        );
        let reason = failure.reason();
        assert!(
            reason.contains("无进展") || reason.contains("振荡"),
            "失败原因必须写明熔断：{reason}"
        );
        ui_runtime.shutdown().unwrap();
    }

    /// 二次恢复：目标二级聊天时遇千星主菜单，必须先 Esc 回一级，再 Return 打开聊天。
    #[test]
    fn secondary_target_escapes_main_menu_first_then_returns() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) = ScriptedStateDevice::new(vec![
            main_menu_frame(&config),
            primary_frame(&config),
            secondary_frame(&config),
        ]);
        let device = device
            .on_key(0, Key::Escape, 1)
            .on_key(1, Key::Return, 2);
        let ui_runtime = start_test_ui_runtime(device, &config, 2);

        ui_runtime
            .handle()
            .submit(EnsureSecondaryChatRoutine {
                config: recovery_config(&config, 8_000),
            })
            .unwrap()
            .wait()
            .unwrap()
            .expect("必须先 Esc 回一级再 Return 回二级聊天");

        assert_eq!(
            *state.lock().unwrap().keys,
            [Key::Escape, Key::Return].to_vec(),
            "动作顺序必须是 Esc → Return"
        );
        ui_runtime.shutdown().unwrap();
    }

    // ---- task-94 迁移测试：发送/好友投递/截图统一走 UiTask ----

    fn migration_config(config: &AppConfig) -> FriendDeliveryRoutineConfig {
        let mut config = config.clone();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        config.timing.workflow.default_timeout_ms = 5_000;
        config.timing.workflow.default_poll_ms = 1;
        FriendDeliveryRoutineConfig::from_app(&config)
    }

    /// 入口一级 → 发送 → 回到一级：只有“打开聊天一次 + 提交一次 + 收尾 Esc 一次”，
    /// 不再出现旧 `finish_hall_batch_residency` 的二次恢复。
    #[test]
    fn hall_batch_from_primary_returns_to_primary_with_one_round_trip() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) = ScriptedStateDevice::new(vec![
            primary_frame(&config),
            secondary_hall_row_frame(&config),
        ]);
        let device = device
            .on_key(0, Key::Return, 1)
            .on_key(1, Key::Escape, 0);
        let ui_runtime = start_test_ui_runtime(device, &config, 4);
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let hall_ui = HallBatchUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            migration_config(&config),
        );

        let outcome = hall_ui
            .submit(SendHallBatch::new(
                ["第一条"],
                UiResidencyTarget::Primary,
                0,
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(outcome.sent(), 1);
        assert!(matches!(outcome.status(), HallBatchStatus::Complete));
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        ));
        let state = state.lock().unwrap();
        assert_eq!(
            state.keys.iter().filter(|key| **key == Key::Escape).count(),
            1,
            "收尾只允许一次 Esc，实际：{:?}",
            state.keys
        );
        assert_eq!(state.pasted, ["第一条"]);
    }

    /// 入口二级 → 发送 → 回到二级当前大厅：不做任何多余导航。
    #[test]
    fn hall_batch_from_secondary_returns_to_current_hall_without_navigation() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) = ScriptedStateDevice::new(vec![secondary_hall_row_frame(&config)]);
        let ui_runtime = start_test_ui_runtime(device, &config, 4);
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let hall_ui = HallBatchUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            migration_config(&config),
        );

        let outcome = hall_ui
            .submit(SendHallBatch::new(
                ["第一条"],
                UiResidencyTarget::SecondaryCurrentHall,
                0,
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(outcome.sent(), 1);
        assert!(matches!(outcome.status(), HallBatchStatus::Complete));
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)
        ));
        let state = state.lock().unwrap();
        assert_eq!(state.keys, [Key::Return].to_vec(), "只允许提交那一次 Return");
        assert_eq!(state.pasted, ["第一条"]);
    }

    /// 消息全送达但收尾失败：发送结果仍是 Complete，驻留单独报 Failed。
    #[test]
    fn delivered_but_residency_failed_keeps_send_status_complete() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) = ScriptedStateDevice::new(vec![
            primary_frame(&config),
            secondary_hall_row_frame(&config),
        ]);
        // 只允许打开聊天，不允许 Esc 回去：收尾必然失败。
        let device = device.on_key(0, Key::Return, 1);
        let ui_runtime = start_test_ui_runtime(device, &config, 4);
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();
        let hall_ui = HallBatchUi::new(
            ui_runtime.handle(),
            ocr_runtime.handle(),
            migration_config(&config),
        );

        let outcome = hall_ui
            .submit(SendHallBatch::new(
                ["第一条", "第二条"],
                UiResidencyTarget::Primary,
                0,
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert_eq!(outcome.sent(), 2, "两条都已送达，不能当未送达");
        assert!(matches!(outcome.status(), HallBatchStatus::Complete));
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Failed(_)
        ));
        let state = state.lock().unwrap();
        assert_eq!(state.pasted, ["第一条", "第二条"], "送达后不得重发");
    }

    /// 好友投递：入口是二级会话，收尾只能回到“二级类型”，必须显式标记会话级降级。
    #[test]
    fn friend_delivery_from_secondary_reports_session_restore_fallback() {
        let mut config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.input.text_ms = 0;
        config.timing.input.send_ms = 0;
        config.timing.invite.step_ms = 0;
        config.timing.workflow.default_timeout_ms = 5_000;
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
                // 避开大厅行（y=365）的高亮区域，确保确认走的是好友行。
                friend_rows: vec![(friend_list.y + 165, "乙".to_string())],
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

        let outcome = friend_ui
            .submit(SendFriendDeliveries::new(
                vec![FriendDelivery::new("乙", ["报名成功"])],
                UiResidencyTarget::SecondaryCurrentHall,
            ))
            .unwrap()
            .wait()
            .unwrap();

        assert!(
            outcome.is_complete(),
            "消息送达且仍在二级，应视为完成: {:?}",
            (
                outcome.deliveries()[0].message_statuses(),
                outcome.deliveries()[0].failure(),
                outcome.residency()
            )
        );
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)
        ));
        assert!(
            outcome.session_restore_degraded(),
            "入口是二级会话时，包装器无法确认回到同一会话，必须显式标记降级"
        );
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    /// 截图例程的安全前置：入口 Unknown 时零按键失败（事故回放）。
    #[test]
    fn capture_from_unknown_entry_sends_no_input() {
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let (device, state) = ScriptedStateDevice::new(vec![unknown_frame(&config)]);
        let ui_runtime = start_test_ui_runtime(device, &config, 4);
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();

        let outcome = ui_runtime
            .handle()
            .submit(CapturePrimaryAndRestoreRoutine {
                ocr: ocr_runtime.handle(),
                config: migration_config(&config),
            })
            .unwrap()
            .wait()
            .unwrap();

        assert!(outcome.image().is_none());
        assert_eq!(
            outcome.failure().map(|failure| failure.stage()),
            Some("observe_capture_residency")
        );
        assert!(state.lock().unwrap().keys.is_empty(), "Unknown 入口不允许任何按键");
        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    // ---- 决策函数的纯单测：不依赖真实帧，覆盖无法用夹具触发的情况 ----

    fn snap(kind: UiStateKind, label: &str, rule: &str) -> UiStateSnapshot {
        UiStateSnapshot {
            kind,
            label: label.to_string(),
            final_rule: rule.to_string(),
            world_guess: "unknown".to_string(),
            last_stable_kind: Some(kind),
            runtime_stable: true,
            candidate_count: 2,
            required_count: 2,
        }
    }

    fn settled_progress(snapshot: &UiStateSnapshot) -> RecoveryProgress {
        let mut progress = RecoveryProgress::default();
        for _ in 0..snapshot.required_count.max(1) {
            progress.record(snapshot, Instant::now());
        }
        progress
    }

    #[test]
    fn recovery_decision_rejects_game_gate_and_overworld_without_input() {
        for kind in [UiStateKind::GameGate, UiStateKind::Overworld] {
            let snapshot = snap(kind, "gate", "game_gate_template");
            let mut progress = settled_progress(&snapshot);
            let action = decide_recovery_step(&snapshot, UiResidencyTarget::Primary, &mut progress);
            assert!(
                matches!(action, RecoveryAction::Reject(_)),
                "{kind:?} 必须拒绝恢复，实际 {action:?}"
            );
        }
    }

    #[test]
    fn recovery_decision_unknown_waits_then_rejects_without_escape() {
        let snapshot = snap(UiStateKind::Unknown, "unknown", "no_reliable_anchor");
        let mut progress = settled_progress(&snapshot);
        assert_eq!(
            decide_recovery_step(&snapshot, UiResidencyTarget::Primary, &mut progress),
            RecoveryAction::Wait,
            "刚出现的 Unknown 只允许等待"
        );
        for _ in 0..RECOVERY_UNKNOWN_MIN_SAMPLES {
            progress.record(&snapshot, Instant::now());
        }
        progress.unknown_since = Some(Instant::now() - Duration::from_millis(RECOVERY_UNKNOWN_WINDOW_MS + 50));
        let action = decide_recovery_step(&snapshot, UiResidencyTarget::Primary, &mut progress);
        assert!(
            matches!(action, RecoveryAction::Reject(_)),
            "持续 Unknown 必须失败而不是 Esc，实际 {action:?}"
        );
    }

    #[test]
    fn recovery_decision_paimon_menu_is_rejected_even_when_stable() {
        let snapshot = snap(UiStateKind::MainMenu, "main_menu", "paimon_menu_panel_template");
        let mut progress = settled_progress(&snapshot);
        let action = decide_recovery_step(&snapshot, UiResidencyTarget::Primary, &mut progress);
        assert!(matches!(action, RecoveryAction::Reject(_)), "派蒙菜单必须拒绝：{action:?}");
        let action = decide_recovery_step(
            &snapshot,
            UiResidencyTarget::SecondaryCurrentHall,
            &mut progress,
        );
        assert!(
            matches!(action, RecoveryAction::Reject(_)),
            "目标二级时派蒙菜单同样必须拒绝：{action:?}"
        );
    }

    #[test]
    fn recovery_decision_panel_requires_two_confirmations() {
        let panel = snap(UiStateKind::Panel, "panel", "panel_template");
        // 新鲜稳定已满足，但 Panel 只确认过一次：必须继续等到第二次确认。
        let mut progress = RecoveryProgress {
            fresh_identity: Some(panel.identity()),
            fresh_samples: panel.required_count,
            panel_samples: 1,
            ..RecoveryProgress::default()
        };
        assert_eq!(
            decide_recovery_step(&panel, UiResidencyTarget::Primary, &mut progress),
            RecoveryAction::Wait
        );
        progress.record(&panel, Instant::now());
        assert_eq!(
            decide_recovery_step(&panel, UiResidencyTarget::Primary, &mut progress),
            RecoveryAction::Escape
        );
    }

    #[test]
    fn recovery_decision_oscillation_trips_breaker() {
        let primary = snap(UiStateKind::Primary, "primary", "primary_friend_template");
        let menu = snap(UiStateKind::MainMenu, "main_menu", "main_menu_template");
        let mut progress = RecoveryProgress::default();
        progress.kind_tail = vec![
            UiStateKind::Primary,
            UiStateKind::MainMenu,
            UiStateKind::Primary,
            UiStateKind::MainMenu,
        ];
        let action = decide_recovery_step(&primary, UiResidencyTarget::Primary, &mut progress);
        assert!(
            matches!(action, RecoveryAction::Reject(_)),
            "Primary↔MainMenu 振荡必须熔断：{action:?}"
        );
        let _ = menu;
    }

    #[test]
    fn recovery_decision_no_progress_trips_breaker() {
        let primary = snap(UiStateKind::Primary, "primary", "primary_friend_template");
        let mut progress = settled_progress(&primary);
        progress.pending_action_identity = Some(primary.identity());
        for _ in 0..RECOVERY_NO_PROGRESS_LIMIT {
            progress.record(&primary, Instant::now());
        }
        let action = decide_recovery_step(&primary, UiResidencyTarget::Primary, &mut progress);
        assert!(
            matches!(action, RecoveryAction::Reject(_)),
            "动作后状态未变必须熔断：{action:?}"
        );
    }

    #[test]
    fn recovery_decision_loading_only_waits() {
        let loading = snap(UiStateKind::Loading, "loading", "loading_dark");
        let mut progress = settled_progress(&loading);
        assert_eq!(
            decide_recovery_step(&loading, UiResidencyTarget::Primary, &mut progress),
            RecoveryAction::Wait,
            "Loading 只能等待，不得发输入"
        );
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
        let mut frame = load_real_frame("tests/fixtures/ui/hall-primary-1920x1080.jpg", config);
        // 夹具是有损 JPEG，底栏没有纯白；吸附成纯白以匹配线上无损抓图的前提。
        crate::ui::state::snap_near_white_to_pure_white(
            &mut frame,
            config.screen.hall_bar_rect.into(),
            225,
        );
        frame
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

    // ==== 二级 → 一级截图 → 返回驻守（task-61）====

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum CaptureResidencyView {
        SecondaryHallRow,
        Primary,
        SecondaryWithoutHallRow,
    }

    struct CaptureResidencyState {
        view: CaptureResidencyView,
        secondary_after_return: CaptureResidencyView,
        keys: Vec<Key>,
        clicks: Vec<(i32, i32)>,
        drags: usize,
    }

    struct CaptureResidencyDevice {
        primary: DynamicImage,
        secondary_hall_row: DynamicImage,
        secondary_without_hall_row: DynamicImage,
        state: Arc<Mutex<CaptureResidencyState>>,
    }

    impl CaptureResidencyDevice {
        fn new(
            config: &AppConfig,
            view: CaptureResidencyView,
            secondary_after_return: CaptureResidencyView,
        ) -> (Self, Arc<Mutex<CaptureResidencyState>>) {
            let state = Arc::new(Mutex::new(CaptureResidencyState {
                view,
                secondary_after_return,
                keys: Vec::new(),
                clicks: Vec::new(),
                drags: 0,
            }));
            (
                Self {
                    primary: primary_frame(config),
                    secondary_hall_row: secondary_hall_row_frame(config),
                    secondary_without_hall_row: secondary_frame(config),
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl UiDevice for CaptureResidencyDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            let view = self.state.lock().unwrap().view;
            Ok(match view {
                CaptureResidencyView::SecondaryHallRow => self.secondary_hall_row.clone(),
                CaptureResidencyView::Primary => self.primary.clone(),
                CaptureResidencyView::SecondaryWithoutHallRow => {
                    self.secondary_without_hall_row.clone()
                }
            })
        }

        fn ensure_ready(&mut self, _after_activate_ms: u64) -> Result<()> {
            Ok(())
        }

        fn click_point(&mut self, x: i32, y: i32) -> Result<()> {
            self.state.lock().unwrap().clicks.push((x, y));
            Ok(())
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            let mut state = self.state.lock().unwrap();
            state.keys.push(key);
            if key == Key::Escape && state.view == CaptureResidencyView::SecondaryHallRow {
                state.view = CaptureResidencyView::Primary;
            } else if key == Key::Return && state.view == CaptureResidencyView::Primary {
                let next = state.secondary_after_return;
                state.view = next;
            }
            Ok(())
        }

        fn drag_point(&mut self, _from_x: i32, _from_y: i32, _to_x: i32, _to_y: i32) -> Result<()> {
            self.state.lock().unwrap().drags += 1;
            Ok(())
        }
    }

    struct EmptyOcr;

    impl OcrDevice for EmptyOcr {
        fn recognize_lines(&mut self, _image: &DynamicImage) -> Result<Vec<OcrLine>> {
            Ok(Vec::new())
        }
    }

    fn capture_test_config() -> AppConfig {
        let mut config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        config.timing.input.after_activate_ms = 0;
        config.timing.input.open_chat_ms = 0;
        config.timing.input.click_ms = 0;
        config.timing.workflow.default_timeout_ms = 5_000;
        config.timing.workflow.default_poll_ms = 1;
        config
    }

    fn capture_routine_config(config: &AppConfig) -> FriendDeliveryRoutineConfig {
        let mut routine = FriendDeliveryRoutineConfig::from_app(config);
        routine.timeout_ms = 5_000;
        routine.poll_ms = 1;
        routine
    }

    fn submit_capture_primary(
        runtime: &UiRuntime,
        ocr: OcrRuntimeHandle,
        config: FriendDeliveryRoutineConfig,
    ) -> CapturedPrimaryOutcome {
        runtime
            .handle()
            .submit(CapturePrimaryAndRestoreRoutine { ocr, config })
            .unwrap()
            .wait()
            .unwrap()
    }

    #[test]
    fn secondary_capture_navigates_to_primary_then_returns_to_secondary_hall() {
        let config = capture_test_config();
        let (device, state) = CaptureResidencyDevice::new(
            &config,
            CaptureResidencyView::SecondaryHallRow,
            CaptureResidencyView::SecondaryHallRow,
        );
        let ui_runtime = start_test_ui_runtime(device, &config, 4);
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();

        let outcome = submit_capture_primary(
            &ui_runtime,
            ocr_runtime.handle(),
            capture_routine_config(&config),
        );

        let image = outcome.image().unwrap_or_else(|| {
            panic!("二级导航到一级后必须拿到截图，实际失败：{:?}", outcome.failure())
        });
        assert_eq!(
            (image.width(), image.height()),
            (config.screen.expected_width, config.screen.expected_height)
        );
        assert!(outcome.navigated(), "从二级截图必须标记发生过导航");
        assert!(outcome.failure().is_none());
        assert_eq!(
            outcome.residency(),
            &UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall),
            "截完图必须回到当前大厅二级"
        );

        let hall_point = hall_row_click_point(&secondary_hall_row_frame(&config), &config);
        let state = state.lock().unwrap();
        assert_eq!(state.keys, [Key::Escape, Key::Return]);
        assert_eq!(state.clicks, [hall_point], "必须点击「当前大厅」行返回驻守");
        drop(state);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn primary_capture_never_sends_navigation_input() {
        let config = capture_test_config();
        let (device, state) = CaptureResidencyDevice::new(
            &config,
            CaptureResidencyView::Primary,
            CaptureResidencyView::Primary,
        );
        let ui_runtime = start_test_ui_runtime(device, &config, 4);
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();

        let outcome = submit_capture_primary(
            &ui_runtime,
            ocr_runtime.handle(),
            capture_routine_config(&config),
        );

        let image = outcome.image().unwrap_or_else(|| {
            panic!("已在一级时必须直接拿到截图，实际失败：{:?}", outcome.failure())
        });
        assert_eq!(
            (image.width(), image.height()),
            (config.screen.expected_width, config.screen.expected_height)
        );
        assert!(!outcome.navigated());
        assert_eq!(
            outcome.residency(),
            &UiResidencyOutcome::Confirmed(UiResidencyTarget::Primary)
        );
        let state = state.lock().unwrap();
        assert!(state.keys.is_empty(), "一级截图不允许任何按键：{:?}", state.keys);
        assert!(state.clicks.is_empty(), "一级截图不允许任何点击");
        drop(state);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn unknown_residency_fails_before_input_without_pressing_escape() {
        let config = capture_test_config();

        struct UnknownCaptureDevice {
            frame: DynamicImage,
            keys: Arc<Mutex<Vec<Key>>>,
        }

        impl UiDevice for UnknownCaptureDevice {
            fn capture(&mut self) -> Result<DynamicImage> {
                Ok(self.frame.clone())
            }

            fn ensure_ready(&mut self, _after_activate_ms: u64) -> Result<()> {
                Ok(())
            }

            fn press_key(&mut self, key: Key) -> Result<()> {
                self.keys.lock().unwrap().push(key);
                Ok(())
            }
        }

        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui_runtime = start_test_ui_runtime(
            UnknownCaptureDevice {
                frame: unknown_frame(&config),
                keys: Arc::clone(&keys),
            },
            &config,
            4,
        );
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();

        let outcome = submit_capture_primary(
            &ui_runtime,
            ocr_runtime.handle(),
            capture_routine_config(&config),
        );

        assert!(outcome.image().is_none());
        assert!(!outcome.navigated());
        let failure = outcome.failure().expect("无法识别的界面必须明确失败");
        assert_eq!(failure.certainty(), InputCertainty::BeforeInput);
        assert_eq!(failure.stage(), "observe_capture_residency");
        assert!(matches!(
            outcome.residency(),
            UiResidencyOutcome::Failed(_)
        ));
        let recorded_keys = keys.lock().unwrap().clone();
        assert!(
            recorded_keys.is_empty(),
            "Unknown 不许盲按 Escape：{recorded_keys:?}"
        );

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn navigation_guard_allows_only_secondary_residency() {
        for kind in [
            UiStateKind::GameGate,
            UiStateKind::Overworld,
            UiStateKind::MainMenu,
            UiStateKind::Loading,
            UiStateKind::Panel,
            UiStateKind::Unknown,
            UiStateKind::Primary,
        ] {
            assert!(
                !capture_navigation_is_allowed(kind),
                "{kind:?} 不允许为截图发送导航输入"
            );
        }
        assert!(capture_navigation_is_allowed(UiStateKind::Secondary));
    }

    struct FailingCaptureRoutine {
        ocr: OcrRuntimeHandle,
        config: FriendDeliveryRoutineConfig,
    }

    impl sealed::UiRoutineSealed for FailingCaptureRoutine {}

    impl UiRoutine for FailingCaptureRoutine {
        type Output = CapturedPrimaryOutcome;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            let config = self.config;
            capture_primary_and_restore_with(
                context,
                &self.ocr,
                &config,
                |_context, _config, navigated| {
                Err(UiRoutineFailure::new(
                    if navigated {
                        InputCertainty::AfterInputUnknown
                    } else {
                        InputCertainty::BeforeInput
                    },
                    "capture_primary_frame",
                    "injected capture failure",
                ))
            })
        }
    }

    #[test]
    fn capture_failure_after_navigation_still_returns_to_secondary_hall() {
        let config = capture_test_config();
        let (device, state) = CaptureResidencyDevice::new(
            &config,
            CaptureResidencyView::SecondaryHallRow,
            CaptureResidencyView::SecondaryHallRow,
        );
        let ui_runtime = start_test_ui_runtime(device, &config, 4);
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();

        let outcome = ui_runtime
            .handle()
            .submit(FailingCaptureRoutine {
                ocr: ocr_runtime.handle(),
                config: capture_routine_config(&config),
            })
            .unwrap()
            .wait()
            .unwrap();

        assert!(outcome.image().is_none());
        assert!(outcome.navigated());
        let failure = outcome.failure().expect("截图失败必须如实回报");
        assert_eq!(failure.stage(), "capture_primary_frame");
        assert_eq!(failure.certainty(), InputCertainty::AfterInputUnknown);
        assert_eq!(
            outcome.residency(),
            &UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall),
            "截图失败也必须回到当前大厅二级"
        );

        let hall_point = hall_row_click_point(&secondary_hall_row_frame(&config), &config);
        let state = state.lock().unwrap();
        assert!(state.keys.contains(&Key::Escape));
        assert!(state.keys.contains(&Key::Return));
        assert_eq!(state.clicks, [hall_point], "截图失败后仍必须点击当前大厅返回");
        drop(state);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }

    #[test]
    fn restore_secondary_failure_is_reported_truthfully_with_captured_image() {
        let config = capture_test_config();
        let (device, state) = CaptureResidencyDevice::new(
            &config,
            CaptureResidencyView::SecondaryHallRow,
            CaptureResidencyView::SecondaryWithoutHallRow,
        );
        let ui_runtime = start_test_ui_runtime(device, &config, 4);
        let ocr_runtime = OcrRuntime::start(EmptyOcr, 1).unwrap();

        let outcome = submit_capture_primary(
            &ui_runtime,
            ocr_runtime.handle(),
            capture_routine_config(&config),
        );

        let image = outcome.image().unwrap_or_else(|| {
            panic!(
                "返回驻守失败不影响已经拿到的截图，实际失败：{:?}",
                outcome.failure()
            )
        });
        assert_eq!(
            (image.width(), image.height()),
            (config.screen.expected_width, config.screen.expected_height)
        );
        assert!(outcome.navigated());
        assert!(
            outcome.failure().is_none(),
            "截图成功不应被记成截图失败"
        );
        match outcome.residency() {
            UiResidencyOutcome::Failed(failure) => {
                assert_eq!(failure.stage(), "restore_secondary_hall");
                assert_eq!(failure.certainty(), InputCertainty::AfterInputUnknown);
            }
            other => panic!("返回驻守失败必须如实回报，实际 {other:?}"),
        }
        let state = state.lock().unwrap();
        assert!(state.drags >= 1, "找不到大厅行时必须尝试向列表顶部拖动");
        assert!(state.clicks.is_empty(), "找不到大厅行时不许乱点");
        drop(state);

        ui_runtime.shutdown().unwrap();
        ocr_runtime.shutdown().unwrap();
    }
}
