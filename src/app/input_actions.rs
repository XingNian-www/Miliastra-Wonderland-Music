use std::thread::sleep;
use std::time::{Duration, Instant};

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

pub(super) fn activate_game(
    window_config: &config::WindowConfig,
    after_activate_ms: u64,
) -> Result<()> {
    let mut window = window::GameWindow::find(window_config)?;
    window.activate(after_activate_ms)
}

pub(super) fn focus_game(
    window_config: &config::WindowConfig,
    after_activate_ms: u64,
) -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
    let mut window = window::GameWindow::find(window_config)?;
    window.activate(after_activate_ms)?;
    window.focus_game(&mut enigo, window_config.focus_point)
}

pub(super) fn ensure_game_ready_for_input(
    window_config: &config::WindowConfig,
    after_activate_ms: u64,
) -> Result<()> {
    focus_game(window_config, after_activate_ms)
}

pub(super) fn paste_text(
    text: &str,
    window_config: &config::WindowConfig,
    clipboard_hold_ms: u64,
) -> Result<()> {
    let started = Instant::now();
    let foreground_started = Instant::now();
    window::ensure_foreground(window_config)?;
    let foreground_ms = elapsed_ms(foreground_started);
    let clipboard_started = Instant::now();
    let _clipboard_guard = clipboard::TextRestoreGuard::replace_with(text)?;
    let clipboard_ms = elapsed_ms(clipboard_started);
    let input_started = Instant::now();
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
    sleep(Duration::from_millis(clipboard_hold_ms));
    log::info!(
        "粘贴文本耗时: total={}ms foreground={}ms clipboard={}ms input={}ms hold={}ms chars={}",
        elapsed_ms(started),
        foreground_ms,
        clipboard_ms,
        elapsed_ms(input_started),
        clipboard_hold_ms,
        text.chars().count()
    );
    paste_result
}

pub(super) fn press_key(key: Key, window_config: &config::WindowConfig) -> Result<()> {
    let started = Instant::now();
    let foreground_started = Instant::now();
    window::ensure_foreground(window_config)?;
    let foreground_ms = elapsed_ms(foreground_started);
    let input_started = Instant::now();
    let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
    enigo.key(key, Direction::Click).context("press key")?;
    log::debug!(
        "按键输入耗时: total={}ms foreground={}ms input={}ms",
        elapsed_ms(started),
        foreground_ms,
        elapsed_ms(input_started)
    );
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

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
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
