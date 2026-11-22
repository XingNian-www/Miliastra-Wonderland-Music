//! 配置中心：结构体定义、schema 声明、SQLite 持久化、迁移与热更新。
//!
//! 对外仍是**一条精炼入口**：全仓既有的 `crate::config::Xxx` 路径全部保持不变，
//! 内部按职责分目录：
//!
//! - [`types`]：各业务域配置结构体、默认值与 `validate()`，以及 `AppConfig` /
//!   `BootstrapConfig` 聚合；
//! - [`schema`]：Web 配置页的字段声明（路径、控件类型、生效级别、来源）；
//! - [`store`]：SQLite 配置中心（按段持久化、历史快照、回滚与脱敏）；
//! - [`migrations`]：旧版本配置的统一版本化迁移管线；
//! - [`live`]：保存后立即生效字段的运行时共享句柄；
//! - [`visual`]：视觉档案（几何/阈值）的导入导出；
//! - [`ai`]：AI 连接的协议、模型能力查询与工具会话。
//!
//! 本文件只保留模块声明与 `pub(crate) use` 重导出，不再承载具体实现。

/// 各业务域配置结构体与聚合入口。
mod types;
pub(crate) use types::*;

/// 配置 schema 声明（阶段 4a）：Web 配置页按此生成表单。
pub(crate) mod ai;
/// 配置 schema 平滑迁移：旧版本字段改名/删除按键搬运，替代断代重置。
mod migrations;
mod schema;
pub(crate) use schema::*;

/// SQLite 配置中心：配置持久化存储（阶段 2），阶段 3 起由启动流程消费。
mod store;
pub(crate) use store::*;

/// Unified visual profile import/export over the existing SQLite fields.
pub(crate) mod visual;

/// 运行时可热更新配置的共享句柄集合（阶段 7）：保存成功后由 HTTP 层 apply，
/// 消费方在运行态读取共享值，使 schema 中标 Live 的字段真正生效。
mod live;
pub(crate) use live::*;

#[cfg(test)]
pub(crate) fn bundled_config_yaml() -> &'static str {
    // 仅供测试的完整配置随源码保存，保证干净检出也能编译单元测试。
    // 仓库与发布包的 config.yaml 已精简为最小启动配置（仅 bootstrap 字段）。
    include_str!("test.full.yaml")
}

/// 内置完整配置解析结果（测试专用）。
///
/// 单元测试**不得**再依赖 `tests/fixtures/config.full.yaml`：`tests/` 不入库，
/// 干净检出时该文件不存在，`cargo test --lib` 会直接 panic。需要完整配置的测试
/// 统一改用本函数（内容与发布包用的 `config.yaml` 同源，见 [`bundled_config_yaml`]）。
#[cfg(test)]
pub(crate) fn bundled_config() -> AppConfig {
    serde_norway::from_str(bundled_config_yaml()).expect("内置完整配置必须能解析")
}
