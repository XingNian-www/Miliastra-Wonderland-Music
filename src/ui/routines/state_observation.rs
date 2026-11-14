use std::time::{Duration, Instant};

use image::DynamicImage;

use crate::runtime::ui::{
    InputCertainty, UiRoutineContext, UiRoutineFailure, UiStateKind, UiStateObservation,
};
use crate::ui::frame::{Canvas, resize_to_canvas};

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

pub(super) fn wait_ms(
    context: &mut UiRoutineContext<'_>,
    ms: u64,
    stage: &'static str,
    certainty: InputCertainty,
) -> Result<(), UiRoutineFailure> {
    context
        .wait_cancellable(Duration::from_millis(ms))
        .map_err(|failure| UiRoutineFailure::new(certainty, stage, failure.reason()))
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
    let canvas = Canvas {
        width: config.canvas_width,
        height: config.canvas_height,
        resize: true,
    };
    let normalized = resize_to_canvas(&image, &canvas, None);
    Ok(normalized.unwrap_or(image))
}

pub(super) fn wait_for_stable_ui_kind(
    context: &mut UiRoutineContext<'_>,
    config: UiStateObservationConfig,
    expected: Option<UiStateKind>,
    timeout_ms: u64,
    stage: &'static str,
    certainty: InputCertainty,
) -> Result<UiStateKind, UiRoutineFailure> {
    wait_for_stable_ui_kind_internal(
        context, config, expected, timeout_ms, stage, certainty, false,
    )
}

pub(super) fn wait_for_stable_ui_kind_or_unknown(
    context: &mut UiRoutineContext<'_>,
    config: UiStateObservationConfig,
    expected: Option<UiStateKind>,
    timeout_ms: u64,
    stage: &'static str,
    certainty: InputCertainty,
) -> Result<UiStateKind, UiRoutineFailure> {
    wait_for_stable_ui_kind_internal(
        context, config, expected, timeout_ms, stage, certainty, true,
    )
}

fn wait_for_stable_ui_kind_internal(
    context: &mut UiRoutineContext<'_>,
    config: UiStateObservationConfig,
    expected: Option<UiStateKind>,
    timeout_ms: u64,
    stage: &'static str,
    certainty: InputCertainty,
    allow_stable_unknown: bool,
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
                let expected_mismatch = expected.is_some_and(|expected| expected != kind);
                if expected_mismatch || (kind == UiStateKind::Unknown && !allow_stable_unknown) {
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
                    let runtime_stable = if kind == UiStateKind::Unknown {
                        allow_stable_unknown
                    } else {
                        state.stable_kind() == Some(kind)
                    };
                    if runtime_stable && fresh_samples >= state.required_count() {
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
        wait_ms(context, config.poll_ms, stage, certainty)?;
    }
}

/// 一次分类后的只读快照：恢复决策与失败诊断所需的最小事实集。
///
/// 只读取 `UiStateClassification` 已公开的字段，不新增运行时接口，
/// 因此可用于 friend_delivery 的状态驱动恢复，而不改 `runtime/ui.rs`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UiStateSnapshot {
    pub(crate) kind: UiStateKind,
    /// 分类标签（如 `main_menu`、`secondary:chat`、`primary:hall`）。
    pub(crate) label: String,
    /// 判据最终规则名（区分主菜单千星 `main_menu_template` 与
    /// 主菜单大世界 `paimon_menu_grid` 的唯一依据）。
    pub(crate) final_rule: String,
    /// 世界判据结论（`teyvat` / `qianxing` / `unknown` / `not_applicable`…）。
    pub(crate) world_guess: String,
    /// 运行时跟踪器给出的"最后一次稳定类型"，用于诊断瞬失。
    pub(crate) last_stable_kind: Option<UiStateKind>,
    /// 本帧是否已被跟踪器判为稳定（连续 required_count 帧一致）。
    pub(crate) runtime_stable: bool,
    pub(crate) candidate_count: u32,
    pub(crate) required_count: u32,
}

