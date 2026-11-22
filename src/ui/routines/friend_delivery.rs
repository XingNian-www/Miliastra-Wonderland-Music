//! 好友投递 / 大厅批量发送 / 驻留恢复 / 表情目录的例程入口。
//!
//! 原先是一个 6179 行的单文件；这里按正交职责拆成子模块（子文件放在
//! `friend_delivery/` 目录，本文件是模块根），本文件只保留
//! **配置结构、公开入口（请求 / 产物 / `*Ui`）、`UiTask` 事务包装、**
//! **一级截图驻留栈**，以及各子模块共用的取帧与失败整形：
//!
//! - `navigation`：好友列表定位、搜索、滚动匹配与会话切换；
//! - `emoji_panel`：表情面板打开、页签定位、卡片网格识别与表情点击输入；
//! - `delivery`：文本分段、混合文本 / 表情发送执行与好友投递自动重试；
//! - `tests` / `emoji_flow_tests`：既有单测，断言一字未改。
//!
//! 模块根必须是这个**文件**（而不是 `friend_delivery/mod.rs`）：
//! `src/runtime/ui.rs` 的「每个 stage 标识都要有中文名」守卫用
//! `include_str!("../ui/routines/friend_delivery.rs")` 读取本文件源码。
//! 拆分出去的 `navigation` / `delivery` / `emoji_panel` 各自另有一份
//! `include_str!`，三条源码全部在该守卫的扫描面内——新增子文件时要同步补上，
//! 否则它里面的 stage 标识会悄悄脱离覆盖。
//!
//! 红线：
//! - 所有 `pub` / `pub(crate)` 路径保持不变（子模块的公开项由本文件统一重导出），
//!   调用方零修改；
//! - 表情依赖只走 `crate::text::emoji`：表情的引擎能力早已下沉到文本层，
//!   本模块（及全部子模块）不得反向引用任何 `features` 下的模块。

mod delivery;
mod emoji_panel;
mod navigation;

#[cfg(test)]
mod emoji_flow_tests;
#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// 子模块的对外点位：保持 `super::friend_delivery::<名字>` 与
// `crate::ui::routines::<名字>` 两条既有路径，调用方零修改。
// ---------------------------------------------------------------------------
pub(crate) use delivery::send_current_chat_message;
pub(crate) use emoji_panel::{EmojiCatalogScanOutcome, EmojiCatalogUi, EmojiTabScan};
pub(crate) use navigation::{
    conversation_row_region, current_hall_restore_drag_points, friend_list_drag_points,
    highlighted_conversation_row, matching_text_rows, ocr_friend_rows, open_friend_conversation,
    point_in_rect,
};

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use image::DynamicImage;

#[cfg(test)]
use crate::config::AppConfig;
use crate::config::{
    InputTimingConfig, OcrConfig, OutputConfig, PointConfig, ScreenConfig, TemplateConfig,
};
use crate::geometry::Rect;
use crate::runtime::ocr::OcrRuntimeHandle;
use crate::runtime::recovery::GameRecoveryCoordinator;
use crate::runtime::ui::{
    InputCertainty, UiOperation, UiRoutine, UiRoutineContext, UiRoutineFailure,
    UiRoutineProgressStage, UiRuntimeHandle, UiStateKind, UiSubmitError, sealed,
};
use crate::text::emoji::EmojiConfig;
use crate::ui::locator::secondary_hall_search_rect;

use super::residency::{ConversationRef, ResidencyTarget, SettleEnvironment, settle_to};
use super::state_observation::{
    UiStateObservationConfig, capture_normalized_ui_state, observe_ui_state_once,
    wait_for_stable_ui_kind,
};
use super::transaction::{EntryPolicy, FinishPolicy, UiTask, UiTaskSpec};

use delivery::{execute_friend_deliveries, execute_hall_batch};

