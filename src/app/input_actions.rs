use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use enigo::{Axis, Direction, Enigo, Key, Keyboard, Settings};

use super::clipboard;
use super::window;
use crate::config;
use crate::config::PointConfig;

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
    let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
    let mut window = window::GameWindow::find(window_config)?;
    let already_foreground = window.is_foreground();
    window.activate(after_activate_ms)?;
    if already_foreground {
        window.ensure_foreground()
    } else {
        window.focus_game(&mut enigo, window_config.focus_point)
    }
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
    log::info!(target: "timing",
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
    log::info!(target: "timing",
        "按键输入耗时: total={}ms foreground={}ms input={}ms",
        elapsed_ms(started),
        foreground_ms,
        elapsed_ms(input_started)
    );
    Ok(())
}

pub(super) fn scroll_game_point(
    point: PointConfig,
    length: i32,
    window_config: &config::WindowConfig,
) -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
    let mut window = window::GameWindow::find(window_config)?;
    window.scroll(&mut enigo, point, length, Axis::Vertical)
}

pub(super) fn hold_key<F>(
    key: Key,
    duration: Duration,
    window_config: &config::WindowConfig,
    mut should_continue: F,
) -> Result<()>
where
    F: FnMut() -> bool,
{
    if duration.is_zero() {
        bail!("按住按键时长必须大于 0 秒");
    }
    if !should_continue() {
        bail!("程序正在退出，未发送按住按键输入");
    }

    let started = Instant::now();
    let foreground_started = Instant::now();
    window::ensure_foreground(window_config)?;
    let foreground_ms = elapsed_ms(foreground_started);
    let input_started = Instant::now();
    let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
    enigo
        .key(key, Direction::Press)
        .context("press and hold key")?;
    let mut release = KeyReleaseGuard::new(&mut enigo, key);
    let deadline = Instant::now() + duration;
    let mut interrupted = false;

    while Instant::now() < deadline {
        if !should_continue() {
            interrupted = true;
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        sleep(remaining.min(Duration::from_millis(50)));
    }

    let release_result = release.release();
    log::info!(target: "timing",
        "按住按键耗时: total={}ms foreground={}ms input={}ms configured={}ms interrupted={} release_success={}",
        elapsed_ms(started),
        foreground_ms,
        elapsed_ms(input_started),
        duration.as_millis(),
        interrupted,
        release_result.is_ok()
    );
    release_result?;
    if interrupted {
        bail!("程序正在退出，已提前松开按键");
    }
    Ok(())
}

struct KeyReleaseGuard<'a> {
    enigo: &'a mut Enigo,
    key: Key,
    released: bool,
}

impl<'a> KeyReleaseGuard<'a> {
    fn new(enigo: &'a mut Enigo, key: Key) -> Self {
        Self {
            enigo,
            key,
            released: false,
        }
    }

    fn release(&mut self) -> Result<()> {
        self.enigo
            .key(self.key, Direction::Release)
            .context("release held key")?;
        self.released = true;
        Ok(())
    }
}

impl Drop for KeyReleaseGuard<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.enigo.key(self.key, Direction::Release);
        }
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