impl UiStateSnapshot {
    /// 状态身份：动作前后比较"是否有进展"用（kind + label，不含证据）。
    pub(super) fn identity(&self) -> String {
        format!("{}|{}", self.kind.name(), self.label)
    }

    /// 失败诊断用的一行事实串（含 rule / world / last_stable）。
    pub(super) fn describe(&self) -> String {
        format!(
            "kind={} label={} rule={} world={} last_stable={:?} stable={} samples={}/{}",
            self.kind.name(),
            self.label,
            self.final_rule,
            self.world_guess,
            self.last_stable_kind,
            self.runtime_stable,
            self.candidate_count,
            self.required_count
        )
    }

    /// 是否为千星奇域主菜单（由「返回提瓦特」模板判出）。
    pub(super) fn is_wonderland_main_menu(&self) -> bool {
        self.kind == UiStateKind::MainMenu && self.final_rule == "main_menu_template"
    }

    /// 是否为主菜单大世界（ESC 系统菜单）：它复用主菜单状态，但绝不是大厅菜单。
    pub(super) fn is_paimon_menu(&self) -> bool {
        self.kind == UiStateKind::MainMenu && self.final_rule == "paimon_menu_grid"
    }
}

/// 二级界面的子型（对用户展示用完整多词名称，不再把 Secondary 当术语）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SecondarySurface {
    /// 二级聊天（当前大厅/好友私聊由会话证据再细分）。
    Chat,
    /// 二级界面·多人面板（大厅内 F2）。
    MultiplayerPanel,
    /// 二级界面·千星地图。
    HallMap,
}

impl SecondarySurface {
    pub(crate) fn chinese_name(self) -> &'static str {
        match self {
            Self::Chat => "二级聊天",
            Self::MultiplayerPanel => "二级界面·多人面板",
            Self::HallMap => "二级界面·千星地图",
        }
    }
}

/// 覆盖层子型：一次 Esc 退一层；同级覆盖层不是父子关系。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlaySurface {
    FriendPanel,
    SearchPage,
    MoreSettings,
    BlockList,
    /// 确认弹窗·进入随机大厅（确认按钮 + 「随机公共大厅」文案双锚点）。
    ConfirmEnterHall,
    /// 确认弹窗·离开大厅（确认按钮 + 「返回提瓦特」文案双锚点）。
    ConfirmLeaveHall,
    /// 确认弹窗·屏蔽玩家（确认按钮 + 「屏蔽吗」文案双锚点）。
    ConfirmBlockPlayer,
    /// 旧档案卡（`panel:archive_card`）：仅作兼容兜底，新分类优先出下面四类。
    ProfileCard,
    /// 提瓦特资料卡·非好友（冒险等阶+世界等级，且命中「添加好友」）。
    TeyvatProfileCardNonFriend,
    /// 提瓦特资料卡·好友（冒险等阶+世界等级，无「添加好友」）。
    TeyvatProfileCardFriend,
    /// 千星资料卡·非好友（奇域等级，且命中「添加好友」）。
    QianxingProfileCardNonFriend,
    /// 千星资料卡·好友（奇域等级，无「添加好友」但命中「查看资料」）。
    QianxingProfileCardFriend,
    /// 个人资料页（右下角「心愿单」+「衣橱」双按钮判据）。
    ProfilePage,
    HallCard,
    AvatarPopup,
    Other,
}

impl OverlaySurface {
    pub(crate) fn chinese_name(self) -> &'static str {
        match self {
            Self::FriendPanel => "覆盖层·好友面板",
            Self::SearchPage => "覆盖层·搜索页",
            Self::MoreSettings => "覆盖层·更多设置",
            Self::BlockList => "覆盖层·拉黑",
            Self::ConfirmEnterHall => "覆盖层·确认弹窗·进入随机大厅",
            Self::ConfirmLeaveHall => "覆盖层·确认弹窗·离开大厅",
            Self::ConfirmBlockPlayer => "覆盖层·确认弹窗·屏蔽玩家",
            Self::ProfileCard => "覆盖层·档案卡",
            Self::TeyvatProfileCardNonFriend => "覆盖层·提瓦特资料卡·非好友",
            Self::TeyvatProfileCardFriend => "覆盖层·提瓦特资料卡·好友",
            Self::QianxingProfileCardNonFriend => "覆盖层·千星资料卡·非好友",
            Self::QianxingProfileCardFriend => "覆盖层·千星资料卡·好友",
            Self::ProfilePage => "覆盖层·个人资料页",
            Self::HallCard => "覆盖层·大厅卡",
            Self::AvatarPopup => "覆盖层·头像弹窗",
            Self::Other => "覆盖层·其他",
        }
    }
}