const FRIEND_ROW_Y_TOLERANCE: i32 = 6;
const MAX_FRIEND_LIST_DRAGS: usize = 2;
const FRIEND_AVATAR_TEXT_OFFSET_X: i32 = 40;
const FRIEND_DRAG_MAX_EDGE_INSET: i32 = 50;
/// 已确认的具体好友会话的**脱敏键**（只含指纹，不含昵称/UID）。
///
/// 为什么不直接复用 `residency::ConversationRef`：`ConversationRef::Friend` 带
/// `Vec<String>` 候选名，无法 `Copy`，而 `UiResidencyTarget` 必须保持 `Copy`
/// （`SendHallBatch` / `EstablishResidency` 等按值复用，见
/// `friend_delivery/delivery.rs` 里 `request.residency` 的两次按值传参）。
/// 把候选名搬进对外驻留结论也会把昵称/备注带进日志与文案（隐私泄漏面）。
///
/// 因此这里只保留**归一化后**的 FNV-1a 指纹：
/// - **稳定**：不用 `DefaultHasher`（它不保证跨进程/跨版本一致，见
///   `config/ai.rs` 里 `prefix_cache_key` 的同类说明），同一会话恒定给同一个键；
/// - **可区分**：足以判定"确实落在好友会话 X"与"落在别的会话 / 当前大厅"；
/// - **不可直读**：对外只出现指纹，不出现备注/昵称原文。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ConversationKey(u64);

impl ConversationKey {
    /// 由会话列表里识别到的备注/昵称生成键。
    ///
    /// 先走 `normalize_comparison_text`（折全角 + 去空白 + 小写）：OCR 的全角/空格
    /// 抖动不能让同一个会话产生两个教科书式的"不同会话"。
    pub(crate) fn from_identity(identity: &str) -> Self {
        let normalized = crate::text::normalize_comparison_text(identity);
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in normalized.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self(hash)
    }
}

impl std::fmt::Display for ConversationKey {
    /// 日志/文案里的短标识：只暴露指纹的低 32 位，足够区分同屏几个会话。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "好友会话#{:08x}", self.0 as u32)
    }
}

/// 驻留结论/请求的目标界面。
///
/// 硬规则（`technical-docs/ui-task-return-design.md:24/55/132`）：
/// `SecondaryCurrentHall` **只**代表"二级聊天 · 当前大厅会话"，不得用来代表
/// 任意二级界面；确认落在具体好友会话时用 `SecondaryConversation` 如实上报。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiResidencyTarget {
    /// 千星大厅（一级大厅）。
    Primary,
    /// 二级聊天 · 当前大厅（"大厅"会话）。
    SecondaryCurrentHall,
    /// 二级聊天 · **已确认**落在某个具体好友会话。
    ///
    /// 与 `SecondaryCurrentHall` 的区别只是"哪条会话"：只关心"已在二级聊天"的
    /// 调用方可以与它等价处理；需要会话身份的地方必须按 `ConversationKey` 区分，
    /// 不能把好友私聊说成当前大厅。
    SecondaryConversation(ConversationKey),
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
    /// 任务声明「不产生聊天驻留」，收尾停在哪里都不构成失败（B16-P1-4）。
    ///
    /// 典型场景：冷启动 `EnterGame` 用 `FinishPolicy::Leave` 收尾，结束时停在
    /// 提瓦特大世界；`residency_for(Overworld)` 给不出聊天目标，旧实现把它翻译
    /// 成 `Failed`，于是每次冷启动都报「驻留恢复失败」。`NotApplicable` 表示
    /// 「这个任务本来就不承诺聊天驻留」，与「承诺了但没做到」（仍报 `Failed`）
    /// 严格区分，调用方不得把它当失败上报。
    NotApplicable,
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
    recovery: GameRecoveryCoordinator,
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

