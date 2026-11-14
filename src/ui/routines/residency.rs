//! 唯一驻留恢复引擎（统一迁移阶段 1）。
//!
//! 设计来源：`technical-docs/ui-task-return-design.md` §3/§4。
//!
//! 本模块把原先散落在 `friend_delivery.rs` 里的 P0 状态驱动恢复
//! （`RecoveryAction` / `RecoveryProgress` / `decide_recovery_step` / `settle_ui`）
//! 收拢成**唯一实现**，并引入会话级目标模型：
//!
//! - [`ResidencyTarget::Primary`]：千星大厅（一级大厅）；
//! - `Secondary`：二级界面/二级聊天。二级聊天按会话确认
//!   （二级聊天·当前大厅 / 二级聊天·好友私聊）；多人面板、千星地图按子型确认；
//!   覆盖层不能作为恢复目标（一次 Esc 退一层）。
//!
//! 约束与动作表：
//! - 只有**已确认**的状态才允许输入；Unknown/Loading 先等待/重采样；
//! - Unknown 允许**一次受控 Esc 探针**（符合最新语义：Esc 退到千星大厅/主世界），
//!   Esc 后必须重新分类：主菜单千星再 Esc；主菜单大世界/Overworld 立即停止；
//! - Panel 连续两次确认仍是 Panel 才 Esc，Esc 后必须重采样；
//! - MainMenu 按 `final_rule` 区分千星（可 Esc）与派蒙（拒绝）；
//! - Loading 只等待；GameGate/Overworld 拒绝并交由启动链；
//! - 动作前后 `(kind,label)` 相同累计 2 次、Primary↔MainMenu 振荡、总 Esc 超限即熔断。
//!
//! `transaction.rs`（阶段 2）与各例程都从这里调用，不再各自实现一份恢复。

use std::sync::Arc;
use std::time::{Duration, Instant};

use enigo::Key;
use image::DynamicImage;

use super::friend_delivery::{
    FriendDeliveryRoutineConfig, conversation_row_region, matching_text_rows, point_in_rect,
};
use super::state_observation::{
    OverlaySurface, SecondarySurface, UiStateSnapshot, UiSurface, capture_normalized_ui_state,
    observe_ui_state_once, wait_ms,
};
use crate::geometry::Point;
use crate::ui::template::{best_template_candidate, best_template_hit};
use crate::runtime::ocr::OcrRuntimeHandle;
use crate::runtime::ui::{
    InputCertainty, UiRoutineContext, UiRoutineFailure, UiStateKind,
};

// ---- 对外常量：调用方与测试共享同一套有界预算 ----

/// 单次恢复最多按 Esc 的次数（超过即失败，不再试探）。
pub(super) const RECOVERY_MAX_ESC: u32 = 3;
/// 无进展熔断：动作后连续两次观察到同一 (kind,label)。
pub(super) const RECOVERY_NO_PROGRESS_LIMIT: u32 = 2;
/// 恢复循环的最大动作步数（等待由时间预算约束，不计入）。
pub(super) const RECOVERY_MAX_STEPS: u32 = 24;
/// 诊断历史保留条数。
pub(super) const RECOVERY_HISTORY_LIMIT: usize = 12;
/// 连续 Unknown 至少采样次数 / 时间窗，之后才允许判定"无证据"。
pub(super) const RECOVERY_UNKNOWN_MIN_SAMPLES: u32 = 3;
pub(super) const RECOVERY_UNKNOWN_WINDOW_MS: u64 = 600;
/// Panel 连续确认次数。
pub(super) const RECOVERY_PANEL_CONFIRM_SAMPLES: u32 = 2;
/// Primary↔MainMenu 振荡熔断阈值。
pub(super) const RECOVERY_OSCILLATION_LIMIT: u32 = 2;
/// 会话级恢复：连续两次确认同一个会话才算落定。
pub(super) const RECOVERY_CONVERSATION_CONFIRM_SAMPLES: u32 = 2;

/// 会话描述。`CurrentHall` 是确定性的安全 fallback；`Friend` 需要身份/候选名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConversationRef {
    /// 当前大厅（二级聊天里的"大厅"会话）。
    CurrentHall,
    /// 好友会话：按候选名 + 高亮行锚点确认，歧义不点击。
    Friend {
        identity: String,
        candidates: Vec<String>,
        /// 入口帧里该会话行的锚点（可选；用于同索引行优先）。
        row_anchor: Option<(i32, i32)>,
    },
    /// 调用方明确只要求"在二级"（导航目标），不确认具体会话。
    AnySecondary,
    /// 二级界面·多人面板（大厅内 F2）。Esc 落点为一级大厅。
    MultiplayerPanel,
    /// 二级界面·千星地图。Esc 落点为一级大厅。
    HallMap,
    /// 覆盖层：一次 Esc 退一层；不可作为恢复目标（无法重建），只能用于描述入口。
    Overlay(OverlaySurface),
    /// 入口观测未能识别会话：恢复时不允许直接当成功，必须走证据/fallback。
    Unknown,
}

impl ConversationRef {
    pub(crate) fn friend(identity: impl Into<String>, candidates: Vec<String>) -> Self {
        Self::Friend {
            identity: identity.into(),
            candidates,
            row_anchor: None,
        }
    }

    pub(crate) fn label(&self) -> String {
        match self {
            Self::CurrentHall => "二级聊天·当前大厅".to_string(),
            Self::Friend { identity, .. } => format!("二级聊天·好友私聊（{identity}）"),
            Self::AnySecondary => "任意二级界面（导航目标）".to_string(),
            Self::MultiplayerPanel => "二级界面·多人面板".to_string(),
            Self::HallMap => "二级界面·千星地图".to_string(),
            Self::Overlay(overlay) => overlay.chinese_name().to_string(),
            Self::Unknown => "未识别的二级界面".to_string(),
        }
    }

    /// 是否是二级聊天（需要会话级确认）。二级界面/覆盖层不走会话确认。
    pub(crate) fn is_chat(&self) -> bool {
        matches!(
            self,
            Self::CurrentHall | Self::Friend { .. } | Self::AnySecondary | Self::Unknown
        )
    }
}

/// 恢复目标。保留 `UiResidencyTarget` 兼容旧调用点。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResidencyTarget {
    Primary,
    Secondary(ConversationRef),
}

impl ResidencyTarget {
    pub(crate) fn current_hall() -> Self {
        Self::Secondary(ConversationRef::CurrentHall)
    }

    fn kind(&self) -> UiStateKind {
        match self {
            Self::Primary => UiStateKind::Primary,
            Self::Secondary(_) => UiStateKind::Secondary,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Primary => "千星大厅（一级大厅）".to_string(),
            Self::Secondary(conversation) => conversation.label(),
        }
    }

    fn conversation(&self) -> Option<&ConversationRef> {
        match self {
            Self::Primary => None,
            Self::Secondary(conversation) => Some(conversation),
        }
    }
}

/// 恢复的可选行为（默认全部开启；调用方可收紧）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct SettleOptions {
    /// 是否允许 Unknown 的一次受控 Esc 探针。
    pub(crate) allow_unknown_escape: bool,
    /// 好友会话无法恢复时，是否允许退化为当前大厅。
    pub(crate) allow_hall_fallback: bool,
    /// 当前大厅行不可见时，是否允许向列表顶部做有界拖动查找。
    pub(crate) allow_conversation_scroll: bool,
}

impl Default for SettleOptions {
    fn default() -> Self {
        Self {
            allow_unknown_escape: true,
            allow_hall_fallback: true,
            allow_conversation_scroll: true,
        }
    }
}

impl SettleOptions {
    pub(crate) fn strict() -> Self {
        Self {
            allow_unknown_escape: false,
            allow_hall_fallback: false,
            allow_conversation_scroll: false,
        }
    }
}

/// 恢复环境：OCR 句柄 + 例程配置。由例程构造后交给唯一恢复入口。
///
/// 之所以把两者打包：恢复当前大厅需要模板/区域，恢复好友会话还需要 OCR；
/// 阶段 2 的 transaction 只需持有 `SettleEnvironment`，不再自带恢复配置。
#[derive(Clone)]
pub(crate) struct SettleEnvironment {
    /// 恢复好友会话时需要；一级/当前大厅路径不需要 OCR。
    ocr: Option<OcrRuntimeHandle>,
    config: FriendDeliveryRoutineConfig,
}

impl SettleEnvironment {
    pub(crate) fn new(ocr: OcrRuntimeHandle, config: FriendDeliveryRoutineConfig) -> Self {
        Self {
            ocr: Some(ocr),
            config,
        }
    }

    /// 不需要 OCR 的恢复（一级 / 当前大厅）：仍走同一入口与环境类型。
    pub(crate) fn without_ocr(config: FriendDeliveryRoutineConfig) -> Self {
        Self { ocr: None, config }
    }

    pub(crate) fn ocr(&self) -> Option<&OcrRuntimeHandle> {
        self.ocr.as_ref()
    }