/// 统一界面表面：分类判据（kind/label/final_rule）到用户语义的唯一映射。
///
/// `UiStateKind` 只保留粗粒度分类；对目标匹配、Esc 动作与日志展示都必须用这里的
/// 完整名称，避免把 Secondary/Panel 单独当术语。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UiSurface {
    /// 千星大厅（一级界面）。
    PrimaryHall,
    /// 二级界面（聊天/多人面板/千星地图）。
    Secondary(SecondarySurface),
    /// 覆盖层（好友面板/搜索页/…），叠在 Primary 或 Secondary 之上。
    Overlay(OverlaySurface),
    /// 主菜单千星（用「返回提瓦特」区分）。
    WonderlandMainMenu,
    /// 主菜单大世界（ESC 系统菜单）。
    PaimonMenu,
    /// 加载界面：Esc 无效。
    Loading,
    /// 提瓦特大世界。
    Overworld,
    /// 游戏登录入口。
    GameGate,
    /// 未识别界面。
    Unknown,
}

impl UiSurface {
    pub(crate) fn chinese_name(&self) -> &'static str {
        match self {
            Self::PrimaryHall => "千星大厅（一级大厅）",
            Self::Secondary(surface) => surface.chinese_name(),
            Self::Overlay(overlay) => overlay.chinese_name(),
            Self::WonderlandMainMenu => "主菜单千星",
            Self::PaimonMenu => "主菜单大世界",
            Self::Loading => "加载界面",
            Self::Overworld => "提瓦特大世界",
            Self::GameGate => "登录入口",
            Self::Unknown => "未识别界面",
        }
    }

    pub(crate) fn is_secondary(&self) -> bool {
        matches!(self, Self::Secondary(_))
    }

    pub(crate) fn is_overlay(&self) -> bool {
        matches!(self, Self::Overlay(_))
    }

    pub(crate) fn is_secondary_chat(&self) -> bool {
        matches!(self, Self::Secondary(SecondarySurface::Chat))
    }

    pub(crate) fn is_secondary_interface(&self) -> bool {
        matches!(
            self,
            Self::Secondary(SecondarySurface::MultiplayerPanel | SecondarySurface::HallMap)
        )
    }
}

impl UiStateSnapshot {
    /// 由 kind/label/final_rule 推导统一表面（不猜：判据不足时返回 Unknown）。
    pub(crate) fn surface(&self) -> UiSurface {
        surface_of(self.kind, &self.label, &self.final_rule)
    }
}

/// 分类事实 -> 统一表面（入口观测与恢复引擎共用同一映射）。
pub(crate) fn surface_of(kind: UiStateKind, label: &str, final_rule: &str) -> UiSurface {
    match kind {
        UiStateKind::Primary => UiSurface::PrimaryHall,
        UiStateKind::Secondary => {
            if label_is_secondary_interface(label, "panel") {
                UiSurface::Secondary(SecondarySurface::MultiplayerPanel)
            } else if label_is_secondary_interface(label, "map") {
                UiSurface::Secondary(SecondarySurface::HallMap)
            } else {
                UiSurface::Secondary(SecondarySurface::Chat)
            }
        }
        UiStateKind::Panel => UiSurface::Overlay(overlay_from_label(label)),
        UiStateKind::MainMenu => {
            if final_rule == "paimon_menu_grid" {
                UiSurface::PaimonMenu
            } else {
                UiSurface::WonderlandMainMenu
            }
        }
        UiStateKind::Loading => UiSurface::Loading,
        UiStateKind::Overworld => UiSurface::Overworld,
        UiStateKind::GameGate => UiSurface::GameGate,
        UiStateKind::Unknown => UiSurface::Unknown,
    }
}

