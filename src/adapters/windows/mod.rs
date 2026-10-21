//! Windows 设备适配层。

mod clipboard;
mod device;
pub(crate) mod dpi;
mod game;
mod input;
mod window;

pub(crate) use device::WindowsUiDevice;
pub(crate) use game::resolve_game_executable;

// 键名解析已提升到 `crate::adapters::input`（跨平台纯逻辑）。
// 这里保留重导出，避免既有 Windows 专属调用点各改一遍。
pub(crate) use crate::adapters::input::parse_key;
