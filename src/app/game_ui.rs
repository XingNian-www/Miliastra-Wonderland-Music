use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use enigo::{Enigo, Key, Keyboard, Settings};
use image::DynamicImage;
use image::imageops::FilterType;

use super::chat_output::{
    ChatBatchSendOutcome, ChatBatchSendStatus, primary_chat_should_close_directly, send_messages,
};
use super::frame_source::Canvas;
use super::geometry::Rect;
use super::input_actions;
use super::template_match::best_template_hit;
use super::window;
use crate::config::{PointConfig, WindowConfig};
use crate::runtime::ui::{
    CaptureFrame, InputCertainty, UiDevice, UiRoutine, UiRoutineFailure, UiRuntimeHandle, sealed,
};

#[derive(Clone, Copy, Debug)]
pub(super) enum ChatBatchTarget {
    Primary { restore_after_task: bool },
    Current,
}

#[derive(Clone, Debug)]
pub(super) struct ChatBatchRequest {
    pub(super) messages: Vec<String>,
    pub(super) delay_ms: u64,
    pub(super) target: ChatBatchTarget,
    pub(super) chat_click: PointConfig,
    pub(super) click_ms: u64,
    pub(super) open_chat_ms: u64,
    pub(super) text_ms: u64,
    pub(super) send_ms: u64,
    pub(super) canvas: Canvas,
    pub(super) secondary_hall_template: PathBuf,
    pub(super) secondary_hall_search_region: Rect,
    pub(super) friend_list_region: Rect,
    pub(super) template_threshold: f32,
}

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
    fn send_chat_batch(&self, request: ChatBatchRequest) -> ChatBatchSendOutcome;
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

    pub(super) fn send_chat_batch(&self, request: ChatBatchRequest) -> ChatBatchSendOutcome {
        self.backend.send_chat_batch(request)
    }

    pub(super) fn runtime(handle: UiRuntimeHandle) -> Self {
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

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device.press_key(self.key).map_err(|error| {
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

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device
            .click_point(self.point.x, self.point.y)
            .map_err(|error| {
                device_failure(InputCertainty::AfterInputUnknown, "click_point", error)
            })
    }
}

struct ScrollPointRoutine {
    point: PointConfig,
    length: i32,
}

impl sealed::UiRoutineSealed for ScrollPointRoutine {}

impl UiRoutine for ScrollPointRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device
            .scroll_point(self.point.x, self.point.y, self.length)
            .map_err(|error| {
                device_failure(InputCertainty::AfterInputUnknown, "scroll_point", error)
            })
    }
}

struct ActivateRoutine {
    after_activate_ms: u64,
}

impl sealed::UiRoutineSealed for ActivateRoutine {}

impl UiRoutine for ActivateRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device.activate(self.after_activate_ms).map_err(|error| {
            device_failure(InputCertainty::AfterInputUnknown, "activate_game", error)
        })
    }
}

struct FocusRoutine {
    after_activate_ms: u64,
}

impl sealed::UiRoutineSealed for FocusRoutine {}

impl UiRoutine for FocusRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device
            .focus(self.after_activate_ms)
            .map_err(|error| device_failure(InputCertainty::AfterInputUnknown, "focus_game", error))
    }
}

struct EnsureReadyRoutine {
    after_activate_ms: u64,
}

impl sealed::UiRoutineSealed for EnsureReadyRoutine {}

impl UiRoutine for EnsureReadyRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device
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

struct PasteTextRoutine {
    text: String,
    clipboard_hold_ms: u64,
}

impl sealed::UiRoutineSealed for PasteTextRoutine {}

impl UiRoutine for PasteTextRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device
            .paste_text(&self.text, self.clipboard_hold_ms)
            .map_err(|error| device_failure(InputCertainty::AfterInputUnknown, "paste_text", error))
    }
}

struct HoldKeyRoutine {
    key: Key,
    duration: Duration,
    running: Arc<AtomicBool>,
}

impl sealed::UiRoutineSealed for HoldKeyRoutine {}

impl UiRoutine for HoldKeyRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device
            .hold_key(self.key, self.duration, self.running)
            .map_err(|error| device_failure(InputCertainty::AfterInputUnknown, "hold_key", error))
    }
}

struct EnsureWindowRoutine;

impl sealed::UiRoutineSealed for EnsureWindowRoutine {}

