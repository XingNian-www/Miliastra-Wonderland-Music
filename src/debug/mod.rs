//! 调试沙盒与完全控制模式。
//!
//! 原先 12 个 `debug_*.rs` 堆在 `src/` 根目录（约 13.7k 行），是已知的结构债：
//! 根目录被这类文件占据后，"哪个文件属于哪个领域"看不出来。
//! 这里按职责分组，公开路径统一变成 `crate::debug::*`。
//!
//! 四个子领域：
//! - `sandbox*`：原子命令、执行器、日志与持久化
//! - `read_only`：只读观察（active/push 两态）与分页
//! - `plugin_*`：外部插件协议（stdio/process/session/dispatch）
//! - `events` / `http_service`：事件流与 HTTP 服务
//!
//! 注意：原先的 `mode`（进程级输入闸门）已上提到 `src/input_gate.rs`。
//! 它服务所有输入路径而不只是调试模式，留在 `debug/` 会让 runtime、adapters
//! 等下游层产生对 debug 领域的虚假依赖。

pub(crate) mod events;
pub(crate) mod http_service;
pub(crate) mod plugin_dispatch;
pub(crate) mod plugin_process;
pub(crate) mod plugin_session;
pub(crate) mod plugin_stdio;
pub(crate) mod read_only;
pub(crate) mod sandbox;
pub(crate) mod sandbox_journal;
pub(crate) mod sandbox_runner;
pub(crate) mod sandbox_store;

// 既有的扁平访问路径在 crate 内被大量使用，这里保留一级重导出以减少改动面。
pub(crate) use events as debug_events;
pub(crate) use http_service as debug_http_service;
pub(crate) use plugin_dispatch as debug_plugin_dispatch;
pub(crate) use plugin_process as debug_plugin_process;
pub(crate) use plugin_session as debug_plugin_session;
pub(crate) use plugin_stdio as debug_plugin_stdio;
pub(crate) use read_only as debug_read_only;
pub(crate) use sandbox as debug_sandbox;
pub(crate) use sandbox_journal as debug_sandbox_journal;
pub(crate) use sandbox_runner as debug_sandbox_runner;
pub(crate) use sandbox_store as debug_sandbox_store;
