//! 同步沙盒执行器。仅持有注入设备；模拟实现绝不访问真实输入设备。
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use enigo::Button;
use image::{DynamicImage, GenericImageView, GrayImage};
use serde::{Deserialize, Serialize};

use crate::debug_sandbox::{
    AtomicCommand, SandboxCommandResult, SandboxRun, SandboxStatus, SandboxTask,
};
use crate::debug_sandbox_store::DebugSandboxStore;
use crate::runtime::ui::{
    UiRoutine, UiRoutineContext, UiRuntimeHandle, UiStateKind, UiStateObservation,
};
use crate::ui::template::{GrayMatcher, best_gray_match};

/// execute 必须在原子动作完成后返回。Capture 由 runner 调用 capture 实现。
/// 真实设备实现应在同一 UI 事务中串行化动作与 capture，并自行执行输入授权。
fn parse_button(value: &str) -> Result<Button, String> {
    match value {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        _ => Err(format!("不支持的鼠标按钮: {value}")),
    }
}

pub(crate) type PlaybackQueueLengthReader = Arc<dyn Fn() -> Result<usize, String> + Send + Sync>;
pub(crate) type PlaybackQueueClearer = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;
pub(crate) type TaskQueueCanceller = Arc<dyn Fn(u64) -> Result<(), String> + Send + Sync>;

/// 播放控制动作。沙盒只认识这个枚举，不认识应用内部的 PlaybackCommand，
/// 因此播放能力的演进不会把应用类型泄漏进协议层。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaybackAction {
    Play,
    Pause,
    SkipNext,
    SkipPrevious,
    SetVolume { volume: u8 },
}

impl PlaybackAction {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Play => "playback_play",
            Self::Pause => "playback_pause",
            Self::SkipNext => "playback_skip_next",
            Self::SkipPrevious => "playback_skip_previous",
            Self::SetVolume { .. } => "playback_set_volume",
        }
    }
}

/// 播放控制边界。应用侧把它映射到自己的播放命令入队路径；
/// "入队成功"即该原子步骤成功，实际播放效果由断言引擎另作验证，
/// 不把"已下发"当成"已生效"。
pub(crate) type PlaybackController =
    Arc<dyn Fn(PlaybackAction) -> Result<(), String> + Send + Sync>;

/// 一次点歌请求的**最小**载荷：关键词、音源、是否偏好伴奏。
/// 去重、排队、点歌人归属等业务规则全部留在应用侧，沙盒不复制也不绕过。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SongRequest {
    pub keyword: String,
    pub source: String,
    pub prefer_accompaniment: bool,
}

pub(crate) type SongRequester = Arc<dyn Fn(SongRequest) -> Result<(), String> + Send + Sync>;

/// 一次聊天发送的**最小**载荷：正文与前缀。
/// 入队、去重、任务归属等业务规则全部留在应用侧，沙盒不复制也不绕过。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChatSendRequest {
    pub text: String,
    pub prefix: String,
}

pub(crate) type ChatSender = Arc<dyn Fn(ChatSendRequest) -> Result<(), String> + Send + Sync>;

/// 一次游戏启动请求的**最小**载荷：只说明"启动什么"。
/// 具体流程、重试与界面识别全部留在应用侧，沙盒不复制也不绕过。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StartupRequest {
    pub kind: String,
}

pub(crate) type StartupLauncher = Arc<dyn Fn(StartupRequest) -> Result<(), String> + Send + Sync>;

impl AtomicCommand {
    /// 把业务原子命令映射成播放控制动作；非播放命令返回 None。
    pub(crate) fn playback_action(&self) -> Option<PlaybackAction> {
        match self {
            Self::PlaybackPlay => Some(PlaybackAction::Play),
            Self::PlaybackPause => Some(PlaybackAction::Pause),
            Self::PlaybackSkipNext => Some(PlaybackAction::SkipNext),
            Self::PlaybackSkipPrevious => Some(PlaybackAction::SkipPrevious),
            Self::PlaybackSetVolume { volume } => {
                Some(PlaybackAction::SetVolume { volume: *volume })
            }
            _ => None,
        }
    }

    /// 把点歌命令映射成载荷；非点歌命令返回 None。
    pub(crate) fn song_request_payload(&self) -> Option<SongRequest> {
        match self {
            Self::RequestSong {
                keyword,
                source,
                prefer_accompaniment,
            } => Some(SongRequest {
                keyword: keyword.clone(),
                source: source.clone(),
                prefer_accompaniment: *prefer_accompaniment,
            }),
            _ => None,
        }
    }

    /// 把游戏启动命令映射成载荷；非启动命令返回 None。
    pub(crate) fn startup_request_payload(&self) -> Option<StartupRequest> {
        match self {
            Self::StartupTask { kind } => Some(StartupRequest { kind: kind.clone() }),
            _ => None,
        }
    }

    /// 把聊天发送命令映射成载荷；非聊天命令返回 None。
    pub(crate) fn chat_send_payload(&self) -> Option<ChatSendRequest> {
        match self {
            Self::ChatSend { text, prefix } => Some(ChatSendRequest {
                text: text.clone(),
                prefix: prefix.clone(),
            }),
            _ => None,
        }
    }
}

/// 沙盒可见的 UI 状态种类。
///
/// 刻意只暴露判别值：UI 分类内部的稳定性窗口、证据矩形等都不进入运行器，
/// 运行器与 UI 分类之间只通过这个窄接口耦合，UI 侧实现可以独立替换。
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SandboxUiStateKind {
    Primary,
    Secondary,
    MainMenu,
    GameGate,
    Overworld,
    Loading,
    Unknown,
}

impl SandboxUiStateKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::MainMenu => "main_menu",
            Self::GameGate => "game_gate",
            Self::Overworld => "overworld",
            Self::Loading => "loading",
            Self::Unknown => "unknown",
        }
    }
}

impl From<UiStateKind> for SandboxUiStateKind {
    fn from(kind: UiStateKind) -> Self {
        match kind {
            UiStateKind::Primary => Self::Primary,
            UiStateKind::Secondary => Self::Secondary,
            UiStateKind::MainMenu => Self::MainMenu,
            UiStateKind::GameGate => Self::GameGate,
            UiStateKind::Overworld => Self::Overworld,
            UiStateKind::Loading => Self::Loading,
            UiStateKind::Unknown => Self::Unknown,
        }
    }
}

/// 只读 UI 状态读数：最新分类结果，以及该状态在稳定性窗口上是否已经稳定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SandboxUiStateReading {
    pub kind: SandboxUiStateKind,
    pub stable: bool,
    pub frame_id: u64,
}

