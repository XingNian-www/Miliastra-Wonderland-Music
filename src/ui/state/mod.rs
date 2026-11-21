//! UI 界面状态识别：拿一帧画面回答「这是哪个界面」。
//!
//! 判据是**分支顺序 + 互斥证据**（模板锚点、整屏标量统计、底栏几何结构），
//! 不是一串可以任意排序的独立判断；改顺序会静默改变识别结果。
//!
//! 对外只暴露 `PanelRegions` / `UiTemplateArgs` / `ResolvedUiTemplateArgs` /
//! `TemplateUiStateClassifier` 与少量分类辅助函数（路径与拆分前完全一致），
//! 内部按职责拆到各子模块。

mod anchors;
mod args;
mod classifier;
mod classify;
mod diagnostic;
mod fallback;
mod hall;
mod paimon;
mod panels;
mod profiles;
mod qianxing;
mod regions;
mod result;
mod sampling;
mod world;

#[cfg(test)]
mod tests;

// ── 公开类型与入口：保持 `crate::ui::state::*` 的既有路径 ──────────────────
//
// 只重导出**跨模块真实使用**的名字：模块边界上每多挂一个名字，就等于多开一个
// 入口，编译器也再难替你发现死代码。本模块自己的测试走 `super::<子模块>::*`
// 直达，不需要经过这里。
pub(crate) use args::{ResolvedUiTemplateArgs, UiTemplateArgs};
pub(crate) use classifier::TemplateUiStateClassifier;
pub(crate) use diagnostic::{CONFIRM_MESSAGE_MIN, CONFIRM_MESSAGE_SCALE};
pub(crate) use fallback::{find_enter_game_text, probe_game_state_without_entry};
pub(crate) use regions::PanelRegions;
pub(crate) use world::world_is_teyvat;

// 仅测试用，但**跨模块**：`ui::routines` 的几个测试夹具需要把有损 JPEG 底栏
// 吸附成纯白（线上无损抓图的前提）。这是子模块实现在测试期的唯一外露点。
#[cfg(test)]
pub(crate) use hall::snap_near_white_to_pure_white;
