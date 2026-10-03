use std::path::Path;
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use enigo::{Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use image::DynamicImage;

use super::{input, window};
use crate::config::{PointConfig, WindowConfig};
use crate::runtime::ui::UiDevice;

#[derive(Default)]
struct HeldInputs {
    keys: Vec<Key>,
    buttons: Vec<Button>,
}
impl HeldInputs {
    fn press_key(&mut self, key: Key) {
        if !self.keys.contains(&key) {
            self.keys.push(key);
        }
    }
    fn release_key(&mut self, key: Key) {
        self.keys.retain(|held| *held != key);
    }
    fn press_button(&mut self, button: Button) {
        if !self.buttons.contains(&button) {
            self.buttons.push(button);
        }
    }
    fn release_button(&mut self, button: Button) {
        self.buttons.retain(|held| *held != button);
    }
    fn recover_with(
        &mut self,
        mut key_up: impl FnMut(Key) -> Result<()>,
        mut button_up: impl FnMut(Button) -> Result<()>,
    ) -> Result<()> {
        let mut first_error = None;
        for key in self.keys.clone().into_iter().rev() {
            match key_up(key) {
                Ok(()) => self.release_key(key),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        for button in self.buttons.clone().into_iter().rev() {
            match button_up(button) {
                Ok(()) => self.release_button(button),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod held_tests {
    use super::*;
    #[test]
    fn recovery_without_held_inputs_does_not_require_target_window() {
        let window = WindowConfig {
            target_process: format!("missing-debug-{}.exe", uuid::Uuid::new_v4()),
            ..WindowConfig::default()
        };
        let mut device = WindowsUiDevice::new(window);
        assert!(!device.window_available().unwrap());
        device.recover().unwrap();
    }

    #[test]
    fn recovery_retries_failed_release_and_continues_other_inputs() {
        let mut held = HeldInputs::default();
        held.press_key(Key::Control);
        held.press_key(Key::Control);
        held.press_key(Key::Shift);
        held.press_button(Button::Left);
        assert_eq!(held.keys.len(), 2);
        let mut attempts = Vec::new();
        assert!(
            held.recover_with(
                |key| {
                    attempts.push(key);
                    if key == Key::Control {
                        anyhow::bail!("failed")
                    }
                    Ok(())
                },
                |_| Ok(()),
            )
            .is_err()
        );
        assert_eq!(attempts, vec![Key::Shift, Key::Control]);
        assert_eq!(held.keys, vec![Key::Control]);
        assert!(held.buttons.is_empty());
        held.recover_with(|_| Ok(()), |_| Ok(())).unwrap();
        assert!(held.keys.is_empty());
    }
}

pub(crate) struct WindowsUiDevice {
    window: WindowConfig,
    held: HeldInputs,
}

impl WindowsUiDevice {
    pub(crate) fn new(window: WindowConfig) -> Self {
        Self {
            window,
            held: HeldInputs::default(),
        }
    }
}

impl UiDevice for WindowsUiDevice {
    fn capture(&mut self) -> Result<DynamicImage> {
        window::capture_game(&self.window)
    }

    /// 只读剪贴板：不要求窗口在前台，也没有输入副作用。
    fn clipboard_text(&mut self) -> Result<Option<String>> {
        super::clipboard::get_text()
    }

    fn press_key(&mut self, key: Key) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::press_key(key, &self.window)
    }

    fn key_down(&mut self, key: Key) -> Result<()> {
        crate::debug_mode::require_input()?;
        window::ensure_foreground(&self.window)?;
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        self.held.press_key(key);
        enigo.key(key, Direction::Press).context("press key down")
    }

    fn key_up(&mut self, key: Key) -> Result<()> {
        crate::debug_mode::require_input()?;
        window::ensure_foreground(&self.window)?;
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        enigo.key(key, Direction::Release).context("release key")?;
        self.held.release_key(key);
        Ok(())
    }

    fn click_point(&mut self, x: i32, y: i32) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::click_game_point(PointConfig::new(x, y), &self.window)
    }

    fn click_button(&mut self, button: Button) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::click_game_button(button, &self.window)
    }

    fn mouse_down(&mut self, button: Button) -> Result<()> {
        crate::debug_mode::require_input()?;
        window::ensure_foreground(&self.window)?;
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        self.held.press_button(button);
        enigo
            .button(button, Direction::Press)
            .context("press mouse button")
    }

    fn mouse_up(&mut self, button: Button) -> Result<()> {
        crate::debug_mode::require_input()?;
        window::ensure_foreground(&self.window)?;
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        enigo
            .button(button, Direction::Release)
            .context("release mouse button")?;
        self.held.release_button(button);
        Ok(())
    }

    fn scroll_point(&mut self, x: i32, y: i32, length: i32) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::scroll_game_point(PointConfig::new(x, y), length, &self.window)
    }

    fn drag_point(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::drag_game_point(
            PointConfig::new(from_x, from_y),
            PointConfig::new(to_x, to_y),
            &self.window,
        )
    }

    fn activate(&mut self, after_activate_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::activate_game(&self.window, after_activate_ms)
    }

    fn focus(&mut self, after_activate_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::focus_game(&self.window, after_activate_ms)
    }

    fn ensure_ready(&mut self, after_activate_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::ensure_game_ready_for_input(&self.window, after_activate_ms)
    }

    fn ensure_foreground(&mut self) -> Result<()> {
        window::ensure_foreground(&self.window)
    }

    fn window_foreground(&mut self) -> Result<bool> {
        Ok(window::GameWindow::find(&self.window)?.is_foreground())
    }

    fn window_available(&mut self) -> Result<bool> {
        match window::GameWindow::find(&self.window) {
            Ok(_) => Ok(true),
            Err(error) if error.is::<window::TargetWindowUnavailable>() => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn paste_text(&mut self, text: &str, clipboard_hold_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::paste_text(text, &self.window, clipboard_hold_ms)
    }

    fn input_text(&mut self, text: &str, input_settle_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        window::ensure_foreground(&self.window)?;
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        enigo.text(text).context("input message text")?;
        sleep(Duration::from_millis(input_settle_ms));
        Ok(())
    }

    fn hold_key(&mut self, key: Key, duration: Duration, running: Arc<AtomicBool>) -> Result<()> {
        crate::debug_mode::require_input()?;
        input::hold_key(key, duration, &self.window, || {
            running.load(Ordering::SeqCst)
        })
    }

    fn ensure_window(&mut self) -> Result<()> {
        window::GameWindow::find(&self.window).map(|_| ())
    }

    fn close_window(&mut self) -> Result<()> {
        crate::debug_mode::require_input()?;
        window::close_game(&self.window)
    }

    fn recover(&mut self) -> Result<()> {
        if self.held.keys.is_empty() && self.held.buttons.is_empty() {
            return Ok(());
        }
        window::ensure_foreground(&self.window)?;
        let enigo = std::cell::RefCell::new(
            Enigo::new(&Settings::default()).context("create enigo for recovery")?,
        );
        let released = self.held.recover_with(
            |key| {
                enigo
                    .borrow_mut()
                    .key(key, Direction::Release)
                    .context("release held key")
            },
            |button| {
                enigo
                    .borrow_mut()
                    .button(button, Direction::Release)
                    .context("release held mouse button")
            },
        );
        released
    }

    fn launch_game(&mut self, executable: &Path, args: &[String]) -> Result<()> {
        crate::debug_mode::require_input()?;
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