pub(crate) trait SandboxExecutor: Send {
    fn set_cancellation_signal(&mut self, _signal: Arc<AtomicBool>) {}
    fn recover(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn supports_window_foreground(&self) -> bool {
        false
    }
    fn window_foreground(&mut self) -> Result<bool, String> {
        Err("设备不支持窗口前台状态查询".into())
    }
    fn supports_window_available(&self) -> bool {
        false
    }
    fn window_available(&mut self) -> Result<bool, String> {
        Err("设备不支持目标窗口可用状态查询".into())
    }
    /// 是否支持只读 UI 状态查询。返回 Ok(None) 表示尚无分类观测。
    fn supports_ui_state(&self) -> bool {
        false
    }
    fn ui_state(&mut self) -> Result<Option<SandboxUiStateReading>, String> {
        Err("设备不支持 UI 状态查询".into())
    }
    fn supports_playback_queue_length(&self) -> bool {
        false
    }
    fn playback_queue_length(&mut self) -> Result<usize, String> {
        Err("设备不支持播放队列状态查询".into())
    }
    fn supports_playback_queue_clear(&self) -> bool {
        false
    }
    fn playback_queue_clear(&mut self) -> Result<(), String> {
        Err("设备不支持播放队列清空".into())
    }
    fn supports_playback_queue_remove_front(&self) -> bool {
        false
    }
    fn playback_queue_remove_front(&mut self) -> Result<(), String> {
        Err("设备不支持移除播放队列首项".into())
    }
    fn supports_task_queue_cancel(&self) -> bool {
        false
    }
    fn task_queue_cancel(&mut self, _task_id: u64) -> Result<(), String> {
        Err("设备不支持任务队列取消".into())
    }
    fn supports_playback_control(&self) -> bool {
        false
    }
    fn playback_control(&mut self, action: PlaybackAction) -> Result<(), String> {
        Err(format!("设备不支持播放控制命令: {}", action.name()))
    }
    fn supports_song_request(&self) -> bool {
        false
    }
    fn song_request(&mut self, _request: SongRequest) -> Result<(), String> {
        Err("设备不支持点歌入队".into())
    }
    fn supports_chat_send(&self) -> bool {
        false
    }
    fn chat_send(&mut self, _request: ChatSendRequest) -> Result<(), String> {
        Err("设备不支持聊天发送".into())
    }
    fn supports_startup(&self) -> bool {
        false
    }
    fn startup(&mut self, _request: StartupRequest) -> Result<(), String> {
        Err("设备不支持游戏启动".into())
    }
    fn supports_clipboard(&self) -> bool {
        false
    }
    fn clipboard_text(&mut self) -> Result<Option<String>, String> {
        Err("设备不支持剪贴板读取".into())
    }
    fn execute(&mut self, command: &AtomicCommand) -> Result<(), String>;
    fn capture(&mut self) -> Result<DynamicImage, String>;
    fn execute_with_frame(
        &mut self,
        command: &AtomicCommand,
    ) -> Result<Option<DynamicImage>, String> {
        self.execute(command)?;
        Ok(None)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum SandboxAssertion {
    WindowForeground {
        expected: bool,
    },
    WindowAvailable {
        expected: bool,
    },
    PlaybackQueueLength {
        expected: usize,
    },
    /// 只读 UI 状态断言：不截图，直接查询 UI 分类结果。
    UiState {
        expected: SandboxUiStateKind,
        /// 是否要求该状态已经稳定（否则只要最新一帧的分类相符即可）。
        #[serde(default)]
        stable: bool,
    },
    /// 只读剪贴板断言：不截图，直接读取剪贴板文本。
    /// 这是"复制"类操作唯一可验证的方式——paste 命令只写不读，此前无法确认复制是否真的生效。
    ClipboardContains {
        text: String,
    },
    Dimensions {
        width: u32,
        height: u32,
    },
    Pixel {
        x: u32,
        y: u32,
        rgba: [u8; 4],
        tolerance: u8,
    },
    FrameDifference {
        #[serde(default)]
        min_changed_pixels: u64,
        max_changed_pixels: u64,
    },
    OcrContains {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        text: String,
    },
    Template {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        template_width: u32,
        template_height: u32,
        grayscale: Vec<u8>,
        min_score: f32,
    },
}

/// 错误信息里截断长文本，避免把整块剪贴板内容塞进运行结果。
fn truncate_for_message(text: &str) -> String {
    const MAX: usize = 64;
    if text.chars().count() <= MAX {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(MAX).collect();
    truncated.push('…');
    truncated
}

impl SandboxAssertion {
    fn validate(&self) -> Result<(), String> {
        if let Self::OcrContains {
            width,
            height,
            text,
            ..
        } = self
        {
            if *width == 0
                || *height == 0
                || u64::from(*width) * u64::from(*height) > 1_048_576
                || text.is_empty()
                || text.len() > 128
            {
                return Err("OCR 断言区域或目标文本无效".into());
            }
        }
        if let Self::FrameDifference {
            min_changed_pixels,
            max_changed_pixels,
        } = self
        {
            if min_changed_pixels > max_changed_pixels {
                return Err("前后帧变化像素下限不能大于上限".into());
            }
        }
        if let Self::Template {
            width,
            height,
            template_width,
            template_height,
            grayscale,
            min_score,
            ..
        } = self
        {
            if *width == 0
                || *height == 0
                || *width > 128
                || *height > 128
                || *template_width == 0
                || *template_height == 0
                || *template_width > 32
                || *template_height > 32
                || template_width > width
                || template_height > height
                || grayscale.len() != (*template_width * *template_height) as usize
                || !min_score.is_finite()
                || !(0.0..=1.0).contains(min_score)
            {
                return Err("模板断言区域、模板像素或阈值无效".into());
            }
        }
        if let Self::ClipboardContains { text } = self {
            if text.is_empty() || text.chars().count() > 256 {
                return Err("剪贴板断言目标文本为空或过长".into());
            }
        }
        Ok(())
    }

    pub(crate) fn verify(&self, image: &DynamicImage) -> Result<(), String> {
        if let Self::FrameDifference { .. } = self {
            return Err("前后帧断言需要上一帧".into());
        }
        match *self {
            Self::WindowForeground { .. }
            | Self::WindowAvailable { .. }
            | Self::UiState { .. }
            | Self::ClipboardContains { .. } => {
                return Err("设备状态断言需要设备状态查询".into());
            }
            Self::Dimensions { width, height } => {
                if image.dimensions() != (width, height) {
                    return Err(format!(
                        "图像尺寸不匹配: expected={width}x{height}, actual={}x{}",
                        image.width(),
                        image.height()
                    ));
                }
            }
            Self::Pixel {
                x,
                y,
                rgba,
                tolerance,
            } => {
                if x >= image.width() || y >= image.height() {
                    return Err(format!("像素断言越界: ({x},{y})"));
                }
                let actual = image.get_pixel(x, y).0;
                if actual
                    .iter()
                    .zip(rgba)
                    .any(|(a, b)| a.abs_diff(b) > tolerance)
                {
                    return Err(format!(
                        "像素不匹配: ({x},{y}) expected={rgba:?}, actual={actual:?}, tolerance={tolerance}"
                    ));
                }
            }
            Self::Template {
                x,
                y,
                width,
                height,
                template_width,
                template_height,
                ref grayscale,
                min_score,
            } => {
                self.validate()?;
                if x.checked_add(width).is_none_or(|end| end > image.width())
                    || y.checked_add(height).is_none_or(|end| end > image.height())
                {
                    return Err("模板断言区域、模板像素或阈值无效".into());
                }
                // 与 UI 侧共用同一个 CPU 匹配核心：没有再建 GPU 设备的固定开销，
                // 分数定义保持一致（1 - MAD/255）。
                let search = image.crop_imm(x, y, width, height).to_luma8();
                let needle =
                    GrayImage::from_raw(template_width, template_height, grayscale.clone())
                        .ok_or("模板断言区域、模板像素或阈值无效")?;
                let matcher = GrayMatcher::opaque(&needle);
                let found =
                    best_gray_match(&search, &matcher).ok_or("模板断言区域、模板像素或阈值无效")?;
                let score = 1.0 - found.sad as f32 / matcher.alpha_sum() as f32;
                if score < min_score {
                    return Err(format!("模板未命中: best={score}, min={min_score}"));
                }
            }
            Self::FrameDifference { .. } | Self::OcrContains { .. } => {
                return Err("断言需要额外的图像识别上下文".into());
            }
            Self::PlaybackQueueLength { .. } => {
                return Err("断言需要额外的状态查询上下文".into());
            }
        }
        Ok(())
    }

    pub(crate) fn verify_against(
        &self,
        previous: Option<&DynamicImage>,
        current: &DynamicImage,
    ) -> Result<(), String> {
        match self {
            Self::FrameDifference {
                min_changed_pixels,
                max_changed_pixels,
            } => {
                let previous = previous.ok_or("前后帧断言缺少上一帧")?;
                if previous.dimensions() != current.dimensions() {
                    return Err("前后帧尺寸不一致".into());
                }
                let changed = previous
                    .pixels()
                    .zip(current.pixels())
                    .filter(|(before, after)| before.2.0 != after.2.0)
                    .count() as u64;
                if changed < *min_changed_pixels {
                    return Err(format!(
                        "前后帧变化像素不足: changed={changed}, min={min_changed_pixels}"
                    ));
                }
                if changed > *max_changed_pixels {
                    return Err(format!(
                        "前后帧变化像素过多: changed={changed}, max={max_changed_pixels}"
                    ));
                }
                Ok(())
            }
            _ => self.verify(current),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StepAssertion {
    pub index: usize,
    pub assertion: SandboxAssertion,
}

#[derive(Clone, Debug)]
pub(crate) struct StepEvidence {
    pub command: AtomicCommand,
    pub result: SandboxCommandResult,
    pub image: Option<Arc<DynamicImage>>,
    pub assertions: Vec<SandboxAssertion>,
}

struct Session {
    assertions: Vec<StepAssertion>,
    evidence: Vec<StepEvidence>,
}

/// 一个运行只能由创建它的 runner 推进，&mut self 保证本实例不重复执行同一步。
/// 取消请求可通过共享 store 提交，在原子命令边界生效，不回滚已发生的输入。
pub(crate) type OcrTextRecognizer =
    Arc<dyn Fn(&DynamicImage) -> Result<String, String> + Send + Sync>;

pub(crate) struct DebugSandboxRunner<E: SandboxExecutor> {
    store: DebugSandboxStore,
    ocr: Option<OcrTextRecognizer>,
    executor: E,
    sessions: HashMap<String, Session>,
    frame_sequence: u64,
}

impl<E: SandboxExecutor> DebugSandboxRunner<E> {
    pub(crate) fn new(store: DebugSandboxStore, executor: E) -> Self {
        Self {
            store,
            ocr: None,
            executor,
            sessions: HashMap::new(),
            frame_sequence: 0,
        }
    }

    pub(crate) fn with_ocr(mut self, recognizer: OcrTextRecognizer) -> Self {
        self.ocr = Some(recognizer);
        self
    }

    pub(crate) fn start(
        &mut self,
        task_id: &str,
        expected_version: Option<u32>,
        assertions: Vec<StepAssertion>,
    ) -> Result<SandboxRun, String> {
        let task = self
            .store
            .task(task_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("沙盒任务不存在: {task_id}"))?;
        self.validate_assertions(&task, &assertions)?;
        // 固定校验时版本，防止保存新版本与 start_run 之间发生竞态。
        if expected_version.is_some_and(|v| v != task.version) {
            return Err("任务版本冲突".into());
        }
        let run = self
            .store
            .start_run(task_id, Some(task.version))
            .map_err(|e| e.to_string())?;
        self.attach_session(run, assertions)
    }

    pub(crate) fn start_snapshot(
        &mut self,
        task: SandboxTask,
        assertions: Vec<StepAssertion>,
    ) -> Result<SandboxRun, String> {
        self.validate_assertions(&task, &assertions)?;
        let run = self
            .store
            .start_run_snapshot(task)
            .map_err(|e| e.to_string())?;
        self.attach_session(run, assertions)
    }

    fn validate_assertions(
        &self,
        task: &SandboxTask,
        assertions: &[StepAssertion],
    ) -> Result<(), String> {
        if self.ocr.is_none()
            && assertions
                .iter()
                .any(|a| matches!(a.assertion, SandboxAssertion::OcrContains { .. }))
        {
            return Err("OCR 断言需要已配置的 OCR runtime".into());
        }
        if assertions.len() > 4096 || assertions.iter().any(|a| a.index >= task.commands.len()) {
            return Err("断言数量或命令序号超出范围".into());
        }
        if task
            .commands
            .iter()
            .any(|c| matches!(c, AtomicCommand::PlaybackQueueClear))
            && !self.executor.supports_playback_queue_clear()
        {
            return Err("当前执行器不支持播放队列清空".into());
        }
        if task
            .commands
            .iter()
            .any(|c| matches!(c, AtomicCommand::PlaybackQueueRemoveFront))
            && !self.executor.supports_playback_queue_remove_front()
        {
            return Err("当前执行器不支持移除播放队列首项".into());
        }
        if task
            .commands
            .iter()
            .any(|c| matches!(c, AtomicCommand::TaskQueueCancel { task_id: 0 }))
        {
            return Err("任务队列取消的 task_id 必须大于 0".into());
        }
        if task
            .commands
            .iter()
            .any(|c| matches!(c, AtomicCommand::TaskQueueCancel { .. }))
            && !self.executor.supports_task_queue_cancel()
        {
            return Err("当前执行器不支持任务队列取消".into());
        }
        if task
            .commands
            .iter()
            .any(|command| command.playback_action().is_some())
            && !self.executor.supports_playback_control()
        {
            return Err("当前执行器不支持播放控制命令".into());
        }
        if task
            .commands
            .iter()
            .any(|command| command.song_request_payload().is_some())
            && !self.executor.supports_song_request()
        {
            return Err("当前执行器不支持点歌入队".into());
        }
        if task
            .commands
            .iter()
            .any(|command| command.chat_send_payload().is_some())
            && !self.executor.supports_chat_send()
        {
            return Err("当前执行器不支持聊天发送".into());
        }
        if task
            .commands
            .iter()
            .any(|command| command.startup_request_payload().is_some())
            && !self.executor.supports_startup()
        {
            return Err("当前执行器不支持游戏启动".into());
        }
        if assertions
            .iter()
            .any(|a| matches!(a.assertion, SandboxAssertion::PlaybackQueueLength { .. }))
            && !self.executor.supports_playback_queue_length()
        {
            return Err("当前执行器不支持播放队列状态断言".into());
        }
        if !self.executor.supports_window_foreground()
            && assertions
                .iter()
                .any(|a| matches!(a.assertion, SandboxAssertion::WindowForeground { .. }))
        {
            return Err("当前设备不支持窗口前台断言".into());
        }
        if !self.executor.supports_window_available()
            && assertions
                .iter()
                .any(|a| matches!(a.assertion, SandboxAssertion::WindowAvailable { .. }))
        {
            return Err("当前设备不支持目标窗口可用状态断言".into());
        }
        if !self.executor.supports_ui_state()
            && assertions
                .iter()
                .any(|a| matches!(a.assertion, SandboxAssertion::UiState { .. }))
        {
            return Err("当前执行器不支持 UI 状态断言".into());
        }
        if !self.executor.supports_clipboard()
            && assertions
                .iter()
                .any(|a| matches!(a.assertion, SandboxAssertion::ClipboardContains { .. }))
        {
            return Err("当前执行器不支持剪贴板断言".into());
        }
        if assertions
            .iter()
            .filter(|a| matches!(a.assertion, SandboxAssertion::Template { .. }))
            .count()
            > 4
        {
            return Err("每次运行最多允许4个模板断言".into());
        }
        for assertion in assertions {
            if matches!(task.commands[assertion.index], AtomicCommand::CloseWindow)
                && !matches!(
                    assertion.assertion,
                    SandboxAssertion::WindowAvailable { .. }
                        | SandboxAssertion::WindowForeground { .. }
                        | SandboxAssertion::UiState { .. }
                        | SandboxAssertion::ClipboardContains { .. }
                )
            {
                return Err("关闭窗口步骤不支持截图断言".into());
            }
            assertion.assertion.validate()?;
        }
        Ok(())
    }

    fn attach_session(
        &mut self,
        run: SandboxRun,
        assertions: Vec<StepAssertion>,
    ) -> Result<SandboxRun, String> {
        self.executor.set_cancellation_signal(
            self.store
                .cancellation_signal(&run.run_id)
                .map_err(|e| e.to_string())?,
        );
        self.sessions.insert(
            run.run_id.clone(),
            Session {
                assertions,
                evidence: Vec::new(),
            },
        );
        Ok(run)
    }

    pub(crate) fn evidence(&self, run_id: &str) -> Result<&[StepEvidence], String> {
        self.sessions
            .get(run_id)
            .map(|s| s.evidence.as_slice())
            .ok_or_else(|| "运行不属于此执行器".to_string())
    }

    pub(crate) fn cancel(&self, run_id: &str) -> Result<SandboxRun, String> {
        self.evidence(run_id)?;
        self.store.cancel_run(run_id).map_err(|e| e.to_string())
    }

    pub(crate) fn step(&mut self, run_id: &str) -> Result<SandboxRun, String> {
        self.evidence(run_id)?;
        let run = self
            .store
            .run(run_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "运行不存在".to_string())?;
        if run.status != SandboxStatus::Running {
            return Ok(run);
        }
        if self
            .store
            .is_cancel_requested(run_id)
            .map_err(|e| e.to_string())?
        {
            self.executor.recover()?;
            return self
                .store
                .finish_run(run_id, SandboxStatus::Cancelled)
                .map_err(|e| e.to_string());
        }
        let task = self.store.run_task(run_id).map_err(|e| e.to_string())?;
        let index = run.results.len();
        let command = task.commands.get(index).ok_or("运行命令序号越界")?.clone();
        let assertions: Vec<_> = self.sessions[run_id]
            .assertions
            .iter()
            .filter(|a| a.index == index)
            .map(|a| a.assertion.clone())
            .collect();
        // 按任务策略采集证据；显式 Capture 或截图断言仍强制采集。
        let should_capture = command.playback_action().is_none()
            && command.song_request_payload().is_none()
            && command.chat_send_payload().is_none()
            && command.startup_request_payload().is_none()
            && !matches!(
                command,
                AtomicCommand::CloseWindow
                    | AtomicCommand::PlaybackQueueClear
                    | AtomicCommand::PlaybackQueueRemoveFront
                    | AtomicCommand::TaskQueueCancel { .. }
            )
            && (task.capture_after_each
                || matches!(command, AtomicCommand::Capture)
                || assertions.iter().any(|a| {
                    !matches!(
                        a,
                        SandboxAssertion::WindowForeground { .. }
                            | SandboxAssertion::WindowAvailable { .. }
                            | SandboxAssertion::PlaybackQueueLength { .. }
                            | SandboxAssertion::UiState { .. }
                            | SandboxAssertion::ClipboardContains { .. }
                    )
                }));
        let business_clear = matches!(command, AtomicCommand::PlaybackQueueClear);
        let business_remove_front = matches!(command, AtomicCommand::PlaybackQueueRemoveFront);
        let business_task_cancel = matches!(command, AtomicCommand::TaskQueueCancel { .. });
        let (mut error, captured) = if let Some(action) = command.playback_action() {
            (self.executor.playback_control(action).err(), None)
        } else if let Some(request) = command.song_request_payload() {
            (self.executor.song_request(request).err(), None)
        } else if let Some(request) = command.chat_send_payload() {
            (self.executor.chat_send(request).err(), None)
        } else if let Some(request) = command.startup_request_payload() {
            (self.executor.startup(request).err(), None)
        } else if business_clear {
            (self.executor.playback_queue_clear().err(), None)
        } else if business_remove_front {
            (self.executor.playback_queue_remove_front().err(), None)
        } else if business_task_cancel {
            let AtomicCommand::TaskQueueCancel { task_id } = command else {
                unreachable!()
            };
            (self.executor.task_queue_cancel(task_id).err(), None)
        } else if matches!(command, AtomicCommand::Capture) {
            (None, None)
        } else if should_capture {
            match self.executor.execute_with_frame(&command) {
                Ok(image) => (None, image),
                Err(reason) => (Some(reason), None),
            }
        } else {
            (self.executor.execute(&command).err(), None)
        };
        let mut sequence = None;
        let image = if let Some(image) = captured {
            self.frame_sequence += 1;
            sequence = Some(self.frame_sequence);
            Some(Arc::new(image))
        } else if should_capture
            && !self
                .store
                .is_cancel_requested(run_id)
                .map_err(|e| e.to_string())?
        {
            match self.executor.capture() {
                Ok(image) => {
                    self.frame_sequence += 1;
                    sequence = Some(self.frame_sequence);
                    Some(Arc::new(image))
                }
                Err(capture_error) => {
                    error = Some(match error {
                        Some(e) => format!("{e}; 截图失败: {capture_error}"),
                        None => format!("截图失败: {capture_error}"),
                    });
                    None
                }
            }
        } else {
            None
        };
        if error.is_none()
            && !self
                .store
                .is_cancel_requested(run_id)
                .map_err(|e| e.to_string())?
        {
            if let Some(image) = &image {
                let previous = self.sessions[run_id]
                    .evidence
                    .iter()
                    .rev()
                    .find_map(|evidence| evidence.image.as_deref());
                for assertion in &assertions {
                    if self
                        .store
                        .is_cancel_requested(run_id)
                        .map_err(|e| e.to_string())?
                    {
                        break;
                    }
                    let issue = (|| match assertion {
                        SandboxAssertion::WindowForeground { .. }
                        | SandboxAssertion::WindowAvailable { .. }
                        | SandboxAssertion::PlaybackQueueLength { .. }
                        | SandboxAssertion::UiState { .. }
                        | SandboxAssertion::ClipboardContains { .. } => None,
                        SandboxAssertion::OcrContains {
                            x,
                            y,
                            width,
                            height,
                            text,
                        } => {
                            if x.checked_add(*width).is_none_or(|end| end > image.width())
                                || y.checked_add(*height)
                                    .is_none_or(|end| end > image.height())
                            {
                                return Some("OCR 断言区域超出截图".into());
                            }
                            let crop = image.crop_imm(*x, *y, *width, *height);
                            self.ocr.as_ref().expect("OCR validated at start")(&crop)
                                .and_then(|recognized| {
                                    if recognized.contains(text.as_str()) {
                                        Ok(())
                                    } else {
                                        Err(format!("OCR 文本未命中: expected={text:?}"))
                                    }
                                })
                                .err()
                        }
                        _ => assertion.verify_against(previous, image).err(),
                    })();
                    if let Some(issue) = issue {
                        error = Some(issue);
                        break;
                    }
                }
            }
        }
        if error.is_none() {
            for assertion in &assertions {
                if self
                    .store
                    .is_cancel_requested(run_id)
                    .map_err(|e| e.to_string())?
                {
                    break;
                }
                let (expected, label, observed) = match assertion {
                    SandboxAssertion::PlaybackQueueLength { expected } => {
                        let actual = self.executor.playback_queue_length();
                        match actual {
                            Ok(actual) if actual == *expected => continue,
                            Ok(actual) => {
                                error = Some(format!(
                                    "播放队列长度不匹配: expected={expected}, actual={actual}"
                                ));
                                break;
                            }
                            Err(reason) => {
                                error = Some(format!("播放队列状态查询失败: {reason}"));
                                break;
                            }
                        }
                    }
                    SandboxAssertion::WindowForeground { expected } => {
                        (*expected, "窗口前台", self.executor.window_foreground())
                    }
                    SandboxAssertion::WindowAvailable { expected } => {
                        (*expected, "目标窗口可用", self.executor.window_available())
                    }
                    SandboxAssertion::UiState { expected, stable } => {
                        match self.executor.ui_state() {
                            Ok(Some(reading)) => {
                                if reading.kind != *expected {
                                    error = Some(format!(
                                        "UI 状态不匹配: expected={}, actual={} (frame {})",
                                        expected.label(),
                                        reading.kind.label(),
                                        reading.frame_id
                                    ));
                                    break;
                                }
                                if *stable && !reading.stable {
                                    error = Some(format!(
                                        "UI 状态 {} 尚未稳定 (frame {})",
                                        expected.label(),
                                        reading.frame_id
                                    ));
                                    break;
                                }
                                continue;
                            }
                            Ok(None) => {
                                error = Some("UI 状态查询失败: 尚无 UI 状态观测".into());
                                break;
                            }
                            Err(reason) => {
                                error = Some(format!("UI 状态查询失败: {reason}"));
                                break;
                            }
                        }
                    }
                    SandboxAssertion::ClipboardContains { text } => {
                        match self.executor.clipboard_text() {
                            Ok(Some(actual)) => {
                                if actual.contains(text.as_str()) {
                                    continue;
                                }
                                error = Some(format!(
                                    "剪贴板内容不匹配: expected 包含 {text:?}, actual={:?}",
                                    truncate_for_message(&actual)
                                ));
                                break;
                            }
                            Ok(None) => {
                                error = Some("剪贴板断言失败: 剪贴板为空".into());
                                break;
                            }
                            Err(reason) => {
                                error = Some(format!("剪贴板读取失败: {reason}"));
                                break;
                            }
                        }
                    }
                    _ => continue,
                };
                match observed {
                    Ok(actual) if actual == expected => {}
                    Ok(actual) => {
                        error = Some(format!(
                            "{label}状态不匹配: expected={expected}, actual={actual}"
                        ));
                        break;
                    }
                    Err(reason) => {
                        error = Some(format!("{label}状态查询失败: {reason}"));
                        break;
                    }
                }
            }
        }
        if let Some(original) = error.take() {
            error = Some(match self.executor.recover() {
                Ok(()) => original,
                Err(recovery) => format!("{original}; 设备恢复失败: {recovery}"),
            });
        }
        let result = SandboxCommandResult {
            index,
            ok: error.is_none(),
            status: if error.is_none() { "passed" } else { "failed" }.into(),
            error,
            frame_sequence: sequence,
        };
        self.sessions
            .get_mut(run_id)
            .expect("validated session")
            .evidence
            .push(StepEvidence {
                command,
                result: result.clone(),
                image,
                assertions,
            });
        let run = self
            .store
            .record_command_result(run_id, result)
            .map_err(|e| e.to_string())?;
        if run.status != SandboxStatus::Running {
            return Ok(run);
        }
        if self
            .store
            .is_cancel_requested(run_id)
            .map_err(|e| e.to_string())?
        {
            self.executor.recover()?;
            return self
                .store
                .finish_run(run_id, SandboxStatus::Cancelled)
                .map_err(|e| e.to_string());
        }
        if run.results.len() == task.commands.len() {
            return self
                .store
                .finish_run(run_id, SandboxStatus::Passed)
                .map_err(|e| e.to_string());
        }
        Ok(run)
    }

    pub(crate) fn run_all(&mut self, run_id: &str) -> Result<SandboxRun, String> {
        loop {
            let run = self.step(run_id)?;
            if run.status != SandboxStatus::Running {
                return Ok(run);
            }
        }
    }
}

struct LiveCommandRoutine(AtomicCommand, Arc<AtomicBool>, bool);
impl crate::runtime::ui::sealed::UiRoutineSealed for LiveCommandRoutine {}
impl UiRoutine for LiveCommandRoutine {
    type Output = anyhow::Result<Option<DynamicImage>>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        use crate::debug_sandbox::AtomicCommand::*;
        let closes_window = matches!(self.0, CloseWindow);
        if self.1.load(Ordering::Acquire) || context.is_cancelled() {
            context.device().recover().map_err(|error| {
                anyhow::anyhow!("live run cancelled; device recovery failed: {error:#}")
            })?;
            anyhow::bail!("live run cancelled");
        }
        match self.0 {
            Capture => {}
            EnsureWindow => context.device().ensure_window()?,
            EnsureForeground => context.device().ensure_foreground()?,
            EnsureReady { after_activate_ms } => {
                context.device().ensure_ready(after_activate_ms)?
            }
            Activate { after_activate_ms } => context.device().activate(after_activate_ms)?,
            Focus { after_activate_ms } => context.device().focus(after_activate_ms)?,
            InputText { text, settle_ms } => context.device().input_text(&text, settle_ms)?,
            CloseWindow => context.device().close_window()?,
            PressKey { key } => context
                .device()
                .press_key(crate::adapters::windows::parse_key(&key)?)?,
            KeyDown { key } => context
                .device()
                .key_down(crate::adapters::windows::parse_key(&key)?)?,
            KeyUp { key } => context
                .device()
                .key_up(crate::adapters::windows::parse_key(&key)?)?,
            MouseDown { button } => context
                .device()
                .mouse_down(parse_button(&button).map_err(anyhow::Error::msg)?)?,
            MouseUp { button } => context
                .device()
                .mouse_up(parse_button(&button).map_err(anyhow::Error::msg)?)?,
            Click { x, y } => context.device().click_point(x, y)?,
            Scroll { x, y, amount } => context.device().scroll_point(x, y, amount)?,
            Drag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => context.device().drag_point(from_x, from_y, to_x, to_y)?,
            Paste { text } => context.device().paste_text(&text, 500)?,
            PlaybackQueueClear
            | PlaybackQueueRemoveFront
            | PlaybackPlay
            | PlaybackPause
            | PlaybackSkipNext
            | PlaybackSkipPrevious
            | PlaybackSetVolume { .. }
            | RequestSong { .. }
            | ChatSend { .. }
            | StartupTask { .. } => {
                anyhow::bail!("业务命令不得提交到 UI runtime")
            }
            TaskQueueCancel { .. } => anyhow::bail!("业务命令不得提交到 UI runtime"),
            Wait { milliseconds } => {
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(milliseconds);
                loop {
                    if self.1.load(Ordering::Acquire) {
                        anyhow::bail!("live run cancelled");
                    }
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    context
                        .wait_cancellable(remaining.min(std::time::Duration::from_millis(25)))?;
                }
            }
        }
        if self.1.load(Ordering::Acquire) || context.is_cancelled() {
            context.device().recover().map_err(|error| {
                anyhow::anyhow!("live run cancelled; device recovery failed: {error:#}")
            })?;
            anyhow::bail!("live run cancelled");
        }
        if closes_window || !self.2 {
            return Ok(None);
        }
        Ok(Some(context.capture_frame()?.into_image()))
    }
}

pub(crate) struct LiveAvailabilityRoutine;
impl crate::runtime::ui::sealed::UiRoutineSealed for LiveAvailabilityRoutine {}
impl UiRoutine for LiveAvailabilityRoutine {
    type Output = anyhow::Result<bool>;
    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context.device().window_available()
    }
}

struct LiveForegroundRoutine;
impl crate::runtime::ui::sealed::UiRoutineSealed for LiveForegroundRoutine {}
impl UiRoutine for LiveForegroundRoutine {
    type Output = anyhow::Result<bool>;
    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context.device().window_foreground()
    }
}

struct LiveRecoveryRoutine;
impl crate::runtime::ui::sealed::UiRoutineSealed for LiveRecoveryRoutine {}
impl UiRoutine for LiveRecoveryRoutine {
    type Output = anyhow::Result<()>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context.device().recover()
    }
}

fn live_command_timeout(command: &AtomicCommand) -> std::time::Duration {
    use AtomicCommand::*;
    let wait_ms = match command {
        Wait { milliseconds } => *milliseconds,
        EnsureReady { after_activate_ms }
        | Activate { after_activate_ms }
        | Focus { after_activate_ms } => *after_activate_ms,
        InputText { settle_ms, .. } => *settle_ms,
        PlaybackQueueClear | TaskQueueCancel { .. } => 0,
        _ => 0,
    };
    std::time::Duration::from_secs(30).max(
        std::time::Duration::from_millis(wait_ms).saturating_add(std::time::Duration::from_secs(5)),
    )
}

struct LiveUiStateRoutine;
impl crate::runtime::ui::sealed::UiRoutineSealed for LiveUiStateRoutine {}
impl UiRoutine for LiveUiStateRoutine {
    type Output = anyhow::Result<Option<SandboxUiStateReading>>;
    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        let Some(observation) = context.latest_ui_state() else {
            // 尚无分类观测：如实返回 None，由断言层报失败，绝不伪造状态。
            return Ok(None);
        };
        match observation {
            UiStateObservation::Classified(state) => Ok(Some(SandboxUiStateReading {
                kind: state.classification().kind().into(),
                stable: state.stable_kind().is_some(),
                frame_id: state.frame_id(),
            })),
            UiStateObservation::Failed { reason, .. } => {
                anyhow::bail!("UI 状态分类失败: {reason}")
            }
        }
    }
}

/// 在 UI worker 上读取剪贴板：与 UI 操作共用同一条串行通道，避免与主循环争用。
struct LiveClipboardRoutine;
impl crate::runtime::ui::sealed::UiRoutineSealed for LiveClipboardRoutine {}
impl UiRoutine for LiveClipboardRoutine {
    type Output = anyhow::Result<Option<String>>;
    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context.device().clipboard_text()
    }
}

pub(crate) struct LiveExecutor {
    handle: UiRuntimeHandle,
    cancellation: Option<Arc<AtomicBool>>,
    playback_queue: Option<PlaybackQueueLengthReader>,
    playback_queue_clear: Option<PlaybackQueueClearer>,
    playback_queue_remove_front: Option<PlaybackQueueClearer>,
    task_queue_canceller: Option<TaskQueueCanceller>,
    playback_controller: Option<PlaybackController>,
    song_requester: Option<SongRequester>,
    chat_sender: Option<ChatSender>,
    startup_launcher: Option<StartupLauncher>,
}
impl LiveExecutor {
    pub(crate) fn new(handle: UiRuntimeHandle) -> Self {
        Self {
            handle,
            cancellation: None,
            playback_queue: None,
            playback_queue_clear: None,
            playback_queue_remove_front: None,
            task_queue_canceller: None,
            playback_controller: None,
            song_requester: None,
            chat_sender: None,
            startup_launcher: None,
        }
    }
    pub(crate) fn with_playback_queue_reader(mut self, reader: PlaybackQueueLengthReader) -> Self {
        self.playback_queue = Some(reader);
        self
    }
    pub(crate) fn with_playback_queue_clearer(mut self, clearer: PlaybackQueueClearer) -> Self {
        self.playback_queue_clear = Some(clearer);
        self
    }
    pub(crate) fn with_playback_queue_remove_front(
        mut self,
        remover: PlaybackQueueClearer,
    ) -> Self {
        self.playback_queue_remove_front = Some(remover);
        self
    }
    pub(crate) fn with_task_queue_canceller(mut self, canceller: TaskQueueCanceller) -> Self {
        self.task_queue_canceller = Some(canceller);
        self
    }
    pub(crate) fn with_playback_controller(mut self, controller: PlaybackController) -> Self {
        self.playback_controller = Some(controller);
        self
    }
    pub(crate) fn with_song_requester(mut self, requester: SongRequester) -> Self {
        self.song_requester = Some(requester);
        self
    }
    pub(crate) fn with_chat_sender(mut self, sender: ChatSender) -> Self {
        self.chat_sender = Some(sender);
        self
    }
    pub(crate) fn with_startup_launcher(mut self, launcher: StartupLauncher) -> Self {
        self.startup_launcher = Some(launcher);
        self
    }
    fn recover_device(&self) -> Result<(), String> {
        self.handle
            .submit_external(LiveRecoveryRoutine)
            .map_err(|error| error.to_string())?
            .wait_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("{error:#}"))
    }

