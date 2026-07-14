use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use enigo::{Axis, Direction, Enigo, Key, Keyboard, Settings};
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

    fn send_chat_batch(&self, request: ChatBatchRequest) -> ChatBatchSendOutcome {
        let _guard = match self.operation_lock.lock() {
            Ok(guard) => guard,
            Err(_) => {
                return ChatBatchSendOutcome::failed(
                    0,
                    anyhow!("game UI operation mutex poisoned"),
                );
            }
        };
        execute_direct_chat_batch(&self.window, request)
    }
}

fn execute_direct_chat_batch(
    window_config: &WindowConfig,
    request: ChatBatchRequest,
) -> ChatBatchSendOutcome {
    if request.messages.is_empty() {
        return ChatBatchSendOutcome::complete(0);
    }
    let mut enigo = match Enigo::new(&Settings::default()).context("create enigo") {
        Ok(enigo) => enigo,
        Err(error) => return ChatBatchSendOutcome::failed(0, error),
    };
    let mut window = match window::GameWindow::find(window_config) {
        Ok(window) => window,
        Err(error) => return ChatBatchSendOutcome::failed(0, error),
    };
    if let Err(error) = window.ensure_foreground() {
        return ChatBatchSendOutcome::failed(0, error);
    }

    match request.target {
        ChatBatchTarget::Current => send_chat_messages(
            &mut window,
            &mut enigo,
            window_config,
            &request,
            request.click_ms,
        ),
        ChatBatchTarget::Primary { restore_after_task } => {
            if let Err(error) = enigo
                .key(Key::Return, Direction::Click)
                .context("open chat")
            {
                return ChatBatchSendOutcome::failed(0, error);
            }
            sleep(Duration::from_millis(request.open_chat_ms));
            if let Err(error) = select_current_hall(&mut window, &mut enigo, &request) {
                return ChatBatchSendOutcome::failed(0, error);
            }
            let outcome = send_chat_messages(
                &mut window,
                &mut enigo,
                window_config,
                &request,
                request.open_chat_ms,
            );
            if !primary_chat_should_close_directly(restore_after_task) {
                return outcome;
            }
            let ChatBatchSendOutcome { sent, status } = outcome;
            match status {
                ChatBatchSendStatus::Complete => {
                    match close_chat(&window, &mut enigo, request.click_ms) {
                        Ok(()) => ChatBatchSendOutcome::complete(sent),
                        Err(error) => ChatBatchSendOutcome::failed(sent, error),
                    }
                }
                ChatBatchSendStatus::Failed(error) => {
                    if let Err(close_error) = close_chat(&window, &mut enigo, request.click_ms) {
                        log::error!("批量回复失败后关闭聊天界面也失败: {close_error:#}");
                    }
                    ChatBatchSendOutcome::failed(sent, error)
                }
            }
        }
    }
}

fn select_current_hall(
    window: &mut window::GameWindow,
    enigo: &mut Enigo,
    request: &ChatBatchRequest,
) -> Result<()> {
    for attempt in 0..=3 {
        let frame = normalized_capture(window, &request.canvas)?;
        if let Some(hit) = best_template_hit(
            &frame,
            Some(request.secondary_hall_search_region),
            &request.secondary_hall_template,
            request.template_threshold,
        )? {
            window.click(enigo, PointConfig::new(hit.center().x, hit.center().y))?;
            sleep(Duration::from_millis(request.click_ms));
            return Ok(());
        }
        if attempt == 3 {
            break;
        }
        let point = request.friend_list_region.center();
        window.scroll(
            enigo,
            PointConfig::new(point.x, point.y),
            -8,
            Axis::Vertical,
        )?;
        sleep(Duration::from_millis(request.click_ms));
    }
    Err(anyhow!("发送前未找到当前大厅模板"))
}

fn normalized_capture(window: &window::GameWindow, canvas: &Canvas) -> Result<DynamicImage> {
    let image = window.capture()?;
    if canvas.resize && (image.width() != canvas.width || image.height() != canvas.height) {
        Ok(image.resize_exact(canvas.width, canvas.height, FilterType::Triangle))
    } else {
        Ok(image)
    }
}

fn send_chat_messages(
    window: &mut window::GameWindow,
    enigo: &mut Enigo,
    window_config: &WindowConfig,
    request: &ChatBatchRequest,
    after_click_ms: u64,
) -> ChatBatchSendOutcome {
    send_messages(&request.messages, request.delay_ms, |message| {
        window.click(enigo, request.chat_click)?;
        sleep(Duration::from_millis(after_click_ms));
        if let Err(error) = input_actions::paste_text(message, window_config, request.text_ms) {
            log::error!("粘贴输入失败，回退到文字输入: {error:#}");
            enigo.text(message).context("input message text")?;
            sleep(Duration::from_millis(request.text_ms));
        }
        enigo
            .key(Key::Return, Direction::Click)
            .context("send message")?;
        sleep(Duration::from_millis(request.send_ms));
        Ok(())
    })
}

fn close_chat(window: &window::GameWindow, enigo: &mut Enigo, click_ms: u64) -> Result<()> {
    window.ensure_foreground()?;
    enigo
        .key(Key::Escape, Direction::Click)
        .context("close chat")?;
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
}
