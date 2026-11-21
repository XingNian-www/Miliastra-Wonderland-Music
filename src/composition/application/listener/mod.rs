//! 聊天值守主循环：从共享观察帧到命令入队的整条链路。
//!
//! ## 职责导航
//!
//! | 子模块 | 回答什么问题 | 主要入口 |
//! |---|---|---|
//! | [`display`] | 日志与事件里「人读到什么」 | 中文标签、[`display::ObservationDedup`] |
//! | [`recovery_guard`] | 现在算不算就绪、界面该不该拉回、要不要重启游戏 | [`recovery_guard::RecoveryGuard`]、`poll_auto_game_restart` |
//! | [`chat_routing`] | 这条消息该不该派发、命令能不能过闸门 | [`chat_routing::CommandGate`]、`consume_shared_chat_snapshots` |
//! | [`loop_driver`] | 主循环本身、面板与热键、值守模式落盘 | `run_scan_loop` |
//!
//! ## 状态归属（拆分后最重要的一条）
//!
//! 这个模块**没有任何进程级可变 `static`**。三类状态各有归属：
//!
//! 1. **本轮循环的进度**（观察帧需求、指纹、退避、告警节流槽、展示去重）——
//!    局部变量，随 `run_scan_loop_inner` 的一次调用生灭。因此单测之间天然隔离。
//! 2. **跨触发源的恢复状态**（重启冷却、恢复互斥、持续无法识别时长）——
//!    进程级 `GameRecoveryCoordinator`，由装配层注入一份、三处共享 clone。
//! 3. **业务事实**（值守模式、命令锁、观察基线）——既有的业务/观察服务，
//!    本模块只经它们的公开方法读写，不另存一份。
//!
//! ## 对外路径
//!
//! 拆分前 `listener::X` 的每一个对外符号在这里按原名重导出，调用点无需改动。
//! 内部子模块一律不 `pub`：跨文件可见性统一用 `pub(in crate::composition::application)`，
//! 与拆分前的 `pub(super)` 覆盖范围一致。

mod chat_routing;
mod display;
mod loop_driver;
mod recovery_guard;

#[cfg(test)]
mod tests;

// ── 对外符号：保持 `listener::*` 的既有路径不变 ──────────────────────────────
pub(in crate::composition::application) use chat_routing::CommandGate;
pub(in crate::composition::application) use loop_driver::load_persisted_chat_listener_mode;
