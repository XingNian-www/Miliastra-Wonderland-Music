//! 面板判据区域：分类器的可选输入（`moderation` / `invite` 配置里的矩形）。

use crate::geometry::Rect;

/// 面板判据用到的区域（来自 `moderation` / `invite` 配置）。
///
/// 由 `TemplateUiStateClassifier::with_panel_regions` 注入；**未注入时面板判据不跑**，
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
