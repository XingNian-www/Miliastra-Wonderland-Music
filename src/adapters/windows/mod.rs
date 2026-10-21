//! Windows 设备适配层。

mod clipboard;
mod device;
pub(crate) mod dpi;
mod game;
mod input;
mod window;

pub(crate) use device::WindowsUiDevice;
pub(crate) use game::resolve_game_executable;
// 跨平台的 game_path 需要从注册表取安装目录；这个查询本身是 Windows 专属，
// 所以留在 game.rs，但必须从模块根导出（否则只在 Windows 构建暴露 E0425）。
pub(crate) use game::registry_game_path;

// 键名解析已提升到 `crate::adapters::input`（跨平台纯逻辑）。
// 这里保留重导出，避免既有 Windows 专属调用点各改一遍。
pub(crate) use crate::adapters::input::parse_key;
