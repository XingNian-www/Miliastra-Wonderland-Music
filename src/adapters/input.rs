//! 输入相关字面量转换（跨平台）。
//!
//! 键名字符串 → `enigo::Key` 的映射本身是纯逻辑，不碰任何设备或 Win32 API，
//! 放在这里是为了让 `composition` 与 `ui::routines` 不必依赖 Windows 才能解析按键。
//!
//! 注意：enigo 把字母键（`Key::A`…`Key::Z`、`Key::Num0`…）标为 Windows-only，
//! 跨平台只能走 `Key::Unicode`。因此这里**永远**返回 `Key::Unicode` 表示字符键，
//! 不要为了"看起来更原生"改成字母变体——那会让本模块无法在非 Windows 上编译。

use anyhow::{Result, bail};
use enigo::Key;

/// 把配置/请求里的键名解析成可发送的按键。
///
/// 支持：回车、Esc、Ctrl、F1–F12、任意单字符。其余一律报错，
/// 不静默忽略——错误的键名应该让调用方看到失败。
pub(crate) fn parse_key(value: &str) -> Result<Key> {
    let normalized = value.trim().to_ascii_lowercase();
    let key = match normalized.as_str() {
        "return" | "enter" => Key::Return,
        "escape" | "esc" => Key::Escape,
        "control" | "ctrl" => Key::Control,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        "n" => Key::Unicode('n'),
        single if single.chars().count() == 1 => Key::Unicode(single.chars().next().unwrap()),
        _ => bail!("unsupported key: {}", value),
    };
    Ok(key)
}
