//! 界面分类结果类型：`UiState` 与内部 `UiStateKind`。

use crate::geometry::Rect;
use crate::runtime::ui::UiStateKind as RuntimeUiStateKind;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum UiStateKind {
    Primary,
    Secondary,
    /// 奇域主菜单：独立状态，不能并进 Unknown，否则会触发游戏态回退（§44.10）。
    MainMenu,
    /// 加载界面：真实界面（两种外观），不能并进 Unknown（§55）。
    Loading,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UiState {
    pub(super) state: UiStateKind,
    pub(super) blue_count: usize,
    pub(super) yellow_count: usize,
    pub(super) pink_count: usize,
    pub(super) secondary_visible: bool,
    pub(super) friend_visible: bool,
    pub(super) source: &'static str,
}

impl UiState {
    pub(super) fn primary_friend() -> Self {
        Self {
            state: UiStateKind::Primary,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: true,
            source: "friend",
        }
    }

    pub(super) fn primary_marker(
        blue_count: usize,
        yellow_count: usize,
        pink_count: usize,
    ) -> Self {
        Self {
            state: UiStateKind::Primary,
            blue_count,
            yellow_count,
            pink_count,
            secondary_visible: false,
            friend_visible: false,
            source: "marker",
        }
    }

    /// 千星大厅：靠**底栏结构**判出来且当场没有聊天标记。
    ///
    /// 大厅是最常见的状态，气泡只是它的可选内容：聊天框关着、或气泡模板没命中时，
    /// 旧逻辑会落到 unknown。这个构造器只服务那条新路径，标签用 `primary:hall`
    /// 与「靠气泡判出来」的 `primary:marker …` 区分开。
    pub(super) fn primary_hall() -> Self {
        Self {
            state: UiStateKind::Primary,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "hall",
        }
    }

    pub(super) fn secondary_chat() -> Self {
        Self {
            state: UiStateKind::Secondary,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: true,
            friend_visible: false,
            source: "back",
        }
    }

    /// 千星奇域主菜单：独立状态。
    pub(super) fn main_menu() -> Self {
        Self {
            state: UiStateKind::MainMenu,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "main_menu",
        }
    }

    /// 主菜单大世界（ESC 系统菜单）。它属于主菜单一族（`MainMenu`），
    /// 但用**专属锚点**判定，因此证据里的规则名与奇域主菜单不同。
    pub(super) fn paimon_menu() -> Self {
        Self {
            state: UiStateKind::MainMenu,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "paimon_menu",
        }
    }

    /// 加载界面。两种外观（黑底徽记 / 白底元素）都归到这一个状态，
    /// 具体是哪一种由证据里的 finalRule（loading_dark_screen / loading_white_screen）区分。
    pub(super) fn loading() -> Self {
        Self {
            state: UiStateKind::Loading,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "loading",
        }
    }

    pub(super) fn unknown() -> Self {
        Self {
            state: UiStateKind::Unknown,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "none",
        }
    }

    pub(super) fn runtime_kind(&self) -> RuntimeUiStateKind {
        match self.state {
            UiStateKind::Primary => RuntimeUiStateKind::Primary,
            UiStateKind::Secondary => RuntimeUiStateKind::Secondary,
            UiStateKind::MainMenu => RuntimeUiStateKind::MainMenu,
            UiStateKind::Loading => RuntimeUiStateKind::Loading,
            UiStateKind::Unknown => RuntimeUiStateKind::Unknown,
        }
    }
}

impl std::fmt::Display for UiState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.state {
            UiStateKind::Primary if self.source == "friend" => {
                write!(formatter, "primary:friend")
            }
            UiStateKind::Primary if self.source == "hall" => {
                write!(formatter, "primary:hall")
            }
            UiStateKind::Primary => write!(
                formatter,
                "primary:marker blue={} yellow={} pink={}",
                self.blue_count, self.yellow_count, self.pink_count
            ),
            UiStateKind::Secondary => write!(formatter, "secondary:chat"),
            UiStateKind::MainMenu => write!(formatter, "main_menu"),
            UiStateKind::Loading => write!(formatter, "loading"),
            UiStateKind::Unknown => write!(formatter, "unknown"),
        }
    }
}

/// 面板判据用到的区域（来自 `moderation` / `invite` 配置）。
///
/// 由 [`TemplateUiStateClassifier::with_panel_regions`] 注入；**未注入时面板判据不跑**，
/// 行为与从前完全一致（因此现有 `new()` 调用点与测试零改动）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct PanelRegions {
    pub(crate) friend_panel: Rect,
    pub(crate) friend_search_panel: Rect,
    pub(crate) friend_block_chat: Rect,
    pub(crate) confirm_button: Rect,
    /// 确认弹窗提示文案行（三种关键词模板共用的宽行搜索区）。
    pub(crate) confirm_message: Rect,
    pub(crate) invite_view_star: Rect,
    pub(crate) invite_goto_hall: Rect,
    pub(crate) invite_enter_hall: Rect,
}