impl UiRoutine for EnsureWindowRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device
            .ensure_window()
            .map_err(|error| device_failure(InputCertainty::BeforeInput, "ensure_window", error))
    }
}

struct CloseWindowRoutine;

impl sealed::UiRoutineSealed for CloseWindowRoutine {}

impl UiRoutine for CloseWindowRoutine {
    type Output = Result<(), UiRoutineFailure>;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        device.close_window().map_err(|error| {
            device_failure(InputCertainty::AfterInputUnknown, "close_window", error)
        })
    }
}

struct SendChatBatchRoutine {
    request: ChatBatchRequest,
}

impl sealed::UiRoutineSealed for SendChatBatchRoutine {}

impl UiRoutine for SendChatBatchRoutine {
    type Output = ChatBatchSendOutcome;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        execute_chat_batch(device, self.request)
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

    fn scroll_point(&self, point: PointConfig, length: i32) -> Result<()> {
        self.execute(ScrollPointRoutine { point, length })??;
        Ok(())
    }

    fn activate(&self, after_activate_ms: u64) -> Result<()> {
        self.execute(ActivateRoutine { after_activate_ms })??;
        Ok(())
    }

    fn focus(&self, after_activate_ms: u64) -> Result<()> {
        self.execute(FocusRoutine { after_activate_ms })??;
        Ok(())
    }

    fn ensure_ready(&self, after_activate_ms: u64) -> Result<()> {
        self.execute(EnsureReadyRoutine { after_activate_ms })??;
        Ok(())
    }

    fn paste_text(&self, text: &str, clipboard_hold_ms: u64) -> Result<()> {
        self.execute(PasteTextRoutine {
            text: text.to_string(),
            clipboard_hold_ms,
        })??;
        Ok(())
    }

    fn hold_key(&self, key: Key, duration: Duration, running: Arc<AtomicBool>) -> Result<()> {
        self.execute(HoldKeyRoutine {
            key,
            duration,
            running,
        })??;
        Ok(())
    }

    fn ensure_window(&self) -> Result<()> {
        self.execute(EnsureWindowRoutine)??;
        Ok(())
    }

    fn close_window(&self) -> Result<()> {
        self.execute(CloseWindowRoutine)??;
        Ok(())
    }

    fn send_chat_batch(&self, request: ChatBatchRequest) -> ChatBatchSendOutcome {
        self.execute(SendChatBatchRoutine { request })
            .unwrap_or_else(|error| ChatBatchSendOutcome::failed(0, error))
    }
}

pub(super) struct WindowsUiDevice {
    window: WindowConfig,
}

impl WindowsUiDevice {
    pub(super) fn new(window: WindowConfig) -> Self {
        Self { window }
    }
}

impl UiDevice for WindowsUiDevice {
    fn capture(&mut self) -> Result<DynamicImage> {
        window::capture_game(&self.window)
    }

    fn press_key(&mut self, key: Key) -> Result<()> {
        input_actions::press_key(key, &self.window)
    }

    fn click_point(&mut self, x: i32, y: i32) -> Result<()> {
        input_actions::click_game_point(PointConfig::new(x, y), &self.window)
    }

    fn scroll_point(&mut self, x: i32, y: i32, length: i32) -> Result<()> {
        input_actions::scroll_game_point(PointConfig::new(x, y), length, &self.window)
    }

    fn activate(&mut self, after_activate_ms: u64) -> Result<()> {
        input_actions::activate_game(&self.window, after_activate_ms)
    }

    fn focus(&mut self, after_activate_ms: u64) -> Result<()> {
        input_actions::focus_game(&self.window, after_activate_ms)
    }

    fn ensure_ready(&mut self, after_activate_ms: u64) -> Result<()> {
        input_actions::ensure_game_ready_for_input(&self.window, after_activate_ms)
    }

    fn ensure_foreground(&mut self) -> Result<()> {
        window::ensure_foreground(&self.window)
    }

    fn paste_text(&mut self, text: &str, clipboard_hold_ms: u64) -> Result<()> {
        input_actions::paste_text(text, &self.window, clipboard_hold_ms)
    }

    fn input_text(&mut self, text: &str, input_settle_ms: u64) -> Result<()> {
        window::ensure_foreground(&self.window)?;
        let mut enigo = Enigo::new(&Settings::default()).context("create enigo")?;
        enigo.text(text).context("input message text")?;
        sleep(Duration::from_millis(input_settle_ms));
        Ok(())
    }