    pub(crate) fn config(&self) -> &FriendDeliveryRoutineConfig {
        &self.config
    }
}

impl std::fmt::Debug for SettleEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettleEnvironment")
            .field("ocr", &self.ocr.is_some())
            .finish_non_exhaustive()
    }
}

impl From<super::friend_delivery::UiResidencyTarget> for ResidencyTarget {
    fn from(target: super::friend_delivery::UiResidencyTarget) -> Self {
        match target {
            super::friend_delivery::UiResidencyTarget::Primary => Self::Primary,
            super::friend_delivery::UiResidencyTarget::SecondaryCurrentHall => {
                Self::Secondary(ConversationRef::CurrentHall)
            }
        }
    }
}

/// 识别入口会话：高亮行 + OCR 候选名 / 当前大厅模板；识别不了返回 `None`（不猜）。
pub(crate) fn identify_conversation(
    context: &mut UiRoutineContext<'_>,
    env: &SettleEnvironment,
) -> Option<ConversationRef> {
    let config = env.config();
    let image = capture_normalized_ui_state(
        context,
        &config.state_observation(),
        "identify_entry_conversation",
        InputCertainty::BeforeInput,
    )
    .ok()?;
    identify_conversation_from_frame(&image, env)
}

fn identify_conversation_from_frame(
    image: &DynamicImage,
    env: &SettleEnvironment,
) -> Option<ConversationRef> {
    let config = env.config();
    let hall_hit = best_template_hit(
        image,
        Some(config.secondary_hall_search_region()),
        config.secondary_hall_template(),
        config.template_threshold(),
    )
    .ok()
    .flatten();
    let highlighted = super::friend_delivery::highlighted_conversation_row(
        image,
        conversation_row_region(config),
    );
    if let Some(hit) = hall_hit {
        let center = hit.center();
        if highlighted.is_none_or(|row| point_in_rect(center, row)) {
            return Some(ConversationRef::CurrentHall);
        }
    }
    let row = highlighted?;
    let ocr = env.ocr()?;
    let mut rows =
        super::friend_delivery::ocr_friend_rows(ocr, image, config.friend_list_region()).ok()?;
    // 选中行可能滚到会话列表配置区上方（真实好友帧选中行 y≈226/188，列表区从 280 起），
    // 整列表 OCR 读不到它：只补扫这条**唯一高亮行本身**，不扩到聊天正文/输入框。
    if !rows
        .iter()
        .any(|candidate| point_in_rect(candidate.point, row))
    {
        rows.extend(super::friend_delivery::ocr_friend_rows(ocr, image, row).ok()?);
    }
    let inside = rows
        .into_iter()
        .filter(|candidate| point_in_rect(candidate.point, row))
        .collect::<Vec<_>>();
    if inside.len() == 1 {
        let candidate = inside.into_iter().next().expect("len checked");
        return Some(ConversationRef::Friend {
            identity: candidate.text.clone(),
            candidates: vec![candidate.text],
            row_anchor: Some((candidate.point.x, candidate.point.y)),
        });
    }
    None
}

/// 恢复成功报告。
#[derive(Clone, Debug)]
pub(crate) struct RecoveryReport {
    pub(crate) target: ResidencyTarget,
    pub(crate) settled_as: UiStateKind,
    pub(crate) settled_conversation: Option<ConversationRef>,
    /// 会话级恢复是否降级（好友会话不可恢复 → 当前大厅 fallback）。
    pub(crate) degraded: bool,
    pub(crate) esc_presses: u32,
    pub(crate) open_chat_presses: u32,
    pub(crate) conversation_actions: u32,
    pub(crate) resamples: u32,
    /// 最后一次观测快照的文本描述（transaction 的 FinishReport 直接包装它）。
    pub(crate) last_snapshot: Option<String>,
    pub(crate) history: Vec<String>,
}

/// 恢复失败诊断（阶段 2 负责把 `frame` 落盘）。
#[derive(Clone, Debug)]
pub(crate) struct RecoveryFailure {
    pub(crate) failure: UiRoutineFailure,
    pub(crate) target: ResidencyTarget,
    pub(crate) last_snapshot: Option<UiStateSnapshot>,
    pub(crate) history: Vec<String>,
    /// 失败现场帧（当前只在能够安全抓取时附加；落盘由阶段 2 做）。
    pub(crate) frame: Option<Arc<DynamicImage>>,
}

impl RecoveryFailure {
    pub(crate) fn into_failure(self) -> UiRoutineFailure {
        self.failure
    }
}

/// 恢复决策动作。
#[derive(Clone, Debug, PartialEq, Eq)]
enum RecoveryAction {
    Done,
    Wait,
    /// 按 Esc：已确认的二级/主菜单千星/稳定 Panel，或 Unknown 的受控探针。
    Escape,
    /// 按 Return 打开二级聊天。
    OpenChat,
    Reject(String),
}

/// 恢复过程的累计状态（纯数据，单测可直接驱动）。
#[derive(Default)]
struct RecoveryProgress {
    esc_presses: u32,
    open_chat_presses: u32,
    conversation_actions: u32,
    resamples: u32,
    fresh_identity: Option<String>,
    fresh_samples: u32,
    unknown_samples: u32,
    unknown_since: Option<Instant>,
    /// 受控 Esc 探针是否已用过（Unknown 只允许一次）。
    unknown_escape_used: bool,
    /// 上一次动作是否就是 Unknown 探针（用于判断"Esc 后还是 Unknown"）。
    unknown_escape_probe: bool,
    loading_since: Option<Instant>,
    panel_samples: u32,
    conversation_samples: u32,
    conversation_identity: Option<String>,
    /// 当前大厅行不可见时的有界滚动次数。
    hall_scroll_attempts: u32,
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

        if let Some(pending) = self.pending_action_identity.as_deref() {
            if pending == identity.as_str() {
                self.no_progress_streak = self.no_progress_streak.saturating_add(1);
            } else {
                self.no_progress_streak = 0;
            }
        }

