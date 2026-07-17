use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use enigo::Key;
use image::DynamicImage;

pub trait UiDevice: Send + 'static {
    fn capture(&mut self) -> Result<DynamicImage>;

    fn press_key(&mut self, _key: Key) -> Result<()> {
        bail!("UI device does not support keyboard input")
    }

    fn click_point(&mut self, _x: i32, _y: i32) -> Result<()> {
        bail!("UI device does not support mouse input")
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
    Unknown,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiMarkerProbeEvidence {
    search_rect: UiEvidenceRect,
    blue_count: usize,
    yellow_count: usize,
    pink_count: usize,
}

impl UiMarkerProbeEvidence {
    pub fn new(
        search_rect: UiEvidenceRect,
        blue_count: usize,
        yellow_count: usize,
        pink_count: usize,
    ) -> Self {
        Self {
            search_rect,
            blue_count,
            yellow_count,
            pink_count,
        }
    }

    pub fn search_rect(&self) -> UiEvidenceRect {
        self.search_rect
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
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiStateEvidence {
    template_probes: Vec<UiTemplateProbeEvidence>,
    marker_probe: Option<UiMarkerProbeEvidence>,
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
            final_rule: final_rule.into(),
        }
    }

    pub fn template_probes(&self) -> &[UiTemplateProbeEvidence] {
        &self.template_probes
    }

    pub fn marker_probe(&self) -> Option<&UiMarkerProbeEvidence> {
        self.marker_probe.as_ref()
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

    fn same_candidate_as(&self, other: &Self) -> bool {
        self.kind == other.kind && self.label == other.label
    }
}

pub trait UiStateClassifier: Send + 'static {
    fn classify(&mut self, image: &DynamicImage) -> Result<UiStateClassification>;
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
        }
    }

    fn capture(&mut self, device: &mut dyn UiDevice) -> Result<CapturedFrame> {
        let image = Arc::new(device.capture()?);
        let captured_at = Instant::now();
        self.next_frame_id = self.next_frame_id.wrapping_add(1).max(1);
        let frame_id = self.next_frame_id;
        let ui_state = self.classifier.as_mut().map(|classifier| {
            let observation = match classifier.classify(&image) {
                Ok(classification) => self.tracker.observe(frame_id, classification),
                Err(error) => self
                    .tracker
                    .observe_failure(frame_id, Arc::<str>::from(format!("{error:#}"))),
            };
            if let Ok(mut latest) = self.latest_state.lock() {
                *latest = Some(observation.clone());
            }
            observation
        });
        Ok(CapturedFrame {
            id: frame_id,
            image,
            captured_at,
            ui_state,
        })
    }
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
}

impl<'a> UiRoutineContext<'a> {
    fn new(
        operation_id: UiOperationId,
        device: &'a mut dyn UiDevice,
        progress: &'a dyn UiRoutineProgressSink,
        observation: &'a mut UiObservationRuntime,
        latest_frame: &'a Mutex<Option<Arc<CapturedFrame>>>,
    ) -> Self {
        Self {
            operation_id,
            device,
            progress,
            observation,
            latest_frame,
        }
    }

    pub fn operation_id(&self) -> UiOperationId {
        self.operation_id
    }

    pub fn device(&mut self) -> &mut dyn UiDevice {
        self.device
    }

    pub fn capture_frame(&mut self) -> Result<CapturedFrame> {
        let frame = self.observation.capture(self.device)?;
        if let Ok(mut latest) = self.latest_frame.lock() {
            *latest = Some(Arc::new(frame.clone()));
        }
        Ok(frame)
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

    fn frame_publication(output: &Self::Output) -> Option<FramePublication> {
        Some(match output {
            Ok(frame) => FramePublication::Captured(Arc::new(frame.clone())),
            Err(failure) => FramePublication::Failed(FrameCaptureFailure {
                failed_at: Instant::now(),
                reason: Arc::from(failure.to_string()),
            }),
        })
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
}

impl<T> UiOperation<T> {
    pub fn id(&self) -> UiOperationId {
        self.id
    }

    pub fn wait(self) -> Result<T, UiReceiveError> {
        self.response.recv().map_err(|_| UiReceiveError)
    }
}

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
        } = *self;
        let mut context =
            UiRoutineContext::new(operation_id, device, progress, observation, latest_frame);
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
        sender: SyncSender<FramePublication>,
    },
    RemoveFrameDemand(u64),
    Shutdown,
}

