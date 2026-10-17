//! HTTP sandbox sessions use simulated fixtures or the injected live UI runtime.
use crate::debug_sandbox::{
    AtomicCommand, Capability, CapabilityPolicy, PluginApiVersion, SandboxFixture, SandboxLimits,
    SandboxRun, SandboxTask,
};
use crate::debug_sandbox_journal::SandboxJournal;
use crate::debug_sandbox_runner::{
    DebugSandboxRunner, LiveExecutor, OcrTextRecognizer, ReadOnlyExecutor, SandboxExecutor,
    SimulatedExecutor, StepAssertion,
};
use crate::debug_sandbox_store::DebugSandboxStore;
use crate::interfaces::http::{HttpCommandError, HttpDebugRunRequest};
use crate::runtime::ui::UiRuntimeHandle;
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::sync::Mutex;

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0] as u32;
        let b = chunk.get(1).copied().unwrap_or(0) as u32;
        let c = chunk.get(2).copied().unwrap_or(0) as u32;
        let value = (a << 16) | (b << 8) | c;
        out.push(TABLE[((value >> 18) & 63) as usize] as char);
        out.push(TABLE[((value >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(value & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 与 base64_encode 配套的标准字母表解码（含 padding），输入非法时返回 None。
pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn symbol_value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let cleaned: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(cleaned.len() / 4 * 3);
    for chunk in cleaned.chunks(4) {
        let mut accumulator = 0u32;
        let mut symbols = 0u32;
        for &byte in chunk {
            if byte == b'=' {
                break;
            }
            accumulator = (accumulator << 6) | symbol_value(byte)?;
            symbols += 1;
        }
        if !(2..=4).contains(&symbols) {
            return None;
        }
        accumulator <<= 6 * (4 - symbols);
        out.extend_from_slice(&accumulator.to_be_bytes()[1..symbols as usize]);
    }
    Some(out)
}

fn png_base64(image: &image::DynamicImage) -> Result<String, String> {
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(base64_encode(bytes.get_ref()))
}

/// 把一张 base64 PNG 缩放到 scale 倍，返回 (新 base64, 新宽, 新高)。
/// 目标尺寸与原图相同时返回 None（不需要缩放）。
fn rescale_png_base64(encoded: &str, scale: f64) -> Result<Option<(String, u32, u32)>, String> {
    let bytes = base64_decode(encoded).ok_or_else(|| "归档帧不是合法 base64".to_owned())?;
    let image = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let (width, height) = (image.width(), image.height());
    let target_width = ((f64::from(width) * scale).round() as u32).max(1);
    let target_height = ((f64::from(height) * scale).round() as u32).max(1);
    if (target_width, target_height) == (width, height) {
        return Ok(None);
    }
    // Triangle 足够：归档帧是给人/AI 看界面用的，不必追求缩放锐度。
    let resized = image.resize_exact(
        target_width,
        target_height,
        image::imageops::FilterType::Triangle,
    );
    Ok(Some((png_base64(&resized)?, target_width, target_height)))
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadOnlyRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReadOnlyReadRequest {
    #[serde(default)]
    pub label: String,
    #[serde(default = "read_only_ocr_default")]
    pub ocr: bool,
    pub rect: Option<ReadOnlyRect>,
    #[serde(default)]
    pub response: ReadOnlyResponse,
}
#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ReadOnlyResponse {
    #[default]
    Full,
    Metadata,
}
fn read_only_ocr_default() -> bool {
    true
}
fn read_only_ui_summary(
    observation: Option<crate::runtime::ui::UiStateObservation>,
) -> serde_json::Value {
    use crate::runtime::ui::UiStateObservation;
    match observation {
        Some(UiStateObservation::Classified(state)) => serde_json::json!({
            "kind":state.classification().kind().name(), "current":state.classification().kind().name(),
            "stable":state.stable_kind().is_some(), "lastStableKind":state.last_stable_kind().map(|kind|kind.name()),
            "frameId":state.frame_id(), "atMs":null,"ageMillis":null,
            "timeMissingReason":"ui_observation_does_not_store_capture_time"
        }),
        Some(UiStateObservation::Failed {
            frame_id,
            last_stable_kind,
            ..
        }) => serde_json::json!({
            "kind":null,"current":null,"stable":null,"lastStableKind":last_stable_kind.map(|kind|kind.name()),
            "frameId":frame_id,"atMs":null,"ageMillis":null,"missingReason":"classification_failed"
        }),
        None => serde_json::json!({"kind":null,"current":null,"stable":null,"lastStableKind":null,
            "frameId":null,"atMs":null,"ageMillis":null,"missingReason":"no_ui_observation"}),
    }
}

struct ReadOnlyFence;
impl crate::runtime::ui::sealed::UiRoutineSealed for ReadOnlyFence {}
impl crate::runtime::ui::UiRoutine for ReadOnlyFence {
    type Output = anyhow::Result<()>;
    fn execute(self, context: &mut crate::runtime::ui::UiRoutineContext<'_>) -> Self::Output {
        anyhow::ensure!(!context.is_cancelled(), "只读切换围栏已取消");
        Ok(())
    }
}
struct ActiveReadRoutine;
impl crate::runtime::ui::sealed::UiRoutineSealed for ActiveReadRoutine {}
impl crate::runtime::ui::UiRoutine for ActiveReadRoutine {
    type Output = anyhow::Result<crate::runtime::ui::CapturedFrame>;
    fn execute(self, context: &mut crate::runtime::ui::UiRoutineContext<'_>) -> Self::Output {
        context.capture_frame()
    }
}

enum SandboxSession {
    Simulated(DebugSandboxRunner<SimulatedExecutor>),
    Live(DebugSandboxRunner<LiveExecutor>),
    ReadOnly(DebugSandboxRunner<ReadOnlyExecutor>),
}

impl SandboxSession {
    fn evidence(&self, id: &str) -> Result<&[crate::debug_sandbox_runner::StepEvidence], String> {
        match self {
            Self::Simulated(runner) => runner.evidence(id),
            Self::Live(runner) => runner.evidence(id),
            Self::ReadOnly(runner) => runner.evidence(id),
        }
    }
}

fn evidence_values(
    items: &[crate::debug_sandbox_runner::StepEvidence],
) -> Result<Vec<serde_json::Value>, String> {
    items.iter().map(|e| {
        let (width, height, image_png_base64) = match e.image.as_ref() {
            Some(image) => (image.width(), image.height(), Some(png_base64(image)?)),
            None => (0, 0, None),
        };
        Ok(serde_json::json!({"index":e.result.index,"command":e.command,"assertions":e.assertions,"result":e.result,"captured":e.image.is_some(),"width":width,"height":height,"imagePngBase64":image_png_base64}))
    }).collect()
}

#[derive(Default)]
struct EvidenceArchive {
    entries: HashMap<String, Vec<serde_json::Value>>,
    order: VecDeque<String>,
}

impl EvidenceArchive {
    fn insert(&mut self, id: String, evidence: Vec<serde_json::Value>) {
        if self.order.len() == 8 {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.order.push_back(id.clone());
        self.entries.insert(id, evidence);
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplayRecipe {
    task: SandboxTask,
    fixture: SandboxFixture,
    assertions: Vec<StepAssertion>,
}

#[derive(Default)]
struct ReplayRecipes {
    entries: HashMap<String, ReplayRecipe>,
    order: VecDeque<String>,
}

impl ReplayRecipes {
    fn insert(&mut self, id: String, recipe: ReplayRecipe) {
        if self.order.len() == 32 {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
        self.order.push_back(id.clone());
        self.entries.insert(id, recipe);
    }
}

pub(crate) struct DebugHttpService {
    pub store: DebugSandboxStore,
    runtime: Option<UiRuntimeHandle>,
    ocr: Option<OcrTextRecognizer>,
    playback_queue: Option<crate::debug_sandbox_runner::PlaybackQueueLengthReader>,
    playback_queue_clear: Option<crate::debug_sandbox_runner::PlaybackQueueClearer>,
    playback_queue_remove_front: Option<crate::debug_sandbox_runner::PlaybackQueueClearer>,
    task_queue_canceller: Option<crate::debug_sandbox_runner::TaskQueueCanceller>,
    playback_controller: Option<crate::debug_sandbox_runner::PlaybackController>,
    song_requester: Option<crate::debug_sandbox_runner::SongRequester>,
    chat_sender: Option<crate::debug_sandbox_runner::ChatSender>,
    startup_launcher: Option<crate::debug_sandbox_runner::StartupLauncher>,
    runners: Mutex<HashMap<String, SandboxSession>>,
    archive: Mutex<EvidenceArchive>,
    recipes: Mutex<ReplayRecipes>,
    journal: Option<SandboxJournal>,
}
impl Default for DebugHttpService {
    fn default() -> Self {
        Self::new(None)
    }
}

impl DebugHttpService {
    pub(crate) fn with_ocr(mut self, recognizer: OcrTextRecognizer) -> Self {
        self.ocr = Some(recognizer);
        self
    }

    pub(crate) fn with_playback_queue_reader(
        mut self,
        reader: crate::debug_sandbox_runner::PlaybackQueueLengthReader,
    ) -> Self {
        self.playback_queue = Some(reader);
        self
    }

    pub(crate) fn with_playback_queue_clearer(
        mut self,
        clearer: crate::debug_sandbox_runner::PlaybackQueueClearer,
    ) -> Self {
        self.playback_queue_clear = Some(clearer);
        self
    }

    pub(crate) fn with_playback_queue_remove_front(
        mut self,
        remover: crate::debug_sandbox_runner::PlaybackQueueClearer,
    ) -> Self {
        self.playback_queue_remove_front = Some(remover);
        self
    }

    pub(crate) fn with_task_queue_canceller(
        mut self,
        canceller: crate::debug_sandbox_runner::TaskQueueCanceller,
    ) -> Self {
        self.task_queue_canceller = Some(canceller);
        self
    }

    pub(crate) fn with_playback_controller(
        mut self,
        controller: crate::debug_sandbox_runner::PlaybackController,
    ) -> Self {
        self.playback_controller = Some(controller);
        self
    }

    pub(crate) fn with_song_requester(
        mut self,
        requester: crate::debug_sandbox_runner::SongRequester,
    ) -> Self {
        self.song_requester = Some(requester);
        self
    }

    pub(crate) fn with_chat_sender(
        mut self,
        sender: crate::debug_sandbox_runner::ChatSender,
    ) -> Self {
        self.chat_sender = Some(sender);
        self
    }

    pub(crate) fn with_startup_launcher(
        mut self,
        launcher: crate::debug_sandbox_runner::StartupLauncher,
    ) -> Self {
        self.startup_launcher = Some(launcher);
        self
    }

    fn read_only_executor(&self, runtime: UiRuntimeHandle) -> ReadOnlyExecutor {
        let executor = ReadOnlyExecutor::new(runtime);
        match &self.playback_queue {
            Some(reader) => executor.with_playback_queue_reader(reader.clone()),
            None => executor,
        }
    }

    pub(crate) fn set_read_only_mode(
        &self,
        enabled: bool,
        state: crate::debug_read_only::ReadOnlyState,
    ) -> Result<serde_json::Value, HttpCommandError> {
        let runners = self.runners.lock().map_err(error)?;
        if enabled
            && runners
                .values()
                .any(|session| matches!(session, SandboxSession::Live(_)))
        {
            return Err(error("请先完成或取消当前live沙盒运行，再开启只读模式"));
        }
        if enabled && let Some(runtime) = &self.runtime {
            // 旧live超时只请求取消；无输入围栏必须排在旧设备调用之后完成，才能承诺只读。
            runtime
                .submit(ReadOnlyFence)
                .map_err(error)?
                .wait_timeout(std::time::Duration::from_secs(10))
                .map_err(error)?
                .map_err(error)?;
        }
        Ok(crate::debug_read_only::observer().set_enabled_state(enabled, state))
    }

    pub(crate) fn read_active(
        &self,
        request: ReadOnlyReadRequest,
    ) -> Result<serde_json::Value, HttpCommandError> {
        let observer = crate::debug_read_only::observer();
        if !observer.enabled() || observer.state() != crate::debug_read_only::ReadOnlyState::Active
        {
            return Err(error("请开启只读模式的active主动读取状态"));
        }
        if request.label.len() > 256 {
            return Err(error("label最多256字节"));
        }
        let runtime = self
            .runtime
            .clone()
            .ok_or_else(|| error("只读 UI runtime未接线"))?;
        let frame = runtime
            .submit(ActiveReadRoutine)
            .map_err(error)?
            .wait_timeout(std::time::Duration::from_secs(30))
            .map_err(error)?
            .map_err(error)?;
        let image = frame.image();
        let rect = request.rect.unwrap_or(ReadOnlyRect {
            x: 0,
            y: 0,
            width: image.width(),
            height: image.height(),
        });
        if rect.width == 0
            || rect.height == 0
            || u64::from(rect.x) + u64::from(rect.width) > u64::from(image.width())
            || u64::from(rect.y) + u64::from(rect.height) > u64::from(image.height())
        {
            return Err(error("rect必须为原始截图范围内的非空区域"));
        }
        let ocr = if request.ocr {
            match &self.ocr {
                Some(recognizer) => {
                    match recognizer(&image.crop_imm(rect.x, rect.y, rect.width, rect.height)) {
                        Ok(text) => {
                            serde_json::json!({"available":true,"text":text,"rect":rect,"coordinateSpace":"rawGame"})
                        }
                        Err(reason) => {
                            serde_json::json!({"available":true,"error":reason,"rect":rect})
                        }
                    }
                }
                None => serde_json::json!({"available":false,"reason":"OCR未接线"}),
            }
        } else {
            serde_json::json!({"requested":false})
        };
        let id = observer.record_active(std::sync::Arc::new(frame), serde_json::json!({"request":request,"ocr":ocr,"steps":["capture current game client without input","classify the same captured frame","OCR the requested source region if enabled","save evidence and export JSON plus original PNG"],"reproduce":{"method":"POST","path":"/debug/read-only/read","body":request},"scope":"diagnostic observation; no dispatch or input"})).map_err(error)?;
        match request.response {
            ReadOnlyResponse::Full => observer.read_entry(&id),
            ReadOnlyResponse::Metadata => observer.entry_metadata(&id),
        }
        .map_err(error)
    }

    /// Cached runtime state only: no UI submission, capture, inference or event publication.
    pub(crate) fn read_only_summary(&self) -> serde_json::Value {
        let mut summary = crate::debug_read_only::observer().summary_snapshot();
        summary["ui"] = read_only_ui_summary(
            self.runtime
                .as_ref()
                .and_then(|runtime| runtime.latest_ui_state()),
        );
        summary["queue"] = match &self.playback_queue {
            Some(reader) => match reader() {
                Ok(length) => serde_json::json!({"length":length}),
                Err(_) => {
                    serde_json::json!({"length":null,"missingReason":"queue_state_unavailable"})
                }
            },
            None => {
                serde_json::json!({"length":null,"missingReason":"queue_reader_not_configured"})
            }
        };
        summary
    }

    fn live_executor(&self, runtime: UiRuntimeHandle) -> LiveExecutor {
        let mut executor = LiveExecutor::new(runtime);
        if let Some(reader) = &self.playback_queue {
            executor = executor.with_playback_queue_reader(reader.clone());
        }
        if let Some(clearer) = &self.playback_queue_clear {
            executor = executor.with_playback_queue_clearer(clearer.clone());
        }
        if let Some(remover) = &self.playback_queue_remove_front {
            executor = executor.with_playback_queue_remove_front(remover.clone());
        }
        if let Some(canceller) = &self.task_queue_canceller {
            executor = executor.with_task_queue_canceller(canceller.clone());
        }
        if let Some(controller) = &self.playback_controller {
            executor = executor.with_playback_controller(controller.clone());
        }
        if let Some(requester) = &self.song_requester {
            executor = executor.with_song_requester(requester.clone());
        }
        if let Some(sender) = &self.chat_sender {
            executor = executor.with_chat_sender(sender.clone());
        }
        if let Some(launcher) = &self.startup_launcher {
            executor = executor.with_startup_launcher(launcher.clone());
        }
        executor
    }

    fn runner<E: SandboxExecutor>(&self, executor: E) -> DebugSandboxRunner<E> {
        let runner = DebugSandboxRunner::new(self.store.clone(), executor);
        match &self.ocr {
            Some(ocr) => runner.with_ocr(ocr.clone()),
            None => runner,
        }
    }

    pub(crate) fn new(runtime: Option<UiRuntimeHandle>) -> Self {
        Self {
            store: DebugSandboxStore::new(SandboxLimits::default()),
            runtime,
            ocr: None,
            playback_queue: None,
            playback_queue_clear: None,
            playback_queue_remove_front: None,
            task_queue_canceller: None,
            playback_controller: None,
            song_requester: None,
            chat_sender: None,
            startup_launcher: None,
            runners: Mutex::new(HashMap::new()),
            archive: Mutex::new(EvidenceArchive::default()),
            recipes: Mutex::new(ReplayRecipes::default()),
            journal: None,
        }
    }

    pub(crate) fn with_journal(
        runtime: Option<UiRuntimeHandle>,
        path: &std::path::Path,
    ) -> Result<Self, String> {
        let mut service = Self::new(runtime);
        let journal = SandboxJournal::open(path)?;
        for task in journal.tasks()? {
            service.store.save_task(task).map_err(|e| e.to_string())?;
        }
        service.journal = Some(journal);
        Ok(service)
    }
}
fn error(e: impl std::fmt::Display) -> HttpCommandError {
    HttpCommandError::bad_request(e.to_string())
}
impl DebugHttpService {
    pub fn save(&self, task: SandboxTask) -> Result<String, HttpCommandError> {
        self.store
            .save_task_with(task, |task| {
                self.journal
                    .as_ref()
                    .map_or(Ok(()), |journal| journal.save_task(task))
            })
            .map(|v| format!("{v:?}"))
            .map_err(error)
    }
    pub fn execute_direct(&self, command: AtomicCommand) -> Result<SandboxRun, HttpCommandError> {
        if !crate::debug_read_only::observer().enabled() && !crate::debug_mode::enabled() {
            return Err(error("请先启用调试模式"));
        }
        let runtime = self
            .runtime
            .clone()
            .ok_or_else(|| error("live UI runtime未接线"))?;
        let task = SandboxTask {
            id: format!("direct:{}", uuid::Uuid::new_v4()),
            version: 1,
            description: String::new(),
            commands: vec![command],
            // 直接命令默认不截图：只有命令本身就是 capture 时才拍。
            // 每条命令都强制截图会让单次调试命令付出约 1.6s（截图 + UI 状态分类），
            // 而绝大多数命令并不需要画面。需要画面的调用方应显式发 capture。
            capture_after_each: false,
        };
        let run_id = {
            let mut runners = self.runners.lock().map_err(error)?;
            self.ensure_session_capacity(&mut runners)?;
            let run = if crate::debug_read_only::observer().enabled() {
                let mut runner = self.runner(self.read_only_executor(runtime));
                let run = runner.start_snapshot(task, Vec::new()).map_err(error)?;
                runners.insert(run.run_id.clone(), SandboxSession::ReadOnly(runner));
                run
            } else {
                if !crate::debug_mode::enabled() {
                    return Err(error("请先启用调试模式"));
                }
                let mut runner = self.runner(self.live_executor(runtime));
                let run = runner.start_snapshot(task, Vec::new()).map_err(error)?;
                runners.insert(run.run_id.clone(), SandboxSession::Live(runner));
                run
            };
            crate::debug_events::bus().publish(
                "run.started",
                serde_json::json!({"runId": run.run_id, "source": "direct"}),
            );
            run.run_id
        };
        self.advance(&run_id, true)
    }

    pub fn start(&self, r: HttpDebugRunRequest) -> Result<SandboxRun, HttpCommandError> {
        match (&r.plugin_id, &r.manifest) {
            (Some(plugin_id), Some(manifest)) => {
                if plugin_id != &manifest.plugin_id {
                    return Err(error("pluginId与manifest.pluginId不一致"));
                }
                let policy = CapabilityPolicy {
                    allowed: [
                        Capability::Observe,
                        Capability::Input,
                        Capability::Task,
                        Capability::Assert,
                    ]
                    .into_iter()
                    .collect(),
                    debug_mode: crate::debug_mode::enabled(),
                };
                manifest
                    .authorize(Capability::Task, PluginApiVersion::new(1, 0), &policy)
                    .map_err(error)?;
                if !r.assertions.is_empty() {
                    manifest
                        .authorize(Capability::Assert, PluginApiVersion::new(1, 0), &policy)
                        .map_err(error)?;
                }
                if r.mode == "readOnly" {
                    manifest
                        .authorize(Capability::Observe, PluginApiVersion::new(1, 0), &policy)
                        .map_err(error)?;
                }
                if r.mode == "live" {
                    manifest
                        .authorize(Capability::Input, PluginApiVersion::new(1, 0), &policy)
                        .map_err(error)?;
                }
            }
            (Some(_), None) => return Err(error("pluginId请求必须包含manifest")),
            (None, Some(_)) => return Err(error("manifest必须与pluginId一起提供")),
            (None, None) => {}
        }
        let task = self
            .store
            .task(&r.task_id)
            .map_err(error)?
            .ok_or_else(|| error("任务不存在"))?;
        if r.expected_version
            .is_some_and(|version| version != task.version)
        {
            return Err(error("任务版本冲突"));
        }
        let pinned_version = Some(task.version);
        let mut runners = self.runners.lock().map_err(error)?;
        self.ensure_session_capacity(&mut runners)?;
        let run = match r.mode.as_str() {
            "simulated" => {
                let f = r.fixture.ok_or_else(|| error("simulated需要fixture"))?;
                if f.width == 0
                    || f.height == 0
                    || u64::from(f.width) * u64::from(f.height) > 1_048_576
                {
                    return Err(error("fixture必须为1至1048576像素"));
                }
                let reserved =
                    u64::from(f.width) * u64::from(f.height) * 4 * (task.commands.len() as u64 + 1);
                if reserved > 8 * 1024 * 1024 {
                    return Err(error("模拟证据容量上限：每运行8MiB"));
                }
                let replay_fixture = f.clone();
                let replay_assertions = r.assertions.clone();
                let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                    f.width,
                    f.height,
                    image::Rgba(f.rgba),
                ));
                let mut runner = self.runner(SimulatedExecutor::with_fixture(image));
                let run = runner
                    .start(&r.task_id, pinned_version, r.assertions)
                    .map_err(error)?;
                self.recipes.lock().map_err(error)?.insert(
                    run.run_id.clone(),
                    ReplayRecipe {
                        task: self.store.run_task(&run.run_id).map_err(error)?,
                        fixture: replay_fixture,
                        assertions: replay_assertions,
                    },
                );
                runners.insert(run.run_id.clone(), SandboxSession::Simulated(runner));
                run
            }
            "readOnly" => {
                if r.fixture.is_some() {
                    return Err(error("readOnly运行不接受fixture"));
                }
                let runtime = self
                    .runtime
                    .clone()
                    .ok_or_else(|| error("只读 UI runtime未接线"))?;
                let mut runner = self.runner(self.read_only_executor(runtime));
                let run = runner
                    .start(&r.task_id, pinned_version, r.assertions)
                    .map_err(error)?;
                runners.insert(run.run_id.clone(), SandboxSession::ReadOnly(runner));
                run
            }
            "live" => {
                if crate::debug_read_only::observer().enabled() {
                    return Err(error("只读模式禁止live写运行，请使用readOnly"));
                }
                if !crate::debug_mode::enabled() {
                    return Err(error("live运行需要先启用调试模式"));
                }
                if r.fixture.is_some() {
                    return Err(error("live运行不接受fixture"));
                }
                let runtime = self
                    .runtime
                    .clone()
                    .ok_or_else(|| error("live UI runtime未接线"))?;
                let mut runner = self.runner(self.live_executor(runtime));
                let run = runner
                    .start(&r.task_id, pinned_version, r.assertions)
                    .map_err(error)?;
                runners.insert(run.run_id.clone(), SandboxSession::Live(runner));
                run
            }
            _ => return Err(error("mode必须是simulated、live或readOnly")),
        };
        Ok(run)
    }
    fn ensure_session_capacity(
        &self,
        runners: &mut HashMap<String, SandboxSession>,
    ) -> Result<(), HttpCommandError> {
        let completed: Vec<_> = runners
            .keys()
            .filter_map(|id| match self.store.run(id) {
                Ok(Some(run)) if run.status != crate::debug_sandbox::SandboxStatus::Running => {
                    Some(Ok(id.clone()))
                }
                Ok(Some(_)) => None,
                Ok(None) => Some(Err(error("运行记录不存在"))),
                Err(e) => Some(Err(error(e))),
            })
            .collect::<Result<_, _>>()?;
        for id in completed {
            let run = self
                .store
                .run(&id)
                .map_err(error)?
                .ok_or_else(|| error("运行不存在"))?;
            let session = runners.get(&id).ok_or_else(|| error("运行不存在"))?;
            self.archive_completed(&id, &run, session)?;
            runners.remove(&id);
            if self.journal.is_some() {
                self.store.release_completed(&id).map_err(error)?;
            }
        }
        if runners.len() >= 8 {
            return Err(error("运行会话容量上限：8个运行"));
        }
        Ok(())
    }

    pub fn replay(&self, source_id: &str) -> Result<SandboxRun, HttpCommandError> {
        let cached = self
            .recipes
            .lock()
            .map_err(error)?
            .entries
            .get(source_id)
            .cloned();
        let recipe = match cached {
            Some(recipe) => recipe,
            None => {
                let record = self
                    .archived_record(source_id)?
                    .ok_or_else(|| error("模拟回放配方不存在或已过期；live运行不可回放"))?;
                serde_json::from_value::<ReplayRecipe>(record["replayRecipe"].clone())
                    .map_err(|_| error("归档不含模拟回放配方；live运行不可回放"))?
            }
        };
        let mut runners = self.runners.lock().map_err(error)?;
        self.ensure_session_capacity(&mut runners)?;
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            recipe.fixture.width,
            recipe.fixture.height,
            image::Rgba(recipe.fixture.rgba),
        ));
        let mut runner = self.runner(SimulatedExecutor::with_fixture(image));
        let run = runner
            .start_snapshot(recipe.task.clone(), recipe.assertions.clone())
            .map_err(error)?;
        self.recipes
            .lock()
            .map_err(error)?
            .insert(run.run_id.clone(), recipe);
        runners.insert(run.run_id.clone(), SandboxSession::Simulated(runner));
        Ok(run)
    }

    pub fn advance(&self, id: &str, all: bool) -> Result<SandboxRun, HttpCommandError> {
        let mut runners = self.runners.lock().map_err(error)?;
        let session = match runners.get_mut(id) {
            Some(session) => session,
            None => {
                let run = match self.store.run(id).map_err(error)? {
                    Some(run) => run,
                    None => return self.archived_run(id)?.ok_or_else(|| error("运行不存在")),
                };
                return if run.status == crate::debug_sandbox::SandboxStatus::Running {
                    Err(error("运行会话不存在"))
                } else {
                    Ok(run)
                };
            }
        };
        let result = match session {
            SandboxSession::Simulated(runner) => {
                if all {
                    runner.run_all(id)
                } else {
                    runner.step(id)
                }
            }
            SandboxSession::Live(runner) => {
                if all {
                    runner.run_all(id)
                } else {
                    runner.step(id)
                }
            }
            SandboxSession::ReadOnly(runner) => {
                if all {
                    runner.run_all(id)
                } else {
                    runner.step(id)
                }
            }
        };
        let run = result.map_err(error)?;
        if run.status != crate::debug_sandbox::SandboxStatus::Running {
            self.archive_completed(id, &run, session)?;
            runners.remove(id);
            if self.journal.is_some() {
                self.store.release_completed(id).map_err(error)?;
            }
            // 推送终态：等运行结束的外部调用方不必再轮询 /debug/run/status。
            crate::debug_events::bus().publish(
                "run.finished",
                serde_json::json!({
                    "runId": run.run_id,
                    "taskId": run.task_id,
                    "taskVersion": run.task_version,
                    "status": run.status,
                }),
            );
        }
        Ok(run)
    }

    fn archive_completed(
        &self,
        id: &str,
        run: &SandboxRun,
        session: &SandboxSession,
    ) -> Result<(), HttpCommandError> {
        let evidence = evidence_values(session.evidence(id).map_err(error)?).map_err(error)?;
        if let Some(journal) = &self.journal {
            let task = self.store.run_task(id).map_err(error)?;
            let recipe = self.recipes.lock().map_err(error)?.entries.get(id).cloned();
            let mut record = serde_json::json!({"run":run, "task":task, "evidence":evidence, "replayRecipe":recipe});
            if !Self::archive_record_fits(&record)? {
                // 第一步：优先缩小而不是丢帧——缩小保留全部步骤的画面，丢帧则永久丢掉某一步。
                let downscaled = Self::downscale_archived_frames(&mut record)?;
                if downscaled > 0 {
                    record["evidenceDownscaled"] = serde_json::json!(true);
                    record["downscaledImageFrames"] = serde_json::json!(downscaled);
                    log::warn!(
                        "沙盒归档超出 12 MiB 上限，已把 {downscaled} 帧缩小以保留全部步骤: run={id}"
                    );
                }
            }
            if !Self::archive_record_fits(&record)? {
                // 第二步：缩到下限仍放不下，才回退到丢帧。
                let dropped = Self::drop_archived_frames(&mut record)?;
                record["evidenceTruncated"] = serde_json::json!(true);
                record["droppedImageFrames"] = serde_json::json!(dropped);
                if !Self::archive_record_fits(&record)? {
                    return Err(error(
                        "sandbox archive record exceeds 12 MiB even after dropping captured frames",
                    ));
                }
                log::warn!(
                    "沙盒归档超出 12 MiB 上限，已丢弃 {dropped} 帧截图并保留命令、结果与断言: run={id}"
                );
            }
            journal.save(id, &record).map_err(error)?;
        } else {
            self.archive
                .lock()
                .map_err(error)?
                .insert(id.to_owned(), evidence);
        }
        Ok(())
    }

    fn archive_record_fits(record: &serde_json::Value) -> Result<bool, HttpCommandError> {
        let json = serde_json::to_string(record).map_err(error)?;
        Ok(json.len() <= crate::debug_sandbox_journal::MAX_RECORD_BYTES)
    }

    /// Drops archived frame images newest-first until the record fits the archive limit.
    /// Command, result and assertion metadata is preserved so the audit trail stays intact,
    /// and each affected entry is marked with `imageDropped` instead of silently losing data.
    fn drop_archived_frames(record: &mut serde_json::Value) -> Result<usize, HttpCommandError> {
        let mut dropped = 0usize;
        while !Self::archive_record_fits(record)? {
            let removed = {
                let Some(entries) = record["evidence"].as_array_mut() else {
                    break;
                };
                let Some(entry) = entries.iter_mut().rev().find(|entry| {
                    entry
                        .get("imagePngBase64")
                        .is_some_and(|value| !value.is_null())
                }) else {
                    break;
                };
                match entry.as_object_mut() {
                    Some(object) => {
                        object.remove("imagePngBase64");
                        object.insert("imageDropped".to_owned(), serde_json::json!(true));
                        true
                    }
                    None => false,
                }
            };
            if !removed {
                break;
            }
            dropped += 1;
        }
        Ok(dropped)
    }

    /// 收集归档里所有带画面的证据条目：(证据下标, base64, 当前宽, 当前高)。
    fn archived_frames(record: &serde_json::Value) -> Vec<(usize, String, u32, u32)> {
        record["evidence"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .enumerate()
                    .filter_map(|(position, entry)| {
                        let encoded = entry.get("imagePngBase64")?.as_str()?.to_owned();
                        let width = entry
                            .get("width")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0) as u32;
                        let height = entry
                            .get("height")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0) as u32;
                        Some((position, encoded, width, height))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 写回某条证据的画面。width/height 始终描述"归档后实际返回的那张图"，
    /// 原尺寸另存为 imageOriginalWidth/Height，避免元数据与内容不一致。
    fn write_archived_frame(
        record: &mut serde_json::Value,
        position: usize,
        encoded: String,
        width: u32,
        height: u32,
        scaled_from: Option<(u32, u32, f64)>,
    ) {
        let Some(object) = record["evidence"]
            .as_array_mut()
            .and_then(|entries| entries.get_mut(position))
            .and_then(|entry| entry.as_object_mut())
        else {
            return;
        };
        object.insert("imagePngBase64".to_owned(), serde_json::json!(encoded));
        object.insert("width".to_owned(), serde_json::json!(width));
        object.insert("height".to_owned(), serde_json::json!(height));
        match scaled_from {
            Some((original_width, original_height, scale)) => {
                object.insert("imageDownscaled".to_owned(), serde_json::json!(true));
                object.insert("imageScale".to_owned(), serde_json::json!(scale));
                object.insert(
                    "imageOriginalWidth".to_owned(),
                    serde_json::json!(original_width),
                );
                object.insert(
                    "imageOriginalHeight".to_owned(),
                    serde_json::json!(original_height),
                );
            }
            None => {
                for key in [
                    "imageDownscaled",
                    "imageScale",
                    "imageOriginalWidth",
                    "imageOriginalHeight",
                ] {
                    object.remove(key);
                }
            }
        }
    }

    /// 归档超限时**先缩小、后丢帧**。
    ///
    /// 丢帧会永久丢掉那一步的画面；缩小至少保留全部步骤的可视信息，只是分辨率下降。
    /// 逐级尝试到下限仍放不下时，把画面恢复成原尺寸再交回丢帧逻辑，
    /// 既避免"缩小和丢帧同时生效"，也不谎报缩小过。
    fn downscale_archived_frames(
        record: &mut serde_json::Value,
    ) -> Result<usize, HttpCommandError> {
        const SCALES: [f64; 3] = [0.5, 0.25, 0.125];
        let originals = Self::archived_frames(record);
        if originals.is_empty() {
            return Ok(0);
        }
        for scale in SCALES {
            let mut applied = 0usize;
            for (position, encoded, width, height) in &originals {
                let Some((rescaled, new_width, new_height)) =
                    rescale_png_base64(encoded, scale).map_err(error)?
                else {
                    continue;
                };
                Self::write_archived_frame(
                    record,
                    *position,
                    rescaled,
                    new_width,
                    new_height,
                    Some((*width, *height, scale)),
                );
                applied += 1;
            }
            if applied == 0 {
                break;
            }
            if Self::archive_record_fits(record)? {
                return Ok(applied);
            }
        }
        for (position, encoded, width, height) in &originals {
            Self::write_archived_frame(record, *position, encoded.clone(), *width, *height, None);
        }
        Ok(0)
    }

    pub fn status_snapshot(&self, id: &str) -> Result<serde_json::Value, HttpCommandError> {
        if let Some(run) = self.store.run(id).map_err(error)? {
            return Ok(
                serde_json::json!({"run":run,"task":self.store.run_task(id).map_err(error)?}),
            );
        }
        self.archived_record(id)?
            .ok_or_else(|| error("运行不存在或归档已过期"))
    }

    fn archived_run(&self, id: &str) -> Result<Option<SandboxRun>, HttpCommandError> {
        self.archived_record(id)?
            .map(|record| serde_json::from_value(record["run"].clone()).map_err(error))
            .transpose()
    }

    fn archived_record(&self, id: &str) -> Result<Option<serde_json::Value>, HttpCommandError> {
        self.journal
            .as_ref()
            .map(|journal| journal.load(id).map_err(error))
            .transpose()
            .map(|record| record.flatten())
    }

    pub fn evidence(
        &self,
        id: &str,
        index: Option<usize>,
    ) -> Result<Vec<serde_json::Value>, HttpCommandError> {
        let runners = self.runners.lock().map_err(error)?;
        let values = if let Some(session) = runners.get(id) {
            evidence_values(session.evidence(id).map_err(error)?).map_err(error)?
        } else {
            if let Some(values) = self.archive.lock().map_err(error)?.entries.get(id).cloned() {
                values
            } else {
                self.archived_record(id)?
                    .and_then(|record| record["evidence"].as_array().cloned())
                    .ok_or_else(|| error("运行证据不存在或已过期"))?
            }
        };
        Ok(values
            .into_iter()
            .filter(|e| index.is_none_or(|i| e["index"].as_u64() == Some(i as u64)))
            .collect())
    }

    pub fn tasks(&self) -> Result<Vec<SandboxTask>, HttpCommandError> {
        self.store.tasks().map_err(error)
    }

    pub fn runs(&self) -> Result<Vec<serde_json::Value>, HttpCommandError> {
        let current = self.store.runs().map_err(error)?;
        let current_ids: std::collections::HashSet<_> =
            current.iter().map(|run| run.run_id.as_str()).collect();
        let mut runs: Vec<_> = current
            .iter()
            .map(|run| serde_json::to_value(run).map_err(error))
            .collect::<Result<_, _>>()?;
        if let Some(journal) = &self.journal {
            for archived in journal.runs().map_err(error)? {
                if archived["runId"]
                    .as_str()
                    .is_some_and(|id| !current_ids.contains(id))
                {
                    runs.push(archived);
                }
            }
        }
        Ok(runs)
    }

    pub fn run_task(&self, id: &str) -> Result<SandboxTask, HttpCommandError> {
        if self.store.run(id).map_err(error)?.is_some() {
            return self.store.run_task(id).map_err(error);
        }
        let record = self
            .archived_record(id)?
            .ok_or_else(|| error("运行不存在"))?;
        serde_json::from_value(record["task"].clone()).map_err(error)
    }

    pub fn status(&self, id: &str) -> Result<SandboxRun, HttpCommandError> {
        match self.store.run(id).map_err(error)? {
            Some(run) => Ok(run),
            None => self.archived_run(id)?.ok_or_else(|| error("运行不存在")),
        }
    }

    pub fn cancel(&self, id: &str) -> Result<SandboxRun, HttpCommandError> {
        // Cancellation only marks shared store state, so it remains responsive while a live UI command waits.
        self.store.cancel_run(id).map_err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn read_only_summary_does_not_capture_ocr_push_or_change_mode() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct NoCapture(Arc<AtomicUsize>);
        impl crate::runtime::ui::UiDevice for NoCapture {
            fn capture(&mut self) -> anyhow::Result<image::DynamicImage> {
                self.0.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("summary must never request capture")
            }
        }
        let captures = Arc::new(AtomicUsize::new(0));
        let inferences = Arc::new(AtomicUsize::new(0));
        let ocr_calls = inferences.clone();
        let runtime = crate::runtime::ui::UiRuntime::start(NoCapture(captures.clone()), 4).unwrap();
        let service = DebugHttpService::new(Some(runtime.handle()))
            .with_ocr(Arc::new(move |_| {
                ocr_calls.fetch_add(1, Ordering::SeqCst);
                Err("summary must not infer".into())
            }))
            .with_playback_queue_reader(Arc::new(|| Ok(7)));
        let observer = crate::debug_read_only::observer();
        let mode = observer.state_snapshot();
        let sequence = observer.bus().latest_sequence();
        for _ in 0..3 {
            let summary = service.read_only_summary();
            assert_eq!(summary["queue"]["length"], 7);
            assert_eq!(summary["ui"]["kind"], serde_json::Value::Null);
            assert_eq!(summary["ui"]["ageMillis"], serde_json::Value::Null);
            assert_eq!(summary["instance"], observer.instance());
            assert!(!summary.to_string().contains("imagePngBase64"));
            assert!(!summary.to_string().contains("fileSha256"));
        }
        assert_eq!(captures.load(Ordering::SeqCst), 0);
        assert_eq!(inferences.load(Ordering::SeqCst), 0);
        assert_eq!(observer.bus().latest_sequence(), sequence);
        assert_eq!(observer.state_snapshot(), mode);
        runtime.shutdown().unwrap();
        let missing = DebugHttpService::default().read_only_summary();
        assert_eq!(missing["queue"]["length"], serde_json::Value::Null);
        assert_eq!(missing["ui"]["stable"], serde_json::Value::Null);
    }

    #[test]
    fn active_response_dto_rejects_unknown_fields_and_preserves_full_default() {
        let legacy: ReadOnlyReadRequest =
            serde_json::from_value(serde_json::json!({"ocr":false})).unwrap();
        assert!(matches!(legacy.response, ReadOnlyResponse::Full));
        let metadata: ReadOnlyReadRequest =
            serde_json::from_value(serde_json::json!({"response":"metadata"})).unwrap();
        assert!(matches!(metadata.response, ReadOnlyResponse::Metadata));
        assert!(
            serde_json::from_value::<ReadOnlyReadRequest>(serde_json::json!({"response":"tiny"}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<ReadOnlyReadRequest>(serde_json::json!({"image":false}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<ReadOnlyReadRequest>(
                serde_json::json!({"rect":{"x":0,"y":0,"width":1,"height":1,"bad":true}})
            )
            .is_err()
        );
    }

    #[test]
    #[ignore = "global observation state; run isolated; no game input"]
    fn read_only_observation_preserves_normal_program_execution() {
        use crate::debug_read_only::{ReadOnlyState, observer};
        use crate::runtime::ui::{UiDevice, UiRoutine, UiRoutineContext, UiRuntime};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Restore(bool, ReadOnlyState);
        impl Drop for Restore {
            fn drop(&mut self) {
                observer().set_enabled_state(self.0, self.1);
            }
        }
        struct Device(Arc<AtomicUsize>);
        impl UiDevice for Device {
            fn capture(&mut self) -> anyhow::Result<image::DynamicImage> {
                anyhow::bail!("no capture requested")
            }
            fn press_key(&mut self, _key: enigo::Key) -> anyhow::Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
        struct NormalBusinessRoutine;
        impl crate::runtime::ui::sealed::UiRoutineSealed for NormalBusinessRoutine {}
        impl UiRoutine for NormalBusinessRoutine {
            type Output = anyhow::Result<()>;
            fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
                anyhow::ensure!(
                    crate::debug_mode::input_allowed(),
                    "normal program permission changed"
                );
                context.device().press_key(enigo::Key::Return)
            }
        }
        let _restore = Restore(observer().enabled(), observer().state());
        assert!(!crate::debug_mode::enabled());
        let generation = crate::debug_mode::generation();
        let writes = Arc::new(AtomicUsize::new(0));
        let runtime = UiRuntime::start(Device(writes.clone()), 8).unwrap();
        let service = DebugHttpService::new(Some(runtime.handle()));
        for state in [ReadOnlyState::Active, ReadOnlyState::Push] {
            service.set_read_only_mode(true, state).unwrap();
            assert!(
                service
                    .execute_direct(AtomicCommand::PressKey {
                        key: "Enter".into()
                    })
                    .is_err()
            );
            runtime
                .handle()
                .submit(NormalBusinessRoutine)
                .unwrap()
                .wait()
                .unwrap()
                .unwrap();
            assert!(!crate::debug_mode::enabled());
            assert_eq!(crate::debug_mode::generation(), generation);
        }
        assert_eq!(
            writes.load(Ordering::SeqCst),
            2,
            "normal business runs in both observation states"
        );
        runtime.shutdown().unwrap();
        // A stopped worker cannot finish the activation fence. Mode must remain disabled.
        observer().set_enabled_state(false, ReadOnlyState::Active);
        assert!(
            service
                .set_read_only_mode(true, ReadOnlyState::Active)
                .is_err()
        );
        assert!(!observer().enabled());
    }
    use crate::debug_sandbox::{AtomicCommand, SandboxFixture, SandboxStatus};

    /// 一张有噪声、压缩不掉的 PNG 帧，尺寸固定便于断言归档后的尺寸。
    fn noise_frame_base64(width: u32, height: u32) -> String {
        let mut pixels = image::RgbaImage::new(width, height);
        let mut state = 0x1234_5678u32;
        for pixel in pixels.pixels_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let byte = (state >> 24) as u8;
            *pixel = image::Rgba([byte, byte.wrapping_mul(3), byte.wrapping_add(97), 255]);
        }
        png_base64(&image::DynamicImage::ImageRgba8(pixels)).unwrap()
    }

    fn evidence_entry(index: usize, encoded: &str, width: u32, height: u32) -> serde_json::Value {
        serde_json::json!({
            "index": index,
            "command": "capture",
            "assertions": [],
            "result": {"index": index},
            "captured": true,
            "width": width,
            "height": height,
            "imagePngBase64": encoded,
        })
    }

    /// 归档超限时必须**先缩小、后丢帧**：缩小保住全部步骤，丢帧会永久丢掉某一步的画面。
    /// 断言不依赖任何具体帧大小，只依赖"缩得下就不许丢"这一条契约。
    #[test]
    fn archive_shrinks_frames_before_dropping_them() {
        let (width, height) = (480u32, 360u32);
        let encoded = noise_frame_base64(width, height);
        let mut evidence = Vec::new();
        let mut index = 0usize;
        loop {
            evidence.push(evidence_entry(index, &encoded, width, height));
            let record = serde_json::json!({"run": {}, "task": {}, "evidence": evidence});
            if !DebugHttpService::archive_record_fits(&record).unwrap() {
                break;
            }
            index += 1;
            assert!(index < 400, "无法构造出超限的归档记录");
        }
        let mut record = serde_json::json!({"run": {}, "task": {}, "evidence": evidence});
        let before = DebugHttpService::archived_frames(&record).len();
        assert!(before >= 2, "至少要有两帧才能说明问题");

        let downscaled = DebugHttpService::downscale_archived_frames(&mut record).unwrap();
        assert_eq!(downscaled, before, "所有帧都应当被缩小");
        assert!(
            DebugHttpService::archive_record_fits(&record).unwrap(),
            "缩小之后必须放得下"
        );
        assert_eq!(
            DebugHttpService::archived_frames(&record).len(),
            before,
            "缩小不该丢掉任何一步"
        );
        assert!(
            record["evidenceTruncated"].is_null(),
            "没有丢帧就不该标记截断"
        );
        for entry in record["evidence"].as_array().unwrap() {
            assert_eq!(entry["imageDownscaled"], true);
            assert_eq!(entry["imageOriginalWidth"], width);
            assert_eq!(entry["imageOriginalHeight"], height);
            // width/height 必须描述归档后实际返回的那张图，否则元数据在说谎。
            assert!(entry["width"].as_u64().unwrap() < u64::from(width));
            assert!(entry["height"].as_u64().unwrap() < u64::from(height));
        }
    }

    /// 缩图解决不了的时候必须如实回报 0，并把画面恢复成原尺寸，
    /// 既不能谎报缩小过，也不能和丢帧逻辑同时生效。
    #[test]
    fn archive_keeps_original_frames_when_shrinking_cannot_help() {
        let (width, height) = (480u32, 360u32);
        let encoded = noise_frame_base64(width, height);
        let mut record = serde_json::json!({
            "run": {},
            "task": {},
            // 大头在别处：缩小截图根本无济于事
            "padding": "x".repeat(crate::debug_sandbox_journal::MAX_RECORD_BYTES + 4096),
            "evidence": [evidence_entry(0, &encoded, width, height)],
        });
        assert!(!DebugHttpService::archive_record_fits(&record).unwrap());

        let downscaled = DebugHttpService::downscale_archived_frames(&mut record).unwrap();
        assert_eq!(downscaled, 0, "缩不进去时必须回报 0");
        let entry = &record["evidence"][0];
        assert_eq!(entry["width"], width);
        assert_eq!(entry["height"], height);
        assert!(entry["imageDownscaled"].is_null());
        assert!(entry["imageScale"].is_null());
        assert!(entry["imageOriginalWidth"].is_null());
        assert_eq!(entry["imagePngBase64"].as_str().unwrap(), encoded);
    }

    fn task() -> SandboxTask {
        SandboxTask {
            id: "http-task".into(),
            version: 1,
            description: "service test".into(),
            commands: vec![
                AtomicCommand::Capture,
                AtomicCommand::Wait { milliseconds: 1 },
            ],
            capture_after_each: true,
        }
    }

    fn request() -> HttpDebugRunRequest {
        HttpDebugRunRequest {
            task_id: "http-task".into(),
            mode: "simulated".into(),
            expected_version: Some(1),
            assertions: vec![],
            fixture: Some(SandboxFixture {
                width: 2,
                height: 2,
                rgba: [1, 2, 3, 255],
            }),
            plugin_id: None,
            manifest: None,
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a visible running game window on the desktop"]
    fn live_service_read_only_capture_smoke() {
        struct RestoreDebugMode(bool);
        impl Drop for RestoreDebugMode {
            fn drop(&mut self) {
                crate::debug_mode::set_enabled(self.0);
            }
        }
        let runtime = crate::runtime::ui::UiRuntime::start(
            crate::adapters::windows::WindowsUiDevice::new(crate::config::WindowConfig::default()),
            2,
        )
        .expect("start live UI worker");
        let _restore = RestoreDebugMode(crate::debug_mode::set_enabled(true));
        let service = DebugHttpService::new(Some(runtime.handle()));
        service
            .save(SandboxTask {
                id: "live-service-smoke".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Capture],
                capture_after_each: true,
            })
            .unwrap();
        let run = service
            .start(HttpDebugRunRequest {
                task_id: "live-service-smoke".into(),
                mode: "live".into(),
                expected_version: Some(1),
                assertions: vec![],
                fixture: None,
                plugin_id: None,
                manifest: None,
            })
            .unwrap();
        let done = service
            .advance(&run.run_id, true)
            .expect("capture live run");
        assert_eq!(
            done.status,
            SandboxStatus::Passed,
            "{:?}",
            done.results[0].error
        );
        assert_eq!(
            service.status(&run.run_id).unwrap().status,
            SandboxStatus::Passed
        );
        let evidence = service.evidence(&run.run_id, Some(0)).unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["captured"], true);
        assert!(evidence[0]["width"].as_u64().unwrap() > 0);
        assert!(evidence[0]["height"].as_u64().unwrap() > 0);
        assert!(
            evidence[0]["imagePngBase64"]
                .as_str()
                .is_some_and(|png| png.starts_with("iVBORw0KGgo"))
        );
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[test]
    fn cancellation_before_first_step_skips_commands_and_keeps_terminal_snapshot() {
        let service = DebugHttpService::default();
        service.save(task()).unwrap();
        let run = service.start(request()).unwrap();
        assert_eq!(
            service.cancel(&run.run_id).unwrap().status,
            SandboxStatus::Running
        );
        let done = service.advance(&run.run_id, true).unwrap();
        assert_eq!(done.status, SandboxStatus::Cancelled);
        assert!(done.results.is_empty());
        assert_eq!(
            service.status(&run.run_id).unwrap().status,
            SandboxStatus::Cancelled
        );
        assert!(service.evidence(&run.run_id, None).unwrap().is_empty());
        assert_eq!(
            service.advance(&run.run_id, false).unwrap().status,
            SandboxStatus::Cancelled
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "changes global debug mode while testing a missing window"]
    fn live_cancel_before_first_step_without_target_window() {
        struct RestoreDebugMode(bool);
        impl Drop for RestoreDebugMode {
            fn drop(&mut self) {
                crate::debug_mode::set_enabled(self.0);
            }
        }
        let window = crate::config::WindowConfig {
            target_process: format!("missing-debug-{}.exe", uuid::Uuid::new_v4()),
            ..crate::config::WindowConfig::default()
        };
        let runtime = crate::runtime::ui::UiRuntime::start(
            crate::adapters::windows::WindowsUiDevice::new(window),
            2,
        )
        .expect("start UI worker");
        let _restore = RestoreDebugMode(crate::debug_mode::set_enabled(true));
        let service = DebugHttpService::new(Some(runtime.handle()));
        service
            .save(SandboxTask {
                id: "cancel-before-live-capture".into(),
                version: 1,
                description: String::new(),
                commands: vec![AtomicCommand::Capture],
                capture_after_each: true,
            })
            .unwrap();
        let run = service
            .start(HttpDebugRunRequest {
                task_id: "cancel-before-live-capture".into(),
                mode: "live".into(),
                expected_version: Some(1),
                assertions: vec![],
                fixture: None,
                plugin_id: None,
                manifest: None,
            })
            .unwrap();
        service.cancel(&run.run_id).unwrap();
        let done = service.advance(&run.run_id, true).unwrap();
        assert_eq!(done.status, SandboxStatus::Cancelled);
        assert!(done.results.is_empty());
        assert!(service.evidence(&run.run_id, None).unwrap().is_empty());
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[test]
    fn plugin_run_with_assertions_requires_assert_capability() {
        use crate::debug_sandbox::{PluginApiVersion, PluginManifest};
        let service = DebugHttpService::default();
        service.save(task()).unwrap();
        let mut req = request();
        req.plugin_id = Some("plugin".into());
        req.manifest = Some(PluginManifest {
            manifest_version: 1,
            plugin_id: "plugin".into(),
            name: "Plugin".into(),
            version: "1".into(),
            api: PluginApiVersion::new(1, 0),
            capabilities: [Capability::Task].into_iter().collect(),
        });
        req.assertions = vec![StepAssertion {
            index: 0,
            assertion: crate::debug_sandbox_runner::SandboxAssertion::Dimensions {
                width: 2,
                height: 2,
            },
        }];
        assert!(
            service
                .start(req.clone())
                .unwrap_err()
                .to_string()
                .contains("Assert")
        );
        assert!(service.runs().unwrap().is_empty());
        req.manifest
            .as_mut()
            .unwrap()
            .capabilities
            .insert(Capability::Assert);
        let run = service.start(req).unwrap();
        assert_eq!(
            service.advance(&run.run_id, true).unwrap().status,
            SandboxStatus::Passed
        );
    }

    #[test]
    fn simulated_run_ocr_assertion_uses_injected_recognizer() {
        let service = DebugHttpService::default().with_ocr(std::sync::Arc::new(|crop| {
            assert_eq!((crop.width(), crop.height()), (1, 2));
            Ok("ready".into())
        }));
        service.save(task()).unwrap();
        let run = service
            .start(HttpDebugRunRequest {
                assertions: vec![StepAssertion {
                    index: 0,
                    assertion: crate::debug_sandbox_runner::SandboxAssertion::OcrContains {
                        x: 1,
                        y: 0,
                        width: 1,
                        height: 2,
                        text: "ready".into(),
                    },
                }],
                ..request()
            })
            .unwrap();
        assert_eq!(
            service.advance(&run.run_id, true).unwrap().status,
            SandboxStatus::Passed
        );
        assert!(
            service.evidence(&run.run_id, Some(0)).unwrap()[0]["imagePngBase64"]
                .as_str()
                .is_some()
        );
    }

    #[test]
    fn completed_run_status_and_png_survive_service_restart() {
        let path =
            std::env::temp_dir().join(format!("sandbox-service-{}.db", uuid::Uuid::new_v4()));
        let run_id = {
            let service = DebugHttpService::with_journal(None, &path).unwrap();
            service.save(task()).unwrap();
            let run = service
                .start(HttpDebugRunRequest {
                    assertions: vec![StepAssertion {
                        index: 0,
                        assertion: crate::debug_sandbox_runner::SandboxAssertion::Dimensions {
                            width: 2,
                            height: 2,
                        },
                    }],
                    ..request()
                })
                .unwrap();
            assert_eq!(
                service.advance(&run.run_id, true).unwrap().status,
                SandboxStatus::Passed
            );
            assert!(service.runners.lock().unwrap().is_empty());
            assert!(service.archive.lock().unwrap().entries.is_empty());
            assert!(service.store.run(&run.run_id).unwrap().is_none());
            assert_eq!(
                service.status(&run.run_id).unwrap().status,
                SandboxStatus::Passed
            );
            assert_eq!(
                service.advance(&run.run_id, false).unwrap().status,
                SandboxStatus::Passed
            );
            assert_eq!(service.run_task(&run.run_id).unwrap().version, 1);
            assert_eq!(service.runs().unwrap().len(), 1);
            assert!(service.evidence(&run.run_id, Some(0)).is_ok());
            let mut updated = task();
            updated.version = 2;
            service.save(updated).unwrap();
            run.run_id
        };
        {
            let service = DebugHttpService::with_journal(None, &path).unwrap();
            assert_eq!(service.tasks().unwrap()[0].version, 2);
            assert!(service.save(task()).is_err());
            assert_eq!(service.runs().unwrap().len(), 1);
            let snapshot = service.status_snapshot(&run_id).unwrap();
            assert_eq!(snapshot["run"]["status"], "passed");
            assert_eq!(snapshot["task"]["version"], 1);
            assert_eq!(snapshot["task"]["commands"][0], "capture");
            let evidence = service.evidence(&run_id, Some(0)).unwrap();
            assert_eq!(evidence.len(), 1);
            assert!(
                evidence[0]["imagePngBase64"]
                    .as_str()
                    .is_some_and(|png| !png.is_empty())
            );
            let replay = service.replay(&run_id).unwrap();
            assert_ne!(replay.run_id, run_id);
            assert_eq!(replay.task_version, 1);
            assert_eq!(
                service.advance(&replay.run_id, true).unwrap().status,
                SandboxStatus::Passed
            );
            assert_eq!(
                service.evidence(&replay.run_id, Some(0)).unwrap()[0]["assertions"],
                evidence[0]["assertions"]
            );
            assert_eq!(
                service.evidence(&replay.run_id, Some(0)).unwrap()[0]["imagePngBase64"],
                evidence[0]["imagePngBase64"]
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn replay_uses_original_task_fixture_and_assertions_after_update() {
        let service = DebugHttpService::default();
        service.save(task()).unwrap();
        let original = service
            .start(HttpDebugRunRequest {
                assertions: vec![StepAssertion {
                    index: 0,
                    assertion: crate::debug_sandbox_runner::SandboxAssertion::Dimensions {
                        width: 2,
                        height: 2,
                    },
                }],
                ..request()
            })
            .unwrap();
        assert_eq!(
            service.advance(&original.run_id, true).unwrap().status,
            SandboxStatus::Passed
        );
        let mut updated = task();
        updated.version = 2;
        updated.commands = vec![AtomicCommand::Wait { milliseconds: 0 }];
        service.save(updated).unwrap();
        let replay = service.replay(&original.run_id).unwrap();
        assert_ne!(replay.run_id, original.run_id);
        assert_eq!(replay.task_version, 1);
        assert_eq!(
            service.advance(&replay.run_id, true).unwrap().status,
            SandboxStatus::Passed
        );
        assert_eq!(service.status(&replay.run_id).unwrap().results.len(), 2);
        let source = service.evidence(&original.run_id, Some(0)).unwrap();
        let repeated = service.evidence(&replay.run_id, Some(0)).unwrap();
        assert_eq!(source[0]["imagePngBase64"], repeated[0]["imagePngBase64"]);
        assert_eq!(source[0]["assertions"], repeated[0]["assertions"]);
        assert!(service.replay("missing").is_err());
    }

    #[test]
    fn run_rejects_stale_version_even_without_a_live_device() {
        let service = DebugHttpService::default();
        service.save(task()).unwrap();
        let mut updated = task();
        updated.version = 2;
        service.save(updated).unwrap();
        assert!(service.start(request()).is_err());
        assert!(service.runs().unwrap().is_empty());
        let run = service
            .start(HttpDebugRunRequest {
                expected_version: None,
                ..request()
            })
            .unwrap();
        assert_eq!(run.task_version, 2);
    }

    #[test]
    fn direct_command_only_screenshots_when_asked() {
        use crate::runtime::ui::UiDevice;
        /// 任何一次截图都会让该步骤失败，因此"步骤通过"本身就证明没有截图。
        struct NoScreenshot;
        impl UiDevice for NoScreenshot {
            fn capture(&mut self) -> anyhow::Result<image::DynamicImage> {
                anyhow::bail!("unexpected screenshot")
            }
            fn close_window(&mut self) -> anyhow::Result<()> {
                Ok(())
            }
        }
        struct RestoreDebugMode(bool);
        impl Drop for RestoreDebugMode {
            fn drop(&mut self) {
                crate::debug_mode::set_enabled(self.0);
            }
        }
        let runtime = crate::runtime::ui::UiRuntime::start(NoScreenshot, 2).unwrap();
        let _restore = RestoreDebugMode(crate::debug_mode::set_enabled(true));
        let service = DebugHttpService::new(Some(runtime.handle()));

        // 非 capture 命令不应截图：Device::capture 一旦被调用这一步就会失败。
        let run = service
            .execute_direct(AtomicCommand::Wait { milliseconds: 0 })
            .unwrap();
        assert_eq!(
            run.status,
            SandboxStatus::Passed,
            "{:?}",
            run.results[0].error
        );
        let evidence = service.evidence(&run.run_id, Some(0)).unwrap();
        assert_eq!(evidence[0]["captured"], false);
        assert_eq!(evidence[0]["width"], 0);
        assert!(evidence[0]["imagePngBase64"].is_null());

        // 显式 capture 仍必须尝试截图。
        let run = service.execute_direct(AtomicCommand::Capture).unwrap();
        assert_eq!(run.status, SandboxStatus::Failed);
        assert!(
            run.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("截图失败")
        );
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[test]
    fn service_runs_and_exposes_evidence() {
        let service = DebugHttpService::default();
        assert_eq!(service.save(task()).unwrap(), "Created");
        let run = service.start(request()).unwrap();
        let step = service.advance(&run.run_id, false).unwrap();
        assert_eq!(step.results.len(), 1);
        assert_eq!(service.status(&run.run_id).unwrap().results.len(), 1);
        let evidence = service.evidence(&run.run_id, Some(0)).unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["captured"], true);
        assert_eq!(evidence[0]["command"], serde_json::json!("capture"));
        assert_eq!(evidence[0]["assertions"], serde_json::json!([]));
        assert!(
            evidence[0]["imagePngBase64"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        let done = service.advance(&run.run_id, true).unwrap();
        assert_eq!(done.status, SandboxStatus::Passed);
        assert_eq!(done.results.len(), 2);
    }

    #[test]
    fn oversized_archive_record_drops_frames_but_keeps_command_metadata() {
        let big = "A".repeat(3 * 1024 * 1024);
        let evidence: Vec<serde_json::Value> = (0..5)
            .map(|index| {
                serde_json::json!({
                    "index": index,
                    "command": "capture",
                    "result": {"index": index, "ok": true, "status": "passed"},
                    "imagePngBase64": big,
                })
            })
            .collect();
        let mut record = serde_json::json!({
            "run": {"runId": "oversized"},
            "task": {"id": "oversized-task"},
            "evidence": evidence,
            "replayRecipe": null,
        });
        assert!(!DebugHttpService::archive_record_fits(&record).unwrap());
        let dropped = DebugHttpService::drop_archived_frames(&mut record).unwrap();
        assert!(dropped > 0, "expected at least one frame to be dropped");
        assert!(
            DebugHttpService::archive_record_fits(&record).unwrap(),
            "record must fit the archive limit after dropping frames"
        );
        let entries = record["evidence"].as_array().unwrap();
        assert_eq!(entries.len(), 5, "command entries must never be removed");
        for entry in entries {
            assert_eq!(entry["command"], "capture");
            assert_eq!(entry["result"]["status"], "passed");
        }
        let marked = entries
            .iter()
            .filter(|entry| entry["imageDropped"] == serde_json::json!(true))
            .count();
        assert_eq!(marked, dropped);
        let kept = entries
            .iter()
            .filter(|entry| entry.get("imagePngBase64").is_some())
            .count();
        assert_eq!(kept + dropped, 5);
    }

    #[test]
    fn failed_archive_keeps_terminal_evidence_and_retry_does_not_repeat_commands() {
        let path =
            std::env::temp_dir().join(format!("sandbox-archive-retry-{}.db", uuid::Uuid::new_v4()));
        let id = {
            let service = DebugHttpService::with_journal(None, &path).unwrap();
            service.save(task()).unwrap();
            let run = service.start(request()).unwrap();
            assert_eq!(
                service.advance(&run.run_id, false).unwrap().results.len(),
                1
            );
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection.execute_batch("CREATE TRIGGER reject_sandbox_archive BEFORE INSERT ON sandbox_archive_v1 BEGIN SELECT RAISE(FAIL, 'injected archive failure'); END;").unwrap();
            assert!(service.advance(&run.run_id, true).is_err());
            assert_eq!(
                service.status(&run.run_id).unwrap().status,
                SandboxStatus::Passed
            );
            assert_eq!(service.status(&run.run_id).unwrap().results.len(), 2);
            assert_eq!(service.evidence(&run.run_id, None).unwrap().len(), 2);
            assert!(service.runners.lock().unwrap().contains_key(&run.run_id));
            connection
                .execute_batch("DROP TRIGGER reject_sandbox_archive")
                .unwrap();
            let retried = service.advance(&run.run_id, true).unwrap();
            assert_eq!(retried.results.len(), 2);
            assert_eq!(retried.status, SandboxStatus::Passed);
            assert!(!service.runners.lock().unwrap().contains_key(&run.run_id));
            assert!(service.store.run(&run.run_id).unwrap().is_none());
            run.run_id
        };
        let restored = DebugHttpService::with_journal(None, &path).unwrap();
        assert_eq!(restored.status(&id).unwrap().status, SandboxStatus::Passed);
        assert_eq!(restored.evidence(&id, None).unwrap().len(), 2);
        drop(restored);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn completed_run_remains_queryable_after_session_reclamation() {
        let service = DebugHttpService::default();
        service.save(task()).unwrap();
        let first = service.start(request()).unwrap();
        assert_eq!(
            service.advance(&first.run_id, true).unwrap().status,
            SandboxStatus::Passed
        );
        assert!(service.runners.lock().unwrap().is_empty());
        assert_eq!(service.archive.lock().unwrap().entries.len(), 1);
        assert_eq!(
            service.advance(&first.run_id, false).unwrap().status,
            SandboxStatus::Passed
        );
        let second = service.start(request()).unwrap();
        assert_ne!(first.run_id, second.run_id);
        assert_eq!(
            service.status(&first.run_id).unwrap().status,
            SandboxStatus::Passed
        );
        let evidence = service.evidence(&first.run_id, Some(0)).unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0]["command"], serde_json::json!("capture"));
        assert!(
            evidence[0]["imagePngBase64"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );
    }

    #[test]
    fn run_task_is_original_version_after_task_update() {
        let service = DebugHttpService::default();
        service.save(task()).unwrap();
        let run = service.start(request()).unwrap();
        let mut updated = task();
        updated.version = 2;
        updated.commands = vec![AtomicCommand::Wait { milliseconds: 3 }];
        service.save(updated).unwrap();
        assert_eq!(service.run_task(&run.run_id).unwrap().version, 1);
        assert!(matches!(
            service.run_task(&run.run_id).unwrap().commands[0],
            AtomicCommand::Capture
        ));
        service.advance(&run.run_id, true).unwrap();
        service
            .start(HttpDebugRunRequest {
                expected_version: Some(2),
                ..request()
            })
            .unwrap();
        assert_eq!(service.run_task(&run.run_id).unwrap().version, 1);
    }

    #[test]
    fn archived_evidence_expires_without_losing_run_status() {
        let service = DebugHttpService::default();
        service.save(task()).unwrap();
        let mut ids = Vec::new();
        for _ in 0..10 {
            let run = service.start(request()).unwrap();
            service.advance(&run.run_id, true).unwrap();
            ids.push(run.run_id);
        }
        let _next = service.start(request()).unwrap();
        assert_eq!(
            service.status(&ids[0]).unwrap().status,
            SandboxStatus::Passed
        );
        assert!(service.evidence(&ids[0], None).is_err());
        assert_eq!(service.evidence(&ids[9], None).unwrap().len(), 2);
    }

    #[test]
    fn service_cancel_marks_run_at_next_boundary() {
        let service = DebugHttpService::default();
        service.save(task()).unwrap();
        let run = service.start(request()).unwrap();
        let pending = service.cancel(&run.run_id).unwrap();
        assert_eq!(pending.status, SandboxStatus::Running);
        assert_eq!(
            service.advance(&run.run_id, false).unwrap().status,
            SandboxStatus::Cancelled
        );
    }
}