        if snapshot.kind == UiStateKind::Unknown {
            self.unknown_samples = self.unknown_samples.saturating_add(1);
            self.unknown_since.get_or_insert(now);
        } else {
            self.unknown_samples = 0;
            self.unknown_since = None;
            // 离开 Unknown 后，探针标记只用于本轮判断；预算保留。
            self.unknown_escape_probe = false;
        }
        if snapshot.kind == UiStateKind::Loading {
            self.loading_since.get_or_insert(now);
        } else {
            self.loading_since = None;
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

    fn settled(&self, snapshot: &UiStateSnapshot) -> bool {
        snapshot.runtime_stable && self.fresh_samples >= snapshot.required_count.max(1)
    }

    fn primary_main_menu_oscillations(&self) -> u32 {
        self.kind_tail
            .windows(2)
            .filter(|pair| {
                matches!(
                    (pair[0], pair[1]),
                    (UiStateKind::Primary, UiStateKind::MainMenu)
                        | (UiStateKind::MainMenu, UiStateKind::Primary)
                )
            })
            .count() as u32
    }

    fn unknown_sustained(&self) -> bool {
        self.unknown_samples >= RECOVERY_UNKNOWN_MIN_SAMPLES
            && self.unknown_since.is_some_and(|since| {
                since.elapsed() >= Duration::from_millis(RECOVERY_UNKNOWN_WINDOW_MS)
            })
    }
}

/// 纯决策：根据一次分类快照与累计进度决定下一步（不访问设备）。
fn target_satisfied(target: &ResidencyTarget, snapshot: &UiStateSnapshot) -> bool {
    let surface = snapshot.surface();
    match target {
        ResidencyTarget::Primary => surface == UiSurface::PrimaryHall,
        ResidencyTarget::Secondary(conversation) => match conversation {
            ConversationRef::AnySecondary => surface.is_secondary(),
            ConversationRef::CurrentHall
            | ConversationRef::Friend { .. }
            | ConversationRef::Unknown => surface.is_secondary_chat(),
            ConversationRef::MultiplayerPanel => {
                surface == UiSurface::Secondary(SecondarySurface::MultiplayerPanel)
            }
            ConversationRef::HallMap => surface == UiSurface::Secondary(SecondarySurface::HallMap),
            // 覆盖层无法重建，不能作为恢复成功目标（入口是覆盖层时交给 fallback）。
            ConversationRef::Overlay(_) => false,
        },
    }
}

fn decide_recovery_step(
    snapshot: &UiStateSnapshot,
    target: &ResidencyTarget,
    progress: &RecoveryProgress,
    options: SettleOptions,
) -> RecoveryAction {
    if progress.no_progress_streak >= RECOVERY_NO_PROGRESS_LIMIT {
        return RecoveryAction::Reject("恢复动作没有产生任何状态变化（无进展熔断）".to_string());
    }
    if progress.primary_main_menu_oscillations() >= RECOVERY_OSCILLATION_LIMIT {
        return RecoveryAction::Reject("千星大厅（一级大厅）与主菜单反复来回（振荡熔断）".to_string());
    }

    let surface = snapshot.surface();
    match &surface {
        UiSurface::PrimaryHall => {
            if !progress.settled(snapshot) {
                return RecoveryAction::Wait;
            }
            match target {
                ResidencyTarget::Primary => RecoveryAction::Done,
                ResidencyTarget::Secondary(conversation) if conversation.is_chat() => {
                    RecoveryAction::OpenChat
                }
                ResidencyTarget::Secondary(conversation) => RecoveryAction::Reject(format!(
                    "{} 不能从千星大厅（一级大厅）直接重建；请由调用方声明 fallback",
                    conversation.label()
                )),
            }
        }
        UiSurface::Secondary(SecondarySurface::Chat) => {
            if !progress.settled(snapshot) {
                return RecoveryAction::Wait;
            }
            match target {
                ResidencyTarget::Primary => {
                    if progress.esc_presses < RECOVERY_MAX_ESC {
                        // 好友私聊/当前大厅按 Esc 都退到一级大厅（用户确认语义）。
                        RecoveryAction::Escape
                    } else {
                        RecoveryAction::Reject("按 Esc 次数已达上限，二级聊天仍未退出一级大厅".to_string())
                    }
                }
                ResidencyTarget::Secondary(conversation) if conversation.is_chat() => {
                    // 目标同为二级聊天：会话确认由 settle_conversation 负责。
                    RecoveryAction::Done
                }
                ResidencyTarget::Secondary(conversation) => RecoveryAction::Reject(format!(
                    "{} 是二级界面/覆盖层，不能从二级聊天重建",
                    conversation.label()
                )),
            }
        }
        UiSurface::Secondary(SecondarySurface::MultiplayerPanel) => {
            if !progress.settled(snapshot) {
                return RecoveryAction::Wait;
            }
            match target {
                ResidencyTarget::Primary => {
                    if progress.esc_presses < RECOVERY_MAX_ESC {
                        RecoveryAction::Escape // F2 多人面板 Esc -> 一级大厅
                    } else {
                        RecoveryAction::Reject("按 Esc 次数已达上限，二级界面·多人面板仍未关闭".to_string())
                    }
                }
                ResidencyTarget::Secondary(ConversationRef::MultiplayerPanel) => {
                    RecoveryAction::Done
                }
                ResidencyTarget::Secondary(conversation) if conversation.is_chat() => {
                    if progress.esc_presses < RECOVERY_MAX_ESC {
                        RecoveryAction::Escape
                    } else {
                        RecoveryAction::Reject("按 Esc 次数已达上限，无法离开二级界面·多人面板".to_string())
                    }
                }
                ResidencyTarget::Secondary(conversation) => RecoveryAction::Reject(format!(
                    "{} 不能从二级界面·多人面板重建",
                    conversation.label()
                )),
            }
        }
        UiSurface::Secondary(SecondarySurface::HallMap) => {
            if !progress.settled(snapshot) {
                return RecoveryAction::Wait;
            }
            match target {
                ResidencyTarget::Primary => {
                    if progress.esc_presses < RECOVERY_MAX_ESC {
                        RecoveryAction::Escape // 千星地图 Esc -> 一级大厅
                    } else {
                        RecoveryAction::Reject("按 Esc 次数已达上限，二级界面·千星地图仍未关闭".to_string())
                    }
                }
                ResidencyTarget::Secondary(ConversationRef::HallMap) => RecoveryAction::Done,
                ResidencyTarget::Secondary(conversation) if conversation.is_chat() => {
                    if progress.esc_presses < RECOVERY_MAX_ESC {
                        RecoveryAction::Escape
                    } else {
                        RecoveryAction::Reject("按 Esc 次数已达上限，无法离开二级界面·千星地图".to_string())
                    }
                }
                ResidencyTarget::Secondary(conversation) => RecoveryAction::Reject(format!(
                    "{} 不能从二级界面·千星地图重建",
                    conversation.label()
                )),
            }
        }
        UiSurface::Overlay(overlay) => {
            // 覆盖层与旧 Panel 判据一致：连续两次确认仍是同一覆盖层才允许 Esc。
            if progress.panel_samples < RECOVERY_PANEL_CONFIRM_SAMPLES
                || !progress.settled(snapshot)
            {
                return RecoveryAction::Wait;
            }
            match target {
                ResidencyTarget::Secondary(ConversationRef::Overlay(_)) => {
                    RecoveryAction::Reject("覆盖层不可重建，不能作为恢复目标".to_string())
                }
                _ => {
                    if progress.esc_presses < RECOVERY_MAX_ESC {
                        // 覆盖层一次 Esc 退一层：叠在 Secondary 上回 Secondary、
                        // 叠在 Primary 上回 Primary；下一帧按实际落点继续判断。
                        let _ = overlay;
                        RecoveryAction::Escape
                    } else {
                        RecoveryAction::Reject("按 Esc 次数已达上限，覆盖层仍未关闭".to_string())
                    }
                }
            }
        }
        UiSurface::WonderlandMainMenu => {
            if !progress.settled(snapshot) {
                return RecoveryAction::Wait;
            }
            // 主菜单千星按 Esc 回千星大厅（「返回提瓦特」用于区分，不进此地）。
            if progress.esc_presses < RECOVERY_MAX_ESC {
                RecoveryAction::Escape
            } else {
                RecoveryAction::Reject("按 Esc 次数已达上限，主菜单千星仍未关闭".to_string())
            }
        }
        UiSurface::PaimonMenu => RecoveryAction::Reject(
            "落到主菜单大世界（ESC 系统菜单），不是千星界面，立即停止并交回启动链".to_string()),
        UiSurface::Loading => RecoveryAction::Wait,
        UiSurface::Overworld => RecoveryAction::Reject(
            "当前是提瓦特大世界，不在千星界面内，交回启动链".to_string()),
        UiSurface::GameGate => RecoveryAction::Reject(
            "当前是登录入口（Game Gate），不在游戏内，交回启动链".to_string()),
        UiSurface::Unknown => {
            if !progress.unknown_sustained() {
                return RecoveryAction::Wait;
            }
            if progress.unknown_escape_probe {
                return RecoveryAction::Reject(
                    "受控 Esc 探针之后界面仍然是无法识别（Unknown）且无任何稳定证据，停止继续按键".to_string());
            }
            if progress.unknown_escape_used || !options.allow_unknown_escape {
                return RecoveryAction::Reject(
                    "界面持续无法识别且没有可用证据，受控 Esc 预算已用尽或不允许探针".to_string());
            }
            if progress.esc_presses < RECOVERY_MAX_ESC {
                RecoveryAction::Escape
            } else {
                RecoveryAction::Reject("按 Esc 次数已达上限，仍未识别出稳定界面".to_string())
            }
        }
    }
}


/// 会话确认/导航结果。
enum ConversationStep {
    /// 已确认目标会话（连续确认由调用方计数）。
    Satisfied,
    /// 需要点击某一行以切换到目标会话。
    Navigate(Point),
    /// 已执行一次有界滚动/导航动作，重新采样后再判断。
    Continue,
    /// 歧义或缺失：不能点。
    Unavailable(String),
}

/// 唯一恢复入口（例程与 transaction 都走这里）。
pub(crate) fn settle_to(
    context: &mut UiRoutineContext<'_>,
    env: &SettleEnvironment,
    target: ResidencyTarget,
) -> std::result::Result<RecoveryReport, UiRoutineFailure> {
    settle_to_with(context, env, target, SettleOptions::default())
        .map_err(RecoveryFailure::into_failure)
}

/// 带完整诊断与可调行为的恢复入口（失败现场帧接口留给阶段 2 落盘）。
pub(crate) fn settle_to_with(
    context: &mut UiRoutineContext<'_>,
    env: &SettleEnvironment,
    target: ResidencyTarget,
    options: SettleOptions,
) -> std::result::Result<RecoveryReport, RecoveryFailure> {
    let ocr = env.ocr();
    let config = env.config();
    let observation_config = config.state_observation();
    let target_kind = target.kind();
    let deadline = Instant::now() + Duration::from_millis(config.timeout_ms().max(1));
    let mut progress = RecoveryProgress::default();
    let mut action_taken = false;
    let mut steps = 0_u32;

    loop {
        if steps >= RECOVERY_MAX_STEPS {
            return Err(build_failure(
                context,
                config,
                &target,
                None,
                &progress,
                None,
                "恢复超过最大动作步数",
            ));
        }

        let now = Instant::now();
        let (stage, certainty) = stage_names(target_kind, action_taken);
        let snapshot = match observe_ui_state_once(context, observation_config, stage, certainty) {
            Ok(snapshot) => snapshot,
            Err(failure) => {
                return Err(RecoveryFailure {
                    failure,
                    target,
                    last_snapshot: None,
                    history: progress.history,
                    frame: None,
                });
            }
        };
        progress.record(&snapshot, now);

        if now >= deadline {
            let reason = match snapshot.kind {
                UiStateKind::Loading => "加载界面持续超出恢复预算",
                UiStateKind::Unknown => "界面持续无法识别且没有任何稳定证据",
                _ => "恢复未在时间预算内到达目标界面",
            };
            return Err(build_failure(
                context,
                config,
                &target,
                Some(snapshot),
                &progress,
                None,
                reason,
            ));
        }

        // 目标界面已确认：二级聊天需要再确认会话；二级界面/覆盖层/一级直接按表面判定。
        if target_satisfied(&target, &snapshot) && progress.settled(&snapshot) {
            match target.conversation().filter(|conversation| conversation.is_chat()) {
                None => {
                    let settled = target.conversation().cloned();
                    return Ok(build_report(target, snapshot, settled, false, progress));
                }
                Some(conversation) => {
                    match settle_conversation(
                        context,
                        ocr,
                        config,
                        &snapshot,
                        conversation,
                        &mut progress,
                        options,
                        &mut steps,
                    ) {
                        Ok(ConversationOutcome::Settled(degraded, settled_conversation)) => {
                            return Ok(build_report(
                                target,
                                snapshot,
                                settled_conversation,
                                degraded,
                                progress,
                            ));
                        }
                        Ok(ConversationOutcome::Continue) => {}
                        Err(reason) => {
                            return Err(build_failure(
                                context,
                                config,
                                &target,
                                Some(snapshot),
                                &progress,
                                None,
                                &reason,
                            ));
                        }
                    }
                    action_taken = true;
                    wait_ms(
                        context,
                        config.poll_ms.max(10),
                        stage,
                        InputCertainty::AfterInputUnknown,
                    )
                    .map_err(|failure| RecoveryFailure {
                        failure,
                        target: target.clone(),
                        last_snapshot: Some(snapshot.clone()),
                        history: progress.history.clone(),
                        frame: None,
                    })?;
                    continue;
                }
            }
        }

        let action = decide_recovery_step(&snapshot, &target, &progress, options);
        match action {
            RecoveryAction::Done => {
                // 表面确认但会话尚未确认的路径应在上面处理；到这里的 Done 只可能是
                // 无需会话确认的目标（一级大厅/二级界面/AnySecondary）。
                return Ok(build_report(target, snapshot, None, false, progress));
            }
            RecoveryAction::Wait => {}
            RecoveryAction::Escape => {
                action_taken = true;
                steps = steps.saturating_add(1);
                let pre_action_identity = snapshot.identity();
                context
                    .device()
                    .press_key(Key::Escape)
                    .map_err(|error| RecoveryFailure {
                        failure: after_input_failure("restore_residency", error),
                        target: target.clone(),
                        last_snapshot: Some(snapshot.clone()),
                        history: progress.history.clone(),
                        frame: None,
                    })?;
                progress.esc_presses = progress.esc_presses.saturating_add(1);
                if snapshot.kind == UiStateKind::Unknown {
                    progress.unknown_escape_used = true;
                    progress.unknown_escape_probe = true;
                }
                progress.pending_action_identity = Some(pre_action_identity);
                wait_ms(
                    context,
                    config.poll_ms.max(10),
                    "restore_residency_wait",
                    InputCertainty::AfterInputUnknown,
                )
                .map_err(|failure| RecoveryFailure {
                    failure,
                    target: target.clone(),
                    last_snapshot: Some(snapshot.clone()),
                    history: progress.history.clone(),
                    frame: None,
                })?;
            }
            RecoveryAction::OpenChat => {
                action_taken = true;
                steps = steps.saturating_add(1);
                let pre_action_identity = snapshot.identity();
                context
                    .device()
                    .press_key(Key::Return)
                    .map_err(|error| RecoveryFailure {
                        failure: after_input_failure("open_secondary_chat", error),
                        target: target.clone(),
                        last_snapshot: Some(snapshot.clone()),
                        history: progress.history.clone(),
                        frame: None,
                    })?;
                progress.open_chat_presses = progress.open_chat_presses.saturating_add(1);
                progress.pending_action_identity = Some(pre_action_identity);
                wait_ms(
                    context,
                    config.open_chat_ms(),
                    "open_secondary_chat",
                    InputCertainty::AfterInputUnknown,
                )
                .map_err(|failure| RecoveryFailure {
                    failure,
                    target: target.clone(),
                    last_snapshot: Some(snapshot.clone()),
                    history: progress.history.clone(),
                    frame: None,
                })?;
            }
            RecoveryAction::Reject(reason) => {
                return Err(build_failure(
                    context,
                    config,
                    &target,
                    Some(snapshot),
                    &progress,
                    None,
                    &reason,
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
        )
        .map_err(|failure| RecoveryFailure {
            failure,
            target: target.clone(),
            last_snapshot: None,
            history: progress.history.clone(),
            frame: None,
        })?;
    }
}

enum ConversationOutcome {
    Settled(bool, Option<ConversationRef>),
    Continue,
}

/// 会话级确认/导航。返回 `Settled` 前要求连续确认同一个会话。
#[allow(clippy::too_many_arguments)]
fn settle_conversation(
    context: &mut UiRoutineContext<'_>,
    ocr: Option<&OcrRuntimeHandle>,
    config: &FriendDeliveryRoutineConfig,
    snapshot: &UiStateSnapshot,
    conversation: &ConversationRef,
    progress: &mut RecoveryProgress,
    options: SettleOptions,
    steps: &mut u32,
) -> std::result::Result<ConversationOutcome, String> {
    // 调用方明确声明"任意二级"（导航目标）：kind 已确认，直接成功。
    if *conversation == ConversationRef::AnySecondary {
        return Ok(ConversationOutcome::Settled(false, Some(ConversationRef::AnySecondary)));
    }
    // Unknown 不能直接当成功：只确认“在二级”无法证明会话，强制按当前大厅
    // 证据确认，确认不了就失败（不冒充 SecondaryCurrentHall）。
    if *conversation == ConversationRef::Unknown {
        // 不冒充当前大厅：按当前大厅证据确认，但**不额外滚动列表**；
        // 确认不了就如实失败（调用方按 fallback 处理）。
        let mut strict = options;
        strict.allow_conversation_scroll = false;
        return settle_conversation(
            context,
            ocr,
            config,
            snapshot,
            &ConversationRef::CurrentHall,
            progress,
            strict,
            steps,
        )
        .map(|outcome| match outcome {
            ConversationOutcome::Settled(_, settled) => {
                ConversationOutcome::Settled(true, settled)
            }
            other => other,
        });
    }
    let clicked = progress.conversation_actions > 0;
    let step = match conversation_step(context, ocr, config, conversation, clicked, progress, options) {
        Ok(step) => step,
        Err(failure) => return Err(failure.reason().to_string()),
    };
    match step {
        ConversationStep::Satisfied => {
            let identity = conversation.label();
            if progress.conversation_identity.as_deref() == Some(identity.as_str()) {
                progress.conversation_samples = progress.conversation_samples.saturating_add(1);
            } else {
                progress.conversation_identity = Some(identity);
                progress.conversation_samples = 1;
            }
            if progress.conversation_samples >= RECOVERY_CONVERSATION_CONFIRM_SAMPLES {
                return Ok(ConversationOutcome::Settled(
                    false,
                    Some(conversation.clone()),
                ));
            }
            Ok(ConversationOutcome::Continue)
        }
        ConversationStep::Continue => Ok(ConversationOutcome::Continue),
        ConversationStep::Navigate(point) => {
            if *steps >= RECOVERY_MAX_STEPS {
                return Err("会话导航动作超过最大步数".to_string());
            }
            progress.conversation_samples = 0;
            progress.conversation_identity = None;
            context
                .device()
                .click_point(point.x, point.y)
                .map_err(|error| format!("{error:#}"))?;
            *steps = steps.saturating_add(1);
            progress.conversation_actions = progress.conversation_actions.saturating_add(1);
            wait_ms(
                context,
                config.click_ms.max(10),
                "restore_residency_conversation",
                InputCertainty::AfterInputUnknown,
            )
            .map_err(|failure| failure.reason().to_string())?;
            Ok(ConversationOutcome::Continue)
        }
        ConversationStep::Unavailable(reason) => {
            if let Some(fallback) = hall_fallback(conversation, options) {
                let point =
                    match conversation_step(context, ocr, config, &fallback, clicked, progress, options)
                    {
                    Ok(ConversationStep::Navigate(point)) => point,
                    Ok(ConversationStep::Satisfied) => {
                        return Ok(ConversationOutcome::Settled(true, Some(ConversationRef::CurrentHall)));
                    }
                    Ok(ConversationStep::Continue) => return Ok(ConversationOutcome::Continue),
                    Ok(ConversationStep::Unavailable(reason)) => {
                        return Err(format!("{reason}（fallback 当前大厅同样不可用）"));
                    }
                    Err(failure) => return Err(failure.reason().to_string()),
                };
                if *steps >= RECOVERY_MAX_STEPS {
                    return Err("会话 fallback 动作超过最大步数".to_string());
                }
                context
                    .device()
                    .click_point(point.x, point.y)
                    .map_err(|error| format!("{error:#}"))?;
                *steps = steps.saturating_add(1);
                progress.conversation_actions = progress.conversation_actions.saturating_add(1);
                wait_ms(
                    context,
                    config.click_ms.max(10),
                    "restore_residency_conversation_fallback",
                    InputCertainty::AfterInputUnknown,
                )
                .map_err(|failure| failure.reason().to_string())?;
                // fallback 后仍需下一次循环重新确认当前大厅。
                progress.conversation_samples = 0;
                return Ok(ConversationOutcome::Continue);
            }
            Err(reason)
        }
    }
    .map(|outcome| {
        let _ = snapshot;
        outcome
    })
}

/// 只有"好友会话不可用 + 明确允许"时才退化为当前大厅；Unknown 目标不退化。
fn hall_fallback(
    conversation: &ConversationRef,
    options: SettleOptions,
) -> Option<ConversationRef> {
    match conversation {
        ConversationRef::Friend { .. } if options.allow_hall_fallback => {
            Some(ConversationRef::CurrentHall)
        }
        ConversationRef::Friend { .. } => None,
        ConversationRef::AnySecondary
        | ConversationRef::CurrentHall
        | ConversationRef::Unknown
        | ConversationRef::MultiplayerPanel
        | ConversationRef::HallMap
        | ConversationRef::Overlay(_) => None,
    }
}

/// 读一帧，判断目标会话当前是否已经满足，或需要点哪一行。
fn conversation_step(
    context: &mut UiRoutineContext<'_>,
    ocr: Option<&OcrRuntimeHandle>,
    config: &FriendDeliveryRoutineConfig,
    conversation: &ConversationRef,
    already_clicked: bool,
    progress: &mut RecoveryProgress,
    options: SettleOptions,
) -> std::result::Result<ConversationStep, UiRoutineFailure> {
    let image = capture_conversation_frame(context, config)?;
    match conversation {
        ConversationRef::CurrentHall => {
            let Some(hit) = best_template_hit(
                &image,
                Some(config.secondary_hall_search_region()),
                config.secondary_hall_template(),
                config.template_threshold(),
            )
            .map_err(|error| {
                UiRoutineFailure::new(
                    InputCertainty::AfterInputUnknown,
                    "restore_residency_conversation",
                    format!("{error:#}"),
                )
            })?
            else {
                if options.allow_conversation_scroll && progress.hall_scroll_attempts < 2 {
                    progress.hall_scroll_attempts = progress.hall_scroll_attempts.saturating_add(1);
                    let (from, to) = super::friend_delivery::current_hall_restore_drag_points(
                        config.friend_list_region(),
                    );
                    context
                        .device()
                        .drag_point(from.x, from.y, to.x, to.y)
                        .map_err(|error| {
                            UiRoutineFailure::new(
                                InputCertainty::AfterInputUnknown,
                                "restore_residency_conversation",
                                format!("{error:#}"),
                            )
                        })?;
                    wait_ms(
                        context,
                        config.poll_ms.max(10),
                        "restore_residency_conversation",
                        InputCertainty::AfterInputUnknown,
                    )?;
                    return Ok(ConversationStep::Continue);
                }
                return Ok(ConversationStep::Unavailable(
                    "当前大厅行不可见（模板未命中）".to_string(),
                ));
            };
            let highlighted = super::friend_delivery::highlighted_conversation_row(
                &image,
                conversation_row_region(config),
            );
            let center = hit.center();
            // 已经点过一次大厅行：模板仍可见即认定已切到当前大厅（有界，避免循环点击）。
            if already_clicked {
                return Ok(ConversationStep::Satisfied);
            }
            match highlighted {
                // 大厅行模板可见且没有别的行被选中：就是当前大厅。
                None => Ok(ConversationStep::Satisfied),
                Some(row) if point_in_rect(center, row) => Ok(ConversationStep::Satisfied),
                Some(_) => Ok(ConversationStep::Navigate(center)),
            }
        }
        ConversationRef::Friend { candidates, .. } => {
            let Some(ocr) = ocr else {
                return Ok(ConversationStep::Unavailable(
                    "缺少 OCR 句柄，无法核对好友会话".to_string(),
                ));
            };
            let targets = candidates
                .iter()
                .map(|candidate| crate::text::normalize_comparison_text(candidate))
                .collect::<Vec<_>>();
            let mut matches = matching_text_rows(
                ocr,
                &image,
                config.friend_list_region(),
                &targets,
            )
            .map_err(|failure| {
                UiRoutineFailure::new(
                    failure.certainty(),
                    "restore_residency_conversation",
                    failure.reason(),
                )
            })?;
            let Some(highlighted) = super::friend_delivery::highlighted_conversation_row(
                &image,
                conversation_row_region(config),
            ) else {
                return Ok(ConversationStep::Unavailable(
                    "未找到唯一的高亮会话行".to_string(),
                ));
            };
            // 选中的会话行可能滚到列表配置区上方（真实好友帧 y≈226/188）：
            // 整列表 OCR 看不到它，只补扫这条**唯一高亮行本身**再判是否已在目标会话。
            if !matches.iter().any(|point| point_in_rect(*point, highlighted)) {
                matches.extend(
                    matching_text_rows(ocr, &image, highlighted, &targets).map_err(|failure| {
                        UiRoutineFailure::new(
                            failure.certainty(),
                            "restore_residency_conversation",
                            failure.reason(),
                        )
                    })?,
                );
            }
            // 歧义不点：同一候选命中**多个不同行**时无法确定目标会话，先判歧义，
            // 不能因为其中一行正好高亮就把同名/重复命中当成确定身份。
            let distinct_rows = matches
                .iter()
                .map(|point| point.y / 40)
                .collect::<std::collections::BTreeSet<_>>();
            if distinct_rows.len() > 1 {
                return Ok(ConversationStep::Unavailable(
                    "目标好友在会话列表里命中多行，存在歧义，拒绝点击".to_string(),
                ));
            }
            if matches
                .iter()
                .any(|point| point_in_rect(*point, highlighted))
            {
                return Ok(ConversationStep::Satisfied);
            }
            if let Some(point) = matches.first() {
                if already_clicked {
                    return Ok(ConversationStep::Satisfied);
                }
                return Ok(ConversationStep::Navigate(*point));
            }
            // 当前列表没看到目标：允许轻量向下/向上滚动查找（有界）。
            let Some(point) = locate_friend_row_with_scroll(context, ocr, config, &targets)?
            else {
                return Ok(ConversationStep::Unavailable(
                    "会话列表中找不到目标好友行".to_string(),
                ));
            };
            Ok(ConversationStep::Navigate(point))
        }
        ConversationRef::AnySecondary | ConversationRef::Unknown => Ok(ConversationStep::Satisfied),
        ConversationRef::MultiplayerPanel | ConversationRef::HallMap | ConversationRef::Overlay(_) => {
            Ok(ConversationStep::Unavailable(
                "目标不是二级聊天会话，不能按会话级恢复".to_string(),
            ))
        }
    }
}

fn capture_conversation_frame(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
) -> std::result::Result<DynamicImage, UiRoutineFailure> {
    capture_normalized_ui_state(
        context,
        &config.state_observation(),
        "confirm_conversation",
        InputCertainty::AfterInputUnknown,
    )
}

fn locate_friend_row_with_scroll(
    context: &mut UiRoutineContext<'_>,
    ocr: &OcrRuntimeHandle,
    config: &FriendDeliveryRoutineConfig,
    targets: &[String],
) -> std::result::Result<Option<Point>, UiRoutineFailure> {
    for _ in 0..2 {
        // 设备不支持滚动（测试替身/降级环境）：把"找不到行"交给调用方走 fallback，
        // 而不是把恢复整个打断。
        if context
            .device()
            .scroll_point(
                config.friend_list_region().x + config.friend_list_region().width as i32 / 2,
                config.friend_list_region().y + config.friend_list_region().height as i32 / 2,
                240,
            )
            .is_err()
        {
            return Ok(None);
        }
        wait_ms(
            context,
            config.poll_ms.max(10),
            "restore_residency_conversation",
            InputCertainty::AfterInputUnknown,
        )?;
        let image = capture_conversation_frame(context, config)?;
        let matches = matching_text_rows(ocr, &image, config.friend_list_region(), targets)?;
        match matches.as_slice() {
            [point] => return Ok(Some(*point)),
            [] => continue,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

fn stage_names(target_kind: UiStateKind, action_taken: bool) -> (&'static str, InputCertainty) {
    match (target_kind, action_taken) {
        (UiStateKind::Primary, true) => ("confirm_primary_residency", InputCertainty::AfterInputUnknown),
        (UiStateKind::Primary, false) => ("observe_primary_residency", InputCertainty::BeforeInput),
        (_, true) => ("confirm_secondary_chat", InputCertainty::AfterInputUnknown),
        (_, false) => ("observe_friend_delivery_start", InputCertainty::BeforeInput),
    }
}

fn build_report(
    target: ResidencyTarget,
    snapshot: UiStateSnapshot,
    settled_conversation: Option<ConversationRef>,
    degraded: bool,
    progress: RecoveryProgress,
) -> RecoveryReport {
    RecoveryReport {
        target,
        settled_as: snapshot.kind,
        settled_conversation,
        degraded,
        esc_presses: progress.esc_presses,
        open_chat_presses: progress.open_chat_presses,
        conversation_actions: progress.conversation_actions,
        resamples: progress.resamples,
        last_snapshot: Some(snapshot.describe()),
        history: progress.history,
    }
}

fn build_failure(
    context: &mut UiRoutineContext<'_>,
    config: &FriendDeliveryRoutineConfig,
    target: &ResidencyTarget,
    last_snapshot: Option<UiStateSnapshot>,
    progress: &RecoveryProgress,
    frame: Option<Arc<DynamicImage>>,
    reason: &str,
) -> RecoveryFailure {
    let stage: &'static str = match target {
        ResidencyTarget::Primary => "confirm_primary_residency",
        ResidencyTarget::Secondary(_) => "confirm_secondary_chat",
    };
    let certainty = if progress.esc_presses + progress.open_chat_presses + progress.conversation_actions
        > 0
    {
        InputCertainty::AfterInputUnknown
    } else {
        InputCertainty::BeforeInput
    };
    let snapshot_text = last_snapshot
        .as_ref()
        .map(UiStateSnapshot::describe)
        .unwrap_or_else(|| "n/a".to_string());
    let failure = UiRoutineFailure::new(
        certainty,
        stage,
        format!(
            "状态驱动恢复中止（{reason}）：目标={}；当前={snapshot_text}；Esc={} Return={} 会话动作={} 采样={}；历史={}",
            target.describe(),
            progress.esc_presses,
            progress.open_chat_presses,
            progress.conversation_actions,
            progress.resamples,
            progress.history.join(" <- ")
        ),
    );
    let frame = frame.or_else(|| {
        // 失败现场帧：尽力抓一帧供阶段 2 落盘；抓不到不影响失败结论。
        capture_normalized_ui_state(
            context,
            &config.state_observation(),
            stage,
            InputCertainty::AfterInputUnknown,
        )
        .ok()
        .map(Arc::new)
    });
    RecoveryFailure {
        failure,
        target: target.clone(),
        last_snapshot,
        history: progress.history.clone(),
        frame,
    }
}

fn after_input_failure(stage: &'static str, error: impl std::fmt::Display) -> UiRoutineFailure {
    UiRoutineFailure::new(
        InputCertainty::AfterInputUnknown,
        stage,
        format!("{error:#}"),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use anyhow::Result;
    use image::{DynamicImage, GenericImage, GenericImageView};

    use super::*;
    use crate::config::AppConfig;
    use crate::geometry::Rect;
    use crate::runtime::ocr::{OcrDevice, OcrLine, OcrRuntime};

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

    fn settled(snapshot: &UiStateSnapshot) -> RecoveryProgress {
        let mut progress = RecoveryProgress::default();
        for _ in 0..snapshot.required_count.max(1) {
            progress.record(snapshot, Instant::now());
        }
        progress
    }

    fn decide(
        snapshot: &UiStateSnapshot,
        target_kind: UiStateKind,
        progress: &RecoveryProgress,
    ) -> RecoveryAction {
        let target = match target_kind {
            UiStateKind::Primary => ResidencyTarget::Primary,
            _ => ResidencyTarget::Secondary(ConversationRef::AnySecondary),
        };
        decide_recovery_step(snapshot, &target, progress, SettleOptions::default())
    }

    #[test]
    fn recovery_decision_rejects_game_gate_and_overworld_without_input() {
        for kind in [UiStateKind::GameGate, UiStateKind::Overworld] {
            let snapshot = snap(kind, "gate", "game_gate_template");
            let progress = settled(&snapshot);
            let action = decide(&snapshot, UiStateKind::Primary, &progress);
            assert!(matches!(action, RecoveryAction::Reject(_)), "{kind:?} 必须拒绝：{action:?}");
        }
    }

    #[test]
    fn recovery_decision_unknown_waits_then_uses_one_controlled_escape() {
        let snapshot = snap(UiStateKind::Unknown, "unknown", "no_reliable_anchor");
        let mut progress = settled(&snapshot);
        // 瞬失：只等待，不按键。
        assert_eq!(decide(&snapshot, UiStateKind::Primary, &progress), RecoveryAction::Wait);
        for _ in 0..RECOVERY_UNKNOWN_MIN_SAMPLES {
            progress.record(&snapshot, Instant::now());
        }
        progress.unknown_since =
            Some(Instant::now() - Duration::from_millis(RECOVERY_UNKNOWN_WINDOW_MS + 50));
        // 持续无证据：允许一次受控 Esc 探针（不是盲按）。
        assert_eq!(decide(&snapshot, UiStateKind::Primary, &progress), RecoveryAction::Escape);
        // 探针之后仍是 Unknown：停止。
        progress.unknown_escape_used = true;
        progress.unknown_escape_probe = true;
        assert!(matches!(
            decide(&snapshot, UiStateKind::Primary, &progress),
            RecoveryAction::Reject(_)
        ));
    }

    #[test]
    fn recovery_decision_unknown_never_escapes_without_sustain_window() {
        let snapshot = snap(UiStateKind::Unknown, "unknown", "no_reliable_anchor");
        let mut progress = RecoveryProgress::default();
        progress.record(&snapshot, Instant::now());
        let strict = decide_recovery_step(
            &snapshot,
            &ResidencyTarget::Primary,
            &progress,
            SettleOptions::strict(),
        );
        assert_eq!(strict, RecoveryAction::Wait);
    }

    #[test]
    fn recovery_decision_paimon_menu_is_rejected_even_when_stable() {
        let snapshot = snap(UiStateKind::MainMenu, "main_menu", "paimon_menu_grid");
        let progress = settled(&snapshot);
        let action = decide(&snapshot, UiStateKind::Secondary, &progress);
        assert!(matches!(action, RecoveryAction::Reject(_)), "主菜单大世界必须拒绝：{action:?}");
    }

    #[test]
    fn recovery_decision_wonderland_menu_escapes() {
        let snapshot = snap(UiStateKind::MainMenu, "main_menu", "main_menu_template");
        let progress = settled(&snapshot);
        assert_eq!(decide(&snapshot, UiStateKind::Primary, &progress), RecoveryAction::Escape);
    }

    #[test]
    fn recovery_decision_panel_requires_two_confirmations() {
        let panel = snap(UiStateKind::Panel, "panel", "panel_hall_card");
        let mut progress = RecoveryProgress {
            fresh_identity: Some(panel.identity()),
            fresh_samples: panel.required_count,
            panel_samples: 1,
            ..RecoveryProgress::default()
        };
        assert_eq!(decide(&panel, UiStateKind::Primary, &progress), RecoveryAction::Wait);
        progress.record(&panel, Instant::now());
        assert_eq!(decide(&panel, UiStateKind::Primary, &progress), RecoveryAction::Escape);
    }

    #[test]
    fn recovery_decision_loading_only_waits() {
        let loading = snap(UiStateKind::Loading, "loading", "loading_dark_screen");
        let progress = settled(&loading);
        assert_eq!(decide(&loading, UiStateKind::Primary, &progress), RecoveryAction::Wait);
    }

    #[test]
    fn recovery_decision_secondary_escape_is_bounded() {
        let secondary = snap(UiStateKind::Secondary, "secondary:chat", "secondary_back_template");
        let mut progress = settled(&secondary);
        progress.esc_presses = RECOVERY_MAX_ESC;
        assert!(matches!(
            decide(&secondary, UiStateKind::Primary, &progress),
            RecoveryAction::Reject(_)
        ));
    }

    #[test]
    fn recovery_decision_oscillation_and_no_progress_trip_breaker() {
        let primary = snap(UiStateKind::Primary, "primary", "primary_friend_template");
        let mut progress = RecoveryProgress {
            kind_tail: vec![
                UiStateKind::Primary,
                UiStateKind::MainMenu,
                UiStateKind::Primary,
                UiStateKind::MainMenu,
            ],
            ..RecoveryProgress::default()
        };
        assert!(matches!(
            decide(&primary, UiStateKind::Primary, &progress),
            RecoveryAction::Reject(_)
        ));

        let mut progress = settled(&primary);
        progress.pending_action_identity = Some(primary.identity());
        for _ in 0..RECOVERY_NO_PROGRESS_LIMIT {
            progress.record(&primary, Instant::now());
        }
        assert!(matches!(
            decide(&primary, UiStateKind::Primary, &progress),
            RecoveryAction::Reject(_)
        ));
    }

    #[test]
    fn recovery_decision_friend_chat_escapes_to_primary_hall() {
        let world = snap(UiStateKind::Secondary, "secondary:chat", "secondary_chat_anchor");
        let progress = settled(&world);
        assert_eq!(
            decide_recovery_step(
                &world,
                &ResidencyTarget::Primary,
                &progress,
                SettleOptions::default()
            ),
            RecoveryAction::Escape,
            "好友私聊 Esc 必须落一级大厅"
        );
    }

    #[test]
    fn recovery_decision_secondary_interfaces_escape_to_primary_hall() {
        for label in [
            "secondary:qianxing_panel",
            "secondary:multimedia_panel",
            "secondary:qianxing_map",
            "secondary:hall_map",
        ] {
            let snapshot = snap(UiStateKind::Secondary, label, "secondary_interface_template");
            let progress = settled(&snapshot);
            assert_eq!(
                decide_recovery_step(
                    &snapshot,
                    &ResidencyTarget::Primary,
                    &progress,
                    SettleOptions::default()
                ),
                RecoveryAction::Escape,
                "{label} 的 Esc 必须落一级大厅"
            );
        }
    }

    #[test]
    fn recovery_decision_overlay_escapes_exactly_one_level() {
        // 覆盖层叠在什么上回什么：这一层只给一次 Esc，下一帧按真实落点继续判断。
        for label in [
            "panel:friend_list",
            "panel:add_friend",
            "panel:more_settings",
            "panel:confirm_block_player",
            "panel:archive_card",
            "panel:avatar_popup",
        ] {
            let overlay = snap(UiStateKind::Panel, label, label);
            let mut progress = settled(&overlay);
            progress.panel_samples = RECOVERY_PANEL_CONFIRM_SAMPLES;
            assert_eq!(
                decide_recovery_step(
                    &overlay,
                    &ResidencyTarget::Primary,
                    &progress,
                    SettleOptions::default()
                ),
                RecoveryAction::Escape,
                "{label} 必须只退一级"
            );
            // 目标仍是覆盖层：不可重建，直接拒绝，不按键。
            assert!(matches!(
                decide_recovery_step(
                    &overlay,
                    &ResidencyTarget::Secondary(ConversationRef::Overlay(
                        super::super::state_observation::OverlaySurface::FriendPanel
                    )),
                    &progress,
                    SettleOptions::default()
                ),
                RecoveryAction::Reject(_)
            ));
        }
    }

    #[test]
    fn recovery_decision_paimon_menu_does_not_escape_to_overworld_blindly() {
        let paimon = snap(
            UiStateKind::MainMenu,
            "paimon_menu",
            "paimon_menu_grid",
        );
        let progress = settled(&paimon);
        assert!(
            matches!(
                decide_recovery_step(
                    &paimon,
                    &ResidencyTarget::Primary,
                    &progress,
                    SettleOptions::default()
                ),
                RecoveryAction::Reject(_)
            ),
            "主菜单大世界 Esc 会落提瓦特大世界，不允许盲按"
        );
    }

    #[test]
    fn hall_fallback_only_for_friend_conversations_and_when_allowed() {
        let friend = ConversationRef::friend("甲", vec!["甲".to_string()]);
        assert!(matches!(
            hall_fallback(&friend, SettleOptions::default()),
            Some(ConversationRef::CurrentHall)
        ));
        assert!(hall_fallback(&friend, SettleOptions::strict()).is_none());
        assert!(hall_fallback(&ConversationRef::CurrentHall, SettleOptions::default()).is_none());
    }

    #[test]
    fn ui_target_maps_to_clean_residency_target() {
        let target: ResidencyTarget =
            crate::ui::routines::friend_delivery::UiResidencyTarget::Primary.into();
        assert!(matches!(target, ResidencyTarget::Primary));
        let target: ResidencyTarget =
            crate::ui::routines::friend_delivery::UiResidencyTarget::SecondaryCurrentHall.into();
        assert!(matches!(
            target,
            ResidencyTarget::Secondary(ConversationRef::CurrentHall)
        ));
    }

    // ==== 真实二级聊天帧：会话确认扫描区（左侧侧栏整条）====

    const SCROLLED_FRIEND_FRAME: &str =
        "tests/fixtures/ui/secondary-chat-scrolled-1920x1080.jpg";
    const HALL_ROW_FRAME: &str = "tests/fixtures/ui/secondary-with-hall-row-1920x1080.png";

    fn fixture_config() -> AppConfig {
        AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap()
    }

    fn fixture_routine() -> FriendDeliveryRoutineConfig {
        FriendDeliveryRoutineConfig::from_app(&fixture_config())
    }

    fn real_frame(path: &str) -> DynamicImage {
        let image = image::open(path).unwrap_or_else(|error| panic!("打不开真实帧 {path}: {error}"));
        assert_eq!((image.width(), image.height()), (1920, 1080), "{path} 尺寸必须与配置画布一致");
        image
    }

    fn darken_sidebar(frame: &mut DynamicImage, region: Rect) {
        for y in region.y.max(0)..region.bottom().min(frame.height() as i32) {
            for x in region.x.max(0)..region.right().min(frame.width() as i32) {
                frame.put_pixel(x as u32, y as u32, image::Rgba([24, 28, 36, 255]));
            }
        }
    }

    fn fill_rect(frame: &mut DynamicImage, rect: Rect) {
        for y in rect.y..rect.bottom().min(frame.height() as i32) {
            for x in rect.x..rect.right().min(frame.width() as i32) {
                frame.put_pixel(x as u32, y as u32, image::Rgba([210, 210, 210, 255]));
            }
        }
    }

    /// 模拟"唯一高亮行"里被 OCR 出的名字；只回答高亮行 crop，其余 crop 一律空。
    struct SelectedRowOcrDevice {
        row: Rect,
        text: &'static str,
        scanned: Arc<std::sync::Mutex<Vec<(u32, u32)>>>,
    }

    impl OcrDevice for SelectedRowOcrDevice {
        fn recognize_lines(&mut self, image: &DynamicImage) -> Result<Vec<OcrLine>> {
            self.scanned
                .lock()
                .unwrap()
                .push((image.width(), image.height()));
            if (image.width(), image.height()) != (self.row.width, self.row.height) {
                return Ok(Vec::new());
            }
            Ok(vec![OcrLine {
                text: self.text.to_string(),
                confidence: 1.0,
                bbox: Rect::new(
                    self.row.width as i32 / 2 - 20,
                    self.row.height as i32 / 2 - 8,
                    40,
                    16,
                ),
            }])
        }
    }

    #[test]
    fn conversation_sidebar_region_covers_full_left_sidebar_without_chat_or_input() {
        let config = fixture_config();
        let routine = FriendDeliveryRoutineConfig::from_app(&config);
        let friends: Rect = config.invite.friend_list_region.into();
        let back: Rect = config.screen.secondary_back_rect.into();
        let region = conversation_row_region(&routine);
        assert_eq!(
            (
                region.x,
                region.y,
                region.width,
                region.height
            ),
            (10, 80, 280, 800),
            "会话确认扫描区必须覆盖左侧侧栏整条（返回按钮下方）"
        );
        assert_eq!(
            (region.x, region.y, region.width, region.height),
            (
                friends.x - 70,
                back.bottom(),
                friends.width + 110,
                (friends.bottom() - back.bottom()) as u32
            ),
            "几何必须与 passive 侧栏扫描区同式"
        );
        assert!(
            region.y < friends.y,
            "必须覆盖列表配置区上方的选中行：{region:?}"
        );
        assert!(
            region.right() <= friends.right() + 40,
            "不能扩进聊天正文：{region:?}"
        );
        assert!(
            region.bottom() <= friends.bottom(),
            "不能扩到输入行：{region:?}"
        );
    }

    #[test]
    fn real_friend_frame_selected_row_above_list_top_is_unique_and_identified_as_friend() {
        let routine = fixture_routine();
        let image = real_frame(SCROLLED_FRIEND_FRAME);
        let friends = routine.friend_list_region();
        let region = conversation_row_region(&routine);
        let row = crate::ui::routines::highlighted_conversation_row(&image, region)
            .expect("真实好友帧的选中行（y≈226）必须被侧栏扫描区唯一命中");
        // 旧扫描窗从 friend_list_region.y=280 起，与选中行只重叠 17px < 24px 最小行高；
        // 这正是线上"好友帧退回 None、在 OCR 前失败"的根因。
        assert!(
            crate::ui::routines::highlighted_conversation_row(&image, friends).is_none(),
            "回归对照：旧列表区扫描窗必须漏掉该选中行"
        );
        assert!(
            row.y < friends.y,
            "选中行应在列表配置区上方：row={row:?} friends={friends:?}"
        );
        assert!(
            row.bottom() > friends.y,
            "选中行应与列表区部分重叠：row={row:?}"
        );

        let scanned = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ocr = OcrRuntime::start(
            SelectedRowOcrDevice {
                row,
                text: "甲",
                scanned: Arc::clone(&scanned),
            },
            1,
        )
        .unwrap();
        let env = SettleEnvironment::new(ocr.handle(), routine);
        let verdict = identify_conversation_from_frame(&image, &env)
            .expect("高亮行 + 行内 OCR 必须识别出好友会话");
        match verdict {
            ConversationRef::Friend {
                identity,
                candidates,
                row_anchor,
            } => {
                assert_eq!(identity, "甲");
                assert_eq!(candidates, vec!["甲".to_string()]);
                let anchor = row_anchor.expect("必须带行锚点");
                assert!(point_in_rect(crate::geometry::Point::new(anchor.0, anchor.1), row));
            }
            other => panic!("真实好友帧必须判 Friend，实际 {other:?}"),
        }
        let scanned = scanned.lock().unwrap().clone();
        assert!(
            scanned.contains(&(row.width, row.height)),
            "必须补扫唯一高亮行本身（实际扫描 crop: {scanned:?}）"
        );
        ocr.shutdown().unwrap();
    }

    #[test]
    fn real_current_hall_frame_still_identified_as_current_hall() {
        let routine = fixture_routine();
        let image = real_frame(HALL_ROW_FRAME);
        let region = conversation_row_region(&routine);
        let row = crate::ui::routines::highlighted_conversation_row(&image, region)
            .expect("真实大厅帧必须返回唯一高亮行");
        let hall = crate::ui::template::best_template_hit(
            &image,
            Some(routine.secondary_hall_search_region()),
            routine.secondary_hall_template(),
            routine.template_threshold(),
        )
        .unwrap()
        .expect("真实大厅帧必须命中大厅模板");
        assert!(
            (row.y..row.bottom()).contains(&hall.center().y),
            "大厅模板中心必须落在高亮行内：row={row:?} center={:?}",
            hall.center()
        );
        assert!(row.y < routine.friend_list_region().y);

        // 无 OCR 也必须在会话确认阶段直接判当前大厅，不能掉到好友分支。
        let env = SettleEnvironment::without_ocr(routine);
        assert_eq!(
            identify_conversation_from_frame(&image, &env),
            Some(ConversationRef::CurrentHall)
        );
    }

    #[test]
    fn highlight_scan_requires_exactly_one_sidebar_row_and_ignores_chat_area() {
        let routine = fixture_routine();
        let region = conversation_row_region(&routine);
        let unique = crate::ui::routines::highlighted_conversation_row;

        // 没有高亮：清空整条侧栏。
        let mut none = real_frame(SCROLLED_FRIEND_FRAME);
        darken_sidebar(&mut none, region);
        assert!(unique(&none, region).is_none(), "没有高亮行时不能猜");

        // 多高亮：真实大厅帧已有的高亮行 + 再画一条侧栏高亮。
        let mut multiple = real_frame(HALL_ROW_FRAME);
        fill_rect(&mut multiple, Rect::new(region.x.max(0), 600, region.width, 70));
        assert!(
            unique(&multiple, region).is_none(),
            "出现两个高亮行时必须判 None（不猜）"
        );

        // 聊天正文里的亮带不是会话行。
        let mut chat = real_frame(SCROLLED_FRIEND_FRAME);
        darken_sidebar(&mut chat, region);
        fill_rect(&mut chat, Rect::new(600, 320, 300, 70));
        assert!(
            unique(&chat, region).is_none(),
            "聊天气泡/正文高亮不能被当成会话行"
        );
    }
}

#[cfg(test)]
mod conversation_substate_evidence {
    //! 只读诊断：把 `identify_conversation` 的入口证据（高亮行 / 大厅模板 /
    //! 会话判定）按真实帧导出成 JSONL，供证据画廊渲染。
    //!
    //! 输入 TSV：`surface_key<TAB>frame`。输出每帧一行 JSON；
    //! 本机没有 OCR 设备，`ocr_available` 恒为 false，判定只走离线分支。

    use std::path::Path;

    use super::*;
    use crate::config::AppConfig;
    use crate::ui::frame::{Canvas, resize_to_canvas};
    use crate::geometry::Rect;
    use crate::ui::routines::friend_delivery::{
        conversation_row_region, highlighted_conversation_row,
    };

    fn rect_json(rect: Rect) -> serde_json::Value {
        serde_json::json!({"x": rect.x, "y": rect.y, "w": rect.width, "h": rect.height})
    }

    #[test]
    #[ignore = "会话子状态证据导出：需要 MILIASTRA_CONVERSATION_FRAMES / MILIASTRA_CONVERSATION_OUT"]
    fn dump_conversation_substate_evidence() {
        let Ok(list_path) = std::env::var("MILIASTRA_CONVERSATION_FRAMES") else {
            return;
        };
        let out_path = std::env::var("MILIASTRA_CONVERSATION_OUT")
            .expect("MILIASTRA_CONVERSATION_OUT 必须给出");
        let config = AppConfig::load(Path::new("tests/fixtures/config.full.yaml")).unwrap();
        let routine = FriendDeliveryRoutineConfig::from_app(&config);
        let canvas = Canvas {
            width: config.screen.expected_width,
            height: config.screen.expected_height,
            resize: true,
        };
        let row_region = conversation_row_region(&routine);
        // 诊断对照：与 passive::passive_sidebar_region 同式（返回按钮下方整条侧栏）。
        let friends: Rect = config.invite.friend_list_region.into();
        let back: Rect = config.screen.secondary_back_rect.into();
        let sidebar_region = Rect::new(
            friends.x.saturating_sub(70),
            back.bottom(),
            friends.width.saturating_add(110),
            (friends.bottom() - back.bottom()).max(0) as u32,
        );
        let hall_search = routine.secondary_hall_search_region();
        let hall_template = routine.secondary_hall_template();
        let threshold = routine.template_threshold();
        let list = std::fs::read_to_string(&list_path).unwrap();
        let mut output = String::new();
        let mut count = 0_usize;
        for (line_number, line) in list.lines().enumerate() {
            if line_number == 0 || line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let columns: Vec<&str> = line.split('\t').collect();
            assert_eq!(columns.len(), 2, "会话证据清单第 {} 行列数不是 2", line_number + 1);
            let (surface_key, frame) = (columns[0], columns[1]);
            let source = image::open(frame).unwrap_or_else(|error| panic!("打不开 {frame}: {error}"));
            let normalized = resize_to_canvas(&source, &canvas, None);
            let image = normalized.as_ref().unwrap_or(&source);

            let hall_candidate = best_template_candidate(image, Some(hall_search), hall_template)
                .unwrap();
            let hall_hit = hall_candidate
                .clone()
                .filter(|candidate| candidate.quality_ok && candidate.score >= threshold);
            let highlighted = highlighted_conversation_row(image, row_region);
            let sidebar_highlight = highlighted_conversation_row(image, sidebar_region);
            let env = SettleEnvironment::without_ocr(routine.clone());
            let verdict = identify_conversation_from_frame(image, &env);
            let verdict_name = match &verdict {
                Some(ConversationRef::CurrentHall) => "CurrentHall",
                Some(ConversationRef::Friend { .. }) => "Friend",
                Some(other) => {
                    let _ = other;
                    "Other"
                }
                None => "Unknown",
            };
            let hall_center = hall_candidate.as_ref().map(|hit| hit.center());
            let matched_center = hall_hit.as_ref().map(|hit| hit.center());
            let record = serde_json::json!({
                "surface_key": surface_key,
                "frame": frame,
                "image": {"w": image.width(), "h": image.height()},
                "kind": "secondary",
                "label": "secondary:chat",
                "left_column": rect_json(row_region),
                "sidebar_region": rect_json(sidebar_region),
                "highlight": highlighted.map(rect_json),
                "sidebar_highlight": sidebar_highlight.map(rect_json),
                "hall": {
                    "template": hall_template.display().to_string(),
                    "threshold": threshold,
                    "search_rect": rect_json(hall_search),
                    "score": hall_candidate.as_ref().map(|hit| hit.score),
                    "hit_rect": hall_candidate.as_ref().map(|hit| rect_json(hit.rect())),
                    "center": hall_center.map(|point| serde_json::json!({"x": point.x, "y": point.y})),
                    "matched": hall_hit.is_some(),
                },
                "hall_center_in_highlight": matched_center.zip(highlighted)
                    .is_some_and(|(point, row)| (row.y..row.bottom()).contains(&point.y)),
                "hall_center_in_sidebar_highlight": matched_center.zip(sidebar_highlight)
                    .is_some_and(|(point, row)| (row.y..row.bottom()).contains(&point.y)),
                "ocr_available": env.ocr().is_some(),
                "verdict": verdict_name,
                "verdict_debug": format!("{verdict:?}"),
            });
            output.push_str(&serde_json::to_string(&record).unwrap());
            output.push('\n');
            count += 1;
        }
        std::fs::write(&out_path, output).unwrap();
        eprintln!("会话子状态证据：已导出 {count} 帧 -> {out_path}");
    }
}
