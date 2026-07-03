use anyhow::{Context, Result, bail};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

use super::clipboard;
use super::config;
use super::config::PointConfig;
use super::window;

pub(super) fn click_game_point(
    point: PointConfig,
    window_config: &config::WindowConfig,
) -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
    let mut window = window::GameWindow::find(window_config)?;
    window.click(&mut enigo, point)?;
    Ok(())
}

pub(super) fn paste_text(text: &str) -> Result<()> {
    clipboard::set_text(text).context("set clipboard text")?;
    let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
    enigo
        .key(Key::Control, Direction::Press)
        .context("press control")?;
    let paste_result = enigo
        .key(Key::Unicode('v'), Direction::Click)
        .context("paste text");
    enigo
        .key(Key::Control, Direction::Release)
        .context("release control")?;
    paste_result
}

pub(super) fn press_key(key: Key, window_config: &config::WindowConfig) -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
    let mut window = window::GameWindow::find(window_config)?;
    window.focus_for_keyboard(&mut enigo)?;
    enigo.key(key, Direction::Click).context("press key")?;
    Ok(())
}

pub(super) fn run_or_print<F>(execute: bool, description: String, action: F) -> Result<()>
where
    F: FnOnce() -> Result<()>,
{
    if execute {
        action()
    } else {
        println!("dry-run: {}", description);
        println!("pass --execute to send real keyboard/mouse input");
        Ok(())
    }
}

pub(super) fn parse_key(value: &str) -> Result<Key> {
    let normalized = value.trim().to_ascii_lowercase();
    let key = match normalized.as_str() {
        "return" | "enter" => Key::Return,
        "escape" | "esc" => Key::Escape,
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