/// 驻留恢复例程的入口（`EstablishResidency` / 一级截图往返）。
///
/// 本类型是**恢复器本身**：它不导航、不重复收尾，只把界面推回目标并如实报告结论。
/// 因此它必须与启动链共享同一份 [`GameRecoveryCoordinator`]——否则本对象发起的
/// 每一次恢复采样都记在另一份私有状态里，进程级冷却与互斥就形同虚设。
#[derive(Clone)]
pub(crate) struct ResidencyUi {
    runtime: UiRuntimeHandle,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
    /// 进程级共享的恢复协调器（见 [`SettleEnvironment::with_recovery`]）。
    recovery: GameRecoveryCoordinator,
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
            recovery: GameRecoveryCoordinator::default(),
        }
    }

    /// 用进程级共享的恢复协调器替换默认实例（装配层只 `default()` 一次，其余全是 clone）。
    ///
    /// 不注入时行为与注入前逐条等价：默认实例就是原先「恢复观测只活在本对象里」的语义；
    /// 注入之后，恢复采样、冷却与互斥与启动链、主循环共享同一份状态。
    pub(crate) fn with_recovery(mut self, recovery: GameRecoveryCoordinator) -> Self {
        self.recovery = recovery;
        self
    }

    pub(crate) fn submit(
        &self,
        request: EstablishResidency,
    ) -> std::result::Result<UiOperation<UiResidencyOutcome>, UiSubmitError> {
        self.runtime.submit(EstablishResidencyRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
            recovery: self.recovery.clone(),
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
            recovery: self.recovery.clone(),
        })
    }
}
struct EstablishResidencyRoutine {
    request: EstablishResidency,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

impl sealed::UiRoutineSealed for EstablishResidencyRoutine {}

impl UiRoutine for EstablishResidencyRoutine {
    type Output = UiResidencyOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let target = self.request.target;
        // 它本身就是恢复器：包装器不导航、不重复收尾（AssumeTarget + KeepCurrent）。
        let environment = settle_environment(&self.ocr, &self.config, &self.recovery);
        let spec = UiTaskSpec::new(target, FinishPolicy::KeepCurrent, environment)
            .with_entry(EntryPolicy::AssumeTarget);
        let task = UiTask::new(
            spec,
            EstablishResidencyExecution {
                target,
                ocr: self.ocr,
                config: self.config,
                recovery: self.recovery,
            },
        );
        let outcome = task.execute(context);
        match outcome.output {
            Some(Ok(())) => UiResidencyOutcome::Confirmed(target),
            Some(Err(failure)) => UiResidencyOutcome::Failed(failure),
            None => {
                UiResidencyOutcome::Failed(outcome.entry_failure.clone().unwrap_or_else(|| {
                    UiRoutineFailure::new(
                        InputCertainty::BeforeInput,
                        "prepare_residency",
                        "驻留恢复任务没有开始",
                    )
                }))
            }
        }
    }
}

/// 驻留恢复内层：保留原有恢复算法（它本身就是恢复器，不应被包装器再导航）。
struct EstablishResidencyExecution {
    target: UiResidencyTarget,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
    recovery: GameRecoveryCoordinator,
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
        settle_to(
            context,
            &settle_environment_without_ocr(&self.config, &self.recovery),
            ResidencyTarget::from(self.target),
        )
        .map(|_| ())
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
    recovery: GameRecoveryCoordinator,
}

impl sealed::UiRoutineSealed for CapturePrimaryAndRestoreRoutine {}

impl UiRoutine for CapturePrimaryAndRestoreRoutine {
    type Output = CapturedPrimaryOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let config = self.config;
        capture_primary_and_restore_with(
            context,
            &self.ocr,
            &config,
            &self.recovery,
            capture_primary_frame,
        )
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
/// `recovery` 是该例程所在的共享协调器，随环境一起交给两次恢复（入口识别与收尾）。
///
/// 迁移（task-94）：导航与收尾改由 [`UiTask`] 统一负责，本函数只保留
/// **安全前置**（非一级/二级一律零输入失败）与结果整形，不再自己
/// `restore_primary` / `restore_secondary_hall` 两段导航。
fn capture_primary_and_restore_with(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
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
                "一级截图只支持千星大厅（一级大厅）或二级聊天界面；当前为{}，不发送任何导航输入",
                entry.surface().chinese_name()
            ),
        );
        return CapturedPrimaryOutcome::Failed {
            residency: UiResidencyOutcome::Failed(failure.clone()),
            failure,
            navigated: false,
        };
    }
    let navigated = entry.kind == UiStateKind::Secondary;
    // 目标一级；收尾回到入口（入口二级回二级会话，入口一级回一级；入口不可重建时退当前大厅）。
    // 会话级恢复由唯一 residency 引擎负责，不再自己调 restore_secondary_hall。
    let environment = settle_environment(ocr, config, recovery);
    let spec = UiTaskSpec::new(
        ResidencyTarget::Primary,
        FinishPolicy::RestoreEntry {
            fallback: ResidencyTarget::Secondary(ConversationRef::CurrentHall),
        },
        environment,
    );
    let task = CapturePrimaryFrameTask {
        config: config.clone(),
        navigated,
        capture,
    };
    let outcome = UiTask::new(spec, task).execute(context);
    let residency = outcome.residency.clone();
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
pub(crate) struct InviteReadyGate {
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
    recovery: GameRecoveryCoordinator,
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
            recovery: GameRecoveryCoordinator::default(),
        }
    }

    /// 用进程级共享的恢复协调器替换默认实例（语义同 [`ResidencyUi::with_recovery`]）。
    pub(crate) fn with_recovery(mut self, recovery: GameRecoveryCoordinator) -> Self {
        self.recovery = recovery;
        self
    }

    pub(crate) fn submit(
        &self,
        request: SendHallBatch,
    ) -> std::result::Result<UiOperation<SendHallBatchOutcome>, UiSubmitError> {
        self.runtime.submit(SendHallBatchRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
            recovery: self.recovery.clone(),
        })
    }
}

