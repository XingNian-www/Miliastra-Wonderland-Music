use std::path::Path;
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use enigo::{Button, Enigo, Key, Keyboard, Settings};
use image::DynamicImage;

use super::{input, window};
use crate::config::{PointConfig, WindowConfig};
use crate::runtime::ui::UiDevice;

pub(crate) struct WindowsUiDevice {
    window: WindowConfig,
}

impl WindowsUiDevice {
    pub(crate) fn new(window: WindowConfig) -> Self {
        Self { window }
    }
}

impl UiDevice for WindowsUiDevice {
    fn capture(&mut self) -> Result<DynamicImage> {
        window::capture_game(&self.window)
    }

    fn press_key(&mut self, key: Key) -> Result<()> {
        input::press_key(key, &self.window)
    }

    fn click_point(&mut self, x: i32, y: i32) -> Result<()> {
        input::click_game_point(PointConfig::new(x, y), &self.window)
    }

    fn click_button(&mut self, button: Button) -> Result<()> {
        input::click_game_button(button, &self.window)
    }

    fn scroll_point(&mut self, x: i32, y: i32, length: i32) -> Result<()> {
        input::scroll_game_point(PointConfig::new(x, y), length, &self.window)
    }

    fn drag_point(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<()> {
        input::drag_game_point(
            PointConfig::new(from_x, from_y),
            PointConfig::new(to_x, to_y),
            &self.window,
        )
    }

    fn activate(&mut self, after_activate_ms: u64) -> Result<()> {
        input::activate_game(&self.window, after_activate_ms)
    }

    fn focus(&mut self, after_activate_ms: u64) -> Result<()> {
        input::focus_game(&self.window, after_activate_ms)
    }

    fn ensure_ready(&mut self, after_activate_ms: u64) -> Result<()> {
        input::ensure_game_ready_for_input(&self.window, after_activate_ms)
    }

    fn ensure_foreground(&mut self) -> Result<()> {
        window::ensure_foreground(&self.window)
    }

    fn paste_text(&mut self, text: &str, clipboard_hold_ms: u64) -> Result<()> {
        input::paste_text(text, &self.window, clipboard_hold_ms)
    }

    fn input_text(&mut self, text: &str, input_settle_ms: u64) -> Result<()> {
        window::ensure_foreground(&self.window)?;
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        enigo.text(text).context("input message text")?;
        sleep(Duration::from_millis(input_settle_ms));
        Ok(())
    }

    fn hold_key(&mut self, key: Key, duration: Duration, running: Arc<AtomicBool>) -> Result<()> {
        input::hold_key(key, duration, &self.window, || {
            running.load(Ordering::SeqCst)
        })
    }

    fn ensure_window(&mut self) -> Result<()> {
        window::GameWindow::find(&self.window).map(|_| ())
    }

    fn close_window(&mut self) -> Result<()> {
        window::close_game(&self.window)
    }

    fn launch_game(&mut self, executable: &Path, args: &[String]) -> Result<()> {
        let mut command = ProcessCommand::new(executable);
        if let Some(parent) = executable
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            command.current_dir(parent);
        }
        command.args(args);
        command
            .spawn()
            .with_context(|| format!("启动游戏失败: {}", executable.display()))?;
        Ok(())
    }
}
