//! 模板匹配：wgpu 计算着色器主路径 + CPU（SEA 剪枝）兜底。
//!
//! 两条路径都必须保留：GPU 只负责定位最优点，随后用 CPU 以该点为初始上界再扫描一次，
//! 保证结果与朴素逐像素扫描逐位一致；GPU 不可用（无适配器 / 驱动异常 / 模板带 alpha）
//! 时整条链路自动退回纯 CPU。对外路径与拆分前完全一致。
//!
//! 重导出只列**跨模块真实使用**的名字：模块边界上每多挂一个名字就等于多开一个
//! 入口，编译器也再难替你发现死代码。本模块测试走 `super::<子模块>::*` 直达，
//! 不经过这里，因此测试专用的内部实现（CPU 对拍基线、文字质量校验等）不在此重导出。

mod api;
mod cache;
mod cpu;
mod gpu;

#[cfg(test)]
mod tests;

// 命中结果只含几何与分数，已移到几何叶子（runtime 也要用）。
// 这里重导出，既有 `ui::template::TemplateHit` 调用点不必改动。
pub(crate) use crate::geometry::TemplateHit;

pub(crate) use api::{
    best_embedded_template_candidate, best_template_candidate, best_template_candidate_downscaled,
    best_template_hit, find_color_template_hit_groups, find_template_hits, template_exists,
};
pub(crate) use cpu::{GrayMatcher, dedupe_hits};
pub(crate) use gpu::best_gray_match;