struct SendHallBatchRoutine {
    request: SendHallBatch,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

impl sealed::UiRoutineSealed for SendHallBatchRoutine {}

impl UiRoutine for SendHallBatchRoutine {
    type Output = SendHallBatchOutcome;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let request = self.request;
        let ready_gated = request.ready.is_some();
        let environment = settle_environment(&self.ocr, &self.config, &self.recovery);
        let spec = hall_batch_task_spec(&request, environment);
        let task = UiTask::new(
            spec,
            HallBatchExecution {
                request,
                ocr: self.ocr,
                config: self.config,
                recovery: self.recovery,
            },
        );
        let outcome = task.execute(context);

        let (sent, status, primary_chat_opened) = match outcome.output.as_ref() {
            Some(inner) => (inner.sent, inner.status.clone(), inner.primary_chat_opened),
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
fn hall_batch_task_spec(request: &SendHallBatch, environment: SettleEnvironment) -> UiTaskSpec {
    if request.ready.is_some() {
        UiTaskSpec::new(
            ResidencyTarget::from(request.residency),
            FinishPolicy::RestoreEntry {
                fallback: ResidencyTarget::from(request.residency),
            },
            environment,
        )
        .with_entry(EntryPolicy::AssumeTarget)
    } else {
        UiTaskSpec::new(
            ResidencyTarget::Secondary(ConversationRef::AnySecondary),
            FinishPolicy::RestoreEntry {
                fallback: ResidencyTarget::from(request.residency),
            },
            environment,
        )
    }
}

struct HallBatchExecution {
    request: SendHallBatch,
    ocr: OcrRuntimeHandle,
    config: FriendDeliveryRoutineConfig,
    recovery: GameRecoveryCoordinator,
}

pub(crate) struct HallBatchInner {
    sent: usize,
    status: HallBatchStatus,
    /// 本次是否主动从一级大厅打开了聊天（业务状态；收尾由包装器负责）。
    primary_chat_opened: bool,
}

impl sealed::UiRoutineSealed for HallBatchExecution {}

impl UiRoutine for HallBatchExecution {
    type Output = HallBatchInner;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        execute_hall_batch(
            context,
            self.request,
            &self.ocr,
            &self.config,
            &self.recovery,
        )
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
            recovery: GameRecoveryCoordinator::default(),
        }
    }

    /// 用进程级共享的恢复协调器替换默认实例（装配层只 `default()` 一次）。
    ///
    /// 语义与 [`ResidencyUi::with_recovery`] 相同：不注入时行为与注入前逐条等价。
    pub(crate) fn with_recovery(mut self, recovery: GameRecoveryCoordinator) -> Self {
        self.recovery = recovery;
        self
    }

