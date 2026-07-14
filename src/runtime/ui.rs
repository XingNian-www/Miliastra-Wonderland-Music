use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputCertainty {
    BeforeInput,
    AfterInputUnknown,
    ConfirmedFailure,
    Cancelled,
}

#[derive(Debug)]
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

#[derive(Debug)]
pub struct CapturedFrame {
    image: Arc<DynamicImage>,
    captured_at: Instant,
}

impl CapturedFrame {
    pub fn image(&self) -> &DynamicImage {
        &self.image
    }

    pub fn image_arc(&self) -> Arc<DynamicImage> {
        Arc::clone(&self.image)
    }

    pub fn captured_at(&self) -> Instant {
        self.captured_at
    }

    pub fn into_image(self) -> DynamicImage {
        Arc::try_unwrap(self.image).unwrap_or_else(|image| (*image).clone())
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

pub(crate) mod sealed {
    pub trait UiRoutineSealed {}
}

pub trait UiRoutine: sealed::UiRoutineSealed + Send + 'static {
    type Output: Send + 'static;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output;
}

impl sealed::UiRoutineSealed for CaptureFrame {}

impl UiRoutine for CaptureFrame {
    type Output = Result<CapturedFrame, UiRoutineFailure>;

    fn execute(self, device: &mut dyn UiDevice) -> Self::Output {
        let image = device.capture().map_err(|error| {
            UiRoutineFailure::before_input("capture_frame", format!("{error:#}"))
        })?;
        Ok(CapturedFrame {
            image: Arc::new(image),
            captured_at: Instant::now(),
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
    fn execute(self: Box<Self>, device: &mut dyn UiDevice);
}

struct TypedUiJob<R: UiRoutine> {
    routine: R,
    response: SyncSender<R::Output>,
}

impl<R: UiRoutine> ErasedUiJob for TypedUiJob<R> {
    fn execute(self: Box<Self>, device: &mut dyn UiDevice) {
        let _ = self.response.send(self.routine.execute(device));
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
        let message = RuntimeMessage::Execute(Box::new(TypedUiJob { routine, response }));
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
        let worker = thread::Builder::new()
            .name("ui-runtime".to_string())
            .spawn(move || run_ui_runtime(device, receiver, worker_latest_frame))
            .map_err(UiRuntimeStartError::Spawn)?;

        Ok(Self {
            handle: UiRuntimeHandle {
                channel,
                next_operation_id: Arc::new(AtomicU64::new(0)),
                next_frame_demand_id: Arc::new(AtomicU64::new(0)),
                latest_frame,
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
) {
    let mut device = device;
    let mut demands = HashMap::<u64, ActiveFrameDemand>::new();
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
            Some(RuntimeMessage::Execute(job)) => job.execute(&mut device),
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
            None => publish_due_frame(&mut device, &mut demands, &latest_frame),
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

    let publication = match device.capture() {
        Ok(image) => FramePublication::Captured(Arc::new(CapturedFrame {
            image: Arc::new(image),
            captured_at: Instant::now(),
        })),
        Err(error) => {
            log::error!("UI runtime 按需截图失败: {error:#}");
            FramePublication::Failed(FrameCaptureFailure {
                failed_at: Instant::now(),
                reason: Arc::from(format!("{error:#}")),
            })
        }
    };
    if let FramePublication::Captured(frame) = &publication
        && let Ok(mut latest) = latest_frame.lock()
    {
        *latest = Some(Arc::clone(frame));
    }

    let completed_at = Instant::now();
    for id in due {
        let Some(demand) = demands.get_mut(&id) else {
            continue;
        };
        demand.next_due = completed_at + demand.interval;
        match demand.sender.try_send(publication.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {
                demands.remove(&id);
            }
        }
    }
}