    fn submit(
        &self,
        command: AtomicCommand,
        capture: bool,
    ) -> Result<Option<DynamicImage>, String> {
        let signal = self
            .cancellation
            .clone()
            .ok_or("live run cancellation signal unavailable")?;
        let timeout = live_command_timeout(&command);
        let operation = self
            .handle
            .submit_external(LiveCommandRoutine(command, signal.clone(), capture))
            .map_err(|e| format!("提交 live UI 命令失败: {e:#}"))?;
        let result = operation.wait_timeout(timeout).map_err(|e| {
            signal.store(true, Ordering::Release);
            format!("等待 live UI 命令失败: {e:#}")
        })?;
        match result {
            Ok(image) => Ok(image),
            Err(command_error) => {
                let cancelled = signal.load(Ordering::Acquire);
                if let Err(recovery_error) = self.recover_device() {
                    return Err(format!(
                        "live UI 命令失败且设备恢复失败: {command_error:#}; recovery={recovery_error}"
                    ));
                }
                if cancelled {
                    return Err(format!("live run cancelled: {command_error:#}"));
                }
                Err(format!("live UI 命令失败: {command_error:#}"))
            }
        }
    }
}
impl SandboxExecutor for LiveExecutor {
    fn supports_playback_queue_length(&self) -> bool {
        self.playback_queue.is_some()
    }
    fn supports_playback_queue_clear(&self) -> bool {
        self.playback_queue_clear.is_some()
    }
    fn supports_playback_queue_remove_front(&self) -> bool {
        self.playback_queue_remove_front.is_some()
    }
    fn playback_queue_remove_front(&mut self) -> Result<(), String> {
        self.playback_queue_remove_front
            .as_ref()
            .ok_or_else(|| "live executor未配置移除播放队列首项adapter".to_string())?()
    }
    fn supports_playback_control(&self) -> bool {
        self.playback_controller.is_some()
    }
    fn supports_song_request(&self) -> bool {
        self.song_requester.is_some()
    }
    fn song_request(&mut self, request: SongRequest) -> Result<(), String> {
        self.song_requester
            .as_ref()
            .ok_or_else(|| "live executor未配置点歌入队adapter".to_string())?(request)
    }
    fn supports_chat_send(&self) -> bool {
        self.chat_sender.is_some()
    }
    fn chat_send(&mut self, request: ChatSendRequest) -> Result<(), String> {
        self.chat_sender
            .as_ref()
            .ok_or_else(|| "live executor未配置聊天发送adapter".to_string())?(request)
    }
    fn supports_startup(&self) -> bool {
        self.startup_launcher.is_some()
    }
    fn startup(&mut self, request: StartupRequest) -> Result<(), String> {
        self.startup_launcher
            .as_ref()
            .ok_or_else(|| "live executor未配置游戏启动adapter".to_string())?(request)
    }
    fn playback_control(&mut self, action: PlaybackAction) -> Result<(), String> {
        self.playback_controller
            .as_ref()
            .ok_or_else(|| "live executor未配置播放控制adapter".to_string())?(action)
    }
    fn supports_task_queue_cancel(&self) -> bool {
        self.task_queue_canceller.is_some()
    }
    fn task_queue_cancel(&mut self, task_id: u64) -> Result<(), String> {
        self.task_queue_canceller
            .as_ref()
            .ok_or_else(|| "live executor未配置任务队列取消adapter".to_string())?(task_id)
    }
    fn playback_queue_clear(&mut self) -> Result<(), String> {
        self.playback_queue_clear
            .as_ref()
            .ok_or_else(|| "live executor未配置播放队列清空adapter".to_string())?()
    }
    fn playback_queue_length(&mut self) -> Result<usize, String> {
        self.playback_queue
            .as_ref()
            .ok_or_else(|| "live executor未配置播放队列状态adapter".to_string())?()
    }
    fn supports_window_available(&self) -> bool {
        true
    }
    fn window_available(&mut self) -> Result<bool, String> {
        self.handle
            .submit_external(LiveAvailabilityRoutine)
            .map_err(|error| error.to_string())?
            .wait_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("{error:#}"))
    }
    fn supports_window_foreground(&self) -> bool {
        true
    }
    fn window_foreground(&mut self) -> Result<bool, String> {
        self.handle
            .submit_external(LiveForegroundRoutine)
            .map_err(|error| error.to_string())?
            .wait_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("{error:#}"))
    }
    fn supports_ui_state(&self) -> bool {
        true
    }
    fn supports_clipboard(&self) -> bool {
        true
    }
    fn clipboard_text(&mut self) -> Result<Option<String>, String> {
        self.handle
            .submit_external(LiveClipboardRoutine)
            .map_err(|error| error.to_string())?
            .wait_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("{error:#}"))
    }
    fn ui_state(&mut self) -> Result<Option<SandboxUiStateReading>, String> {
        self.handle
            .submit_external(LiveUiStateRoutine)
            .map_err(|error| error.to_string())?
            .wait_timeout(std::time::Duration::from_secs(5))
            .map_err(|error| error.to_string())?
            .map_err(|error| format!("{error:#}"))
    }
    fn set_cancellation_signal(&mut self, signal: Arc<AtomicBool>) {
        self.cancellation = Some(signal);
    }
    fn recover(&mut self) -> Result<(), String> {
        self.recover_device()
    }
    fn execute(&mut self, command: &AtomicCommand) -> Result<(), String> {
        self.submit(command.clone(), false).map(|_| ())
    }
    fn execute_with_frame(
        &mut self,
        command: &AtomicCommand,
    ) -> Result<Option<DynamicImage>, String> {
        self.submit(command.clone(), true)
    }
    fn capture(&mut self) -> Result<DynamicImage, String> {
        self.submit(AtomicCommand::Capture, true)?
            .ok_or_else(|| "live capture did not return a frame".into())
    }
}