    pub(crate) fn submit(
        &self,
        request: SendFriendDeliveries,
    ) -> std::result::Result<UiOperation<SendFriendDeliveriesOutcome>, UiSubmitError> {
        self.runtime.submit(SendFriendDeliveriesRoutine {
            request,
            ocr: self.ocr.clone(),
            config: self.config.clone(),
            recovery: self.recovery.clone(),
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
    emoji_enabled: bool,
    emoji_capture_debug: bool,
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
    pub(crate) emoji: &'a EmojiConfig,
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
            emoji_enabled: source.emoji.enabled,
            emoji_capture_debug: source.emoji.capture_debug,
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
            emoji: &config.emoji,
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

    pub(super) fn secondary_hall_search_region(&self) -> Rect {
        self.secondary_hall_search_region
    }

    pub(super) fn secondary_hall_template(&self) -> &std::path::Path {
        &self.secondary_hall_template
    }

    pub(super) fn template_threshold(&self) -> f32 {
        self.template_threshold
    }

    pub(super) fn open_chat_ms(&self) -> u64 {
        self.open_chat_ms
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
    recovery: GameRecoveryCoordinator,
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
        let environment = settle_environment(&ocr, &config, &self.recovery);
        let spec = UiTaskSpec::new(
            ResidencyTarget::Secondary(ConversationRef::AnySecondary),
            FinishPolicy::RestoreEntry {
                fallback: ResidencyTarget::from(request.residency),
            },
            environment,
        );
        let task = UiTask::new(
            spec,
            FriendDeliveriesExecution {
                request,
                ocr: self.ocr,
                config: self.config,
                recovery: self.recovery,
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
        // 收尾由 `RestoreEntry` 带着**入口真实会话**完成：
        // - 入口好友 A：恢复同一个好友（候选名 + 高亮行），而不是猜当前大厅；
        // - 入口会话无法重建：引擎显式 fallback 当前大厅并置 degraded。
        let session_restore_degraded = outcome
            .finish
            .recovery
            .as_ref()
            .is_some_and(|recovery| recovery.degraded)
            || (entry_was_secondary
                && matches!(
                    residency,
                    UiResidencyOutcome::Confirmed(UiResidencyTarget::SecondaryCurrentHall)
                )
                && outcome
                    .finish
                    .recovery
                    .as_ref()
                    .and_then(|recovery| recovery.settled_conversation.as_ref())
                    == Some(&ConversationRef::CurrentHall)
                && outcome
                    .entry
                    .as_ref()
                    .and_then(|entry| entry.conversation.as_ref())
                    .is_some_and(|conversation| *conversation != ConversationRef::CurrentHall));
        if session_restore_degraded {
            log::warn!(
                "好友投递收尾未能回到入口的具体会话，已按明确 fallback 回到当前大厅（具体会话身份见入场识别），请重新确认当前会话"
            );
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
    recovery: GameRecoveryCoordinator,
}

struct FriendDeliveriesInner {
    outcomes: Vec<FriendDeliveryOutcome>,
}

impl sealed::UiRoutineSealed for FriendDeliveriesExecution {}

impl UiRoutine for FriendDeliveriesExecution {
    type Output = FriendDeliveriesInner;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        FriendDeliveriesInner {
            outcomes: execute_friend_deliveries(
                context,
                self.request,
                &self.ocr,
                &self.config,
                &self.recovery,
            ),
        }
    }
}
/// 子模块共用的恢复环境：本模块内的每一次 `settle_to` 都必须带上**同一份**
/// 恢复协调器，否则「进程级共享冷却与互斥」只在包装器那一层成立，
/// 内层会话级恢复仍会各记各的账。
///
/// 分两种形态：需要 OCR 的会话级恢复用 [`settle_environment`]，
/// 一级 / 当前大厅这类不需要 OCR 的用 [`settle_environment_without_ocr`]。
pub(super) fn settle_environment(
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
) -> SettleEnvironment {
    SettleEnvironment::new(ocr.clone(), config.clone()).with_recovery(recovery.clone())
}

/// 不需要 OCR 的恢复环境（一级 / 当前大厅 / 任意二级）；与 [`settle_environment`] 同源。
pub(super) fn settle_environment_without_ocr(
    config: &FriendDeliveryRoutineConfig,
    recovery: &GameRecoveryCoordinator,
) -> SettleEnvironment {
    SettleEnvironment::without_ocr(config.clone()).with_recovery(recovery.clone())
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
