use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use enigo::Key;
use image::DynamicImage;

use super::input_actions;
use super::window;
use crate::config::{PointConfig, WindowConfig};

pub(super) trait GameUiBackend: Send + Sync + 'static {
    fn capture(&self) -> Result<DynamicImage>;
    fn press_key(&self, key: Key) -> Result<()>;
    fn click_point(&self, point: PointConfig) -> Result<()>;
    fn scroll_point(&self, point: PointConfig, length: i32) -> Result<()>;
    fn activate(&self, after_activate_ms: u64) -> Result<()>;
    fn focus(&self, after_activate_ms: u64) -> Result<()>;
    fn ensure_ready(&self, after_activate_ms: u64) -> Result<()>;
    fn paste_text(&self, text: &str, clipboard_hold_ms: u64) -> Result<()>;
    fn hold_key(&self, key: Key, duration: Duration, running: Arc<AtomicBool>) -> Result<()>;
    fn ensure_window(&self) -> Result<()>;
    fn close_window(&self) -> Result<()>;
}

#[derive(Clone)]
pub(super) struct GameUi {
    backend: Arc<dyn GameUiBackend>,
}

impl GameUi {
    pub(super) fn new(backend: impl GameUiBackend) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    pub(super) fn capture(&self) -> Result<DynamicImage> {
        self.backend.capture()
    }

    pub(super) fn press_key(&self, key: Key) -> Result<()> {
        self.backend.press_key(key)
    }

    pub(super) fn click_point(&self, point: PointConfig) -> Result<()> {
        self.backend.click_point(point)
    }

    pub(super) fn scroll_point(&self, point: PointConfig, length: i32) -> Result<()> {
        self.backend.scroll_point(point, length)
    }

    pub(super) fn activate(&self, after_activate_ms: u64) -> Result<()> {
        self.backend.activate(after_activate_ms)
    }

    pub(super) fn focus(&self, after_activate_ms: u64) -> Result<()> {
        self.backend.focus(after_activate_ms)
    }

    pub(super) fn ensure_ready(&self, after_activate_ms: u64) -> Result<()> {
        self.backend.ensure_ready(after_activate_ms)
    }

    pub(super) fn paste_text(&self, text: &str, clipboard_hold_ms: u64) -> Result<()> {
        self.backend.paste_text(text, clipboard_hold_ms)
    }

    pub(super) fn hold_key(
        &self,
        key: Key,
        duration: Duration,
        running: Arc<AtomicBool>,
    ) -> Result<()> {
        self.backend.hold_key(key, duration, running)
    }

    pub(super) fn ensure_window(&self) -> Result<()> {
        self.backend.ensure_window()
    }

    pub(super) fn close_window(&self) -> Result<()> {
        self.backend.close_window()
    }

    pub(super) fn direct(window: WindowConfig) -> Self {
        Self::new(DirectGameUiBackend {
            window,
            operation_lock: Mutex::new(()),
        })
    }
}

impl fmt::Debug for GameUi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("GameUi").finish_non_exhaustive()
    }
}

struct DirectGameUiBackend {
    window: WindowConfig,
    operation_lock: Mutex<()>,
}

impl DirectGameUiBackend {
    fn with_operation<T>(&self, operation: impl FnOnce(&WindowConfig) -> Result<T>) -> Result<T> {
        let _guard = self
            .operation_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("game UI operation mutex poisoned"))?;
        operation(&self.window)
    }
}

impl GameUiBackend for DirectGameUiBackend {
    fn capture(&self) -> Result<DynamicImage> {
        self.with_operation(window::capture_game)
    }

    fn press_key(&self, key: Key) -> Result<()> {
        self.with_operation(|window| input_actions::press_key(key, window))
    }

    fn click_point(&self, point: PointConfig) -> Result<()> {
        self.with_operation(|window| input_actions::click_game_point(point, window))
    }