struct RuntimeChannel {
    sender: SyncSender<RuntimeMessage>,
    accepting: Mutex<bool>,
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
        let message = RuntimeMessage::Execute(Box::new(TypedUiJob {
            operation_id: id,
            routine,
            response,
        }));
        match self.channel.sender.try_send(message) {
            Ok(()) => Ok(UiOperation {
                id,
                response: receiver,
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
        let (sender, receiver) = mpsc::sync_channel(1);
        let message = RuntimeMessage::AddFrameDemand { id, demand, sender };
        match self.channel.sender.try_send(message) {
            Ok(()) => Ok(FrameDemandSubscription {
                id,
                receiver,
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

pub struct FrameDemandSubscription {
    id: u64,
    receiver: Receiver<FramePublication>,
    channel: Arc<RuntimeChannel>,
    active: bool,
}

impl FrameDemandSubscription {
    pub fn recv(&self) -> Result<FramePublication, RecvError> {
        self.receiver.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<FramePublication, RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
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
            .send(RuntimeMessage::RemoveFrameDemand(self.id))
            .map_err(|_| UiSubmitError::RuntimeStopped)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for FrameDemandSubscription {
    fn drop(&mut self) {
        if self.active {
            let _ = self
                .channel
                .sender
                .try_send(RuntimeMessage::RemoveFrameDemand(self.id));
        }
    }
}

pub struct UiRuntime {
    handle: UiRuntimeHandle,
    worker: Option<JoinHandle<()>>,
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

        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let channel = Arc::new(RuntimeChannel {
            sender,
            accepting: Mutex::new(true),
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
            worker: Some(worker),
        })
    }

    pub fn handle(&self) -> UiRuntimeHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) -> Result<(), UiShutdownError> {
        self.stop_worker()
    }

    fn stop_worker(&mut self) -> Result<(), UiShutdownError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        if let Ok(mut accepting) = self.handle.channel.accepting.lock() {
            *accepting = false;
            let _ = self.handle.channel.sender.send(RuntimeMessage::Shutdown);
        }
        worker.join().map_err(|_| UiShutdownError)
    }
}

impl Drop for UiRuntime {
    fn drop(&mut self) {
        let _ = self.stop_worker();
    }
}

struct ActiveFrameDemand {
    interval: Duration,
    next_due: Instant,
    sender: SyncSender<FramePublication>,
}

fn run_ui_runtime(
    device: impl UiDevice,
    receiver: Receiver<RuntimeMessage>,
    latest_frame: Arc<Mutex<Option<Arc<CapturedFrame>>>>,
    latest_ui_state: Arc<Mutex<Option<UiStateObservation>>>,
    progress: Arc<dyn UiRoutineProgressSink>,
    classifier: Option<Box<dyn UiStateClassifier>>,
    stable_count: u32,
) {
    let mut device = device;
    let mut demands = HashMap::<u64, ActiveFrameDemand>::new();
    let mut observation = UiObservationRuntime::new(classifier, stable_count, latest_ui_state);
    loop {
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
            Some(RuntimeMessage::Execute(job)) => job.execute(
                &mut device,
                progress.as_ref(),
                &mut observation,
                &mut demands,
                &latest_frame,
            ),
            Some(RuntimeMessage::AddFrameDemand { id, demand, sender }) => {
                demands.insert(
                    id,
                    ActiveFrameDemand {
                        interval: demand.interval,
                        next_due: Instant::now(),
                        sender,
                    },
                );
            }
            Some(RuntimeMessage::RemoveFrameDemand(id)) => {
                demands.remove(&id);
            }
            Some(RuntimeMessage::Shutdown) => break,
            None => publish_due_frame(&mut device, &mut observation, &mut demands, &latest_frame),
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
    let ids = demands.keys().copied().collect::<Vec<_>>();
    let mut disconnected = Vec::new();
    for id in ids {
        let Some(demand) = demands.get_mut(&id) else {
            continue;
        };
        demand.next_due = completed_at + demand.interval;
        match demand.sender.try_send(publication.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                disconnected.push(id);
            }
        }
    }
    for id in disconnected {
        demands.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classified(kind: UiStateKind) -> UiStateClassification {
        UiStateClassification::new(kind, format!("{kind:?}"))
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

    struct CapturingRoutine;

    impl sealed::UiRoutineSealed for CapturingRoutine {}

    impl UiRoutine for CapturingRoutine {
        type Output = u64;

        fn execute(self, context: &mut UiRoutineContext<'_>) -> Self::Output {
            context.capture_frame().unwrap().id()
        }
    }

    struct BlankDevice;

    impl UiDevice for BlankDevice {
        fn capture(&mut self) -> Result<DynamicImage> {
            Ok(DynamicImage::new_rgba8(16, 16))
        }
    }

    #[test]
    fn routine_captures_update_state_without_publishing_transient_chat_frames() {
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

        let routine_id = handle.submit(CapturingRoutine).unwrap().wait().unwrap();
        assert!(routine_id > initial_id);
        assert_eq!(handle.latest_ui_state().unwrap().frame_id(), routine_id);
        assert!(matches!(
            subscription.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Timeout)
        ));

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
        runtime.shutdown().unwrap();
    }
}
