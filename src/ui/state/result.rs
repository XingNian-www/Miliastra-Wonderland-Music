//! 界面分类结果 [`UiState`]：检测链**直接携带**运行时权威类型 [`UiStateKind`]。

use crate::runtime::ui::UiStateKind;
use serde::Serialize;

/// 逐帧检测结果：**界面类型 + 判别来源**；证据由调用方另行携带。
///
/// `kind` 就是运行时权威类型 [`UiStateKind`]（`crate::runtime::ui`）。本模块不再
/// 维护第二份子集枚举，也不再需要把内部枚举逐个映射成运行时类型的中间方法——
/// 检测链与 `UiStateClassification` 从一开始就共用同一份定义。
///
/// 检测链只产出 `Primary` / `Secondary` / `MainMenu` / `Loading` / `Unknown`
/// 五种；`GameGate` / `Overworld` 由未知态回退探针产出，`Panel` 由面板判据产出，
/// 这五种之外的构造器在这里**刻意不存在**（见 [`UiState`] 的各构造器）。
#[derive(Clone, Debug, Serialize)]
pub(crate) struct UiState {
    /// 历史序列化键名是 `state`。字段按内容改名为 `kind` 之后键名保持不变，
    /// 以免按旧键读取这份序列化结果的离线工具静默失配。
    #[serde(rename = "state")]
    pub(super) kind: UiStateKind,
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
            kind: UiStateKind::Primary,
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
            kind: UiStateKind::Primary,
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
            kind: UiStateKind::Primary,
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
            kind: UiStateKind::Secondary,
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
            kind: UiStateKind::MainMenu,
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
            kind: UiStateKind::MainMenu,
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
            kind: UiStateKind::Loading,
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
            kind: UiStateKind::Unknown,
            blue_count: 0,
            yellow_count: 0,
            pink_count: 0,
            secondary_visible: false,
            friend_visible: false,
            source: "none",
        }
    }
}

impl std::fmt::Display for UiState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
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
            // 检测链不产出这三种（见类型文档）：真被构造出来时退回规范名，
            // 绝不臆造一个看似更具体的标签。列全而不写 `_`，是为了让将来
            // `UiStateKind` 新增变体时编译器在这里拦下我。
            UiStateKind::GameGate | UiStateKind::Overworld | UiStateKind::Panel => {
                formatter.write_str(self.kind.name())
            }
        }
    }
}