/// 二级界面 label 尾部（`…panel` / `…map`）判断，兼容 `qianxing_panel`/`multimedia_panel`/
/// `hall_map` 等命名差异；只认完整词尾，避免误命中。
fn label_is_secondary_interface(label: &str, suffix: &str) -> bool {
    label
        .rsplit(':')
        .next()
        .is_some_and(|tail| tail == suffix || tail.ends_with(&format!("_{suffix}")))
}

fn overlay_from_label(label: &str) -> OverlaySurface {
    match label {
        "panel:friend_list" => OverlaySurface::FriendPanel,
        "panel:add_friend" | "panel:search" => OverlaySurface::SearchPage,
        "panel:more_settings" => OverlaySurface::MoreSettings,
        "panel:block_list" | "panel:block_chat" => OverlaySurface::BlockList,
        "panel:confirm_enter_hall" => OverlaySurface::ConfirmEnterHall,
        "panel:confirm_leave_hall" => OverlaySurface::ConfirmLeaveHall,
        "panel:confirm_block_player" => OverlaySurface::ConfirmBlockPlayer,
        "panel:archive_card" => OverlaySurface::ProfileCard,
        "panel:teyvat_profile_nonfriend" => OverlaySurface::TeyvatProfileCardNonFriend,
        "panel:teyvat_profile_friend" => OverlaySurface::TeyvatProfileCardFriend,
        "panel:qianxing_profile_nonfriend" => OverlaySurface::QianxingProfileCardNonFriend,
        "panel:qianxing_profile_friend" => OverlaySurface::QianxingProfileCardFriend,
        "panel:profile_page" => OverlaySurface::ProfilePage,
        "panel:hall_card" => OverlaySurface::HallCard,
        "panel:avatar_popup" => OverlaySurface::AvatarPopup,
        _ => OverlaySurface::Other,
    }
}