    fn hold_key(&mut self, key: Key, duration: Duration, running: Arc<AtomicBool>) -> Result<()> {
        input_actions::hold_key(key, duration, &self.window, || {
            running.load(Ordering::SeqCst)
        })
    }

    fn ensure_window(&mut self) -> Result<()> {
        window::GameWindow::find(&self.window).map(|_| ())
    }

    fn close_window(&mut self) -> Result<()> {
        window::close_game(&self.window)
    }
}

fn execute_chat_batch(
    device: &mut dyn UiDevice,
    request: ChatBatchRequest,
) -> ChatBatchSendOutcome {
    if request.messages.is_empty() {
        return ChatBatchSendOutcome::complete(0);
    }
    if let Err(error) = device.ensure_foreground() {
        return ChatBatchSendOutcome::failed(0, error);
    }

    match request.target {
        ChatBatchTarget::Current => send_chat_messages(device, &request, request.click_ms),
        ChatBatchTarget::Primary { restore_after_task } => {
            if let Err(error) = device.press_key(Key::Return) {
                return ChatBatchSendOutcome::failed(0, error);
            }
            sleep(Duration::from_millis(request.open_chat_ms));
            if let Err(error) = select_current_hall(device, &request) {
                return ChatBatchSendOutcome::failed(0, error);
            }
            let outcome = send_chat_messages(device, &request, request.open_chat_ms);
            if !primary_chat_should_close_directly(restore_after_task) {
                return outcome;
            }
            let ChatBatchSendOutcome { sent, status } = outcome;
            match status {
                ChatBatchSendStatus::Complete => match close_chat(device, request.click_ms) {
                    Ok(()) => ChatBatchSendOutcome::complete(sent),
                    Err(error) => ChatBatchSendOutcome::failed(sent, error),
                },
                ChatBatchSendStatus::Failed(error) => {
                    if let Err(close_error) = close_chat(device, request.click_ms) {
                        log::error!("批量回复失败后关闭聊天界面也失败: {close_error:#}");
                    }
                    ChatBatchSendOutcome::failed(sent, error)
                }
            }
        }
    }
}

fn select_current_hall(device: &mut dyn UiDevice, request: &ChatBatchRequest) -> Result<()> {
    for attempt in 0..=3 {
        let frame = normalized_capture(device, &request.canvas)?;
        if let Some(hit) = best_template_hit(
            &frame,
            Some(request.secondary_hall_search_region),
            &request.secondary_hall_template,
            request.template_threshold,
        )? {
            device.click_point(hit.center().x, hit.center().y)?;
            sleep(Duration::from_millis(request.click_ms));
            return Ok(());
        }
        if attempt == 3 {
            break;
        }
        let point = request.friend_list_region.center();
        device.scroll_point(point.x, point.y, -8)?;
        sleep(Duration::from_millis(request.click_ms));
    }
    Err(anyhow!("发送前未找到当前大厅模板"))
}

fn normalized_capture(device: &mut dyn UiDevice, canvas: &Canvas) -> Result<DynamicImage> {
    let image = device.capture()?;
    if canvas.resize && (image.width() != canvas.width || image.height() != canvas.height) {
        Ok(image.resize_exact(canvas.width, canvas.height, FilterType::Triangle))
    } else {
        Ok(image)
    }
}

fn send_chat_messages(
    device: &mut dyn UiDevice,
    request: &ChatBatchRequest,
    after_click_ms: u64,
) -> ChatBatchSendOutcome {
    send_messages(&request.messages, request.delay_ms, |message| {
        device.click_point(request.chat_click.x, request.chat_click.y)?;
        sleep(Duration::from_millis(after_click_ms));
        if let Err(error) = device.paste_text(message, request.text_ms) {
            log::error!("粘贴输入失败，回退到文字输入: {error:#}");
            device.input_text(message, request.text_ms)?;
        }
        device.press_key(Key::Return)?;
        sleep(Duration::from_millis(request.send_ms));
        Ok(())
    })
}

fn close_chat(device: &mut dyn UiDevice, click_ms: u64) -> Result<()> {
    device.ensure_foreground()?;
    device.press_key(Key::Escape)?;
    sleep(Duration::from_millis(click_ms));
    Ok(())
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

            fn send_chat_batch(&self, request: ChatBatchRequest) -> ChatBatchSendOutcome {
                ChatBatchSendOutcome::complete(request.messages.len())
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
