use std::fmt;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use image::DynamicImage;

use super::window;
use crate::config::WindowConfig;

pub(super) trait GameUiBackend: Send + Sync + 'static {
    fn capture(&self) -> Result<DynamicImage>;
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

    pub(super) fn direct(window: WindowConfig) -> Self {
        Self::new(DirectGameUiBackend {
            window,
            capture_lock: Mutex::new(()),
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
    capture_lock: Mutex<()>,
}

impl GameUiBackend for DirectGameUiBackend {
    fn capture(&self) -> Result<DynamicImage> {
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("game UI capture mutex poisoned"))?;
        window::capture_game(&self.window)
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use image::DynamicImage;

    use super::*;

    struct FakeGameUiBackend;

    impl GameUiBackend for FakeGameUiBackend {
        fn capture(&self) -> Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(3, 2))
        }
    }

    #[test]
    fn shared_game_ui_delegates_capture_to_its_backend() {
        let ui = GameUi::new(FakeGameUiBackend);

        let image = ui.capture().unwrap();

        assert_eq!(image.width(), 3);
        assert_eq!(image.height(), 2);
    }
}