/// 采一帧并返回分类快照；分类失败也如实返回错误（带 last_stable 诊断）。
///
/// 与 [`wait_for_stable_ui_kind`] 的区别：它只采样一次、把完整证据交回调用方，
/// 由调用方按状态机决策下一步动作（恢复入口 `settle_to` 用）。
pub(super) fn observe_ui_state_once(
    context: &mut UiRoutineContext<'_>,
    config: UiStateObservationConfig,
    stage: &'static str,
    certainty: InputCertainty,
) -> Result<UiStateSnapshot, UiRoutineFailure> {
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
            let classification = state.classification();
            Ok(UiStateSnapshot {
                kind: classification.kind(),
                label: classification.label().to_string(),
                final_rule: classification.evidence().final_rule().to_string(),
                world_guess: classification
                    .evidence()
                    .world_probe()
                    .map(|probe| probe.guess().to_string())
                    .unwrap_or_else(|| "none".to_string()),
                last_stable_kind: state.last_stable_kind(),
                runtime_stable: state.stable_kind() == Some(classification.kind()),
                candidate_count: state.candidate_count(),
                required_count: state.required_count(),
            })
        }
        UiStateObservation::Failed {
            reason,
            last_stable_kind,
            ..
        } => Err(UiRoutineFailure::new(
            certainty,
            stage,
            format!(
                "template UI state classification failed: {reason}; last_stable={last_stable_kind:?}"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(kind: UiStateKind, label: &str, final_rule: &str) -> UiStateSnapshot {
        UiStateSnapshot {
            kind,
            label: label.to_string(),
            final_rule: final_rule.to_string(),
            world_guess: "none".to_string(),
            last_stable_kind: Some(kind),
            runtime_stable: true,
            candidate_count: 2,
            required_count: 2,
        }
    }

    #[test]
    fn secondary_labels_map_to_full_surface_names() {
        assert_eq!(
            snapshot(UiStateKind::Secondary, "secondary:chat", "secondary_chat_anchor")
                .surface()
                .chinese_name(),
            "二级聊天"
        );
        // 兼容 task-102 前后两种界面 label 命名。
        for label in ["secondary:qianxing_panel", "secondary:multimedia_panel"] {
            let surface = snapshot(UiStateKind::Secondary, label, "panel_template").surface();
            assert_eq!(surface.chinese_name(), "二级界面·多人面板", "{label}");
        }
        for label in ["secondary:qianxing_map", "secondary:hall_map"] {
            let surface = snapshot(UiStateKind::Secondary, label, "map_template").surface();
            assert_eq!(surface.chinese_name(), "二级界面·千星地图", "{label}");
        }
    }

    #[test]
    fn overlay_labels_map_to_full_surface_names() {
        let cases = [
            ("panel:friend_list", "覆盖层·好友面板"),
            ("panel:add_friend", "覆盖层·搜索页"),
            ("panel:more_settings", "覆盖层·更多设置"),
            (
                "panel:confirm_enter_hall",
                "覆盖层·确认弹窗·进入随机大厅",
            ),
            (
                "panel:confirm_leave_hall",
                "覆盖层·确认弹窗·离开大厅",
            ),
            (
                "panel:confirm_block_player",
                "覆盖层·确认弹窗·屏蔽玩家",
            ),
            ("panel:archive_card", "覆盖层·档案卡"),
            (
                "panel:teyvat_profile_nonfriend",
                "覆盖层·提瓦特资料卡·非好友",
            ),
            ("panel:teyvat_profile_friend", "覆盖层·提瓦特资料卡·好友"),
            (
                "panel:qianxing_profile_nonfriend",
                "覆盖层·千星资料卡·非好友",
            ),
            ("panel:qianxing_profile_friend", "覆盖层·千星资料卡·好友"),
            ("panel:profile_page", "覆盖层·个人资料页"),
            ("panel:hall_card", "覆盖层·大厅卡"),
            ("panel:avatar_popup", "覆盖层·头像弹窗"),
            ("panel:unknown_new", "覆盖层·其他"),
        ];
        for (label, expected) in cases {
            let surface = snapshot(UiStateKind::Panel, label, label).surface();
            assert_eq!(surface.chinese_name(), expected, "{label}");
            assert!(surface.is_overlay(), "{label}");
        }
    }

    #[test]
    fn main_menu_branches_are_distinguished_by_return_to_teyvat_rule() {
        let wonderland = snapshot(UiStateKind::MainMenu, "main_menu", "main_menu_template");
        assert_eq!(wonderland.surface(), UiSurface::WonderlandMainMenu);
        let paimon = snapshot(
            UiStateKind::MainMenu,
            "main_menu",
            "paimon_menu_grid",
        );
        assert_eq!(paimon.surface(), UiSurface::PaimonMenu);
        // 主菜单千星与主菜单大世界共用返回按钮；判据永远不能靠商城按钮之外的模糊规则冒充。
        assert_ne!(
            wonderland.surface().chinese_name(),
            paimon.surface().chinese_name()
        );
    }

    #[test]
    fn loading_and_world_surfaces_use_full_names() {
        assert_eq!(
            snapshot(UiStateKind::Loading, "loading", "loading_dark_screen")
                .surface()
                .chinese_name(),
            "加载界面"
        );
        assert_eq!(
            snapshot(UiStateKind::Overworld, "overworld", "overworld_world_criterion")
                .surface()
                .chinese_name(),
            "提瓦特大世界"
        );
        assert_eq!(
            snapshot(UiStateKind::GameGate, "game_gate", "game_gate_template")
                .surface()
                .chinese_name(),
            "登录入口"
        );
        assert_eq!(
            snapshot(UiStateKind::Unknown, "unknown", "no_reliable_anchor")
                .surface()
                .chinese_name(),
            "未识别界面"
        );
    }
}