/// 模拟设备只有 fixture 与命令日志；Wait 累加虚拟时间，不 sleep。
#[derive(Default)]
pub(crate) struct SimulatedExecutor {
    fixture: Option<DynamicImage>,
    pub commands: Vec<AtomicCommand>,
    pub elapsed_ms: u64,
    pub playback_queue_length: usize,
    /// 可控的 UI 状态读数；None 表示尚无分类观测。
    pub ui_state: Option<SandboxUiStateReading>,
    /// 可控的剪贴板读数；None 表示剪贴板为空。
    pub clipboard: Option<String>,
    /// 已执行的播放控制动作，供测试断言"真的下发过、且顺序正确"。
    pub playback_actions: Vec<PlaybackAction>,
    /// 已执行的点歌请求，供测试断言。
    pub song_requests: Vec<SongRequest>,
    /// 已执行的聊天发送请求，供测试断言。
    pub chat_sends: Vec<ChatSendRequest>,
    /// 已执行的游戏启动请求，供测试断言。
    pub startups: Vec<StartupRequest>,
}
impl SimulatedExecutor {
    pub(crate) fn with_fixture(image: DynamicImage) -> Self {
        Self {
            fixture: Some(image),
            ..Self::default()
        }
    }
    pub(crate) fn set_fixture(&mut self, image: DynamicImage) {
        self.fixture = Some(image);
    }
}
impl SandboxExecutor for SimulatedExecutor {
    fn supports_playback_queue_length(&self) -> bool {
        true
    }
    fn playback_queue_length(&mut self) -> Result<usize, String> {
        Ok(self.playback_queue_length)
    }
    fn supports_playback_queue_clear(&self) -> bool {
        true
    }
    fn playback_queue_clear(&mut self) -> Result<(), String> {
        self.playback_queue_length = 0;
        Ok(())
    }
    fn supports_playback_queue_remove_front(&self) -> bool {
        true
    }
    fn playback_queue_remove_front(&mut self) -> Result<(), String> {
        if self.playback_queue_length == 0 {
            return Err("播放队列为空".into());
        }
        self.playback_queue_length -= 1;
        Ok(())
    }
    fn supports_playback_control(&self) -> bool {
        true
    }
    fn playback_control(&mut self, action: PlaybackAction) -> Result<(), String> {
        self.playback_actions.push(action);
        Ok(())
    }
    fn supports_song_request(&self) -> bool {
        true
    }
    fn song_request(&mut self, request: SongRequest) -> Result<(), String> {
        self.song_requests.push(request);
        Ok(())
    }
    fn supports_chat_send(&self) -> bool {
        true
    }
    fn chat_send(&mut self, request: ChatSendRequest) -> Result<(), String> {
        self.chat_sends.push(request);
        Ok(())
    }
    fn supports_startup(&self) -> bool {
        true
    }
    fn startup(&mut self, request: StartupRequest) -> Result<(), String> {
        self.startups.push(request);
        Ok(())
    }
    fn supports_task_queue_cancel(&self) -> bool {
        true
    }
    fn task_queue_cancel(&mut self, task_id: u64) -> Result<(), String> {
        if task_id == 0 {
            Err("任务 id 必须大于 0".into())
        } else {
            Ok(())
        }
    }
    fn supports_ui_state(&self) -> bool {
        true
    }
    fn ui_state(&mut self) -> Result<Option<SandboxUiStateReading>, String> {
        Ok(self.ui_state)
    }
    fn supports_clipboard(&self) -> bool {
        true
    }
    fn clipboard_text(&mut self) -> Result<Option<String>, String> {
        Ok(self.clipboard.clone())
    }
    fn execute(&mut self, command: &AtomicCommand) -> Result<(), String> {
        self.commands.push(command.clone());
        if let AtomicCommand::Wait { milliseconds } = command {
            self.elapsed_ms = self
                .elapsed_ms
                .checked_add(*milliseconds)
                .ok_or("虚拟时间溢出")?;
        }
        Ok(())
    }
    fn capture(&mut self) -> Result<DynamicImage, String> {
        self.fixture
            .clone()
            .ok_or_else(|| "模拟设备未提供截图 fixture".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug_sandbox::{SandboxLimits, SandboxTask};

    #[test]
    fn playback_queue_length_business_assertion_is_adapter_backed() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "queue-state".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 0 }],
                capture_after_each: false,
            })
            .unwrap();
        let executor = SimulatedExecutor {
            playback_queue_length: 2,
            ..SimulatedExecutor::default()
        };
        let assertion = StepAssertion {
            index: 0,
            assertion: SandboxAssertion::PlaybackQueueLength { expected: 2 },
        };
        let mut runner = DebugSandboxRunner::new(store.clone(), executor);
        let run = runner
            .start("queue-state", Some(1), vec![assertion.clone()])
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(
            done.status,
            SandboxStatus::Passed,
            "{:?}",
            done.results[0].error
        );
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_none());
        let wrong = SimulatedExecutor {
            playback_queue_length: 3,
            ..SimulatedExecutor::default()
        };
        let mut runner = DebugSandboxRunner::new(store, wrong);
        let run = runner
            .start("queue-state", Some(1), vec![assertion])
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Failed);
        assert!(
            done.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("播放队列长度不匹配")
        );
    }

    #[test]
    fn clipboard_assertion_is_adapter_backed_and_frame_free() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "clipboard".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 0 }],
                capture_after_each: false,
            })
            .unwrap();
        let assertion = StepAssertion {
            index: 0,
            assertion: SandboxAssertion::ClipboardContains {
                text: "千星奇域".into(),
            },
        };

        let mut runner = DebugSandboxRunner::new(
            store.clone(),
            SimulatedExecutor {
                clipboard: Some("复制的文本：千星奇域邀请码".into()),
                ..SimulatedExecutor::default()
            },
        );
        let run = runner
            .start("clipboard", Some(1), vec![assertion.clone()])
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(
            done.status,
            SandboxStatus::Passed,
            "{:?}",
            done.results[0].error
        );
        // 剪贴板断言是只读查询，不应产生截图。
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_none());
    }

    #[test]
    fn clipboard_assertion_reports_mismatch_empty_and_unsupported() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "clipboard".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 0 }],
                capture_after_each: false,
            })
            .unwrap();
        let assertion = StepAssertion {
            index: 0,
            assertion: SandboxAssertion::ClipboardContains {
                text: "期望文本".into(),
            },
        };

        // 内容不匹配：要报出实际内容，不能只报"失败"。
        let mut runner = DebugSandboxRunner::new(
            store.clone(),
            SimulatedExecutor {
                clipboard: Some("完全不同的内容".into()),
                ..SimulatedExecutor::default()
            },
        );
        let run = runner
            .start("clipboard", Some(1), vec![assertion.clone()])
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Failed);
        let error = done.results[0].error.clone().unwrap_or_default();
        assert!(error.contains("剪贴板内容不匹配"), "{error}");
        assert!(error.contains("完全不同的内容"), "{error}");

        // 剪贴板为空：必须与"不匹配"区分开。
        let mut runner = DebugSandboxRunner::new(
            store.clone(),
            SimulatedExecutor {
                clipboard: None,
                ..SimulatedExecutor::default()
            },
        );
        let run = runner
            .start("clipboard", Some(1), vec![assertion.clone()])
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Failed);
        assert!(
            done.results[0]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("剪贴板为空")
        );

        // 适配器不支持：必须在创建运行之前拒绝，而不是跑完再失败。
        struct NoClipboard;
        impl SandboxExecutor for NoClipboard {
            fn execute(&mut self, _command: &AtomicCommand) -> Result<(), String> {
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                Err("no capture".into())
            }
        }
        let mut runner = DebugSandboxRunner::new(store.clone(), NoClipboard);
        assert!(
            runner
                .start("clipboard", Some(1), vec![assertion.clone()])
                .unwrap_err()
                .contains("不支持剪贴板断言")
        );
    }

    #[test]
    fn clipboard_assertion_rejects_empty_target_and_truncates_long_actual() {
        let empty = SandboxAssertion::ClipboardContains {
            text: String::new(),
        };
        assert!(empty.validate().unwrap_err().contains("为空或过长"));
        let oversized = SandboxAssertion::ClipboardContains {
            text: "字".repeat(257),
        };
        assert!(oversized.validate().unwrap_err().contains("为空或过长"));
        let ok = SandboxAssertion::ClipboardContains { text: "ok".into() };
        assert!(ok.validate().is_ok());

        // 超长内容在错误信息里必须被截断，避免把整块剪贴板塞进运行结果。
        let long = "长".repeat(200);
        let truncated = truncate_for_message(&long);
        assert!(
            truncated.chars().count() < 100,
            "{}",
            truncated.chars().count()
        );
        assert!(truncated.ends_with('…'));
        assert_eq!(truncate_for_message("短"), "短");
    }

    #[test]
    fn ui_state_assertion_is_adapter_backed_and_frame_free() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "ui-state".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 0 }],
                capture_after_each: false,
            })
            .unwrap();
        let reading = SandboxUiStateReading {
            kind: SandboxUiStateKind::Primary,
            stable: true,
            frame_id: 7,
        };
        let assertion = StepAssertion {
            index: 0,
            assertion: SandboxAssertion::UiState {
                expected: SandboxUiStateKind::Primary,
                stable: true,
            },
        };

        let mut runner = DebugSandboxRunner::new(
            store.clone(),
            SimulatedExecutor {
                ui_state: Some(reading),
                ..SimulatedExecutor::default()
            },
        );
        let run = runner
            .start("ui-state", Some(1), vec![assertion.clone()])
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(
            done.status,
            SandboxStatus::Passed,
            "{:?}",
            done.results[0].error
        );
        // 状态断言是只读查询，不应产生截图。
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_none());

        let mismatched = DebugSandboxRunner::new(
            store.clone(),
            SimulatedExecutor {
                ui_state: Some(SandboxUiStateReading {
                    kind: SandboxUiStateKind::Overworld,
                    ..reading
                }),
                ..SimulatedExecutor::default()
            },
        );
        let mut runner = mismatched;
        let run = runner
            .start("ui-state", Some(1), vec![assertion.clone()])
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Failed);
        assert!(
            done.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("UI 状态不匹配: expected=primary, actual=overworld")
        );

        let mut runner = DebugSandboxRunner::new(
            store.clone(),
            SimulatedExecutor {
                ui_state: Some(SandboxUiStateReading {
                    stable: false,
                    ..reading
                }),
                ..SimulatedExecutor::default()
            },
        );
        let run = runner.start("ui-state", Some(1), vec![assertion]).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Failed);
        assert!(
            done.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("尚未稳定")
        );

        // 尚无观测时必须明确失败，不能把 None 当成通过。
        let relaxed = StepAssertion {
            index: 0,
            assertion: SandboxAssertion::UiState {
                expected: SandboxUiStateKind::Primary,
                stable: false,
            },
        };
        let mut runner = DebugSandboxRunner::new(store, SimulatedExecutor::default());
        let run = runner.start("ui-state", Some(1), vec![relaxed]).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Failed);
        assert!(
            done.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("尚无 UI 状态观测")
        );
    }

    #[test]
    fn ui_state_assertion_rejected_without_adapter_and_honest_when_live_unclassified() {
        use crate::runtime::ui::UiDevice;
        struct NoCapture;
        impl UiDevice for NoCapture {
            fn capture(&mut self) -> anyhow::Result<DynamicImage> {
                anyhow::bail!("unexpected screenshot")
            }
            fn close_window(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
        }
        /// 未接入 UI 状态 adapter 的执行器。
        struct NoAdapter;
        impl SandboxExecutor for NoAdapter {
            fn execute(&mut self, _command: &AtomicCommand) -> Result<(), String> {
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                Err("unused".into())
            }
        }

        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "ui-state-live".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 0 }],
                capture_after_each: false,
            })
            .unwrap();
        let assertions = vec![StepAssertion {
            index: 0,
            assertion: SandboxAssertion::UiState {
                expected: SandboxUiStateKind::Primary,
                stable: false,
            },
        }];
        let mut runner = DebugSandboxRunner::new(store.clone(), NoAdapter);
        assert!(
            runner
                .start("ui-state-live", None, assertions.clone())
                .unwrap_err()
                .contains("不支持 UI 状态断言")
        );

        // live 路径：尚无分类观测时必须明确失败，而不是伪造状态。
        let runtime = crate::runtime::ui::UiRuntime::start(NoCapture, 2).unwrap();
        let mut runner = DebugSandboxRunner::new(store, LiveExecutor::new(runtime.handle()));
        let run = runner.start("ui-state-live", None, assertions).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Failed);
        assert!(
            done.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("UI 状态查询失败")
        );
    }

    #[test]
    fn playback_queue_clear_is_audited_and_observable() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        let task = SandboxTask {
            id: "queue-clear".into(),
            version: 1,
            description: String::new(),
            commands: vec![
                AtomicCommand::PlaybackQueueClear,
                AtomicCommand::Wait { milliseconds: 0 },
            ],
            capture_after_each: false,
        };
        store.save_task(task).unwrap();
        let mut runner = DebugSandboxRunner::new(
            store,
            SimulatedExecutor {
                playback_queue_length: 4,
                ..SimulatedExecutor::default()
            },
        );
        let assertions = vec![StepAssertion {
            index: 1,
            assertion: SandboxAssertion::PlaybackQueueLength { expected: 0 },
        }];
        let run = runner.start("queue-clear", Some(1), assertions).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Passed, "{:?}", done.results);
        assert_eq!(done.results.len(), 2);
        assert_eq!(
            runner.evidence(&run.run_id).unwrap()[0].command,
            AtomicCommand::PlaybackQueueClear
        );
    }

    /// 播放控制是业务原子命令：不进 UI runtime、默认不截图，但每一步都要留证据。
    /// 这里断言的是与具体播放器实现无关的通用性质——顺序、顺序、还是顺序。
    #[test]
    fn playback_control_commands_are_audited_in_order_and_capture_free() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        let commands = vec![
            AtomicCommand::PlaybackPause,
            AtomicCommand::PlaybackSetVolume { volume: 30 },
            AtomicCommand::PlaybackSkipNext,
            AtomicCommand::PlaybackSkipPrevious,
            AtomicCommand::PlaybackPlay,
        ];
        store
            .save_task(SandboxTask {
                id: "playback".into(),
                version: 1,
                description: String::new(),
                commands: commands.clone(),
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, SimulatedExecutor::default());
        let run = runner.start("playback", Some(1), vec![]).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Passed, "{:?}", done.results);
        let evidence = runner.evidence(&run.run_id).unwrap();
        assert_eq!(
            evidence
                .iter()
                .map(|item| item.command.clone())
                .collect::<Vec<_>>(),
            commands,
            "审计轨迹必须与下发顺序一致"
        );
        // 业务命令不产生截图：没有画面可看，就不该伪造一帧来充数。
        assert!(evidence.iter().all(|item| item.image.is_none()));
        assert!(
            done.results
                .iter()
                .all(|item| item.frame_sequence.is_none())
        );
        assert_eq!(
            runner.executor.playback_actions,
            vec![
                PlaybackAction::Pause,
                PlaybackAction::SetVolume { volume: 30 },
                PlaybackAction::SkipNext,
                PlaybackAction::SkipPrevious,
                PlaybackAction::Play,
            ],
            "必须真的逐条下发过，而不是只记了日志"
        );
    }

    /// 执行器没有播放控制 adapter 时，必须在创建运行之前拒绝，
    /// 而不是跑了一半才失败、留下一个半成品运行。
    #[test]
    fn playback_control_is_rejected_before_run_creation_without_adapter() {
        struct NoPlayback;
        impl SandboxExecutor for NoPlayback {
            fn execute(&mut self, _command: &AtomicCommand) -> Result<(), String> {
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                Err("unused".into())
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "playback-live".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::PlaybackPause],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store.clone(), NoPlayback);
        let error = runner.start("playback-live", Some(1), vec![]).unwrap_err();
        assert!(format!("{error}").contains("播放控制"), "{error}");
        assert!(store.runs().unwrap().is_empty(), "被拒绝时不应留下运行");
    }

    /// 聊天发送是业务原子命令：正文与前缀必须**原样**交给适配层。
    /// 沙盒不替应用拼前缀、不做去重、也不伪造截图充数。
    #[test]
    fn chat_send_is_audited_and_forwards_exact_payload() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "chat-send".into(),
                version: 1,
                description: String::new(),
                commands: vec![
                    AtomicCommand::ChatSend {
                        text: "大家好".into(),
                        prefix: "[控制台]: ".into(),
                    },
                    AtomicCommand::ChatSend {
                        text: "不带前缀".into(),
                        prefix: String::new(),
                    },
                ],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, SimulatedExecutor::default());
        let run = runner.start("chat-send", Some(1), vec![]).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Passed, "{:?}", done.results);
        let evidence = runner.evidence(&run.run_id).unwrap();
        assert_eq!(
            evidence
                .iter()
                .map(|item| item.command.clone())
                .collect::<Vec<_>>(),
            vec![
                AtomicCommand::ChatSend {
                    text: "大家好".into(),
                    prefix: "[控制台]: ".into(),
                },
                AtomicCommand::ChatSend {
                    text: "不带前缀".into(),
                    prefix: String::new(),
                },
            ],
            "审计轨迹必须与下发顺序一致"
        );
        assert_eq!(
            runner.executor.chat_sends,
            vec![
                ChatSendRequest {
                    text: "大家好".into(),
                    prefix: "[控制台]: ".into(),
                },
                ChatSendRequest {
                    text: "不带前缀".into(),
                    prefix: String::new(),
                },
            ],
            "正文与前缀必须原样下发，空前缀要能表达不带前缀"
        );
        // 业务命令没有画面可看，就不该伪造一帧来充数。
        assert!(evidence.iter().all(|item| item.image.is_none()));
    }

    /// 没有聊天发送 adapter 时必须在创建运行之前拒绝，
    /// 而不是跑了一半才失败、留下半成品运行。
    #[test]
    fn chat_send_is_rejected_before_run_creation_without_adapter() {
        struct NoChat;
        impl SandboxExecutor for NoChat {
            fn execute(&mut self, _command: &AtomicCommand) -> Result<(), String> {
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                Err("unused".into())
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "chat-live".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::ChatSend {
                    text: "hi".into(),
                    prefix: String::new(),
                }],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store.clone(), NoChat);
        let error = runner.start("chat-live", Some(1), vec![]).unwrap_err();
        assert!(format!("{error}").contains("聊天发送"), "{error}");
        assert!(store.runs().unwrap().is_empty(), "被拒绝时不应留下运行");
    }

    /// 游戏启动是业务原子命令：只表达"启动什么"，原样交给适配层。
    /// 沙盒不替应用决定流程、不重试、也不伪造截图充数。
    #[test]
    fn startup_task_is_audited_and_forwards_exact_payload() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "startup".into(),
                version: 1,
                description: String::new(),
                commands: vec![
                    AtomicCommand::StartupTask {
                        kind: "start_game".into(),
                    },
                    AtomicCommand::StartupTask {
                        kind: "enter_wonderland".into(),
                    },
                ],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, SimulatedExecutor::default());
        let run = runner.start("startup", Some(1), vec![]).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Passed, "{:?}", done.results);
        assert_eq!(
            runner.executor.startups,
            vec![
                StartupRequest {
                    kind: "start_game".into()
                },
                StartupRequest {
                    kind: "enter_wonderland".into()
                },
            ],
            "启动类型必须原样下发、且顺序与审计轨迹一致"
        );
        let evidence = runner.evidence(&run.run_id).unwrap();
        assert!(evidence.iter().all(|item| item.image.is_none()));
    }

    /// 没有游戏启动 adapter 时必须在创建运行之前拒绝。
    #[test]
    fn startup_task_is_rejected_before_run_creation_without_adapter() {
        struct NoStartup;
        impl SandboxExecutor for NoStartup {
            fn execute(&mut self, _command: &AtomicCommand) -> Result<(), String> {
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                Err("unused".into())
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "startup-live".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::StartupTask {
                    kind: "start_game".into(),
                }],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store.clone(), NoStartup);
        let error = runner.start("startup-live", Some(1), vec![]).unwrap_err();
        assert!(format!("{error}").contains("游戏启动"), "{error}");
        assert!(store.runs().unwrap().is_empty(), "被拒绝时不应留下运行");
    }

    /// 点歌是业务原子命令：关键词、音源、伴奏偏好必须**原样**交给适配层。
    /// 沙盒不替应用做去重、不改写音源、也不伪造截图充数。
    #[test]
    fn song_request_is_audited_and_forwards_exact_payload() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "song-request".into(),
                version: 1,
                description: String::new(),
                commands: vec![
                    AtomicCommand::RequestSong {
                        keyword: "千本桜".into(),
                        source: "kugou".into(),
                        prefer_accompaniment: true,
                    },
                    AtomicCommand::PlaybackPause,
                ],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, SimulatedExecutor::default());
        let run = runner.start("song-request", Some(1), vec![]).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Passed, "{:?}", done.results);
        assert_eq!(
            runner.executor.song_requests,
            vec![SongRequest {
                keyword: "千本桜".into(),
                source: "kugou".into(),
                prefer_accompaniment: true,
            }],
            "载荷必须原样透传，不能在沙盒里被改写"
        );
        let evidence = runner.evidence(&run.run_id).unwrap();
        assert!(evidence.iter().all(|item| item.image.is_none()));
        assert!(
            done.results
                .iter()
                .all(|item| item.frame_sequence.is_none())
        );
    }

    #[test]
    fn song_request_is_rejected_before_run_creation_without_adapter() {
        struct NoSongRequest;
        impl SandboxExecutor for NoSongRequest {
            fn execute(&mut self, _command: &AtomicCommand) -> Result<(), String> {
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                Err("unused".into())
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "song-live".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::RequestSong {
                    keyword: "任意".into(),
                    source: String::new(),
                    prefer_accompaniment: false,
                }],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store.clone(), NoSongRequest);
        let error = runner.start("song-live", Some(1), vec![]).unwrap_err();
        assert!(format!("{error}").contains("点歌"), "{error}");
        assert!(store.runs().unwrap().is_empty());
    }

    #[test]
    fn playback_queue_remove_front_is_audited_and_updates_state() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "queue-remove-front".into(),
                version: 1,
                description: String::new(),
                commands: vec![
                    AtomicCommand::PlaybackQueueRemoveFront,
                    AtomicCommand::Wait { milliseconds: 0 },
                ],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(
            store,
            SimulatedExecutor {
                playback_queue_length: 3,
                ..SimulatedExecutor::default()
            },
        );
        let run = runner
            .start(
                "queue-remove-front",
                Some(1),
                vec![StepAssertion {
                    index: 1,
                    assertion: SandboxAssertion::PlaybackQueueLength { expected: 2 },
                }],
            )
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Passed, "{:?}", done.results);
        assert_eq!(
            runner.evidence(&run.run_id).unwrap()[0].command,
            AtomicCommand::PlaybackQueueRemoveFront
        );
    }

    #[test]
    fn playback_queue_remove_front_fails_on_empty_queue() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "queue-remove-empty".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::PlaybackQueueRemoveFront],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, SimulatedExecutor::default());
        let run = runner
            .start("queue-remove-empty", Some(1), Vec::new())
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Failed);
        assert!(
            done.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("播放队列为空")
        );
    }

    #[test]
    fn task_queue_cancel_is_audited_and_validates_task_id() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "task-cancel".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::TaskQueueCancel { task_id: 42 }],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, SimulatedExecutor::default());
        let run = runner.start("task-cancel", Some(1), Vec::new()).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Passed, "{:?}", done.results);
        assert_eq!(
            runner.evidence(&run.run_id).unwrap()[0].command,
            AtomicCommand::TaskQueueCancel { task_id: 42 }
        );

        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "task-cancel-invalid".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::TaskQueueCancel { task_id: 0 }],
                capture_after_each: false,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, SimulatedExecutor::default());
        let error = runner
            .start("task-cancel-invalid", Some(1), Vec::new())
            .unwrap_err();
        assert!(error.contains("task_id"));
    }

    #[test]
    fn live_timeout_includes_headroom_for_bounded_waits() {
        let expected = std::time::Duration::from_secs(35);
        for command in [
            AtomicCommand::Wait {
                milliseconds: 30_000,
            },
            AtomicCommand::EnsureReady {
                after_activate_ms: 30_000,
            },
            AtomicCommand::Activate {
                after_activate_ms: 30_000,
            },
            AtomicCommand::Focus {
                after_activate_ms: 30_000,
            },
            AtomicCommand::InputText {
                text: "ok".into(),
                settle_ms: 30_000,
            },
        ] {
            assert_eq!(live_command_timeout(&command), expected);
        }
        assert_eq!(
            live_command_timeout(&AtomicCommand::Capture),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            live_command_timeout(&AtomicCommand::Wait { milliseconds: 1 }),
            std::time::Duration::from_secs(30)
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires Windows UI runtime, does not contact or operate the game"]
    fn live_executor_rejects_unavailable_playback_state_adapter_before_run_creation() {
        let runtime = crate::runtime::ui::UiRuntime::start(
            crate::adapters::windows::WindowsUiDevice::new(crate::config::WindowConfig::default()),
            1,
        )
        .expect("start UI worker");
        let store = DebugSandboxStore::new(SandboxLimits::default());
        let task = SandboxTask {
            id: "live-queue-state".into(),
            version: 1,
            description: String::new(),
            commands: vec![AtomicCommand::Wait { milliseconds: 0 }],
            capture_after_each: false,
        };
        let assertion = StepAssertion {
            index: 0,
            assertion: SandboxAssertion::PlaybackQueueLength { expected: 0 },
        };
        let mut runner =
            DebugSandboxRunner::new(store.clone(), LiveExecutor::new(runtime.handle()));
        let error = runner.start_snapshot(task, vec![assertion]).unwrap_err();
        assert!(error.contains("不支持播放队列状态断言"), "{error}");
        assert!(store.runs().unwrap().is_empty());
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a visible running game window on the desktop"]
    fn live_capture_run_read_only_smoke() {
        let runtime = crate::runtime::ui::UiRuntime::start(
            crate::adapters::windows::WindowsUiDevice::new(crate::config::WindowConfig::default()),
            2,
        )
        .expect("start live UI worker");
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "live-capture-smoke".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Capture],
                capture_after_each: true,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, LiveExecutor::new(runtime.handle()));
        let run = runner
            .start(
                "live-capture-smoke",
                None,
                vec![StepAssertion {
                    index: 0,
                    assertion: SandboxAssertion::WindowAvailable { expected: true },
                }],
            )
            .unwrap();
        let done = runner.step(&run.run_id).expect("execute read-only capture");
        assert_eq!(
            done.status,
            SandboxStatus::Passed,
            "{:?}",
            done.results[0].error
        );
        let evidence = runner.evidence(&run.run_id).unwrap();
        let frame = evidence[0].image.as_ref().expect("live frame");
        println!("live sandbox frame: {}x{}", frame.width(), frame.height());
        assert!(frame.width() > 0 && frame.height() > 0);
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[test]
    fn frame_difference_checks_lower_and_upper_bounds_with_legacy_default() {
        let old: SandboxAssertion =
            serde_json::from_str(r#"{"frame_difference":{"max_changed_pixels":1}}"#).unwrap();
        assert_eq!(
            old,
            SandboxAssertion::FrameDifference {
                min_changed_pixels: 0,
                max_changed_pixels: 1
            }
        );
        let before = DynamicImage::new_rgba8(2, 2);
        let mut after = before.clone();
        after
            .as_mut_rgba8()
            .unwrap()
            .put_pixel(0, 0, image::Rgba([1, 2, 3, 255]));
        assert!(old.verify_against(Some(&before), &after).is_ok());
        let changed = SandboxAssertion::FrameDifference {
            min_changed_pixels: 1,
            max_changed_pixels: 1,
        };
        assert!(changed.validate().is_ok());
        assert!(
            changed
                .verify_against(Some(&before), &before)
                .unwrap_err()
                .contains("不足")
        );
        assert!(changed.verify_against(Some(&before), &after).is_ok());
        assert!(
            SandboxAssertion::FrameDifference {
                min_changed_pixels: 2,
                max_changed_pixels: 1
            }
            .validate()
            .is_err()
        );
        assert!(
            SandboxAssertion::FrameDifference {
                min_changed_pixels: 0,
                max_changed_pixels: 0
            }
            .verify_against(Some(&before), &after)
            .unwrap_err()
            .contains("过多")
        );
    }

    #[test]
    fn ocr_assertion_uses_step_crop_and_preserves_failed_evidence() {
        let image = DynamicImage::new_rgba8(3, 2);
        let (_, runner) = setup(vec![AtomicCommand::Capture], Some(image.clone()));
        let assertion = StepAssertion {
            index: 0,
            assertion: SandboxAssertion::OcrContains {
                x: 1,
                y: 0,
                width: 2,
                height: 1,
                text: "ready".into(),
            },
        };
        let mut without_ocr = runner;
        assert!(
            without_ocr
                .start("demo", None, vec![assertion.clone()])
                .unwrap_err()
                .contains("OCR runtime")
        );
        let (_, runner) = setup(vec![AtomicCommand::Capture], Some(image));
        let mut runner = runner.with_ocr(Arc::new(|crop| {
            assert_eq!(crop.dimensions(), (2, 1));
            Ok("not ready".into())
        }));
        let run = runner.start("demo", None, vec![assertion.clone()]).unwrap();
        assert_eq!(
            runner.step(&run.run_id).unwrap().status,
            SandboxStatus::Passed
        );
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_some());
        let (_, runner) = setup(
            vec![AtomicCommand::Capture],
            Some(DynamicImage::new_rgba8(3, 2)),
        );
        let mut runner = runner.with_ocr(Arc::new(|_| Ok("waiting".into())));
        let run = runner.start("demo", None, vec![assertion]).unwrap();
        assert_eq!(
            runner.step(&run.run_id).unwrap().status,
            SandboxStatus::Failed
        );
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_some());
        assert!(
            runner.evidence(&run.run_id).unwrap()[0]
                .result
                .error
                .as_ref()
                .unwrap()
                .contains("OCR 文本未命中")
        );
    }

    fn setup(
        commands: Vec<AtomicCommand>,
        fixture: Option<DynamicImage>,
    ) -> (DebugSandboxStore, DebugSandboxRunner<SimulatedExecutor>) {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "demo".into(),
                version: 1,
                description: String::new(),
                commands,
                capture_after_each: true,
            })
            .unwrap();
        let executor = fixture
            .map(SimulatedExecutor::with_fixture)
            .unwrap_or_default();
        (store.clone(), DebugSandboxRunner::new(store, executor))
    }
    struct CancelDuringExecute {
        signal: Option<Arc<AtomicBool>>,
        captures: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl SandboxExecutor for CancelDuringExecute {
        fn set_cancellation_signal(&mut self, signal: Arc<AtomicBool>) {
            self.signal = Some(signal);
        }
        fn execute(&mut self, _: &AtomicCommand) -> Result<(), String> {
            self.signal.as_ref().unwrap().store(true, Ordering::Release);
            Err("UI operation timed out".into())
        }
        fn capture(&mut self) -> Result<DynamicImage, String> {
            self.captures.fetch_add(1, Ordering::Relaxed);
            Ok(DynamicImage::new_rgba8(1, 1))
        }
    }

    #[test]
    fn cancellation_during_command_does_not_queue_another_capture_or_step() {
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "cancel-no-capture".into(),
                version: 1,
                description: String::new(),
                commands: vec![
                    AtomicCommand::Wait { milliseconds: 1 },
                    AtomicCommand::Capture,
                ],
                capture_after_each: true,
            })
            .unwrap();
        let captures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let executor = CancelDuringExecute {
            signal: None,
            captures: captures.clone(),
        };
        let mut runner = DebugSandboxRunner::new(store, executor);
        let run = runner.start("cancel-no-capture", Some(1), vec![]).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Cancelled);
        assert_eq!(done.results.len(), 1);
        assert_eq!(captures.load(Ordering::Relaxed), 0);
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_none());
    }

    #[test]
    fn cancellation_after_atomic_capture_skips_ocr_and_keeps_frame() {
        use std::sync::atomic::AtomicUsize;
        struct Device {
            signal: Option<Arc<AtomicBool>>,
            recoveries: Arc<AtomicUsize>,
        }
        impl SandboxExecutor for Device {
            fn set_cancellation_signal(&mut self, signal: Arc<AtomicBool>) {
                self.signal = Some(signal);
            }
            fn execute(&mut self, _: &AtomicCommand) -> Result<(), String> {
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                panic!("unexpected capture")
            }
            fn execute_with_frame(
                &mut self,
                _: &AtomicCommand,
            ) -> Result<Option<DynamicImage>, String> {
                self.signal.as_ref().unwrap().store(true, Ordering::Release);
                Ok(Some(DynamicImage::new_rgba8(1, 1)))
            }
            fn recover(&mut self) -> Result<(), String> {
                self.recoveries.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "cancel-ocr".into(),
                version: 1,
                description: String::new(),
                commands: vec![
                    AtomicCommand::Wait { milliseconds: 1 },
                    AtomicCommand::Capture,
                ],
                capture_after_each: true,
            })
            .unwrap();
        let recoveries = Arc::new(AtomicUsize::new(0));
        let ocr_calls = Arc::new(AtomicUsize::new(0));
        let calls = ocr_calls.clone();
        let mut runner = DebugSandboxRunner::new(
            store,
            Device {
                signal: None,
                recoveries: recoveries.clone(),
            },
        )
        .with_ocr(Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("ready".into())
        }));
        let assertions = vec![StepAssertion {
            index: 0,
            assertion: SandboxAssertion::OcrContains {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                text: "ready".into(),
            },
        }];
        let run = runner.start("cancel-ocr", None, assertions).unwrap();
        let done = runner.step(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Cancelled);
        assert_eq!(done.results.len(), 1);
        assert_eq!(ocr_calls.load(Ordering::SeqCst), 0);
        assert_eq!(recoveries.load(Ordering::SeqCst), 1);
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_some());
    }

    #[test]
    fn cancellation_during_first_ocr_skips_second_ocr() {
        use std::sync::{Mutex, atomic::AtomicUsize};
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "cancel-between-ocr".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Capture],
                capture_after_each: true,
            })
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let run_id = Arc::new(Mutex::new(String::new()));
        let (cancel_store, cancel_id, count) = (store.clone(), run_id.clone(), calls.clone());
        let mut runner = DebugSandboxRunner::new(
            store,
            SimulatedExecutor::with_fixture(DynamicImage::new_rgba8(1, 1)),
        )
        .with_ocr(Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            cancel_store.cancel_run(&cancel_id.lock().unwrap()).unwrap();
            Ok("ready".into())
        }));
        let assertion = StepAssertion {
            index: 0,
            assertion: SandboxAssertion::OcrContains {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                text: "ready".into(),
            },
        };
        let run = runner
            .start(
                "cancel-between-ocr",
                None,
                vec![assertion.clone(), assertion],
            )
            .unwrap();
        *run_id.lock().unwrap() = run.run_id.clone();
        let done = runner.step(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Cancelled);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_some());
    }

    #[test]
    fn cancel_after_successful_command_recovers_before_finishing() {
        use std::sync::atomic::AtomicUsize;
        struct Device {
            signal: Option<Arc<AtomicBool>>,
            recovered: Arc<AtomicUsize>,
        }
        impl SandboxExecutor for Device {
            fn set_cancellation_signal(&mut self, signal: Arc<AtomicBool>) {
                self.signal = Some(signal);
            }
            fn execute(&mut self, _: &AtomicCommand) -> Result<(), String> {
                self.signal.as_ref().unwrap().store(true, Ordering::Release);
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                panic!("cancelled run must not capture")
            }
            fn recover(&mut self) -> Result<(), String> {
                self.recovered.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "cancel-after".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 1 }],
                capture_after_each: true,
            })
            .unwrap();
        let recovered = Arc::new(AtomicUsize::new(0));
        let mut runner = DebugSandboxRunner::new(
            store,
            Device {
                signal: None,
                recovered: recovered.clone(),
            },
        );
        let run = runner.start("cancel-after", None, vec![]).unwrap();
        assert_eq!(
            runner.step(&run.run_id).unwrap().status,
            SandboxStatus::Cancelled
        );
        assert_eq!(recovered.load(Ordering::SeqCst), 1);
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_none());
    }

    #[test]
    fn pre_step_cancel_recovers_before_finishing_and_retries_failure() {
        use std::sync::atomic::AtomicUsize;
        struct Device {
            recoveries: Arc<AtomicUsize>,
            executions: Arc<AtomicUsize>,
        }
        impl SandboxExecutor for Device {
            fn execute(&mut self, _: &AtomicCommand) -> Result<(), String> {
                self.executions.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                Ok(DynamicImage::new_rgba8(1, 1))
            }
            fn recover(&mut self) -> Result<(), String> {
                if self.recoveries.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("release failed".into())
                } else {
                    Ok(())
                }
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "recover-cancel".into(),
                version: 1,
                description: String::new(),
                commands: vec![
                    AtomicCommand::Wait { milliseconds: 1 },
                    AtomicCommand::Wait { milliseconds: 1 },
                ],
                capture_after_each: false,
            })
            .unwrap();
        let recoveries = Arc::new(AtomicUsize::new(0));
        let executions = Arc::new(AtomicUsize::new(0));
        let mut runner = DebugSandboxRunner::new(
            store.clone(),
            Device {
                recoveries: recoveries.clone(),
                executions: executions.clone(),
            },
        );
        let run = runner.start("recover-cancel", None, vec![]).unwrap();
        assert_eq!(
            runner.step(&run.run_id).unwrap().status,
            SandboxStatus::Running
        );
        store.cancel_run(&run.run_id).unwrap();
        assert_eq!(runner.step(&run.run_id).unwrap_err(), "release failed");
        assert_eq!(
            store.run(&run.run_id).unwrap().unwrap().status,
            SandboxStatus::Running
        );
        assert_eq!(
            runner.step(&run.run_id).unwrap().status,
            SandboxStatus::Cancelled
        );
        assert_eq!(recoveries.load(Ordering::SeqCst), 2);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(runner.evidence(&run.run_id).unwrap().len(), 1);
    }

    #[test]
    fn live_foreground_assertion_observes_without_capture_or_activation() {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        struct Device {
            foreground: Arc<AtomicBool>,
            captures: Arc<AtomicUsize>,
        }
        impl crate::runtime::ui::UiDevice for Device {
            fn capture(&mut self) -> anyhow::Result<DynamicImage> {
                self.captures.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("unexpected screenshot")
            }
            fn window_foreground(&mut self) -> anyhow::Result<bool> {
                Ok(self.foreground.load(Ordering::SeqCst))
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "foreground-check".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 0 }],
                capture_after_each: false,
            })
            .unwrap();
        let assertion = vec![StepAssertion {
            index: 0,
            assertion: SandboxAssertion::WindowForeground { expected: true },
        }];
        let mut simulated = DebugSandboxRunner::new(store.clone(), SimulatedExecutor::default());
        assert!(
            simulated
                .start("foreground-check", None, assertion.clone())
                .unwrap_err()
                .contains("不支持窗口前台断言")
        );
        let foreground = Arc::new(AtomicBool::new(true));
        let captures = Arc::new(AtomicUsize::new(0));
        let runtime = crate::runtime::ui::UiRuntime::start(
            Device {
                foreground: foreground.clone(),
                captures: captures.clone(),
            },
            2,
        )
        .unwrap();
        let mut runner = DebugSandboxRunner::new(store, LiveExecutor::new(runtime.handle()));
        let passing = runner
            .start("foreground-check", None, assertion.clone())
            .unwrap();
        let step = runner.step(&passing.run_id).unwrap();
        assert_eq!(
            step.status,
            SandboxStatus::Passed,
            "results={:?}",
            step.results
        );
        assert!(runner.evidence(&passing.run_id).unwrap()[0].image.is_none());
        foreground.store(false, Ordering::SeqCst);
        let failing = runner.start("foreground-check", None, assertion).unwrap();
        let step = runner.step(&failing.run_id).unwrap();
        assert_eq!(
            step.status,
            SandboxStatus::Failed,
            "results={:?}",
            step.results
        );
        assert!(
            runner.evidence(&failing.run_id).unwrap()[0]
                .result
                .error
                .as_ref()
                .unwrap()
                .contains("actual=false")
        );
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[test]
    fn live_command_captures_one_frame_within_its_ui_operation() {
        use std::sync::atomic::AtomicUsize;
        struct Device(Arc<AtomicUsize>);
        impl crate::runtime::ui::UiDevice for Device {
            fn capture(&mut self) -> anyhow::Result<DynamicImage> {
                let count = self.0.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                    1,
                    1,
                    image::Rgba([count as u8, 0, 0, 255]),
                )))
            }
        }
        let captures = Arc::new(AtomicUsize::new(0));
        let runtime = crate::runtime::ui::UiRuntime::start(Device(captures.clone()), 2).unwrap();
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "atomic-frame".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 0 }],
                capture_after_each: true,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, LiveExecutor::new(runtime.handle()));
        let run = runner.start("atomic-frame", None, vec![]).unwrap();
        assert_eq!(
            runner.step(&run.run_id).unwrap().status,
            SandboxStatus::Passed
        );
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        let evidence = &runner.evidence(&run.run_id).unwrap()[0];
        assert_eq!(evidence.image.as_ref().unwrap().get_pixel(0, 0).0[0], 1);
        assert_eq!(evidence.result.frame_sequence, Some(1));
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[test]
    fn window_available_assertion_checks_missing_window_without_screenshot() {
        use std::sync::atomic::AtomicUsize;
        struct Device {
            available: Arc<AtomicBool>,
            queries: Arc<AtomicUsize>,
        }
        impl crate::runtime::ui::UiDevice for Device {
            fn capture(&mut self) -> anyhow::Result<DynamicImage> {
                anyhow::bail!("unexpected screenshot")
            }
            fn close_window(&mut self) -> anyhow::Result<()> {
                self.available.store(false, Ordering::SeqCst);
                Ok(())
            }
            fn window_available(&mut self) -> anyhow::Result<bool> {
                self.queries.fetch_add(1, Ordering::SeqCst);
                Ok(self.available.load(Ordering::SeqCst))
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "close-available".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::CloseWindow],
                capture_after_each: true,
            })
            .unwrap();
        let assertion = vec![StepAssertion {
            index: 0,
            assertion: SandboxAssertion::WindowAvailable { expected: false },
        }];
        let mut simulated = DebugSandboxRunner::new(store.clone(), SimulatedExecutor::default());
        assert!(
            simulated
                .start("close-available", None, assertion.clone())
                .unwrap_err()
                .contains("不支持目标窗口可用状态断言")
        );
        let available = Arc::new(AtomicBool::new(true));
        let queries = Arc::new(AtomicUsize::new(0));
        let runtime = crate::runtime::ui::UiRuntime::start(
            Device {
                available: available.clone(),
                queries: queries.clone(),
            },
            2,
        )
        .unwrap();
        let mut runner = DebugSandboxRunner::new(store, LiveExecutor::new(runtime.handle()));
        let run = runner
            .start("close-available", None, assertion.clone())
            .unwrap();
        assert_eq!(
            runner.step(&run.run_id).unwrap().status,
            SandboxStatus::Passed
        );
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_none());
        let mismatch = runner
            .start(
                "close-available",
                None,
                vec![StepAssertion {
                    index: 0,
                    assertion: SandboxAssertion::WindowAvailable { expected: true },
                }],
            )
            .unwrap();
        assert_eq!(
            runner.step(&mismatch.run_id).unwrap().status,
            SandboxStatus::Failed
        );
        assert!(
            runner.evidence(&mismatch.run_id).unwrap()[0]
                .result
                .error
                .as_deref()
                .unwrap()
                .contains("actual=false")
        );
        assert_eq!(queries.load(Ordering::SeqCst), 2);
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    struct LiveTestDevice;

    impl crate::runtime::ui::UiDevice for LiveTestDevice {
        fn capture(&mut self) -> anyhow::Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(1, 1))
        }
    }

    #[test]
    fn live_close_window_succeeds_without_attempting_post_close_capture() {
        use std::sync::atomic::AtomicUsize;
        struct ClosingDevice {
            captures: Arc<AtomicUsize>,
        }
        impl crate::runtime::ui::UiDevice for ClosingDevice {
            fn capture(&mut self) -> anyhow::Result<DynamicImage> {
                self.captures.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("window is closed")
            }
            fn close_window(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
        }
        struct RestoreMode(bool);
        impl Drop for RestoreMode {
            fn drop(&mut self) {
                crate::debug_mode::set_enabled(self.0);
            }
        }
        let _mode = RestoreMode(crate::debug_mode::set_enabled(true));
        let captures = Arc::new(AtomicUsize::new(0));
        let runtime = crate::runtime::ui::UiRuntime::start(
            ClosingDevice {
                captures: captures.clone(),
            },
            2,
        )
        .unwrap();
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "close-only".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::CloseWindow],
                capture_after_each: true,
            })
            .unwrap();
        let mut runner = DebugSandboxRunner::new(store, LiveExecutor::new(runtime.handle()));
        let rejection = runner.start(
            "close-only",
            None,
            vec![StepAssertion {
                index: 0,
                assertion: SandboxAssertion::Dimensions {
                    width: 1,
                    height: 1,
                },
            }],
        );
        assert!(
            rejection
                .unwrap_err()
                .contains("关闭窗口步骤不支持截图断言")
        );
        let run = runner.start("close-only", None, vec![]).unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Passed);
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_none());
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[test]
    fn live_wait_observes_cancel_before_its_deadline() {
        use std::sync::mpsc;
        use std::time::Duration;
        let runtime = crate::runtime::ui::UiRuntime::start(LiveTestDevice, 2).unwrap();
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "live-wait".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 5000 }],
                capture_after_each: true,
            })
            .unwrap();
        let mut runner =
            DebugSandboxRunner::new(store.clone(), LiveExecutor::new(runtime.handle()));
        let run = runner.start("live-wait", Some(1), vec![]).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let id = run.run_id.clone();
        let worker = std::thread::spawn(move || {
            done_tx.send(runner.step(&id)).unwrap();
        });
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(80)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        store.cancel_run(&run.run_id).unwrap();
        let result = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("cancel must interrupt wait")
            .unwrap();
        assert_eq!(result.status, SandboxStatus::Cancelled);
        worker.join().unwrap();
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[test]
    fn capture_after_each_false_skips_implicit_capture() {
        let (store, mut runner) = setup(
            vec![AtomicCommand::Wait { milliseconds: 0 }],
            Some(DynamicImage::new_rgba8(1, 1)),
        );
        store
            .save_task(SandboxTask {
                id: "demo".into(),
                version: 2,
                description: String::new(),
                commands: vec![AtomicCommand::Wait { milliseconds: 0 }],
                capture_after_each: false,
            })
            .unwrap();
        let run = runner.start("demo", Some(2), vec![]).unwrap();
        let done = runner.step(&run.run_id).unwrap();
        assert!(done.results[0].ok);
        assert_eq!(done.results[0].frame_sequence, None);
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_none());
    }

    #[test]
    fn steps_record_input_and_fresh_evidence_once() {
        let (store, mut runner) = setup(
            vec![
                AtomicCommand::Click { x: 1, y: 1 },
                AtomicCommand::Wait { milliseconds: 100 },
            ],
            Some(DynamicImage::new_rgba8(2, 3)),
        );
        let run = runner.start("demo", Some(1), vec![]).unwrap();
        assert_eq!(
            runner.step(&run.run_id).unwrap().status,
            SandboxStatus::Running
        );
        store
            .save_task(SandboxTask {
                id: "demo".into(),
                version: 2,
                description: String::new(),
                commands: vec![AtomicCommand::Capture],
                capture_after_each: true,
            })
            .unwrap();
        assert_eq!(
            runner.run_all(&run.run_id).unwrap().status,
            SandboxStatus::Passed
        );
        assert_eq!(runner.executor.commands.len(), 2);
        assert_eq!(runner.executor.elapsed_ms, 100);
        assert_eq!(
            runner.evidence(&run.run_id).unwrap()[1]
                .result
                .frame_sequence,
            Some(2)
        );
        runner.step(&run.run_id).unwrap();
        assert_eq!(runner.executor.commands.len(), 2);
    }
    #[test]
    fn failed_pixel_assertion_stops_and_preserves_screenshot() {
        let (_, mut runner) = setup(
            vec![AtomicCommand::Capture, AtomicCommand::Click { x: 0, y: 0 }],
            Some(DynamicImage::new_rgba8(1, 1)),
        );
        let run = runner
            .start(
                "demo",
                None,
                vec![StepAssertion {
                    index: 0,
                    assertion: SandboxAssertion::Pixel {
                        x: 0,
                        y: 0,
                        rgba: [255, 0, 0, 0],
                        tolerance: 0,
                    },
                }],
            )
            .unwrap();
        assert_eq!(
            runner.run_all(&run.run_id).unwrap().status,
            SandboxStatus::Failed
        );
        assert!(runner.executor.commands.is_empty());
        assert!(runner.evidence(&run.run_id).unwrap()[0].image.is_some());
    }
    #[test]
    fn assertion_failure_records_recovery_error_and_preserves_frame() {
        use std::sync::atomic::AtomicUsize;
        struct Device(Arc<AtomicUsize>);
        impl SandboxExecutor for Device {
            fn execute(&mut self, _: &AtomicCommand) -> Result<(), String> {
                Ok(())
            }
            fn capture(&mut self) -> Result<DynamicImage, String> {
                Ok(DynamicImage::new_rgba8(1, 1))
            }
            fn recover(&mut self) -> Result<(), String> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err("release blocked".into())
            }
        }
        let store = DebugSandboxStore::new(SandboxLimits::default());
        store
            .save_task(SandboxTask {
                id: "assert-recover".into(),
                version: 1,
                description: String::new(),
                commands: vec![
                    AtomicCommand::Capture,
                    AtomicCommand::Wait { milliseconds: 1 },
                ],
                capture_after_each: true,
            })
            .unwrap();
        let recovered = Arc::new(AtomicUsize::new(0));
        let mut runner = DebugSandboxRunner::new(store, Device(recovered.clone()));
        let run = runner
            .start(
                "assert-recover",
                None,
                vec![StepAssertion {
                    index: 0,
                    assertion: SandboxAssertion::Dimensions {
                        width: 2,
                        height: 1,
                    },
                }],
            )
            .unwrap();
        let done = runner.run_all(&run.run_id).unwrap();
        assert_eq!(done.status, SandboxStatus::Failed);
        assert_eq!(done.results.len(), 1);
        assert_eq!(recovered.load(Ordering::SeqCst), 1);
        let evidence = &runner.evidence(&run.run_id).unwrap()[0];
        assert!(evidence.image.is_some());
        let error = evidence.result.error.as_ref().unwrap();
        assert!(error.contains("图像尺寸不匹配"));
        assert!(error.contains("设备恢复失败: release blocked"));
    }

    #[test]
    fn cancellation_between_steps_retains_completed_evidence() {
        let (_, mut runner) = setup(
            vec![AtomicCommand::Capture, AtomicCommand::Click { x: 0, y: 0 }],
            Some(DynamicImage::new_rgba8(1, 1)),
        );
        let run = runner.start("demo", None, vec![]).unwrap();
        runner.step(&run.run_id).unwrap();
        runner.cancel(&run.run_id).unwrap();
        assert_eq!(
            runner.run_all(&run.run_id).unwrap().status,
            SandboxStatus::Cancelled
        );
        assert_eq!(runner.evidence(&run.run_id).unwrap().len(), 1);
        assert!(runner.executor.commands.is_empty());
    }
    #[test]
    fn missing_fixture_fails_instead_of_fabricating_success() {
        let (_, mut runner) = setup(vec![AtomicCommand::Capture], None);
        let run = runner.start("demo", None, vec![]).unwrap();
        let result = runner.run_all(&run.run_id).unwrap();
        assert_eq!(result.status, SandboxStatus::Failed);
        assert!(result.results[0].frame_sequence.is_none());
    }
    #[test]
    fn inline_template_matches_bounded_grayscale_region() {
        let mut pixels = image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 255]));
        pixels.put_pixel(2, 1, image::Rgba([255, 255, 255, 255]));
        let image = DynamicImage::ImageRgba8(pixels);
        let template = SandboxAssertion::Template {
            x: 1,
            y: 0,
            width: 3,
            height: 3,
            template_width: 2,
            template_height: 2,
            grayscale: vec![0, 0, 0, 255],
            min_score: 0.99,
        };
        template.verify(&image).unwrap();
        assert!(
            SandboxAssertion::Template {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
                template_width: 2,
                template_height: 2,
                grayscale: vec![255; 4],
                min_score: 0.99
            }
            .verify(&image)
            .is_err()
        );
        assert!(
            SandboxAssertion::Template {
                x: 3,
                y: 0,
                width: 2,
                height: 2,
                template_width: 2,
                template_height: 2,
                grayscale: vec![0; 4],
                min_score: 0.99
            }
            .verify(&image)
            .is_err()
        );
    }

    /// 自匹配必须恰好是 1.0。这是匹配器的通用性质，**不需要挑阈值**：
    /// 模板直接取自同一张图的同一区域，阈值取理论上界 1.0（不是从结果里拟合出来的数）。
    /// 模板字节直接调用库自身的 to_luma8，避免手写灰度公式与实现不一致
    /// （真机试验中就因为把 Rec.709 写成 Rec.601 而得到 0.707 的假失败）。
    #[test]
    fn inline_template_self_match_is_exact_across_sizes_and_positions() {
        let (width, height) = (16u32, 12u32);
        let mut pixels = image::RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                // 确定性伪随机：各区域互不相同，避免常量模板掩盖偏差。
                let value = ((x * 37 + y * 91 + x * y * 7) % 251) as u8;
                pixels.put_pixel(x, y, image::Rgba([value, value, value, 255]));
            }
        }
        let image = DynamicImage::ImageRgba8(pixels);
        let luma = image.to_luma8();
        let mut cases = 0usize;
        for (template_width, template_height) in [(1u32, 1u32), (2, 3), (4, 4), (7, 5), (16, 12)] {
            for (x, y) in [(0u32, 0u32), (1, 2), (5, 7)] {
                if x + template_width > width || y + template_height > height {
                    continue;
                }
                let mut grayscale = Vec::with_capacity((template_width * template_height) as usize);
                for row in 0..template_height {
                    for column in 0..template_width {
                        grayscale.push(luma.get_pixel(x + column, y + row).0[0]);
                    }
                }
                let assertion = SandboxAssertion::Template {
                    x,
                    y,
                    width: (width - x).min(128),
                    height: (height - y).min(128),
                    template_width,
                    template_height,
                    grayscale,
                    min_score: 1.0,
                };
                assertion.verify(&image).unwrap_or_else(|error| {
                    panic!("size={template_width}x{template_height} at ({x},{y}): {error}")
                });
                cases += 1;
            }
        }
        assert!(cases >= 10, "覆盖用例过少: {cases}");
    }

    #[test]
    fn invalid_inline_template_is_rejected_before_run_starts() {
        let (store, mut runner) = setup(vec![AtomicCommand::Capture], None);
        let result = runner.start(
            "demo",
            None,
            vec![StepAssertion {
                index: 0,
                assertion: SandboxAssertion::Template {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                    template_width: 2,
                    template_height: 2,
                    grayscale: vec![0; 3],
                    min_score: 0.9,
                },
            }],
        );
        assert!(result.is_err());
        assert!(store.runs().unwrap().is_empty());
    }

    #[test]
    fn dimensions_pixels_tolerance_and_bounds_are_verified() {
        let image = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            3,
            image::Rgba([10, 20, 30, 255]),
        ));
        SandboxAssertion::Dimensions {
            width: 2,
            height: 3,
        }
        .verify(&image)
        .unwrap();
        assert!(
            SandboxAssertion::Dimensions {
                width: 3,
                height: 2
            }
            .verify(&image)
            .is_err()
        );
        SandboxAssertion::Pixel {
            x: 1,
            y: 2,
            rgba: [12, 18, 32, 255],
            tolerance: 2,
        }
        .verify(&image)
        .unwrap();
        assert!(
            SandboxAssertion::Pixel {
                x: 2,
                y: 0,
                rgba: [0; 4],
                tolerance: 255
            }
            .verify(&image)
            .is_err()
        );
    }
}
