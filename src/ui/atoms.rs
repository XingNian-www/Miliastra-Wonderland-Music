use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use enigo::Key;
use image::DynamicImage;

use crate::config::PointConfig;
use crate::runtime::ui::{
    CaptureFrame, InputCertainty, UiRoutine, UiRoutineContext, UiRoutineFailure, UiRuntimeHandle,
    sealed,
};

pub(crate) trait GameUiBackend: Send + Sync + 'static {
    fn capture(&self) -> Result<DynamicImage>;
    fn press_key(&self, key: Key) -> Result<()>;
    fn click_point(&self, point: PointConfig) -> Result<()>;
    fn ensure_ready(&self, after_activate_ms: u64) -> Result<()>;
    fn close_window(&self) -> Result<()>;
}

#[derive(Clone)]
pub(crate) struct GameUi {
    backend: Arc<dyn GameUiBackend>,
}

impl GameUi {
    pub(crate) fn new(backend: impl GameUiBackend) -> Self {
        Self {
            backend: Arc::new(backend),
        }
    }

    pub(crate) fn capture(&self) -> Result<DynamicImage> {
        self.backend.capture()
    }

    pub(crate) fn press_key(&self, key: Key) -> Result<()> {
        self.backend.press_key(key)
    }

    pub(crate) fn click_point(&self, point: PointConfig) -> Result<()> {
        self.backend.click_point(point)
    }

    pub(crate) fn ensure_ready(&self, after_activate_ms: u64) -> Result<()> {
        self.backend.ensure_ready(after_activate_ms)
    }

    pub(crate) fn close_window(&self) -> Result<()> {
        self.backend.close_window()
    }

    pub(crate) fn runtime(handle: UiRuntimeHandle) -> Self {
        Self::new(RuntimeGameUiBackend { handle })
    }
}

impl fmt::Debug for GameUi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("GameUi").finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct RuntimeGameUiBackend {
    handle: UiRuntimeHandle,
}

impl RuntimeGameUiBackend {
    fn execute<R: UiRoutine>(&self, routine: R) -> Result<R::Output> {
        Ok(self.handle.submit(routine)?.wait()?)
    }
}

struct PressKeyRoutine {
    key: Key,
}

impl sealed::UiRoutineSealed for PressKeyRoutine {}

impl UiRoutine for PressKeyRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context.device().press_key(self.key).map_err(|error| {
            UiRoutineFailure::new(
                InputCertainty::AfterInputUnknown,
                "press_key",
                format!("{error:#}"),
            )
        })
    }
}

fn device_failure(
    certainty: InputCertainty,
    stage: &'static str,
    error: anyhow::Error,
) -> UiRoutineFailure {
    UiRoutineFailure::new(certainty, stage, format!("{error:#}"))
}

struct ClickPointRoutine {
    point: PointConfig,
}

impl sealed::UiRoutineSealed for ClickPointRoutine {}

impl UiRoutine for ClickPointRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context
            .device()
            .click_point(self.point.x, self.point.y)
            .map_err(|error| {
                device_failure(InputCertainty::AfterInputUnknown, "click_point", error)
            })
    }
}

struct EnsureReadyRoutine {
    after_activate_ms: u64,
}

impl sealed::UiRoutineSealed for EnsureReadyRoutine {}

impl UiRoutine for EnsureReadyRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context
            .device()
            .ensure_ready(self.after_activate_ms)
            .map_err(|error| {
                device_failure(
                    InputCertainty::AfterInputUnknown,
                    "ensure_game_ready_for_input",
                    error,
                )
            })
    }
}

struct CloseWindowRoutine;

impl sealed::UiRoutineSealed for CloseWindowRoutine {}

impl UiRoutine for CloseWindowRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context.device().close_window().map_err(|error| {
            device_failure(InputCertainty::AfterInputUnknown, "close_window", error)
        })
    }
}

impl GameUiBackend for RuntimeGameUiBackend {
    fn capture(&self) -> Result<DynamicImage> {
        Ok(self.execute(CaptureFrame)??.into_image())
    }

    fn press_key(&self, key: Key) -> Result<()> {
        self.execute(PressKeyRoutine { key })??;
        Ok(())
    }

    fn click_point(&self, point: PointConfig) -> Result<()> {
        self.execute(ClickPointRoutine { point })??;
        Ok(())
    }

    fn ensure_ready(&self, after_activate_ms: u64) -> Result<()> {
        self.execute(EnsureReadyRoutine { after_activate_ms })??;
        Ok(())
    }

    fn close_window(&self) -> Result<()> {
        self.execute(CloseWindowRoutine)??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::Result;
    use enigo::Key;
    use image::DynamicImage;

    use super::*;
    use crate::runtime::ui::{UiDevice, UiRuntime};

    macro_rules! impl_noop_input_backend {
        () => {
            fn click_point(&self, _point: PointConfig) -> Result<()> {
                Ok(())
            }

            fn ensure_ready(&self, _after_activate_ms: u64) -> Result<()> {
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

    struct RecordingUiDevice {
        keys: Arc<Mutex<Vec<Key>>>,
    }

    impl UiDevice for RecordingUiDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(1, 1))
        }

        fn press_key(&mut self, key: Key) -> Result<()> {
            self.keys.lock().unwrap().push(key);
            Ok(())
        }
    }

    #[test]
    fn runtime_game_ui_handles_execute_on_one_owned_device() {
        let keys = Arc::new(Mutex::new(Vec::new()));
        let runtime = UiRuntime::start(RecordingUiDevice { keys: keys.clone() }, 2).unwrap();
        let ui = GameUi::runtime(runtime.handle());
        let clone = ui.clone();

        ui.press_key(Key::Return).unwrap();
        clone.press_key(Key::Escape).unwrap();

        assert_eq!(*keys.lock().unwrap(), vec![Key::Return, Key::Escape]);
        runtime.shutdown().unwrap();
    }
}
