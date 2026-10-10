use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use enigo::{Button, Key};
use image::DynamicImage;

pub trait UiDevice: Send + 'static {
    fn capture(&mut self) -> Result<DynamicImage>;

    fn press_key(&mut self, _key: Key) -> Result<()> {
        bail!("UI device does not support keyboard input")
    }

    fn key_down(&mut self, _key: Key) -> Result<()> {
        bail!("UI device does not support key-down input")
    }

    fn key_up(&mut self, _key: Key) -> Result<()> {
        bail!("UI device does not support key-up input")
    }

    fn click_point(&mut self, _x: i32, _y: i32) -> Result<()> {
        bail!("UI device does not support mouse input")
    }

    fn click_button(&mut self, _button: Button) -> Result<()> {
        bail!("UI device does not support mouse button input")
    }

    fn mouse_down(&mut self, _button: Button) -> Result<()> {
        bail!("UI device does not support mouse-down input")
    }

    fn mouse_up(&mut self, _button: Button) -> Result<()> {
        bail!("UI device does not support mouse-up input")
    }

    fn scroll_point(&mut self, _x: i32, _y: i32, _length: i32) -> Result<()> {
        bail!("UI device does not support mouse scrolling")
    }

    fn drag_point(&mut self, _from_x: i32, _from_y: i32, _to_x: i32, _to_y: i32) -> Result<()> {
        bail!("UI device does not support mouse dragging")
    }

    fn activate(&mut self, _after_activate_ms: u64) -> Result<()> {
        bail!("UI device does not support window activation")
    }

    fn focus(&mut self, _after_activate_ms: u64) -> Result<()> {
        bail!("UI device does not support window focus")
    }

    fn ensure_ready(&mut self, _after_activate_ms: u64) -> Result<()> {
        bail!("UI device does not support input preparation")
    }

    fn ensure_foreground(&mut self) -> Result<()> {
        bail!("UI device does not support foreground validation")
    }

    fn window_foreground(&mut self) -> Result<bool> {
        bail!("UI device does not support foreground observation")
    }

    fn window_available(&mut self) -> Result<bool> {
        bail!("UI device does not support window availability observation")
    }

    /// 读取剪贴板文本，用于验证"复制/粘贴"类操作是否真的生效。
    fn clipboard_text(&mut self) -> Result<Option<String>> {
        bail!(
            "UI device does not support clipboard observation (type={})",
            std::any::type_name::<Self>()
        )
    }

    fn paste_text(&mut self, _text: &str, _clipboard_hold_ms: u64) -> Result<()> {
        bail!("UI device does not support clipboard input")
    }

    fn input_text(&mut self, _text: &str, _input_settle_ms: u64) -> Result<()> {
        bail!("UI device does not support text input")
    }

    fn hold_key(
        &mut self,
        _key: Key,
        _duration: Duration,
        _running: Arc<AtomicBool>,
    ) -> Result<()> {
        bail!("UI device does not support held keyboard input")
    }

    fn ensure_window(&mut self) -> Result<()> {
        bail!("UI device does not support window availability checks")
    }

    fn close_window(&mut self) -> Result<()> {
        bail!("UI device does not support window closing")
    }

    fn launch_game(&mut self, _executable: &Path, _args: &[String]) -> Result<()> {
        bail!("UI device does not support game process launch")
    }

    fn recover(&mut self) -> Result<()> {
        Ok(())
    }
}

// All routines receive this device, including routines bypassing GameUi atoms.
/// 设备装饰器：给底层设备加"帧观测"和"输入授权"两层，其余方法必须**逐个显式转发**。
///
/// 维护注意：新增 UiDevice 方法后，这里若忘记转发，调用会**静默退回 trait 默认实现**。
/// 只读观测方法的默认实现会报错（真机上表现为 does not support ... observation），
/// 但默认实现返回 Ok(()) 的方法会无声失败。新增方法时务必同步这里，并补一条转发用例。
struct IsolatedDevice<D: UiDevice>(D);
impl<D: UiDevice> UiDevice for IsolatedDevice<D> {
    fn capture(&mut self) -> Result<DynamicImage> {
        self.0.capture()
    }
    fn ensure_foreground(&mut self) -> Result<()> {
        self.0.ensure_foreground()
    }
    fn window_foreground(&mut self) -> Result<bool> {
        self.0.window_foreground()
    }
    fn window_available(&mut self) -> Result<bool> {
        self.0.window_available()
    }
    /// 只读观测：与 capture / window_* 一致，不做输入授权检查。
    fn clipboard_text(&mut self) -> Result<Option<String>> {
        self.0.clipboard_text()
    }
    fn ensure_window(&mut self) -> Result<()> {
        self.0.ensure_window()
    }
    fn press_key(&mut self, key: Key) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.press_key(key)
    }
    fn key_down(&mut self, key: Key) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.key_down(key)
    }
    fn key_up(&mut self, key: Key) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.key_up(key)
    }
    fn click_point(&mut self, x: i32, y: i32) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.click_point(x, y)
    }
    fn click_button(&mut self, button: Button) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.click_button(button)
    }
    fn mouse_down(&mut self, button: Button) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.mouse_down(button)
    }
    fn mouse_up(&mut self, button: Button) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.mouse_up(button)
    }
    fn scroll_point(&mut self, x: i32, y: i32, length: i32) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.scroll_point(x, y, length)
    }
    fn drag_point(&mut self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.drag_point(from_x, from_y, to_x, to_y)
    }
    fn activate(&mut self, after_activate_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.activate(after_activate_ms)
    }
    fn focus(&mut self, after_activate_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.focus(after_activate_ms)
    }
    fn ensure_ready(&mut self, after_activate_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.ensure_ready(after_activate_ms)
    }
    fn paste_text(&mut self, text: &str, clipboard_hold_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.paste_text(text, clipboard_hold_ms)
    }
    fn input_text(&mut self, text: &str, input_settle_ms: u64) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.input_text(text, input_settle_ms)
    }
    fn hold_key(&mut self, key: Key, duration: Duration, running: Arc<AtomicBool>) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.hold_key(key, duration, running)
    }
    fn close_window(&mut self) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.close_window()
    }
    fn launch_game(&mut self, executable: &Path, args: &[String]) -> Result<()> {
        crate::debug_mode::require_input()?;
        self.0.launch_game(executable, args)
    }
    fn recover(&mut self) -> Result<()> {
        self.0.recover()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputCertainty {
    BeforeInput,
    AfterInputUnknown,
    ConfirmedFailure,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiRoutineFailure {
    certainty: InputCertainty,
    stage: &'static str,
    reason: String,
}

impl UiRoutineFailure {
    pub(crate) fn new(
        certainty: InputCertainty,
        stage: &'static str,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            certainty,
            stage,
            reason: reason.into(),
        }
    }

    fn before_input(stage: &'static str, reason: impl Into<String>) -> Self {
        Self {
            certainty: InputCertainty::BeforeInput,
            stage,
            reason: reason.into(),
        }
    }

    pub fn certainty(&self) -> InputCertainty {
        self.certainty
    }

    pub fn stage(&self) -> &'static str {
        self.stage
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl Display for UiRoutineFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "UI routine failed at {} ({:?}): {}",
            self.stage, self.certainty, self.reason
        )
    }
}

impl Error for UiRoutineFailure {}

#[derive(Clone, Debug)]
pub struct CapturedFrame {
    id: u64,
    image: Arc<DynamicImage>,
    captured_at: Instant,
    ui_state: Option<UiStateObservation>,
}

impl CapturedFrame {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn image(&self) -> &DynamicImage {
        &self.image
    }

    pub fn image_arc(&self) -> Arc<DynamicImage> {
        Arc::clone(&self.image)
    }

    pub fn captured_at(&self) -> Instant {
        self.captured_at
    }

    pub fn ui_state(&self) -> Option<&UiStateObservation> {
        self.ui_state.as_ref()
    }