    fn scroll_point(&self, point: PointConfig, length: i32) -> Result<()> {
        self.with_operation(|window| input_actions::scroll_game_point(point, length, window))
    }

    fn activate(&self, after_activate_ms: u64) -> Result<()> {
        self.with_operation(|window| input_actions::activate_game(window, after_activate_ms))
    }

    fn focus(&self, after_activate_ms: u64) -> Result<()> {
        self.with_operation(|window| input_actions::focus_game(window, after_activate_ms))
    }

    fn ensure_ready(&self, after_activate_ms: u64) -> Result<()> {
        self.with_operation(|window| {
            input_actions::ensure_game_ready_for_input(window, after_activate_ms)
        })
    }

    fn paste_text(&self, text: &str, clipboard_hold_ms: u64) -> Result<()> {
        self.with_operation(|window| input_actions::paste_text(text, window, clipboard_hold_ms))
    }

    fn hold_key(&self, key: Key, duration: Duration, running: Arc<AtomicBool>) -> Result<()> {
        self.with_operation(|window| {
            input_actions::hold_key(key, duration, window, || running.load(Ordering::SeqCst))
        })
    }

    fn ensure_window(&self) -> Result<()> {
        self.with_operation(|window| window::GameWindow::find(window).map(|_| ()))
    }

    fn close_window(&self) -> Result<()> {
        self.with_operation(window::close_game)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Result;
    use enigo::Key;
    use image::DynamicImage;

    use super::*;

    macro_rules! impl_noop_input_backend {
        () => {
            fn click_point(&self, _point: PointConfig) -> Result<()> {
                Ok(())
            }

            fn scroll_point(&self, _point: PointConfig, _length: i32) -> Result<()> {
                Ok(())
            }

            fn activate(&self, _after_activate_ms: u64) -> Result<()> {
                Ok(())
            }

            fn focus(&self, _after_activate_ms: u64) -> Result<()> {
                Ok(())
            }

            fn ensure_ready(&self, _after_activate_ms: u64) -> Result<()> {
                Ok(())
            }

            fn paste_text(&self, _text: &str, _clipboard_hold_ms: u64) -> Result<()> {
                Ok(())
            }

            fn hold_key(
                &self,
                _key: Key,
                _duration: Duration,
                _running: Arc<AtomicBool>,
            ) -> Result<()> {
                Ok(())
            }

            fn ensure_window(&self) -> Result<()> {
                Ok(())
            }

            fn close_window(&self) -> Result<()> {
                Ok(())
            }
        };
    }

    struct FakeGameUiBackend;

    impl GameUiBackend for FakeGameUiBackend {
        fn capture(&self) -> Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(3, 2))
        }

        fn press_key(&self, _key: Key) -> Result<()> {
            Ok(())
        }

        impl_noop_input_backend!();
    }

    #[test]
    fn shared_game_ui_delegates_capture_to_its_backend() {
        let ui = GameUi::new(FakeGameUiBackend);

        let image = ui.capture().unwrap();

        assert_eq!(image.width(), 3);
        assert_eq!(image.height(), 2);
    }

    struct RecordingGameUiBackend {
        keys: Arc<Mutex<Vec<Key>>>,
    }

    impl GameUiBackend for RecordingGameUiBackend {
        fn capture(&self) -> Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(1, 1))
        }

        fn press_key(&self, key: Key) -> Result<()> {
            self.keys.lock().unwrap().push(key);
            Ok(())
        }

        impl_noop_input_backend!();
    }

    #[test]
    fn cloned_game_ui_handles_share_one_input_backend() {
        let keys = Arc::new(Mutex::new(Vec::new()));
        let ui = GameUi::new(RecordingGameUiBackend { keys: keys.clone() });
        let clone = ui.clone();

        ui.press_key(Key::Return).unwrap();
        clone.press_key(Key::Escape).unwrap();

        assert_eq!(*keys.lock().unwrap(), vec![Key::Return, Key::Escape]);
    }
}