    pub fn into_image(self) -> DynamicImage {
        Arc::try_unwrap(self.image).unwrap_or_else(|image| (*image).clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiStateKind {
    Primary,
    Secondary,
    /// 千星奇域主菜单（枢纽节点）。它是一个**独立状态**而不是 unknown：
    /// 主菜单与二级界面共用左上角返回按钮，但右下角多一个「返回提瓦特」按钮。
    /// 必须独立成一类，否则它会落进 unknown 并触发游戏态回退，
    /// 被 world_wish 误报成 overworld（见 technical-docs/ui-state-machine.md §44.10）。
    MainMenu,
    GameGate,
    Overworld,
    /// 加载界面。它是**真实界面**，不是 unknown：实测有两种外观，
    /// 都能用整屏像素占比判出来（见 §55）：
    /// - **黑底徽记型**（传送等短加载）：暗像素(<40) 占比 98.46~98.53%
    /// - **白底元素型**（冷启动进世界）：近白像素占比 99.77~100%
    /// 具体是哪一种由证据里的 finalRule 区分，状态本身按需求统一成 loading。
    Loading,
    /// 游戏内**模态面板覆盖层**：好友面板、添加好友页、玩家/千星档案卡、大厅卡、
    /// 更多设置菜单、确认弹窗、密码键盘、头像弹窗等。它们是真实界面，不该落进
    /// `unknown`；具体是哪一种由 label 与 finalRule 区分（见 §6x 面板判据）。
    /// 对例程而言它与 `unknown` 同义（按 Esc 关闭），所以行为不变。
    Panel,
    Unknown,
}

impl UiStateKind {
    /// 规范名，用作事件与查询接口里的稳定标识。
    /// 这几个就是当前**全部**可判别的界面状态；`unknown` 是显式的"判不出来"，不是"其它"。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::MainMenu => "main_menu",
            Self::GameGate => "game_gate",
            Self::Overworld => "overworld",
            Self::Loading => "loading",
            Self::Panel => "panel",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiEvidenceRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl UiEvidenceRect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn x(&self) -> i32 {
        self.x
    }

    pub fn y(&self) -> i32 {
        self.y
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UiTemplateProbeEvidence {
    template: String,
    search_rect: UiEvidenceRect,
    best_score: Option<f32>,
    threshold: f32,
    hit_rect: Option<UiEvidenceRect>,
    outcome: String,
}

impl UiTemplateProbeEvidence {
    pub fn new(
        template: impl Into<String>,
        search_rect: UiEvidenceRect,
        best_score: Option<f32>,
        threshold: f32,
        hit_rect: Option<UiEvidenceRect>,
        outcome: impl Into<String>,
    ) -> Self {
        Self {
            template: template.into(),
            search_rect,
            best_score,
            threshold,
            hit_rect,
            outcome: outcome.into(),
        }
    }

    pub fn template(&self) -> &str {
        &self.template
    }

    pub fn search_rect(&self) -> UiEvidenceRect {
        self.search_rect
    }

    pub fn best_score(&self) -> Option<f32> {
        self.best_score
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn hit_rect(&self) -> Option<UiEvidenceRect> {
        self.hit_rect
    }

    pub fn outcome(&self) -> &str {
        &self.outcome
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UiMarkerProbeEvidence {
    source_frame_id: Option<u64>,
    validated_frame_id: Option<u64>,
    search_rect: UiEvidenceRect,
    coordinate_size: (u32, u32),
    blue_count: usize,
    yellow_count: usize,
    pink_count: usize,
    marker_hits: Vec<crate::ui::template::TemplateHit>,
}

impl UiMarkerProbeEvidence {
    pub(crate) fn new(
        search_rect: UiEvidenceRect,
        coordinate_size: (u32, u32),
        blue_count: usize,
        yellow_count: usize,
        pink_count: usize,
        marker_hits: Vec<crate::ui::template::TemplateHit>,
    ) -> Self {
        Self {
            source_frame_id: None,
            validated_frame_id: None,
            search_rect,
            coordinate_size,
            blue_count,
            yellow_count,
            pink_count,
            marker_hits,
        }
    }

    pub fn search_rect(&self) -> UiEvidenceRect {
        self.search_rect
    }

    pub(crate) fn source_frame_id(&self) -> Option<u64> {
        self.source_frame_id
    }

    /// A later frame may reuse this probe only after exact whole-image equality validation.
    pub(crate) fn validated_frame_id(&self) -> Option<u64> {
        self.validated_frame_id
    }

    /// 发现标记命中位置的归一化图像尺寸。
    pub(crate) fn coordinate_size(&self) -> (u32, u32) {
        self.coordinate_size
    }

    pub fn blue_count(&self) -> usize {
        self.blue_count
    }

    pub fn yellow_count(&self) -> usize {
        self.yellow_count
    }

    pub fn pink_count(&self) -> usize {
        self.pink_count
    }

    pub(crate) fn marker_hits(&self) -> &[crate::ui::template::TemplateHit] {
        &self.marker_hits
    }
}

/// 世界判据（提瓦特 vs 千星）的证据。
///
/// 这是一条**正交于界面状态**的判据：它回答"当前画面属于哪个世界"，
/// 而不是"当前是什么界面"。计数落在两侧实测空档之间时 `guess` 是 `unknown`，
/// 与 `UiStateKind::Unknown` 同样的纪律：判不出来就不猜。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiWorldProbeEvidence {
    region: UiEvidenceRect,
    count: Option<usize>,
    teyvat_max: usize,
    qianxing_min: usize,
    guess: &'static str,
}

impl UiWorldProbeEvidence {
    pub fn new(
        region: UiEvidenceRect,
        count: Option<usize>,
        teyvat_max: usize,
        qianxing_min: usize,
        guess: &'static str,
    ) -> Self {
        Self {
            region,
            count,
            teyvat_max,
            qianxing_min,
            guess,
        }
    }

    /// 统计区域（相对整帧的绝对坐标）。
    pub fn region(&self) -> UiEvidenceRect {
        self.region
    }

    /// 区域内的青色条纹带像素数；画面太小（不是整帧）时为 `None`。
    pub fn count(&self) -> Option<usize> {
        self.count
    }

    /// 判为提瓦特的计数上限。
    pub fn teyvat_max(&self) -> usize {
        self.teyvat_max
    }

    /// 判为千星的计数下限。与 `teyvat_max` 之间是显式拒判的空档。
    pub fn qianxing_min(&self) -> usize {
        self.qianxing_min
    }

    /// `teyvat` / `qianxing` / `unknown`。
    pub fn guess(&self) -> &'static str {
        self.guess
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiStateEvidence {
    template_probes: Vec<UiTemplateProbeEvidence>,
    marker_probe: Option<UiMarkerProbeEvidence>,
    world_probe: Option<UiWorldProbeEvidence>,
    final_rule: String,
}

impl UiStateEvidence {
    pub fn new(
        template_probes: Vec<UiTemplateProbeEvidence>,
        marker_probe: Option<UiMarkerProbeEvidence>,
        final_rule: impl Into<String>,
    ) -> Self {
        Self {
            template_probes,
            marker_probe,
            world_probe: None,
            final_rule: final_rule.into(),
        }
    }

    pub fn template_probes(&self) -> &[UiTemplateProbeEvidence] {
        &self.template_probes
    }

    pub fn marker_probe(&self) -> Option<&UiMarkerProbeEvidence> {
        self.marker_probe.as_ref()
    }

    pub fn world_probe(&self) -> Option<&UiWorldProbeEvidence> {
        self.world_probe.as_ref()
    }

    /// 挂上世界判据证据。用 builder 而不是给 `new` 加参数，
    /// 是为了不动现有十几个 `UiStateEvidence::new(...)` 调用点。
    pub fn with_world_probe(mut self, probe: UiWorldProbeEvidence) -> Self {
        self.world_probe = Some(probe);
        self
    }

    pub fn final_rule(&self) -> &str {
        &self.final_rule
    }
}

impl Display for UiStateEvidence {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "rule={}", self.final_rule)?;
        for probe in &self.template_probes {
            write!(
                formatter,
                " template={} region={},{},{},{} score={} threshold={:.4} hit={} outcome={}",
                probe.template,
                probe.search_rect.x,
                probe.search_rect.y,
                probe.search_rect.width,
                probe.search_rect.height,
                probe
                    .best_score
                    .map(|score| format!("{score:.4}"))
                    .unwrap_or_else(|| "none".to_string()),
                probe.threshold,
                probe.hit_rect.map_or_else(
                    || "none".to_string(),
                    |rect| format!("{},{},{},{}", rect.x, rect.y, rect.width, rect.height)
                ),
                probe.outcome
            )?;
        }
        if let Some(marker) = &self.marker_probe {
            write!(
                formatter,
                " markers_region={},{},{},{} blue={} yellow={} pink={}",
                marker.search_rect.x,
                marker.search_rect.y,
                marker.search_rect.width,
                marker.search_rect.height,
                marker.blue_count,
                marker.yellow_count,
                marker.pink_count
            )?;
        }
        if let Some(world) = &self.world_probe {
            write!(
                formatter,
                " world_region={},{},{},{} world_count={} world_band={}..{} world_guess={}",
                world.region.x,
                world.region.y,
                world.region.width,
                world.region.height,
                world
                    .count
                    .map(|count| count.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
                world.teyvat_max,
                world.qianxing_min,
                world.guess
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UiStateClassification {
    kind: UiStateKind,
    label: String,
    evidence: UiStateEvidence,
}

impl UiStateClassification {
    pub fn new(kind: UiStateKind, label: impl Into<String>) -> Self {
        let label = label.into();
        Self {
            kind,
            evidence: UiStateEvidence::new(Vec::new(), None, label.clone()),
            label,
        }
    }

    pub fn with_evidence(
        kind: UiStateKind,
        label: impl Into<String>,
        evidence: UiStateEvidence,
    ) -> Self {
        Self {
            kind,
            label: label.into(),
            evidence,
        }
    }

    pub fn kind(&self) -> UiStateKind {
        self.kind
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn evidence(&self) -> &UiStateEvidence {
        &self.evidence
    }

    pub(crate) fn set_marker_source_frame(&mut self, frame_id: Option<u64>) {
        if let Some(probe) = self.evidence.marker_probe.as_mut() {
            probe.source_frame_id = frame_id;
            probe.validated_frame_id = frame_id;
        }
    }

    fn validate_cached_marker_frame(&mut self, frame_id: u64) {
        if let Some(probe) = self.evidence.marker_probe.as_mut() {
            probe.validated_frame_id = probe.source_frame_id.map(|_| frame_id);
        }
    }

    fn same_candidate_as(&self, other: &Self) -> bool {
        self.kind == other.kind && self.label == other.label
    }
}

pub trait UiStateClassifier: Send + 'static {
    fn classify(&mut self, image: &DynamicImage) -> Result<UiStateClassification>;

    /// Called only for pixel-identical frames. Stateful classifiers must still advance time.
    fn classify_cached(
        &mut self,
        image: &DynamicImage,
        _cached: &UiStateClassification,
    ) -> Result<UiStateClassification> {
        self.classify(image)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackedUiState {
    frame_id: u64,
    classification: UiStateClassification,
    stable_kind: Option<UiStateKind>,
    candidate_count: u32,
    required_count: u32,
    last_stable_kind: Option<UiStateKind>,
}

impl TrackedUiState {
    pub fn frame_id(&self) -> u64 {
        self.frame_id
    }

    pub fn classification(&self) -> &UiStateClassification {
        &self.classification
    }

    pub fn stable_kind(&self) -> Option<UiStateKind> {
        self.stable_kind
    }

    pub fn is_transitioning(&self) -> bool {
        self.stable_kind.is_none()
    }

    pub fn candidate_count(&self) -> u32 {
        self.candidate_count
    }

    pub fn required_count(&self) -> u32 {
        self.required_count
    }

    pub fn last_stable_kind(&self) -> Option<UiStateKind> {
        self.last_stable_kind
    }
}

impl Display for TrackedUiState {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        if self.stable_kind.is_some() {
            return formatter.write_str(self.classification.label());
        }
        match self.classification.kind() {
            UiStateKind::Unknown => formatter.write_str("transition:unknown"),
            _ => write!(
                formatter,
                "transition:{} {}/{}",
                self.classification.label(),
                self.candidate_count,
                self.required_count
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiStateObservation {
    Classified(TrackedUiState),
    Failed {
        frame_id: u64,
        reason: Arc<str>,
        last_stable_kind: Option<UiStateKind>,
    },
}

impl UiStateObservation {
    pub fn frame_id(&self) -> u64 {
        match self {
            Self::Classified(state) => state.frame_id(),
            Self::Failed { frame_id, .. } => *frame_id,
        }
    }

    pub fn classified(&self) -> Option<&TrackedUiState> {
        match self {
            Self::Classified(state) => Some(state),
            Self::Failed { .. } => None,
        }
    }

    pub fn failure_reason(&self) -> Option<&str> {
        match self {
            Self::Classified(_) => None,
            Self::Failed { reason, .. } => Some(reason),
        }
    }

    pub fn diagnostic(&self) -> String {
        match self {
            Self::Classified(state) => {
                format!("{} {}", state, state.classification().evidence())
            }
            Self::Failed {
                reason,
                last_stable_kind,
                ..
            } => format!(
                "ui-state-error:{} last_stable={}",
                reason,
                last_stable_kind
                    .map(|kind| format!("{kind:?}"))
                    .unwrap_or_else(|| "none".to_string())
            ),
        }
    }
}

impl Display for UiStateObservation {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Classified(state) => Display::fmt(state, formatter),
            Self::Failed { reason, .. } => write!(formatter, "ui-state-error:{reason}"),
        }
    }
}

#[derive(Debug)]
pub struct UiStateTracker {
    required_count: u32,
    last_frame_id: Option<u64>,
    candidate: Option<UiStateClassification>,
    candidate_count: u32,
    last_stable_kind: Option<UiStateKind>,
    last_observation: Option<UiStateObservation>,
}

impl UiStateTracker {
    pub fn new(required_count: u32) -> Self {
        Self {
            required_count: required_count.max(2),
            last_frame_id: None,
            candidate: None,
            candidate_count: 0,
            last_stable_kind: None,
            last_observation: None,
        }
    }

    pub fn observe(
        &mut self,
        frame_id: u64,
        classification: UiStateClassification,
    ) -> UiStateObservation {
        if self.last_frame_id == Some(frame_id)
            && let Some(observation) = &self.last_observation
        {
            return observation.clone();
        }
        self.last_frame_id = Some(frame_id);

        let kind = classification.kind();
        let stable_kind = if kind == UiStateKind::Unknown {
            self.candidate = None;
            self.candidate_count = 0;
            None
        } else {
            if self
                .candidate
                .as_ref()
                .is_some_and(|candidate| candidate.same_candidate_as(&classification))
            {
                self.candidate_count = self.candidate_count.saturating_add(1);
            } else {
                self.candidate = Some(classification.clone());
                self.candidate_count = 1;
            }
            (self.candidate_count >= self.required_count).then_some(kind)
        };
        if let Some(kind) = stable_kind {
            self.last_stable_kind = Some(kind);
        }
        let observation = UiStateObservation::Classified(TrackedUiState {
            frame_id,
            classification,
            stable_kind,
            candidate_count: self.candidate_count,
            required_count: self.required_count,
            last_stable_kind: self.last_stable_kind,
        });
        self.last_observation = Some(observation.clone());
        observation
    }

    pub fn observe_failure(
        &mut self,
        frame_id: u64,
        reason: impl Into<Arc<str>>,
    ) -> UiStateObservation {
        if self.last_frame_id == Some(frame_id)
            && let Some(observation) = &self.last_observation
        {
            return observation.clone();
        }
        self.last_frame_id = Some(frame_id);
        self.candidate = None;
        self.candidate_count = 0;
        let observation = UiStateObservation::Failed {
            frame_id,
            reason: reason.into(),
            last_stable_kind: self.last_stable_kind,
        };
        self.last_observation = Some(observation.clone());
        observation
    }
}

#[derive(Clone, Debug)]
pub struct FrameCaptureFailure {
    failed_at: Instant,
    reason: Arc<str>,
}

impl FrameCaptureFailure {
    pub fn failed_at(&self) -> Instant {
        self.failed_at
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Clone, Debug)]
pub enum FramePublication {
    Captured(Arc<CapturedFrame>),
    Failed(FrameCaptureFailure),
}

struct UiObservationRuntime {
    classifier: Option<Box<dyn UiStateClassifier>>,
    tracker: UiStateTracker,
    next_frame_id: u64,
    latest_state: Arc<Mutex<Option<UiStateObservation>>>,
    /// 稀疏签名仅作快速排除；必须整帧像素相同才能进入分类器的缓存路径。
    cached_frame: Option<(u64, Arc<DynamicImage>, UiStateClassification)>,
}

impl UiObservationRuntime {
    fn new(
        classifier: Option<Box<dyn UiStateClassifier>>,
        stable_count: u32,
        latest_state: Arc<Mutex<Option<UiStateObservation>>>,
    ) -> Self {
        Self {
            classifier,
            tracker: UiStateTracker::new(stable_count),
            next_frame_id: 0,
            latest_state,
            cached_frame: None,
        }
    }

    fn capture(&mut self, device: &mut dyn UiDevice) -> Result<CapturedFrame> {
        let capture_started = Instant::now();
        let image = Arc::new(device.capture()?);
        let captured_at = Instant::now();
        let capture_elapsed = captured_at.duration_since(capture_started);
        let classification_started = Instant::now();
        let mut classification_cache_hit = false;
        self.next_frame_id = self.next_frame_id.wrapping_add(1).max(1);
        let frame_id = self.next_frame_id;
        let ui_state = match self.classifier.as_mut() {
            None => None,
            Some(classifier) => {
                let signature = frame_signature(&image);
                let cached = match &self.cached_frame {
                    Some((cached_signature, cached_image, classification))
                        if *cached_signature == signature
                            && cached_image.as_ref() == image.as_ref() =>
                    {
                        Some(classification.clone())
                    }
                    _ => None,
                };
                let cache_hit = cached.is_some();
                classification_cache_hit = cache_hit;
                let classification = match cached {
                    Some(classification) => classifier.classify_cached(&image, &classification),
                    None => classifier.classify(&image),
                };
                let observation = match classification {
                    Ok(mut classification) => {
                        if cache_hit {
                            // Exact pixels revalidate evidence without changing its original source.
                            classification.validate_cached_marker_frame(frame_id);
                        } else {
                            classification.set_marker_source_frame(Some(frame_id));
                        }
                        self.cached_frame =
                            Some((signature, Arc::clone(&image), classification.clone()));
                        self.tracker.observe(frame_id, classification)
                    }
                    Err(error) => {
                        self.cached_frame = None;
                        self.tracker
                            .observe_failure(frame_id, Arc::<str>::from(format!("{error:#}")))
                    }
                };
                if let Ok(mut latest) = self.latest_state.lock() {
                    *latest = Some(observation.clone());
                }
                Some(observation)
            }
        };
        log::debug!(target: "performance",
            "ui_observation frame_id={} cache_hit={} capture_ms={} classify_ms={} frame_age_ms={}",
            frame_id, classification_cache_hit, capture_elapsed.as_millis(),
            classification_started.elapsed().as_millis(), captured_at.elapsed().as_millis());
        Ok(CapturedFrame {
            id: frame_id,
            image,
            captured_at,
            ui_state,
        })
    }
}

/// 缓存的快速排除签名；相同签名不能证明整帧或聊天 ROI 相同。
fn frame_signature(image: &DynamicImage) -> u64 {
    use image::GenericImageView;
    use std::hash::{Hash, Hasher};

    const COLS: u32 = 64;
    const ROWS: u32 = 36;
    let width = image.width().max(1);
    let height = image.height().max(1);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    for row in 0..ROWS {
        for col in 0..COLS {
            let x = (col * width / COLS).min(width - 1);
            let y = (row * height / ROWS).min(height - 1);
            image.get_pixel(x, y).0.hash(&mut hasher);
        }
    }
    hasher.finish()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameDemand {
    interval: Duration,
}

impl FrameDemand {
    pub fn new(interval: Duration) -> Result<Self, FrameDemandError> {
        if interval.is_zero() {
            return Err(FrameDemandError);
        }
        Ok(Self { interval })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameDemandError;

impl Display for FrameDemandError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("frame demand interval must be greater than zero")
    }
}

impl Error for FrameDemandError {}

#[derive(Clone, Default)]
pub struct UiCoordinator {
    state: Arc<Mutex<UiCoordinatorState>>,
    scans_drained: Arc<std::sync::Condvar>,
}

#[derive(Default)]
struct UiCoordinatorState {
    next_lease_id: u64,
    formal_leases: HashMap<u64, UiFormalOwner>,
    in_flight_scans: usize,
    scan_drainers: usize,
    scan_not_before: Option<Instant>,
}

struct UiFormalOwner {
    thread: thread::ThreadId,
    scan_permits: usize,
}

impl UiCoordinatorState {
    fn scan_may_run(&self) -> bool {
        self.scan_drainers == 0
            && self
                .formal_leases
                .values()
                .all(|lease| lease.scan_permits > 0)
    }
}

impl UiCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retain formal ownership and wait for already admitted scans to finish.
    /// Acquire and use this lease on the thread executing the formal task.
    /// Never acquire it while holding a scan lease on this thread.
    pub fn acquire_formal(&self) -> UiFormalLease {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.scan_not_before = Some(Instant::now());
        let id = state.next_lease_id;
        state.next_lease_id = id.checked_add(1).expect("UI lease IDs exhausted");
        state.formal_leases.insert(
            id,
            UiFormalOwner {
                thread: thread::current().id(),
                scan_permits: 0,
            },
        );
        while state.in_flight_scans != 0 {
            state = self
                .scans_drained
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        UiFormalLease {
            coordinator: self.clone(),
            id,
            _thread_bound: std::marker::PhantomData,
        }
    }

    /// Temporarily permit scans for the current thread's existing formal leases.
    /// Other owners and leases acquired later remain independently blocking.
    /// Nested permits each retain their own contribution. Drop all active permits
    /// before resuming UI work; each drop revokes its contribution and drains
    /// admitted scans, but an outer permit can admit scans again afterwards.
    /// Do not hold a scan lease on this thread when dropping the guard.
    pub fn permit_scans_for_current_thread(&self) -> UiScanPermit {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let owner = thread::current().id();
        let mut lease_ids = Vec::new();
        for (&id, lease) in &mut state.formal_leases {
            if lease.thread == owner {
                lease.scan_permits += 1;
                lease_ids.push(id);
            }
        }
        if !lease_ids.is_empty() {
            state.scan_not_before = Some(Instant::now());
        }
        UiScanPermit {
            coordinator: self.clone(),
            lease_ids,
            _thread_bound: std::marker::PhantomData,
        }
    }

    /// Advisory only. Use try_acquire_scan to authorize actual scan UI work.
    pub fn scan_may_run(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .scan_may_run()
    }

    /// Atomically check admission and register an in-flight scan. Keep the
    /// returned lease alive through all scan UI work, including error paths.
    pub fn try_acquire_scan(&self) -> Option<UiScanLease> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.scan_may_run() {
            return None;
        }
        state.in_flight_scans += 1;
        Some(UiScanLease {
            coordinator: self.clone(),
            not_before: state.scan_not_before,
        })
    }
}

#[must_use = "keep the formal lease alive while performing UI work"]
pub struct UiFormalLease {
    coordinator: UiCoordinator,
    id: u64,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for UiFormalLease {
    fn drop(&mut self) {
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.formal_leases.remove(&self.id);
        state.scan_not_before = Some(Instant::now());
    }
}

#[must_use = "keep the permit alive only while the formal owner is waiting"]
pub struct UiScanPermit {
    coordinator: UiCoordinator,
    lease_ids: Vec<u64>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for UiScanPermit {
    fn drop(&mut self) {
        if self.lease_ids.is_empty() {
            return;
        }
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for id in &self.lease_ids {
            if let Some(lease) = state.formal_leases.get_mut(id) {
                lease.scan_permits -= 1;
            }
        }
        // Block new admissions even when an outer permit still exists. This
        // makes every drop a drain barrier without cancelling nested permits.
        state.scan_not_before = Some(Instant::now());
        state.scan_drainers += 1;
        while state.in_flight_scans != 0 {
            state = self
                .coordinator
                .scans_drained
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        state.scan_drainers -= 1;
    }
}

#[must_use = "keep the scan lease alive through all scan UI work"]
pub struct UiScanLease {
    coordinator: UiCoordinator,
    not_before: Option<Instant>,
}

impl UiScanLease {
    /// Reject frames captured before this admission boundary. The snapshot stays
    /// fixed for this lease; ordinary scans do not advance the boundary.
    pub fn not_before(&self) -> Option<Instant> {
        self.not_before
    }
}

impl Drop for UiScanLease {
    fn drop(&mut self) {
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.in_flight_scans -= 1;
        if state.in_flight_scans == 0 {
            self.coordinator.scans_drained.notify_all();
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureFrame;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiRoutineProgressStage {
    NormalizingStart,
    LocatingFriend {
        recipient_index: usize,
        recipient_count: usize,
    },
    SendingFriendMessage {
        recipient_index: usize,
        recipient_count: usize,
        message_index: usize,
        message_count: usize,
    },
    ExecutingCustomAction {
        operation_index: usize,
        operation_count: usize,
    },
    ConfirmingUi,
    RecoveringResidency,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiRoutineProgress {
    operation_id: UiOperationId,
    stage: UiRoutineProgressStage,
}

impl UiRoutineProgress {
    pub fn new(operation_id: UiOperationId, stage: UiRoutineProgressStage) -> Self {
        Self {
            operation_id,
            stage,
        }
    }

    pub fn operation_id(&self) -> UiOperationId {
        self.operation_id
    }

    pub fn stage(&self) -> &UiRoutineProgressStage {
        &self.stage
    }
}

pub trait UiRoutineProgressSink: Send + Sync + 'static {
    fn publish(&self, progress: UiRoutineProgress);
}

struct DiscardUiRoutineProgress;

impl UiRoutineProgressSink for DiscardUiRoutineProgress {
    fn publish(&self, _progress: UiRoutineProgress) {}
}

pub struct UiRoutineContext<'a> {
    operation_id: UiOperationId,
    device: &'a mut dyn UiDevice,
    progress: &'a dyn UiRoutineProgressSink,
    observation: &'a mut UiObservationRuntime,
    latest_frame: &'a Mutex<Option<Arc<CapturedFrame>>>,
    demands: &'a mut HashMap<u64, ActiveFrameDemand>,
    cancelled: &'a AtomicBool,
}

impl<'a> UiRoutineContext<'a> {
    fn new(
        operation_id: UiOperationId,
        device: &'a mut dyn UiDevice,
        progress: &'a dyn UiRoutineProgressSink,
        observation: &'a mut UiObservationRuntime,
        latest_frame: &'a Mutex<Option<Arc<CapturedFrame>>>,
        demands: &'a mut HashMap<u64, ActiveFrameDemand>,
        cancelled: &'a AtomicBool,
    ) -> Self {
        Self {
            operation_id,
            device,
            progress,
            observation,
            latest_frame,
            demands,
            cancelled,
        }
    }

    pub fn operation_id(&self) -> UiOperationId {
        self.operation_id
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub fn wait_cancellable(
        &mut self,
        duration: Duration,
    ) -> std::result::Result<(), UiRoutineFailure> {
        let deadline = Instant::now() + duration;
        loop {
            if self.is_cancelled() {
                return Err(UiRoutineFailure::new(
                    InputCertainty::Cancelled,
                    "wait_cancellable",
                    "UI operation cancelled",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            if !self.demands.is_empty() {
                publish_due_frame(
                    self.device,
                    self.observation,
                    self.demands,
                    self.latest_frame,
                );
            }
            if self.is_cancelled() {
                return Err(UiRoutineFailure::new(
                    InputCertainty::Cancelled,
                    "wait_cancellable",
                    "UI operation cancelled",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            let until_capture = next_frame_timeout(self.demands).unwrap_or(remaining);
            thread::sleep(remaining.min(until_capture).min(Duration::from_millis(25)));
        }
    }

    pub fn device(&mut self) -> &mut dyn UiDevice {
        self.device
    }

    pub fn capture_frame(&mut self) -> Result<CapturedFrame> {
        match self.observation.capture(self.device) {
            Ok(frame) => {
                publish_frame(
                    FramePublication::Captured(Arc::new(frame.clone())),
                    self.demands,
                    self.latest_frame,
                );
                Ok(frame)
            }
            Err(error) => {
                publish_frame(
                    FramePublication::Failed(FrameCaptureFailure {
                        failed_at: Instant::now(),
                        reason: Arc::from(format!("{error:#}")),
                    }),
                    self.demands,
                    self.latest_frame,
                );
                Err(error)
            }
        }
    }

    pub fn latest_ui_state(&self) -> Option<UiStateObservation> {
        self.observation
            .latest_state
            .lock()
            .ok()
            .and_then(|state| state.clone())
    }

    pub fn publish_progress(&self, stage: UiRoutineProgressStage) {
        self.progress
            .publish(UiRoutineProgress::new(self.operation_id, stage));
    }
}

pub(crate) mod sealed {
    pub trait UiRoutineSealed {}
}

pub trait UiRoutine: sealed::UiRoutineSealed + Send + 'static {
    type Output: Send + 'static;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output;

    fn frame_publication(_output: &Self::Output) -> Option<FramePublication> {
        None
    }
}

impl sealed::UiRoutineSealed for CaptureFrame {}

impl UiRoutine for CaptureFrame {
    type Output = Result<CapturedFrame, UiRoutineFailure>;

    fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
        context
            .capture_frame()
            .map_err(|error| UiRoutineFailure::before_input("capture_frame", format!("{error:#}")))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UiOperationId(u64);

impl UiOperationId {
    pub fn get(self) -> u64 {
        self.0
    }
}

pub struct UiOperation<T> {
    id: UiOperationId,
    response: Receiver<T>,
    cancelled: Arc<AtomicBool>,
}

impl<T> UiOperation<T> {
    pub fn id(&self) -> UiOperationId {
        self.id
    }

    pub fn wait(self) -> Result<T, UiReceiveError> {
        self.response.recv().map_err(|_| UiReceiveError)
    }

    pub fn wait_timeout(self, timeout: Duration) -> Result<T, UiWaitTimeoutError> {
        match self.response.recv_timeout(timeout) {
            Ok(value) => Ok(value),
            Err(RecvTimeoutError::Timeout) => {
                self.cancelled.store(true, Ordering::Release);
                Err(UiWaitTimeoutError {
                    timeout,
                    disconnected: false,
                })
            }
            Err(RecvTimeoutError::Disconnected) => Err(UiWaitTimeoutError {
                timeout,
                disconnected: true,
            }),
        }
    }

    pub fn cancel(self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiWaitTimeoutError {
    timeout: Duration,
    /// 真实失败原因：false = 确实等满了 timeout；true = UI runtime 在回传结果之前就结束了。
    disconnected: bool,
}

impl UiWaitTimeoutError {
    /// 是否为"runtime 提前结束"而非"等待超时"。
    pub fn disconnected(&self) -> bool {
        self.disconnected
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl Display for UiWaitTimeoutError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        if self.disconnected {
            // 通道断开 = 工作线程没有回传结果就结束了（崩溃或操作被丢弃）。
            // 以前这里一律报"超过 N 秒"，把排查方向引向超时，实际根本没等那么久。
            return formatter.write_str("UI runtime stopped before returning the operation result");
        }
        write!(
            formatter,
            "UI runtime operation exceeded {:?}",
            self.timeout
        )
    }
}

impl Error for UiWaitTimeoutError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiReceiveError;

impl Display for UiReceiveError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UI runtime stopped before returning a result")
    }
}

impl Error for UiReceiveError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiSubmitError {
    QueueFull,
    RuntimeStopped,
}

impl Display for UiSubmitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => formatter.write_str("UI runtime queue is full"),
            Self::RuntimeStopped => formatter.write_str("UI runtime is stopped"),
        }
    }
}

impl Error for UiSubmitError {}

#[derive(Debug)]
pub enum UiRuntimeStartError {
    ZeroQueueCapacity,
    Spawn(std::io::Error),
}

impl Display for UiRuntimeStartError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroQueueCapacity => {
                formatter.write_str("UI runtime queue capacity must be greater than zero")
            }
            Self::Spawn(error) => write!(formatter, "failed to start UI runtime: {error}"),
        }
    }
}

impl Error for UiRuntimeStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ZeroQueueCapacity => None,
            Self::Spawn(error) => Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiShutdownError;

impl Display for UiShutdownError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UI runtime worker panicked")
    }
}

impl Error for UiShutdownError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UiShutdownReport {
    timed_out: bool,
    detached: bool,
}

impl UiShutdownReport {
    pub fn timed_out(&self) -> bool {
        self.timed_out
    }

    pub fn detached(&self) -> bool {
        self.detached
    }
}

trait ErasedUiJob: Send {
    fn execute(
        self: Box<Self>,
        device: &mut dyn UiDevice,
        progress: &dyn UiRoutineProgressSink,
        observation: &mut UiObservationRuntime,
        demands: &mut HashMap<u64, ActiveFrameDemand>,
        latest_frame: &Mutex<Option<Arc<CapturedFrame>>>,
    );
}

struct TypedUiJob<R: UiRoutine> {
    operation_id: UiOperationId,
    routine: R,
    response: SyncSender<R::Output>,
    generation: u64,
    external: bool,
    cancelled: Arc<AtomicBool>,
}

impl<R: UiRoutine> ErasedUiJob for TypedUiJob<R> {
    fn execute(
        self: Box<Self>,
        device: &mut dyn UiDevice,
        progress: &dyn UiRoutineProgressSink,
        observation: &mut UiObservationRuntime,
        demands: &mut HashMap<u64, ActiveFrameDemand>,
        latest_frame: &Mutex<Option<Arc<CapturedFrame>>>,
    ) {
        let Self {
            operation_id,
            routine,
            response,
            generation,
            external,
            cancelled,
        } = *self;
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let Some(_execution_guard) = crate::debug_mode::execution_guard(generation, external)
        else {
            // Mode changed after enqueue; dropping the response makes wait() fail without executing.
            return;
        };
        let mut context = UiRoutineContext::new(
            operation_id,
            device,
            progress,
            observation,
            latest_frame,
            demands,
            &cancelled,
        );
        let output = routine.execute(&mut context);
        if let Some(publication) = R::frame_publication(&output) {
            publish_frame(publication, demands, latest_frame);
        }
        let _ = response.send(output);
    }
}

enum RuntimeMessage {
    Execute(Box<dyn ErasedUiJob>),
    AddFrameDemand {
        id: u64,
        demand: FrameDemand,
        slot: Arc<FramePublicationSlot>,
    },
    RemoveFrameDemand(u64),
    Shutdown,
}

struct RuntimeChannel {
    sender: SyncSender<RuntimeMessage>,
    accepting: Mutex<bool>,
    stop_requested: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct UiRuntimeHandle {
    channel: Arc<RuntimeChannel>,
    next_operation_id: Arc<AtomicU64>,
    next_frame_demand_id: Arc<AtomicU64>,
    latest_frame: Arc<Mutex<Option<Arc<CapturedFrame>>>>,
    latest_ui_state: Arc<Mutex<Option<UiStateObservation>>>,
}

impl UiRuntimeHandle {
    pub fn submit<R: UiRoutine>(
        &self,
        routine: R,
    ) -> Result<UiOperation<R::Output>, UiSubmitError> {
        self.submit_authorized(routine, false)
    }

    /// Explicit authorization for external debug API commands, carried onto the worker.
    pub(crate) fn submit_external<R: UiRoutine>(
        &self,
        routine: R,
    ) -> Result<UiOperation<R::Output>, UiSubmitError> {
        self.submit_authorized(routine, true)
    }

    fn submit_authorized<R: UiRoutine>(
        &self,
        routine: R,
        external: bool,
    ) -> Result<UiOperation<R::Output>, UiSubmitError> {
        let generation = crate::debug_mode::generation();
        let accepting = self
            .channel
            .accepting
            .lock()
            .map_err(|_| UiSubmitError::RuntimeStopped)?;
        if !*accepting {
            return Err(UiSubmitError::RuntimeStopped);
        }

        let id = UiOperationId(
            self.next_operation_id
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1),
        );
        let (response, receiver) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let message = RuntimeMessage::Execute(Box::new(TypedUiJob {
            operation_id: id,
            routine,
            response,
            generation,
            external,
            cancelled: Arc::clone(&cancelled),
        }));
        match self.channel.sender.try_send(message) {
            Ok(()) => Ok(UiOperation {
                id,
                response: receiver,
                cancelled,
            }),
            Err(TrySendError::Full(_)) => Err(UiSubmitError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(UiSubmitError::RuntimeStopped),
        }
    }

    pub fn declare_frame_demand(
        &self,
        demand: FrameDemand,
    ) -> Result<FrameDemandSubscription, UiSubmitError> {
        let accepting = self
            .channel
            .accepting
            .lock()
            .map_err(|_| UiSubmitError::RuntimeStopped)?;
        if !*accepting {
            return Err(UiSubmitError::RuntimeStopped);
        }

        let id = self
            .next_frame_demand_id
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let slot = Arc::new(FramePublicationSlot::default());
        let message = RuntimeMessage::AddFrameDemand {
            id,
            demand,
            slot: Arc::clone(&slot),
        };
        match self.channel.sender.try_send(message) {
            Ok(()) => Ok(FrameDemandSubscription {
                id,
                slot,
                channel: Arc::clone(&self.channel),
                active: true,
            }),
            Err(TrySendError::Full(_)) => Err(UiSubmitError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(UiSubmitError::RuntimeStopped),
        }
    }

    pub fn latest_frame(&self) -> Option<Arc<CapturedFrame>> {
        self.latest_frame
            .lock()
            .ok()
            .and_then(|frame| frame.clone())
    }

    pub fn latest_ui_state(&self) -> Option<UiStateObservation> {
        self.latest_ui_state
            .lock()
            .ok()
            .and_then(|state| state.clone())
    }
}

#[derive(Default)]
struct FramePublicationSlot {
    state: Mutex<FramePublicationState>,
    available: Condvar,
}

#[derive(Default)]
struct FramePublicationState {
    pending: Option<FramePublication>,
    closed: bool,
}

impl FramePublicationSlot {
    fn publish(&self, publication: FramePublication) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.closed {
            state.pending = Some(publication);
            self.available.notify_one();
        }
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        self.available.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed
    }

    fn recv(&self) -> Result<FramePublication, RecvError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(publication) = state.pending.take() {
                return Ok(publication);
            }
            if state.closed {
                return Err(RecvError);
            }
            state = self
                .available
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<FramePublication, RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(publication) = state.pending.take() {
                return Ok(publication);
            }
            if state.closed {
                return Err(RecvTimeoutError::Disconnected);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            state = self
                .available
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }
}

pub struct FrameDemandSubscription {
    id: u64,
    slot: Arc<FramePublicationSlot>,
    channel: Arc<RuntimeChannel>,
    active: bool,
}

impl FrameDemandSubscription {
    pub fn recv(&self) -> Result<FramePublication, RecvError> {
        self.slot.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<FramePublication, RecvTimeoutError> {
        self.slot.recv_timeout(timeout)
    }

    pub fn cancel(mut self) -> Result<(), UiSubmitError> {
        let accepting = self
            .channel
            .accepting
            .lock()
            .map_err(|_| UiSubmitError::RuntimeStopped)?;
        if !*accepting {
            return Err(UiSubmitError::RuntimeStopped);
        }
        self.channel
            .sender
            .try_send(RuntimeMessage::RemoveFrameDemand(self.id))
            .map_err(|error| match error {
                TrySendError::Full(_) => UiSubmitError::QueueFull,
                TrySendError::Disconnected(_) => UiSubmitError::RuntimeStopped,
            })?;
        self.active = false;
        self.slot.close();
        Ok(())
    }
}

impl Drop for FrameDemandSubscription {
    fn drop(&mut self) {
        self.slot.close();
        if self.active {
            let _ = self
                .channel
                .sender
                .try_send(RuntimeMessage::RemoveFrameDemand(self.id));
        }
    }
}

/// 测试期串行化所有 `UiRuntime` 实例。
///
/// UI worker 在执行前会校验入队时记录的 `debug_mode` generation，一旦变化就**丢弃该操作且不回传**
/// （见 `TypedUiJob::run`，这是防止陈旧外部命令执行的正确安全行为）。而 `debug_mode` 是**进程全局**状态。
/// 于是并行测试里只要有一个用例切换调试模式，**其他用例正在等待的 live 操作就会被丢弃**，
/// 表现为随机的 `UI runtime stopped before returning the operation result`。
///
/// 生产环境只有一个 runtime，且模式切换是人为动作，因此这条约束只在测试期施加。
#[cfg(test)]
mod test_runtime_serial {
    use std::sync::{Condvar, Mutex};

    static HOLDER: Mutex<Option<usize>> = Mutex::new(None);
    static RELEASED: Condvar = Condvar::new();

    /// 可重入占有凭据：同一线程可以同时持有多个 runtime，
    /// 避免同一个用例创建两个 runtime 时自锁。
    pub(super) struct Permit;

    impl Permit {
        pub(super) fn acquire() -> Self {
            let mut holder = HOLDER.lock().unwrap_or_else(|error| error.into_inner());
            while holder.is_some() {
                holder = RELEASED
                    .wait(holder)
                    .unwrap_or_else(|error| error.into_inner());
            }
            *holder = Some(1);
            Self
        }
    }

    impl Drop for Permit {
        fn drop(&mut self) {
            let mut holder = HOLDER.lock().unwrap_or_else(|error| error.into_inner());
            // 不校验线程：即使 runtime 被移到别的线程释放，计数也保持平衡，不会永久占锁。
            if let Some(depth) = holder.as_mut() {
                *depth -= 1;
                if *depth == 0 {
                    *holder = None;
                    RELEASED.notify_all();
                }
            }
        }
    }
}

pub struct UiRuntime {
    handle: UiRuntimeHandle,
    worker: Option<JoinHandle<()>>,
    /// 关闭超时后分离的 join 线程,后续 stop 时回收已结束的线程。
    detached_joins: Vec<JoinHandle<()>>,
    #[cfg(test)]
    _serial: test_runtime_serial::Permit,
}

impl UiRuntime {
    pub fn start(
        device: impl UiDevice,
        queue_capacity: usize,
    ) -> Result<Self, UiRuntimeStartError> {
        Self::start_with_progress(device, queue_capacity, Arc::new(DiscardUiRoutineProgress))
    }

    pub fn start_with_progress(
        device: impl UiDevice,
        queue_capacity: usize,
        progress: Arc<dyn UiRoutineProgressSink>,
    ) -> Result<Self, UiRuntimeStartError> {
        Self::start_configured(device, queue_capacity, progress, None, 2)
    }

    pub fn start_with_state_classifier(
        device: impl UiDevice,
        queue_capacity: usize,
        classifier: impl UiStateClassifier,
        stable_count: u32,
    ) -> Result<Self, UiRuntimeStartError> {
        Self::start_configured(
            device,
            queue_capacity,
            Arc::new(DiscardUiRoutineProgress),
            Some(Box::new(classifier)),
            stable_count,
        )
    }

    pub fn start_with_progress_and_state_classifier(
        device: impl UiDevice,
        queue_capacity: usize,
        progress: Arc<dyn UiRoutineProgressSink>,
        classifier: impl UiStateClassifier,
        stable_count: u32,
    ) -> Result<Self, UiRuntimeStartError> {
        Self::start_configured(
            device,
            queue_capacity,
            progress,
            Some(Box::new(classifier)),
            stable_count,
        )
    }

    fn start_configured(
        device: impl UiDevice,
        queue_capacity: usize,
        progress: Arc<dyn UiRoutineProgressSink>,
        classifier: Option<Box<dyn UiStateClassifier>>,
        stable_count: u32,
    ) -> Result<Self, UiRuntimeStartError> {
        if queue_capacity == 0 {
            return Err(UiRuntimeStartError::ZeroQueueCapacity);
        }
        // 测试期串行化：见 test_runtime_serial 的说明。acquire 失败路径上凭据会随栈释放。
        #[cfg(test)]
        let _serial = test_runtime_serial::Permit::acquire();

        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let channel = Arc::new(RuntimeChannel {
            sender,
            accepting: Mutex::new(true),
            stop_requested: Arc::clone(&stop_requested),
        });
        let latest_frame = Arc::new(Mutex::new(None));
        let worker_latest_frame = Arc::clone(&latest_frame);
        let latest_ui_state = Arc::new(Mutex::new(None));
        let worker_latest_ui_state = Arc::clone(&latest_ui_state);
        let worker = thread::Builder::new()
            .name("ui-runtime".to_string())
            .spawn(move || {
                run_ui_runtime(
                    device,
                    receiver,
                    worker_latest_frame,
                    worker_latest_ui_state,
                    progress,
                    classifier,
                    stable_count,
                    stop_requested,
                )
            })
            .map_err(UiRuntimeStartError::Spawn)?;

        Ok(Self {
            handle: UiRuntimeHandle {
                channel,
                next_operation_id: Arc::new(AtomicU64::new(0)),
                next_frame_demand_id: Arc::new(AtomicU64::new(0)),
                latest_frame,
                latest_ui_state,
            },
            detached_joins: Vec::new(),
            worker: Some(worker),
            #[cfg(test)]
            _serial,
        })
    }

    pub fn handle(&self) -> UiRuntimeHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) -> Result<UiShutdownReport, UiShutdownError> {
        self.stop_worker(Duration::from_secs(2))
    }

    fn stop_worker(&mut self, grace_period: Duration) -> Result<UiShutdownReport, UiShutdownError> {
        // 先回收上一轮超时分离的 join 线程。
        self.reap_detached_joins();
        let Some(worker) = self.worker.take() else {
            return Ok(UiShutdownReport::default());
        };
        if let Ok(mut accepting) = self.handle.channel.accepting.lock() {
            *accepting = false;
        }
        self.handle
            .channel
            .stop_requested
            .store(true, Ordering::Release);
        let _ = self
            .handle
            .channel
            .sender
            .try_send(RuntimeMessage::Shutdown);

        let (joined_sender, joined_receiver) = mpsc::sync_channel(1);
        let join_thread = thread::spawn(move || {
            let _ = joined_sender.send(worker.join().map_err(|_| UiShutdownError));
        });
        match joined_receiver.recv_timeout(grace_period) {
            Ok(result) => result.map(|()| UiShutdownReport::default()),
            Err(RecvTimeoutError::Timeout) => {
                // join 线程可能长期阻塞在 join 上,保留句柄供后续回收。
                self.detached_joins.push(join_thread);
                Ok(UiShutdownReport {
                    timed_out: true,
                    detached: true,
                })
            }
            Err(RecvTimeoutError::Disconnected) => Err(UiShutdownError),
        }
    }

    fn reap_detached_joins(&mut self) {
        let mut index = 0;
        while index < self.detached_joins.len() {
            if self.detached_joins[index].is_finished() {
                let join_thread = self.detached_joins.swap_remove(index);
                let _ = join_thread.join();
            } else {
                index += 1;
            }
        }
        if self.detached_joins.len() > 8 {
            log::error!(
                "UI worker 累计 {} 个未退出 join 线程,可能存在设备调用卡死",
                self.detached_joins.len()
            );
        }
    }
}

impl Drop for UiRuntime {
    fn drop(&mut self) {
        let _ = self.stop_worker(Duration::from_millis(100));
    }
}

struct ActiveFrameDemand {
    interval: Duration,
    next_due: Instant,
    slot: Arc<FramePublicationSlot>,
}

#[allow(clippy::too_many_arguments)]
fn run_ui_runtime(
    device: impl UiDevice,
    receiver: Receiver<RuntimeMessage>,
    latest_frame: Arc<Mutex<Option<Arc<CapturedFrame>>>>,
    latest_ui_state: Arc<Mutex<Option<UiStateObservation>>>,
    progress: Arc<dyn UiRoutineProgressSink>,
    classifier: Option<Box<dyn UiStateClassifier>>,
    stable_count: u32,
    stop_requested: Arc<AtomicBool>,
) {
    let mut device = IsolatedDevice(device);
    let mut demands = HashMap::<u64, ActiveFrameDemand>::new();
    let mut observation = UiObservationRuntime::new(classifier, stable_count, latest_ui_state);
    loop {
        if stop_requested.load(Ordering::Acquire) {
            break;
        }
        let message = match next_frame_timeout(&demands) {
            Some(timeout) => match receiver.recv_timeout(timeout) {
                Ok(message) => Some(message),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match receiver.recv() {
                Ok(message) => Some(message),
                Err(_) => break,
            },
        };
        match message {
            Some(RuntimeMessage::Execute(job)) => {
                job.execute(
                    &mut device,
                    progress.as_ref(),
                    &mut observation,
                    &mut demands,
                    &latest_frame,
                );
                publish_due_frame(&mut device, &mut observation, &mut demands, &latest_frame);
            }
            Some(RuntimeMessage::AddFrameDemand { id, demand, slot }) => {
                demands.insert(
                    id,
                    ActiveFrameDemand {
                        interval: demand.interval,
                        next_due: Instant::now(),
                        slot,
                    },
                );
            }
            Some(RuntimeMessage::RemoveFrameDemand(id)) => {
                if let Some(demand) = demands.remove(&id) {
                    demand.slot.close();
                }
            }
            Some(RuntimeMessage::Shutdown) => break,
            None => publish_due_frame(&mut device, &mut observation, &mut demands, &latest_frame),
        }
    }
    for demand in demands.values() {
        demand.slot.close();
    }
    while let Ok(message) = receiver.try_recv() {
        if let RuntimeMessage::AddFrameDemand { slot, .. } = message {
            slot.close();
        }
    }
}

fn next_frame_timeout(demands: &HashMap<u64, ActiveFrameDemand>) -> Option<Duration> {
    let now = Instant::now();
    demands
        .values()
        .map(|demand| demand.next_due.saturating_duration_since(now))
        .min()
}

fn publish_due_frame(
    device: &mut dyn UiDevice,
    observation: &mut UiObservationRuntime,
    demands: &mut HashMap<u64, ActiveFrameDemand>,
    latest_frame: &Mutex<Option<Arc<CapturedFrame>>>,
) {
    demands.retain(|_, demand| !demand.slot.is_closed());
    let now = Instant::now();
    let due = demands
        .iter()
        .filter_map(|(&id, demand)| (demand.next_due <= now).then_some(id))
        .collect::<Vec<_>>();
    if due.is_empty() {
        return;
    }

    let publication = match observation.capture(device) {
        Ok(frame) => FramePublication::Captured(Arc::new(frame)),
        Err(error) => {
            log::error!("UI runtime 按需截图失败: {error:#}");
            FramePublication::Failed(FrameCaptureFailure {
                failed_at: Instant::now(),
                reason: Arc::from(format!("{error:#}")),
            })
        }
    };
    publish_frame(publication, demands, latest_frame);
}

fn publish_frame(
    publication: FramePublication,
    demands: &mut HashMap<u64, ActiveFrameDemand>,
    latest_frame: &Mutex<Option<Arc<CapturedFrame>>>,
) {
    if let FramePublication::Captured(frame) = &publication
        && let Ok(mut latest) = latest_frame.lock()
    {
        *latest = Some(Arc::clone(frame));
    }

    let completed_at = match &publication {
        FramePublication::Captured(frame) => frame.captured_at(),
        FramePublication::Failed(failure) => failure.failed_at(),
    };
    demands.retain(|_, demand| !demand.slot.is_closed());
    for demand in demands.values_mut() {
        demand.next_due = completed_at + demand.interval;
        demand.slot.publish(publication.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classified(kind: UiStateKind) -> UiStateClassification {
        UiStateClassification::new(kind, format!("{kind:?}"))
    }

    struct ImageDevice(DynamicImage);

    impl UiDevice for ImageDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(self.0.clone())
        }
    }

    struct PixelClassifier(Arc<AtomicU64>);

    impl UiStateClassifier for PixelClassifier {
        fn classify(&mut self, image: &DynamicImage) -> Result<UiStateClassification> {
            use image::GenericImageView;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(UiStateClassification::with_evidence(
                UiStateKind::Primary,
                "primary:marker",
                UiStateEvidence::new(
                    Vec::new(),
                    Some(UiMarkerProbeEvidence::new(
                        UiEvidenceRect::new(0, 0, 128, 72),
                        (128, 72),
                        1,
                        0,
                        0,
                        vec![crate::ui::template::TemplateHit {
                            kind: "blue".into(),
                            x: i32::from(image.get_pixel(1, 1).0[0]),
                            y: 1,
                            width: 1,
                            height: 1,
                            score: 1.0,
                        }],
                    )),
                    "pixel_probe",
                ),
            ))
        }

        fn classify_cached(
            &mut self,
            _: &DynamicImage,
            cached: &UiStateClassification,
        ) -> Result<UiStateClassification> {
            Ok(cached.clone())
        }
    }

    #[test]
    fn sparse_signature_collision_in_chat_roi_reclassifies_and_identical_pixels_revalidate() {
        use image::GenericImage;
        let calls = Arc::new(AtomicU64::new(0));
        let mut runtime = UiObservationRuntime::new(
            Some(Box::new(PixelClassifier(calls.clone()))),
            1,
            Arc::new(Mutex::new(None)),
        );
        let mut device = ImageDevice(DynamicImage::new_rgba8(128, 72));
        let canvas = crate::ui::frame::Canvas {
            width: 128,
            height: 72,
            resize: false,
        };
        let first = runtime.capture(&mut device).unwrap();
        assert_eq!(
            crate::ui::frame::from_captured_frame(&first, &canvas)
                .marker_hits_for_image()
                .unwrap()[0]
                .x,
            0
        );
        // The 64x36 grid samples even coordinates; this chat ROI pixel is invisible to it.
        device.0.put_pixel(1, 1, image::Rgba([7, 0, 0, 255]));
        assert_eq!(frame_signature(first.image()), frame_signature(&device.0));
        let changed = runtime.capture(&mut device).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            crate::ui::frame::from_captured_frame(&changed, &canvas)
                .marker_hits_for_image()
                .unwrap()[0]
                .x,
            7
        );
        let identical = runtime.capture(&mut device).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            crate::ui::frame::from_captured_frame(&identical, &canvas)
                .marker_hits_for_image()
                .unwrap()[0]
                .x,
            7
        );
        let probe = identical
            .ui_state()
            .unwrap()
            .classified()
            .unwrap()
            .classification()
            .evidence()
            .marker_probe()
            .unwrap();
        assert_eq!(probe.source_frame_id(), Some(changed.id()));
        assert_eq!(probe.validated_frame_id(), Some(identical.id()));
        // Old observation attached to changed pixels has not been revalidated.
        let stale = CapturedFrame {
            ui_state: first.ui_state().cloned(),
            ..changed
        };
        assert!(
            crate::ui::frame::from_captured_frame(&stale, &canvas)
                .marker_hits_for_image()
                .is_none()
        );
        assert_eq!(
            identical
                .ui_state()
                .unwrap()
                .classified()
                .unwrap()
                .stable_kind(),
            Some(UiStateKind::Primary)
        );
    }

    #[derive(Default)]
    struct IsolationProbeDevice {
        captures: Arc<Mutex<usize>>,
        presses: Arc<Mutex<usize>>,
        foreground_checks: Arc<Mutex<usize>>,
    }

    impl UiDevice for IsolationProbeDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            *self.captures.lock().unwrap() += 1;
            Ok(DynamicImage::new_rgba8(2, 2))
        }

        fn press_key(&mut self, _key: Key) -> Result<()> {
            *self.presses.lock().unwrap() += 1;
            Ok(())
        }

        fn ensure_foreground(&mut self) -> Result<()> {
            *self.foreground_checks.lock().unwrap() += 1;
            Ok(())
        }

        fn clipboard_text(&mut self) -> Result<Option<String>> {
            Ok(Some("probe-clipboard".to_owned()))
        }
    }

    /// 装饰器必须**逐个显式转发**每个只读观测方法：漏掉一个就会静默退回 trait 默认实现。
    /// 真机上表现为 does not support ... observation —— 本轮就是这样漏掉了剪贴板读取，
    /// 而 capture / window_* 因为转发过所以看起来一切正常。
    #[test]
    fn isolated_device_forwards_read_only_observations() {
        let mut isolated = IsolatedDevice(IsolationProbeDevice::default());
        crate::debug_mode::with_test_execution(true, false, || {
            assert_eq!(
                isolated.clipboard_text().unwrap(),
                Some("probe-clipboard".to_owned())
            );
        });
    }

    #[test]
    fn isolated_device_rejects_internal_input_but_preserves_capture() {
        let device = IsolationProbeDevice::default();
        let captures = device.captures.clone();
        let presses = device.presses.clone();
        let mut isolated = IsolatedDevice(device);

        crate::debug_mode::with_test_execution(true, false, || {
            assert!(isolated.press_key(Key::Return).is_err());
            let frame = isolated.capture().unwrap();
            assert_eq!((frame.width(), frame.height()), (2, 2));
        });

        assert_eq!(*captures.lock().unwrap(), 1);
        assert_eq!(*presses.lock().unwrap(), 0);
    }

    #[test]
    fn isolated_device_allows_only_explicit_external_input() {
        let device = IsolationProbeDevice::default();
        let presses = device.presses.clone();
        let foreground_checks = device.foreground_checks.clone();
        let mut isolated = IsolatedDevice(device);

        crate::debug_mode::with_test_execution(true, true, || {
            isolated.ensure_foreground().unwrap();
            isolated.press_key(Key::Return).unwrap();
        });

        assert_eq!(*foreground_checks.lock().unwrap(), 1);
        assert_eq!(*presses.lock().unwrap(), 1);
    }

    #[test]
    fn isolated_device_does_not_leak_test_authorization_between_scopes() {
        let device = IsolationProbeDevice::default();
        let presses = device.presses.clone();
        let mut isolated = IsolatedDevice(device);

        crate::debug_mode::with_test_execution(true, true, || {
            isolated.press_key(Key::Return).unwrap();
        });
        crate::debug_mode::with_test_execution(true, false, || {
            assert!(isolated.press_key(Key::Return).is_err());
        });

        assert_eq!(*presses.lock().unwrap(), 1);
    }

    #[test]
    fn formal_leases_are_counted_and_release_independently() {
        let coordinator = UiCoordinator::new();
        let first = coordinator.acquire_formal();
        let second = coordinator.acquire_formal();
        assert!(!coordinator.scan_may_run());
        drop(first);
        assert!(!coordinator.scan_may_run());
        drop(second);
        assert!(coordinator.scan_may_run());
    }

    #[test]
    fn formal_lease_is_released_during_unwind() {
        let coordinator = UiCoordinator::new();
        let unwind = std::panic::catch_unwind({
            let coordinator = coordinator.clone();
            move || {
                let _lease = coordinator.acquire_formal();
                panic!("测试 panic");
            }
        });
        assert!(unwind.is_err());
        assert!(coordinator.scan_may_run());
    }

    #[test]
    fn scan_freshness_boundary_tracks_ui_ownership_but_not_ordinary_scans() {
        let coordinator = UiCoordinator::new();
        assert_eq!(coordinator.try_acquire_scan().unwrap().not_before(), None);
        let empty = coordinator.permit_scans_for_current_thread();
        drop(empty);
        assert_eq!(coordinator.try_acquire_scan().unwrap().not_before(), None);
        let formal = coordinator.acquire_formal();
        let acquired = coordinator.state.lock().unwrap().scan_not_before.unwrap();
        let reply_frame_captured_at = Instant::now();
        let permit = coordinator.permit_scans_for_current_thread();
        let cutoff = coordinator
            .try_acquire_scan()
            .unwrap()
            .not_before()
            .unwrap();
        assert!(cutoff >= acquired);
        assert!(cutoff >= reply_frame_captured_at);
        assert_eq!(
            coordinator.try_acquire_scan().unwrap().not_before(),
            Some(cutoff)
        );
        drop(permit);
        let revoked = coordinator.state.lock().unwrap().scan_not_before.unwrap();
        assert!(revoked >= cutoff);
        assert!(coordinator.try_acquire_scan().is_none());
        drop(formal);
        let released = coordinator
            .try_acquire_scan()
            .unwrap()
            .not_before()
            .unwrap();
        assert!(released >= revoked);
        assert_eq!(
            coordinator.try_acquire_scan().unwrap().not_before(),
            Some(released)
        );
    }

    #[test]
    fn nested_permit_updates_freshness_without_changing_existing_scan_snapshot() {
        let coordinator = UiCoordinator::new();
        let _formal = coordinator.acquire_formal();
        let outer = coordinator.permit_scans_for_current_thread();
        let scan = coordinator.try_acquire_scan().unwrap();
        let snapshot = scan.not_before().unwrap();
        let inner = coordinator.permit_scans_for_current_thread();
        let nested = coordinator
            .try_acquire_scan()
            .unwrap()
            .not_before()
            .unwrap();
        assert!(nested >= snapshot);
        assert_eq!(scan.not_before(), Some(snapshot));
        drop(scan);
        drop(inner);
        let reopened = coordinator
            .try_acquire_scan()
            .unwrap()
            .not_before()
            .unwrap();
        assert!(reopened >= nested);
        drop(outer);
        assert!(coordinator.try_acquire_scan().is_none());
    }

    #[test]
    fn scan_permits_are_nested_and_do_not_cover_later_leases() {
        let coordinator = UiCoordinator::new();
        let first = coordinator.acquire_formal();
        let second = coordinator.acquire_formal();
        let outer = coordinator.permit_scans_for_current_thread();
        let inner = coordinator.permit_scans_for_current_thread();
        assert!(coordinator.try_acquire_scan().is_some());
        drop(outer);
        assert!(coordinator.try_acquire_scan().is_some());
        let later = coordinator.acquire_formal();
        assert!(coordinator.try_acquire_scan().is_none());
        drop(later);
        assert!(coordinator.try_acquire_scan().is_some());
        drop(first);
        drop(inner);
        assert!(coordinator.try_acquire_scan().is_none());
        drop(second);
        assert!(coordinator.try_acquire_scan().is_some());
    }

    #[test]
    fn scan_permits_cannot_override_another_owner() {
        let coordinator = UiCoordinator::new();
        let _formal = coordinator.acquire_formal();
        let other = coordinator.clone();
        thread::spawn(move || {
            let _empty = other.permit_scans_for_current_thread();
            assert!(other.try_acquire_scan().is_none());
            let _other_formal = other.acquire_formal();
            let _permit = other.permit_scans_for_current_thread();
            assert!(other.try_acquire_scan().is_none());
        })
        .join()
        .unwrap();
        assert!(coordinator.try_acquire_scan().is_none());
    }

    #[test]
    fn scan_permit_and_scan_lease_release_during_unwind() {
        let coordinator = UiCoordinator::new();
        let _formal = coordinator.acquire_formal();
        let unwind = std::panic::catch_unwind({
            let coordinator = coordinator.clone();
            move || {
                let _permit = coordinator.permit_scans_for_current_thread();
                let _scan = coordinator.try_acquire_scan().unwrap();
                panic!("scan failure");
            }
        });
        assert!(unwind.is_err());
        assert!(coordinator.try_acquire_scan().is_none());
        let _permit = coordinator.permit_scans_for_current_thread();
        assert!(coordinator.try_acquire_scan().is_some());
    }

    // Observe the actual admission barrier, rather than assuming a sleeping
    // worker reached it. Bound the wait so a regression fails instead of hangs.
    fn wait_until_scans_blocked(coordinator: &UiCoordinator) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while coordinator.scan_may_run() {
            assert!(Instant::now() < deadline, "scan admission was not revoked");
            thread::yield_now();
        }
    }

    #[test]
    fn scan_permit_drop_revokes_before_waiting_for_all_in_flight_scans() {
        let coordinator = UiCoordinator::new();
        let (permitted_tx, permitted_rx) = mpsc::channel();
        let (revoke_tx, revoke_rx) = mpsc::channel();
        let (resumed_tx, resumed_rx) = mpsc::channel();
        let (finish_tx, finish_rx) = mpsc::channel();
        let owner = coordinator.clone();
        let worker = thread::spawn(move || {
            let _formal = owner.acquire_formal();
            let permit = owner.permit_scans_for_current_thread();
            permitted_tx.send(()).unwrap();
            revoke_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(permit);
            resumed_tx.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        permitted_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let first = coordinator.try_acquire_scan().unwrap();
        let second = coordinator.try_acquire_scan().unwrap();
        revoke_tx.send(()).unwrap();
        wait_until_scans_blocked(&coordinator);
        assert!(coordinator.try_acquire_scan().is_none());
        assert!(resumed_rx.try_recv().is_err());
        drop(first);
        assert!(resumed_rx.try_recv().is_err());
        drop(second);
        resumed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(coordinator.try_acquire_scan().is_none());
        finish_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(coordinator.try_acquire_scan().is_some());
    }

    #[test]
    fn formal_acquisition_waits_for_already_admitted_scan() {
        let coordinator = UiCoordinator::new();
        let scan = coordinator.try_acquire_scan().unwrap();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let owner = coordinator.clone();
        let worker = thread::spawn(move || {
            let _formal = owner.acquire_formal();
            acquired_tx.send(()).unwrap();
        });
        wait_until_scans_blocked(&coordinator);
        assert!(coordinator.try_acquire_scan().is_none());
        assert!(acquired_rx.try_recv().is_err());
        drop(scan);
        acquired_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn nested_scan_permit_drop_drains_without_revoking_outer_permit() {
        let coordinator = UiCoordinator::new();
        let _formal = coordinator.acquire_formal();
        let outer = coordinator.permit_scans_for_current_thread();
        let inner = coordinator.permit_scans_for_current_thread();
        let scan = coordinator.try_acquire_scan().unwrap();
        let observer = coordinator.clone();
        let worker = thread::spawn(move || {
            wait_until_scans_blocked(&observer);
            assert!(observer.try_acquire_scan().is_none());
            drop(scan);
        });
        drop(inner);
        worker.join().unwrap();
        assert!(coordinator.try_acquire_scan().is_some());
        drop(outer);
        assert!(coordinator.try_acquire_scan().is_none());
    }

    #[test]
    fn unknown_is_a_transition_and_known_states_must_restabilize() {
        let mut tracker = UiStateTracker::new(2);

        let first = tracker.observe(1, classified(UiStateKind::Primary));
        assert!(first.classified().unwrap().is_transitioning());
        let stable = tracker.observe(2, classified(UiStateKind::Primary));
        assert_eq!(
            stable.classified().unwrap().stable_kind(),
            Some(UiStateKind::Primary)
        );

        let unknown = tracker.observe(3, classified(UiStateKind::Unknown));
        let unknown = unknown.classified().unwrap();
        assert!(unknown.is_transitioning());
        assert_eq!(unknown.last_stable_kind(), Some(UiStateKind::Primary));

        let returning = tracker.observe(4, classified(UiStateKind::Primary));
        assert!(returning.classified().unwrap().is_transitioning());
        let stable_again = tracker.observe(5, classified(UiStateKind::Primary));
        assert_eq!(
            stable_again.classified().unwrap().stable_kind(),
            Some(UiStateKind::Primary)
        );
    }

    #[test]
    fn the_same_frame_cannot_satisfy_stability_twice() {
        let mut tracker = UiStateTracker::new(2);

        let first = tracker.observe(7, classified(UiStateKind::Secondary));
        let duplicate = tracker.observe(7, classified(UiStateKind::Secondary));

        assert_eq!(first, duplicate);
        assert_eq!(duplicate.classified().unwrap().candidate_count(), 1);
        assert!(duplicate.classified().unwrap().is_transitioning());
    }

    #[test]
    fn game_states_use_shared_stability_and_do_not_survive_unknown_frames() {
        for kind in [UiStateKind::GameGate, UiStateKind::Overworld] {
            let mut tracker = UiStateTracker::new(2);
            assert!(
                tracker
                    .observe(1, classified(kind))
                    .classified()
                    .unwrap()
                    .is_transitioning()
            );
            assert_eq!(
                tracker
                    .observe(2, classified(kind))
                    .classified()
                    .unwrap()
                    .stable_kind(),
                Some(kind)
            );
            let unknown = tracker.observe(3, classified(UiStateKind::Unknown));
            assert_eq!(unknown.classified().unwrap().stable_kind(), None);
            assert_eq!(unknown.classified().unwrap().last_stable_kind(), Some(kind));
            assert!(
                tracker
                    .observe(4, classified(kind))
                    .classified()
                    .unwrap()
                    .is_transitioning()
            );
        }
    }

    #[test]
    fn classification_failure_resets_the_candidate_without_reusing_last_stable() {
        let mut tracker = UiStateTracker::new(2);
        tracker.observe(1, classified(UiStateKind::Primary));
        tracker.observe(2, classified(UiStateKind::Primary));

        let failed = tracker.observe_failure(3, Arc::<str>::from("template unavailable"));
        assert_eq!(failed.failure_reason(), Some("template unavailable"));
        let next = tracker.observe(4, classified(UiStateKind::Primary));
        assert!(next.classified().unwrap().is_transitioning());
        assert_eq!(next.classified().unwrap().candidate_count(), 1);
    }

    #[test]
    fn different_template_labels_do_not_share_stability_progress() {
        let mut tracker = UiStateTracker::new(2);

        tracker.observe(
            1,
            UiStateClassification::new(UiStateKind::Primary, "primary:friend"),
        );
        let changed = tracker.observe(
            2,
            UiStateClassification::new(UiStateKind::Primary, "primary:marker"),
        );
        assert!(changed.classified().unwrap().is_transitioning());
        assert_eq!(changed.classified().unwrap().candidate_count(), 1);

        let stable = tracker.observe(
            3,
            UiStateClassification::new(UiStateKind::Primary, "primary:marker"),
        );
        assert_eq!(
            stable.classified().unwrap().stable_kind(),
            Some(UiStateKind::Primary)
        );
    }

    struct AlwaysPrimaryClassifier;

    impl UiStateClassifier for AlwaysPrimaryClassifier {
        fn classify(&mut self, _image: &DynamicImage) -> Result<UiStateClassification> {
            Ok(UiStateClassification::new(
                UiStateKind::Primary,
                "primary:test",
            ))
        }
    }

    struct CapturingRoutine {
        captured: SyncSender<u64>,
        release: Receiver<()>,
    }

    impl sealed::UiRoutineSealed for CapturingRoutine {}

    impl UiRoutine for CapturingRoutine {
        type Output = u64;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            let id = context.capture_frame().unwrap().id();
            self.captured.send(id).unwrap();
            self.release.recv().unwrap();
            id
        }
    }

    struct BlankDevice;

    impl UiDevice for BlankDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(16, 16))
        }
    }

    #[test]
    fn routine_captures_publish_immediately_and_update_state() {
        let runtime =
            UiRuntime::start_with_state_classifier(BlankDevice, 4, AlwaysPrimaryClassifier, 2)
                .unwrap();
        let handle = runtime.handle();
        let subscription = handle
            .declare_frame_demand(FrameDemand::new(Duration::from_secs(1)).unwrap())
            .unwrap();
        let initial_id = match subscription.recv().unwrap() {
            FramePublication::Captured(frame) => frame.id(),
            FramePublication::Failed(failure) => panic!("initial capture failed: {failure:?}"),
        };

        let (captured_tx, captured_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let operation = handle
            .submit(CapturingRoutine {
                captured: captured_tx,
                release: release_rx,
            })
            .unwrap();
        let routine_id = captured_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(routine_id > initial_id);
        assert_eq!(handle.latest_ui_state().unwrap().frame_id(), routine_id);
        match subscription.recv_timeout(Duration::from_secs(1)).unwrap() {
            FramePublication::Captured(frame) => {
                assert_eq!(frame.id(), routine_id);
                assert_eq!(handle.latest_frame().unwrap().id(), routine_id);
            }
            FramePublication::Failed(failure) => panic!("routine capture failed: {failure:?}"),
        }
        assert!(matches!(
            subscription.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));
        release_tx.send(()).unwrap();
        assert_eq!(operation.wait().unwrap(), routine_id);

        runtime.shutdown().unwrap();
    }

    #[test]
    fn frame_slot_replaces_stale_frames_and_wakes_for_failures() {
        let slot = Arc::new(FramePublicationSlot::default());
        let captured = |id| {
            FramePublication::Captured(Arc::new(CapturedFrame {
                id,
                image: Arc::new(DynamicImage::new_rgba8(2, 2)),
                captured_at: Instant::now(),
                ui_state: None,
            }))
        };
        slot.publish(captured(1));
        slot.publish(captured(2));
        match slot.recv_timeout(Duration::ZERO).unwrap() {
            FramePublication::Captured(frame) => assert_eq!(frame.id(), 2),
            FramePublication::Failed(_) => panic!("expected fresh frame"),
        }
        let waiting = Arc::clone(&slot);
        let waiter = thread::spawn(move || waiting.recv_timeout(Duration::from_secs(1)));
        slot.publish(FramePublication::Failed(FrameCaptureFailure {
            failed_at: Instant::now(),
            reason: Arc::from("capture failed"),
        }));
        match waiter.join().unwrap().unwrap() {
            FramePublication::Failed(failure) => assert_eq!(failure.reason(), "capture failed"),
            FramePublication::Captured(_) => panic!("expected failure wakeup"),
        }
        slot.close();
        assert!(matches!(
            slot.recv_timeout(Duration::ZERO),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    struct CountingDevice {
        captures: Arc<AtomicU64>,
        owner: Option<thread::ThreadId>,
        fail_after_first: bool,
    }

    impl UiDevice for CountingDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            let current = thread::current().id();
            if let Some(owner) = self.owner {
                assert_eq!(current, owner, "captures must remain on the UI worker");
            } else {
                self.owner = Some(current);
            }
            let count = self.captures.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_after_first && count > 1 {
                bail!("synthetic capture failure");
            }
            Ok(DynamicImage::new_rgba8(16, 16))
        }
    }

    struct WaitingRoutine {
        started: SyncSender<()>,
        duration: Duration,
    }

    impl sealed::UiRoutineSealed for WaitingRoutine {}

    impl UiRoutine for WaitingRoutine {
        type Output = Result<()>;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            self.started.send(()).unwrap();
            Ok(context.wait_cancellable(self.duration)?)
        }
    }

    #[test]
    fn long_routine_wait_publishes_bounded_frames_on_the_ui_worker() {
        let captures = Arc::new(AtomicU64::new(0));
        let runtime = UiRuntime::start(
            CountingDevice {
                captures: captures.clone(),
                owner: None,
                fail_after_first: false,
            },
            4,
        )
        .unwrap();
        let handle = runtime.handle();
        let subscription = handle
            .declare_frame_demand(FrameDemand::new(Duration::from_millis(35)).unwrap())
            .unwrap();
        let initial = match subscription.recv_timeout(Duration::from_secs(1)).unwrap() {
            FramePublication::Captured(frame) => frame,
            FramePublication::Failed(failure) => panic!("initial capture failed: {failure:?}"),
        };
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let operation = handle
            .submit(WaitingRoutine {
                started: started_tx,
                duration: Duration::from_secs(2),
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let first = match subscription
            .recv_timeout(Duration::from_millis(500))
            .unwrap()
        {
            FramePublication::Captured(frame) => frame,
            FramePublication::Failed(failure) => panic!("wait capture failed: {failure:?}"),
        };
        let second = match subscription
            .recv_timeout(Duration::from_millis(500))
            .unwrap()
        {
            FramePublication::Captured(frame) => frame,
            FramePublication::Failed(failure) => panic!("wait capture failed: {failure:?}"),
        };
        assert!(initial.id() < first.id() && first.id() < second.id());
        assert!(captures.load(Ordering::SeqCst) >= 3);
        operation.cancel();
        assert!(!runtime.shutdown().unwrap().timed_out());
        let stopped_at = captures.load(Ordering::SeqCst);
        let _ = subscription.recv_timeout(Duration::from_millis(80));
        assert_eq!(captures.load(Ordering::SeqCst), stopped_at);
    }

    #[test]
    fn cooperative_wait_publishes_capture_failures_without_aborting_routine() {
        let captures = Arc::new(AtomicU64::new(0));
        let runtime = UiRuntime::start(
            CountingDevice {
                captures: captures.clone(),
                owner: None,
                fail_after_first: true,
            },
            4,
        )
        .unwrap();
        let handle = runtime.handle();
        let subscription = handle
            .declare_frame_demand(FrameDemand::new(Duration::from_millis(30)).unwrap())
            .unwrap();
        assert!(matches!(
            subscription.recv_timeout(Duration::from_secs(1)).unwrap(),
            FramePublication::Captured(_)
        ));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let operation = handle
            .submit(WaitingRoutine {
                started: started_tx,
                duration: Duration::from_millis(180),
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        match subscription
            .recv_timeout(Duration::from_millis(500))
            .unwrap()
        {
            FramePublication::Failed(failure) => {
                assert!(failure.reason().contains("synthetic capture failure"))
            }
            FramePublication::Captured(_) => panic!("expected synthetic failure"),
        };
        assert!(operation.wait().unwrap().is_ok());
        assert!(captures.load(Ordering::SeqCst) >= 2);
        runtime.shutdown().unwrap();
    }

    #[derive(Default)]
    struct RecordingProgressSink {
        events: Mutex<Vec<UiRoutineProgress>>,
    }

    impl UiRoutineProgressSink for RecordingProgressSink {
        fn publish(&self, progress: UiRoutineProgress) {
            self.events.lock().unwrap().push(progress);
        }
    }

    struct ContextProbeRoutine;

    impl sealed::UiRoutineSealed for ContextProbeRoutine {}

    impl UiRoutine for ContextProbeRoutine {
        type Output = UiOperationId;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            let operation_id = context.operation_id();
            context.publish_progress(UiRoutineProgressStage::NormalizingStart);
            operation_id
        }
    }

    struct UnusedDevice;

    impl UiDevice for UnusedDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            bail!("context probe should not capture")
        }
    }

    #[test]
    fn routine_context_correlates_redacted_progress_with_the_operation() {
        let progress = Arc::new(RecordingProgressSink::default());
        let runtime = UiRuntime::start_with_progress(UnusedDevice, 1, progress.clone()).unwrap();

        let operation = runtime.handle().submit(ContextProbeRoutine).unwrap();
        let operation_id = operation.id();

        assert_eq!(operation.wait().unwrap(), operation_id);
        assert_eq!(
            *progress.events.lock().unwrap(),
            vec![UiRoutineProgress::new(
                operation_id,
                UiRoutineProgressStage::NormalizingStart,
            )]
        );
        let report = runtime.shutdown().unwrap();
        assert!(!report.timed_out());
        assert!(!report.detached());
    }

    struct BlockingRoutine {
        started: SyncSender<()>,
        release: Receiver<()>,
    }

    impl sealed::UiRoutineSealed for BlockingRoutine {}

    impl UiRoutine for BlockingRoutine {
        type Output = ();

        fn execute(self, _context: &mut UiRoutineContext<'_>) -> Self::Output {
            let _ = self.started.send(());
            let _ = self.release.recv();
        }
    }

    struct CancellableWaitRoutine {
        started: SyncSender<()>,
        finished: SyncSender<bool>,
    }

    impl sealed::UiRoutineSealed for CancellableWaitRoutine {}

    impl UiRoutine for CancellableWaitRoutine {
        type Output = ();

        fn execute(self, context: &mut UiRoutineContext<'_>) {
            let _ = self.started.send(());
            let cancelled = context.wait_cancellable(Duration::from_secs(5)).is_err();
            let _ = self.finished.send(cancelled);
        }
    }

    #[test]
    fn timeout_cancels_running_cooperative_wait() {
        let runtime = UiRuntime::start(UnusedDevice, 1).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let operation = runtime
            .handle()
            .submit(CancellableWaitRoutine {
                started: started_tx,
                finished: finished_tx,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(
            operation.wait_timeout(Duration::from_millis(10)),
            Err(UiWaitTimeoutError { .. })
        ));
        assert!(finished_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    struct NotifyingRoutine(SyncSender<()>);

    impl sealed::UiRoutineSealed for NotifyingRoutine {}

    impl UiRoutine for NotifyingRoutine {
        type Output = ();

        fn execute(self, _context: &mut UiRoutineContext<'_>) {
            let _ = self.0.send(());
        }
    }

    #[test]
    fn cancelled_queued_routine_never_executes() {
        let runtime = UiRuntime::start(UnusedDevice, 2).unwrap();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let blocker = runtime
            .handle()
            .submit(BlockingRoutine {
                started: started_tx,
                release: release_rx,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let (notified_tx, notified_rx) = mpsc::sync_channel(1);
        let queued = runtime
            .handle()
            .submit(NotifyingRoutine(notified_tx))
            .unwrap();
        queued.cancel();
        release_tx.send(()).unwrap();
        blocker.wait().unwrap();
        let followup = runtime.handle().submit(ContextProbeRoutine).unwrap();
        let followup_id = followup.id();
        assert_eq!(followup_id, followup.wait().unwrap());
        assert!(matches!(
            notified_rx.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        assert!(!runtime.shutdown().unwrap().timed_out());
    }

    #[test]
    fn shutdown_does_not_block_when_queue_is_full() {
        let mut runtime = UiRuntime::start(UnusedDevice, 1).unwrap();
        let handle = runtime.handle();
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let _blocking = handle
            .submit(BlockingRoutine {
                started: started_sender,
                release: release_receiver,
            })
            .unwrap();
        started_receiver.recv().unwrap();
        let _queued = handle.submit(ContextProbeRoutine).unwrap();

        let started_at = Instant::now();
        let report = runtime
            .stop_worker(Duration::from_millis(50))
            .expect("关闭应返回报告");

        assert!(started_at.elapsed() < Duration::from_secs(1));
        assert!(report.timed_out());
        assert!(report.detached());
        assert!(matches!(
            handle.submit(ContextProbeRoutine),
            Err(UiSubmitError::RuntimeStopped)
        ));
        let _ = release_sender.send(());
    }

    #[test]
    fn blocking_job_shutdown_reports_timeout_and_detachment() {
        let mut runtime = UiRuntime::start(UnusedDevice, 1).unwrap();
        let (started_sender, started_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let _operation = runtime
            .handle()
            .submit(BlockingRoutine {
                started: started_sender,
                release: release_receiver,
            })
            .unwrap();
        started_receiver.recv().unwrap();

        let report = runtime
            .stop_worker(Duration::from_millis(50))
            .expect("关闭应返回报告");

        assert!(report.timed_out());
        assert!(report.detached());
        let _ = release_sender.send(());
    }
}
