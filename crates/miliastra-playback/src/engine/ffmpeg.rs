use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use ffmpeg_next as ffmpeg;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use super::{AudioEngine, EngineCommand, EngineError};
use crate::domain::{
    EndBehavior, EndCause, EngineState, Failure, PlaybackSnapshot, SessionRef, StreamSource,
};

const COMMAND_QUEUE_CAPACITY: usize = 32;
const PCM_BUFFER_DEFAULT_DURATION: Duration = Duration::from_secs(3);
const PCM_WAIT_INTERVAL: Duration = Duration::from_millis(20);
const PROGRESS_POLL_INTERVAL: Duration = Duration::from_millis(200);
const MAX_CONSECUTIVE_INVALID_PACKETS: usize = 16;

static FFMPEG_INITIALIZED: OnceLock<Result<(), ()>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct FfmpegConfig {
    pub command_timeout: Duration,
    pub io_timeout: Duration,
    pub pcm_buffer_duration: Duration,
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self {
            command_timeout: Duration::from_secs(3),
            io_timeout: Duration::from_secs(15),
            pcm_buffer_duration: PCM_BUFFER_DEFAULT_DURATION,
        }
    }
}

#[derive(Clone)]
pub struct FfmpegEngine {
    command_tx: mpsc::Sender<CommandEnvelope>,
    snapshots: watch::Receiver<PlaybackSnapshot>,
    command_timeout: Duration,
    runtime: Arc<RuntimeControl>,
}

struct RuntimeControl {
    shutdown_tx: watch::Sender<bool>,
    shutdown_result_tx: watch::Sender<Option<Result<(), EngineError>>>,
    _supervisor: JoinHandle<()>,
}

impl FfmpegEngine {
    /// Starts the process-local FFmpeg decoder and CPAL output stream.
    pub async fn spawn(config: FfmpegConfig) -> Result<Self, EngineError> {
        if config.command_timeout.is_zero()
            || config.io_timeout.is_zero()
            || config.pcm_buffer_duration.is_zero()
        {
            return Err(EngineError::Unavailable(
                "FFmpeg engine timeouts and PCM buffer duration must be positive".to_owned(),
            ));
        }
        initialize_ffmpeg()?;

        let (worker_events_tx, worker_events_rx) = mpsc::unbounded_channel();
        let (stream, output) = open_output_stream(
            config.pcm_buffer_duration,
            config.io_timeout,
            worker_events_tx.clone(),
        )?;
        stream.play().map_err(|error| {
            EngineError::Unavailable(format!("start default audio output device: {error}"))
        })?;

        let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let (snapshot_tx, snapshots) = watch::channel(PlaybackSnapshot::default());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (shutdown_result_tx, _) = watch::channel(None);
        let actor = tokio::spawn(run_actor(
            command_rx,
            worker_events_rx,
            worker_events_tx,
            shutdown_rx,
            snapshot_tx,
            stream,
            output,
        ));
        let result_publisher = shutdown_result_tx.clone();
        let supervisor = tokio::spawn(async move {
            let result = actor
                .await
                .map_err(|error| EngineError::Unavailable(format!("FFmpeg actor failed: {error}")));
            result_publisher.send_replace(Some(result));
        });

        Ok(Self {
            command_tx,
            snapshots,
            command_timeout: config.command_timeout.saturating_mul(4),
            runtime: Arc::new(RuntimeControl {
                shutdown_tx,
                shutdown_result_tx,
                _supervisor: supervisor,
            }),
        })
    }

    pub async fn shutdown(&self) -> Result<(), EngineError> {
        let mut shutdown_result = self.runtime.shutdown_result_tx.subscribe();
        self.runtime.shutdown_tx.send_replace(true);
        loop {
            if let Some(result) = shutdown_result.borrow_and_update().clone() {
                return result;
            }
            shutdown_result.changed().await.map_err(|_| {
                EngineError::Unavailable("FFmpeg shutdown result sender dropped".to_owned())
            })?;
        }
    }
}

#[async_trait]
impl AudioEngine for FfmpegEngine {
    async fn command(&self, command: EngineCommand) -> Result<(), EngineError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(CommandEnvelope {
                command,
                response_tx,
            })
            .await
            .map_err(|_| EngineError::Unavailable("FFmpeg engine task has stopped".to_owned()))?;

        tokio::time::timeout(self.command_timeout, response_rx)
            .await
            .map_err(|_| EngineError::TimedOut)?
            .map_err(|_| EngineError::Unavailable("FFmpeg engine task has stopped".to_owned()))?
    }

    fn subscribe(&self) -> watch::Receiver<PlaybackSnapshot> {
        self.snapshots.clone()
    }
}

struct CommandEnvelope {
    command: EngineCommand,
    response_tx: oneshot::Sender<Result<(), EngineError>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AttemptIdentity {
    id: u64,
    session: SessionRef,
}

#[derive(Clone)]
struct ActivePlayback {
    session: SessionRef,
    stream: StreamSource,
    end_behavior: EndBehavior,
    paused: bool,
}

#[derive(Clone)]
struct PendingLaunch {
    session: SessionRef,
    stream: StreamSource,
    end_behavior: EndBehavior,
    seek_seconds: Option<f64>,
}

struct ActiveWorker {
    identity: AttemptIdentity,
    cancel: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

struct ActorState {
    snapshot: PlaybackSnapshot,
    active: Option<ActivePlayback>,
    active_worker: Option<ActiveWorker>,
    retiring_workers: Vec<ActiveWorker>,
    pending_launch: Option<PendingLaunch>,
    playback_committed: bool,
    position_base_seconds: Option<f64>,
    next_attempt_id: u64,
}

impl ActorState {
    fn new() -> Self {
        Self {
            snapshot: PlaybackSnapshot::default(),
            active: None,
            active_worker: None,
            retiring_workers: Vec::new(),
            pending_launch: None,
            playback_committed: false,
            position_base_seconds: None,
            next_attempt_id: 1,
        }
    }

    fn publish(&self, snapshots: &watch::Sender<PlaybackSnapshot>) {
        snapshots.send_replace(self.snapshot.clone());
    }

    fn current_session_matches(&self, session: SessionRef) -> bool {
        self.snapshot.session_id == Some(session.session_id)
            && self.snapshot.generation == session.generation
    }

    fn active_attempt_matches(&self, identity: AttemptIdentity) -> bool {
        self.active_worker
            .as_ref()
            .is_some_and(|worker| worker.identity == identity)
            && self
                .active
                .as_ref()
                .is_some_and(|active| active.session == identity.session)
    }
}

#[derive(Clone, Copy)]
struct OutputFormat {
    sample_rate: u32,
    channels: u16,
}

impl OutputFormat {
    fn samples_per_second(self) -> f64 {
        f64::from(self.sample_rate) * f64::from(self.channels)
    }
}

struct PcmBuffer {
    samples: Mutex<VecDeque<f32>>,
    space_available: Condvar,
    capacity_samples: usize,
    active_attempt: AtomicU64,
    paused: AtomicBool,
    rendered_samples: AtomicU64,
}

impl PcmBuffer {
    fn new(capacity_samples: usize) -> Self {
        Self {
            samples: Mutex::new(VecDeque::with_capacity(capacity_samples)),
            space_available: Condvar::new(),
            capacity_samples,
            active_attempt: AtomicU64::new(0),
            paused: AtomicBool::new(false),
            rendered_samples: AtomicU64::new(0),
        }
    }

    fn activate(&self, attempt_id: u64, paused: bool) {
        self.active_attempt.store(attempt_id, Ordering::Release);
        self.paused.store(paused, Ordering::Release);
        self.rendered_samples.store(0, Ordering::Release);
        self.clear();
    }

    fn retire(&self, attempt_id: u64) {
        if self.active_attempt.load(Ordering::Acquire) == attempt_id {
            self.active_attempt.store(0, Ordering::Release);
            self.paused.store(false, Ordering::Release);
            self.clear();
        }
    }

    fn clear(&self) {
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        samples.clear();
        drop(samples);
        self.space_available.notify_all();
    }

    fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
        self.space_available.notify_all();
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    fn is_current(&self, attempt_id: u64, cancelled: &AtomicBool) -> bool {
        !cancelled.load(Ordering::Acquire)
            && self.active_attempt.load(Ordering::Acquire) == attempt_id
    }

    fn push(&self, attempt_id: u64, pcm: &[f32], cancelled: &AtomicBool) -> bool {
        let mut offset = 0;
        while offset < pcm.len() {
            if !self.is_current(attempt_id, cancelled) {
                return false;
            }
            let mut samples = self
                .samples
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !self.is_current(attempt_id, cancelled) {
                return false;
            }
            let available = self.capacity_samples.saturating_sub(samples.len());
            if available == 0 {
                let (guard, _) = self
                    .space_available
                    .wait_timeout(samples, PCM_WAIT_INTERVAL)
                    .unwrap_or_else(|error| error.into_inner());
                drop(guard);
                continue;
            }
            let end = (offset + available).min(pcm.len());
            samples.extend(pcm[offset..end].iter().copied());
            offset = end;
        }
        true
    }

    fn wait_until_empty(&self, attempt_id: u64, cancelled: &AtomicBool) -> bool {
        loop {
            if !self.is_current(attempt_id, cancelled) {
                return false;
            }
            let samples = self
                .samples
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if samples.is_empty() {
                return true;
            }
            let (guard, _) = self
                .space_available
                .wait_timeout(samples, PCM_WAIT_INTERVAL)
                .unwrap_or_else(|error| error.into_inner());
            drop(guard);
        }
    }

    fn reset_rendered_samples(&self) {
        self.rendered_samples.store(0, Ordering::Release);
    }

    fn rendered_samples(&self) -> u64 {
        self.rendered_samples.load(Ordering::Acquire)
    }

    fn fill<T>(&self, output: &mut [T], volume: u8)
    where
        T: SizedSample + FromSample<f32>,
    {
        let silence = T::from_sample(0.0_f32);
        if self.is_paused() {
            output.fill(silence);
            return;
        }

        let Ok(mut samples) = self.samples.try_lock() else {
            output.fill(silence);
            return;
        };
        let gain = f32::from(volume) / 100.0;
        let mut consumed = 0_u64;
        for value in output {
            match samples.pop_front() {
                Some(sample) => {
                    *value = T::from_sample(sample * gain);
                    consumed += 1;
                }
                None => *value = silence,
            }
        }
        self.rendered_samples.fetch_add(consumed, Ordering::Relaxed);
        drop(samples);
        if consumed > 0 {
            self.space_available.notify_one();
        }
    }
}

enum WorkerEvent {
    Opened {
        identity: AttemptIdentity,
        duration_seconds: Option<f64>,
    },
    Started {
        identity: AttemptIdentity,
        position_seconds: Option<f64>,
    },
    Looping {
        identity: AttemptIdentity,
    },
    Failed {
        identity: AttemptIdentity,
        phase: WorkerFailurePhase,
    },
    EofDrained {
        identity: AttemptIdentity,
    },
    Retired {
        identity: AttemptIdentity,
    },
    OutputFailed,
}

#[derive(Clone, Copy, Debug)]
enum WorkerFailurePhase {
    Opening,
    Decoding(DecodeFailureStage),
}

#[derive(Clone, Copy, Debug)]
enum DecodeFailureStage {
    Seek,
    PacketRead,
    SubmitPacket,
    DecoderDrain,
    Resample,
    ResamplerDrain,
    EofSignal,
    NoAudioFrames,
}

impl DecodeFailureStage {
    fn label(self) -> &'static str {
        match self {
            Self::Seek => "seek",
            Self::PacketRead => "packet_read",
            Self::SubmitPacket => "submit_packet",
            Self::DecoderDrain => "decoder_drain",
            Self::Resample => "resample",
            Self::ResamplerDrain => "resampler_drain",
            Self::EofSignal => "eof_signal",
            Self::NoAudioFrames => "no_audio_frames",
        }
    }
}

async fn run_actor(
    mut commands: mpsc::Receiver<CommandEnvelope>,
    mut worker_events: mpsc::UnboundedReceiver<WorkerEvent>,
    worker_events_tx: mpsc::UnboundedSender<WorkerEvent>,
    mut shutdown: watch::Receiver<bool>,
    snapshots: watch::Sender<PlaybackSnapshot>,
    _stream: Stream,
    output: OutputRuntime,
) {
    let mut state = ActorState::new();
    let mut progress = tokio::time::interval(PROGRESS_POLL_INTERVAL);
    progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            shutdown_result = shutdown.changed() => {
                if shutdown_result.is_err() || *shutdown.borrow_and_update() {
                    shutdown_workers(&mut state, &output.audio).await;
                    break;
                }
            }
            Some(event) = worker_events.recv() => {
                handle_worker_event(
                    event,
                    &mut state,
                    &snapshots,
                    &output,
                    &worker_events_tx,
                ).await;
            }
            Some(envelope) = commands.recv() => {
                let result = handle_command(
                    envelope.command,
                    &mut state,
                    &snapshots,
                    &output,
                    &worker_events_tx,
                );
                let _ = envelope.response_tx.send(result);
            }
            _ = progress.tick() => {
                refresh_progress(&mut state, &snapshots, &output);
            }
            else => {
                shutdown_workers(&mut state, &output.audio).await;
                break;
            }
        }
    }
}

struct OutputRuntime {
    audio: Arc<PcmBuffer>,
    volume: Arc<AtomicU8>,
    format: OutputFormat,
    io_timeout: Duration,
}

fn open_output_stream(
    pcm_buffer_duration: Duration,
    io_timeout: Duration,
    worker_events_tx: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<(Stream, OutputRuntime), EngineError> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| {
        EngineError::Unavailable("no default audio output device is available".to_owned())
    })?;
    let supported = device.default_output_config().map_err(|error| {
        EngineError::Unavailable(format!("query default audio output device: {error}"))
    })?;
    let sample_format = supported.sample_format();
    let stream_config: StreamConfig = supported.into();
    if stream_config.channels == 0 || stream_config.sample_rate == 0 {
        return Err(EngineError::Unavailable(
            "default audio output device returned an invalid format".to_owned(),
        ));
    }
    let format = OutputFormat {
        sample_rate: stream_config.sample_rate,
        channels: stream_config.channels,
    };
    let capacity_samples = (format.samples_per_second() * pcm_buffer_duration.as_secs_f64())
        .ceil()
        .max(f64::from(format.channels)) as usize;
    let audio = Arc::new(PcmBuffer::new(capacity_samples));
    let volume = Arc::new(AtomicU8::new(100));
    let stream = build_output_stream(
        &device,
        stream_config,
        sample_format,
        audio.clone(),
        volume.clone(),
        worker_events_tx,
    )?;
    Ok((
        stream,
        OutputRuntime {
            audio,
            volume,
            format,
            io_timeout,
        },
    ))
}

fn build_output_stream(
    device: &cpal::Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    audio: Arc<PcmBuffer>,
    volume: Arc<AtomicU8>,
    worker_events_tx: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<Stream, EngineError> {
    macro_rules! build {
        ($sample:ty) => {
            build_typed_output_stream::<$sample>(device, config, audio, volume, worker_events_tx)
        };
    }

    match sample_format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I24 => build!(cpal::I24),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U24 => build!(cpal::U24),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        unsupported => Err(EngineError::Unavailable(format!(
            "default audio output sample format {unsupported:?} is unsupported"
        ))),
    }
}

fn build_typed_output_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    audio: Arc<PcmBuffer>,
    volume: Arc<AtomicU8>,
    worker_events_tx: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<Stream, EngineError>
where
    T: SizedSample + FromSample<f32>,
{
    let callback_audio = audio;
    let callback_volume = volume;
    let stream_events = worker_events_tx;
    device
        .build_output_stream(
            config,
            move |output: &mut [T], _| {
                callback_audio.fill(output, callback_volume.load(Ordering::Relaxed));
            },
            move |_| {
                let _ = stream_events.send(WorkerEvent::OutputFailed);
            },
            None,
        )
        .map_err(|error| EngineError::Unavailable(format!("create audio output stream: {error}")))
}

fn handle_command(
    command: EngineCommand,
    state: &mut ActorState,
    snapshots: &watch::Sender<PlaybackSnapshot>,
    output: &OutputRuntime,
    worker_events_tx: &mpsc::UnboundedSender<WorkerEvent>,
) -> Result<(), EngineError> {
    match command {
        EngineCommand::Start {
            session_id,
            generation,
            song_key,
            stream,
            end_behavior,
            seek_seconds,
        } => {
            let session = SessionRef {
                session_id,
                generation,
            };
            if generation < state.snapshot.generation {
                return Err(EngineError::Rejected(format!(
                    "stale playback generation {generation}; current generation is {}",
                    state.snapshot.generation
                )));
            }
            if generation == state.snapshot.generation
                && state.snapshot.session_id == Some(session_id)
            {
                return Ok(());
            }
            if generation == state.snapshot.generation && state.snapshot.session_id.is_some() {
                return Err(EngineError::Rejected(format!(
                    "playback generation {generation} is already in use"
                )));
            }

            retire_active_worker(state, &output.audio);
            state.active = Some(ActivePlayback {
                session,
                stream: stream.clone(),
                end_behavior,
                paused: false,
            });
            state.pending_launch = Some(PendingLaunch {
                session,
                stream,
                end_behavior,
                seek_seconds,
            });
            state.playback_committed = false;
            state.position_base_seconds = None;
            state.snapshot.generation = generation;
            state.snapshot.session_id = Some(session_id);
            state.snapshot.song_key = Some(song_key);
            state.snapshot.end_behavior = Some(end_behavior);
            state.snapshot.state = EngineState::Loading;
            state.snapshot.position_seconds = None;
            state.snapshot.duration_seconds = None;
            state.snapshot.last_end_cause = None;
            state.snapshot.error = None;
            state.snapshot.failure = None;
            state.publish(snapshots);
            maybe_start_pending(state, output, worker_events_tx);
            Ok(())
        }
        EngineCommand::RefreshStream {
            session_id,
            generation,
            stream,
        } => {
            let session = SessionRef {
                session_id,
                generation,
            };
            ensure_current_session(state, session, "stream refresh")?;
            if state.snapshot.state != EngineState::Failed {
                return Err(EngineError::Rejected(
                    "stream refresh requires a failed playback session".to_owned(),
                ));
            }
            let position = trustworthy_position(state.snapshot.position_seconds);
            if state.playback_committed && position.is_none() {
                recovery_position_unknown(state, snapshots);
                return Err(EngineError::Rejected(
                    "same-track recovery position is unknown".to_owned(),
                ));
            }
            let active = state.active.as_mut().ok_or_else(|| {
                EngineError::Rejected("failed playback session is no longer active".to_owned())
            })?;
            active.stream = stream.clone();
            state.pending_launch = Some(PendingLaunch {
                session,
                stream,
                end_behavior: active.end_behavior,
                seek_seconds: position,
            });
            state.position_base_seconds = None;
            state.snapshot.state = EngineState::Loading;
            state.snapshot.position_seconds = None;
            state.snapshot.duration_seconds = None;
            state.snapshot.last_end_cause = None;
            state.snapshot.error = None;
            state.snapshot.failure = None;
            state.publish(snapshots);
            retire_active_worker(state, &output.audio);
            maybe_start_pending(state, output, worker_events_tx);
            Ok(())
        }
        EngineCommand::Pause { session } => {
            ensure_current_session(state, session, "pause")?;
            let Some(active) = state.active.as_mut() else {
                return Err(EngineError::Rejected(
                    "pause requires a loading or playing session".to_owned(),
                ));
            };
            match state.snapshot.state {
                EngineState::Paused => return Ok(()),
                EngineState::Loading | EngineState::Playing => {}
                _ => {
                    return Err(EngineError::Rejected(
                        "pause requires a loading or playing session".to_owned(),
                    ));
                }
            }
            active.paused = true;
            output.audio.set_paused(true);
            if state.snapshot.state == EngineState::Playing {
                state.snapshot.state = EngineState::Paused;
                state.publish(snapshots);
            }
            Ok(())
        }
        EngineCommand::Resume { session } => {
            ensure_current_session(state, session, "resume")?;
            let Some(active) = state.active.as_mut() else {
                return Err(EngineError::Rejected(
                    "resume requires a loading or paused session".to_owned(),
                ));
            };
            match state.snapshot.state {
                EngineState::Playing => return Ok(()),
                EngineState::Loading | EngineState::Paused => {}
                _ => {
                    return Err(EngineError::Rejected(
                        "resume requires a loading or paused session".to_owned(),
                    ));
                }
            }
            active.paused = false;
            output.audio.set_paused(false);
            if state.snapshot.state == EngineState::Paused {
                state.snapshot.state = EngineState::Playing;
                state.publish(snapshots);
            }
            Ok(())
        }
        EngineCommand::Stop { session } => {
            ensure_current_session(state, session, "stop")?;
            if state.active.is_none()
                && matches!(
                    state.snapshot.state,
                    EngineState::Idle | EngineState::Stopped
                )
            {
                return Ok(());
            }
            state.pending_launch = None;
            retire_active_worker(state, &output.audio);
            state.active = None;
            state.playback_committed = false;
            state.position_base_seconds = None;
            state.snapshot.state = EngineState::Stopped;
            state.snapshot.last_end_cause = Some(EndCause::StoppedByController);
            state.snapshot.error = None;
            state.snapshot.failure = None;
            state.publish(snapshots);
            Ok(())
        }
        EngineCommand::Seek {
            session,
            position_seconds,
        } => {
            ensure_current_session(state, session, "seek")?;
            if !position_seconds.is_finite() || position_seconds < 0.0 {
                return Err(EngineError::Rejected(
                    "seek position must be a finite non-negative number".to_owned(),
                ));
            }
            let Some(active) = state.active.as_ref() else {
                return Err(EngineError::Rejected(
                    "seek requires a playing or paused session".to_owned(),
                ));
            };
            if !matches!(
                state.snapshot.state,
                EngineState::Playing | EngineState::Paused
            ) {
                return Err(EngineError::Rejected(
                    "seek requires a playing or paused session".to_owned(),
                ));
            }
            state.pending_launch = Some(PendingLaunch {
                session,
                stream: active.stream.clone(),
                end_behavior: active.end_behavior,
                seek_seconds: Some(position_seconds),
            });
            state.position_base_seconds = None;
            state.snapshot.state = EngineState::Loading;
            state.snapshot.position_seconds = None;
            state.snapshot.duration_seconds = None;
            state.snapshot.last_end_cause = None;
            state.snapshot.error = None;
            state.snapshot.failure = None;
            state.publish(snapshots);
            retire_active_worker(state, &output.audio);
            maybe_start_pending(state, output, worker_events_tx);
            Ok(())
        }
        EngineCommand::SetVolume { volume } => {
            if volume > 100 {
                return Err(EngineError::Rejected(
                    "volume must be between 0 and 100".to_owned(),
                ));
            }
            output.volume.store(volume, Ordering::Release);
            state.snapshot.volume = volume;
            state.publish(snapshots);
            Ok(())
        }
    }
}

fn ensure_current_session(
    state: &ActorState,
    session: SessionRef,
    operation: &str,
) -> Result<(), EngineError> {
    if state.current_session_matches(session) {
        return Ok(());
    }
    Err(EngineError::Rejected(format!(
        "{operation} targets stale playback {}/{}; current playback is {}/{}",
        session.session_id,
        session.generation,
        state
            .snapshot
            .session_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_owned()),
        state.snapshot.generation,
    )))
}

fn retire_active_worker(state: &mut ActorState, audio: &PcmBuffer) {
    if let Some(worker) = state.active_worker.take() {
        worker.cancel.store(true, Ordering::Release);
        audio.retire(worker.identity.id);
        state.retiring_workers.push(worker);
    }
}

fn maybe_start_pending(
    state: &mut ActorState,
    output: &OutputRuntime,
    worker_events_tx: &mpsc::UnboundedSender<WorkerEvent>,
) {
    if state.active_worker.is_some() || !state.retiring_workers.is_empty() {
        return;
    }
    let Some(launch) = state.pending_launch.take() else {
        return;
    };
    let Some(active) = state.active.as_ref() else {
        return;
    };
    if active.session != launch.session {
        return;
    }

    let identity = AttemptIdentity {
        id: state.next_attempt_id,
        session: launch.session,
    };
    state.next_attempt_id = state.next_attempt_id.wrapping_add(1).max(1);
    let cancel = Arc::new(AtomicBool::new(false));
    output.audio.activate(identity.id, active.paused);
    let request = WorkerRequest {
        identity,
        stream: launch.stream,
        end_behavior: launch.end_behavior,
        seek_seconds: launch.seek_seconds,
        output: output.format,
        io_timeout: output.io_timeout,
    };
    let task_audio = output.audio.clone();
    let task_cancel = cancel.clone();
    let task_events = worker_events_tx.clone();
    let task = tokio::task::spawn_blocking(move || {
        run_decode_worker(request, task_audio, task_cancel, task_events);
    });
    state.active_worker = Some(ActiveWorker {
        identity,
        cancel,
        task,
    });
}

async fn handle_worker_event(
    event: WorkerEvent,
    state: &mut ActorState,
    snapshots: &watch::Sender<PlaybackSnapshot>,
    output: &OutputRuntime,
    worker_events_tx: &mpsc::UnboundedSender<WorkerEvent>,
) {
    match event {
        WorkerEvent::Opened {
            identity,
            duration_seconds,
        } if state.active_attempt_matches(identity) => {
            state.snapshot.duration_seconds = duration_seconds;
            state.publish(snapshots);
        }
        WorkerEvent::Started {
            identity,
            position_seconds,
        } if state.active_attempt_matches(identity) => {
            state.playback_committed = true;
            state.position_base_seconds = trustworthy_position(position_seconds);
            state.snapshot.position_seconds = state.position_base_seconds;
            state.snapshot.state = if state.active.as_ref().is_some_and(|active| active.paused) {
                EngineState::Paused
            } else {
                EngineState::Playing
            };
            state.snapshot.error = None;
            state.snapshot.failure = None;
            state.publish(snapshots);
        }
        WorkerEvent::Looping { identity } if state.active_attempt_matches(identity) => {
            // The next repeat attempt starts from the beginning. Retain that
            // trustworthy position so an expired URL can be refreshed safely.
            state.position_base_seconds = Some(0.0);
            state.snapshot.position_seconds = Some(0.0);
            state.snapshot.state = EngineState::Loading;
            state.snapshot.last_end_cause = None;
            state.snapshot.error = None;
            state.snapshot.failure = None;
            state.publish(snapshots);
        }
        WorkerEvent::Failed { identity, phase } if state.active_attempt_matches(identity) => {
            output.audio.retire(identity.id);
            let (cause, code, message) = match phase {
                WorkerFailurePhase::Opening => (
                    EndCause::StreamRejected,
                    "stream_rejected",
                    "FFmpeg could not open the stream".to_owned(),
                ),
                WorkerFailurePhase::Decoding(stage) => (
                    EndCause::DecodeFailure,
                    "decode_failure",
                    format!("FFmpeg could not decode the stream ({})", stage.label()),
                ),
            };
            state.snapshot.state = EngineState::Failed;
            state.snapshot.last_end_cause = Some(cause);
            state.snapshot.error = Some(message.clone());
            state.snapshot.failure = Some(Failure::new(code, message));
            state.publish(snapshots);
        }
        WorkerEvent::EofDrained { identity } if state.active_attempt_matches(identity) => {
            refresh_progress(state, snapshots, output);
            let end_cause = classify_eof(&state.snapshot);
            state.snapshot.state = if end_cause == EndCause::NaturalEnd {
                EngineState::Stopped
            } else {
                EngineState::Failed
            };
            state.snapshot.last_end_cause = Some(end_cause);
            state.snapshot.error = (end_cause == EndCause::DecodeFailure)
                .then_some("stream ended without trustworthy completion evidence".to_owned());
            state.snapshot.failure =
                (end_cause == EndCause::DecodeFailure).then_some(Failure::new(
                    "decode_failure",
                    "stream ended without trustworthy completion evidence",
                ));
            if end_cause == EndCause::NaturalEnd {
                state.active = None;
                state.playback_committed = false;
                state.position_base_seconds = None;
            }
            state.publish(snapshots);
        }
        WorkerEvent::Retired { identity } => {
            retire_finished_worker(identity, state).await;
            maybe_start_pending(state, output, worker_events_tx);
        }
        WorkerEvent::OutputFailed => {
            state.pending_launch = None;
            retire_active_worker(state, &output.audio);
            state.active = None;
            state.playback_committed = false;
            state.position_base_seconds = None;
            state.snapshot.state = EngineState::Failed;
            state.snapshot.last_end_cause = Some(EndCause::EngineExited);
            state.snapshot.error = Some("default audio output device failed".to_owned());
            state.snapshot.failure = Some(Failure::new(
                "engine_unavailable",
                "default audio output device failed",
            ));
            state.publish(snapshots);
        }
        _ => {}
    }
}

async fn retire_finished_worker(identity: AttemptIdentity, state: &mut ActorState) {
    if state
        .active_worker
        .as_ref()
        .is_some_and(|worker| worker.identity == identity)
    {
        if let Some(worker) = state.active_worker.take() {
            let _ = worker.task.await;
        }
        return;
    }
    if let Some(index) = state
        .retiring_workers
        .iter()
        .position(|worker| worker.identity == identity)
    {
        let worker = state.retiring_workers.swap_remove(index);
        let _ = worker.task.await;
    }
}

fn refresh_progress(
    state: &mut ActorState,
    snapshots: &watch::Sender<PlaybackSnapshot>,
    output: &OutputRuntime,
) {
    if !matches!(
        state.snapshot.state,
        EngineState::Playing | EngineState::Paused
    ) || state.active_worker.is_none()
    {
        return;
    }
    let Some(base) = state.position_base_seconds else {
        return;
    };
    let elapsed = output.audio.rendered_samples() as f64 / output.format.samples_per_second();
    let mut position = base + elapsed;
    if let Some(duration) = state.snapshot.duration_seconds {
        position = position.min(duration);
    }
    let position = trustworthy_position(Some(position));
    if state.snapshot.position_seconds != position {
        state.snapshot.position_seconds = position;
        state.publish(snapshots);
    }
}

async fn shutdown_workers(state: &mut ActorState, audio: &PcmBuffer) {
    state.pending_launch = None;
    if let Some(worker) = state.active_worker.take() {
        worker.cancel.store(true, Ordering::Release);
        audio.retire(worker.identity.id);
        state.retiring_workers.push(worker);
    }
    for worker in state.retiring_workers.drain(..) {
        worker.cancel.store(true, Ordering::Release);
        let _ = worker.task.await;
    }
    audio.clear();
}

fn recovery_position_unknown(state: &mut ActorState, snapshots: &watch::Sender<PlaybackSnapshot>) {
    state.snapshot.state = EngineState::Failed;
    state.snapshot.last_end_cause = Some(EndCause::RecoveryPositionUnknown);
    state.snapshot.error = Some(
        "same-track recovery cannot resume without a trustworthy playback position".to_owned(),
    );
    state.snapshot.failure = Some(Failure::new(
        "recovery_position_unknown",
        "same-track recovery requires a trustworthy playback position",
    ));
    state.active = None;
    state.pending_launch = None;
    state.playback_committed = false;
    state.position_base_seconds = None;
    state.publish(snapshots);
}

fn classify_eof(snapshot: &PlaybackSnapshot) -> EndCause {
    match (snapshot.position_seconds, snapshot.duration_seconds) {
        (Some(position), Some(duration))
            if position.is_finite()
                && position >= 0.0
                && duration.is_finite()
                && duration > 0.0 =>
        {
            let remaining = duration - position;
            if remaining <= 5.0 || position / duration >= 0.98 {
                EndCause::NaturalEnd
            } else {
                EndCause::DecodeFailure
            }
        }
        _ => EndCause::DecodeFailure,
    }
}

fn trustworthy_position(value: Option<f64>) -> Option<f64> {
    value.filter(|position| position.is_finite() && *position >= 0.0)
}

struct WorkerRequest {
    identity: AttemptIdentity,
    stream: StreamSource,
    end_behavior: EndBehavior,
    seek_seconds: Option<f64>,
    output: OutputFormat,
    io_timeout: Duration,
}

fn run_decode_worker(
    request: WorkerRequest,
    audio: Arc<PcmBuffer>,
    cancelled: Arc<AtomicBool>,
    events: mpsc::UnboundedSender<WorkerEvent>,
) {
    let outcome = if initialize_ffmpeg().is_err() {
        WorkerOutcome::Failed(WorkerFailurePhase::Opening)
    } else {
        decode_worker_loop(&request, &audio, &cancelled, &events)
    };
    match outcome {
        WorkerOutcome::Failed(phase) => {
            let _ = events.send(WorkerEvent::Failed {
                identity: request.identity,
                phase,
            });
        }
        WorkerOutcome::EofDrained => {
            let _ = events.send(WorkerEvent::EofDrained {
                identity: request.identity,
            });
        }
        WorkerOutcome::Cancelled => {}
    }
    let _ = events.send(WorkerEvent::Retired {
        identity: request.identity,
    });
}

enum WorkerOutcome {
    Cancelled,
    Failed(WorkerFailurePhase),
    EofDrained,
}

fn decode_worker_loop(
    request: &WorkerRequest,
    audio: &PcmBuffer,
    cancelled: &Arc<AtomicBool>,
    events: &mpsc::UnboundedSender<WorkerEvent>,
) -> WorkerOutcome {
    let mut seek_seconds = request.seek_seconds;
    loop {
        let outcome = decode_once(request, seek_seconds, audio, cancelled, events);
        match outcome {
            WorkerOutcome::EofDrained if request.end_behavior == EndBehavior::RepeatCurrent => {
                if !audio.is_current(request.identity.id, cancelled) {
                    return WorkerOutcome::Cancelled;
                }
                audio.reset_rendered_samples();
                let _ = events.send(WorkerEvent::Looping {
                    identity: request.identity,
                });
                seek_seconds = None;
            }
            outcome => return outcome,
        }
    }
}

fn decode_once(
    request: &WorkerRequest,
    seek_seconds: Option<f64>,
    audio: &PcmBuffer,
    cancelled: &Arc<AtomicBool>,
    events: &mpsc::UnboundedSender<WorkerEvent>,
) -> WorkerOutcome {
    if !audio.is_current(request.identity.id, cancelled) {
        return WorkerOutcome::Cancelled;
    }
    let options = match ffmpeg_input_options(&request.stream.headers, request.io_timeout) {
        Ok(options) => options,
        Err(()) => return WorkerOutcome::Failed(WorkerFailurePhase::Opening),
    };
    let interrupt = cancelled.clone();
    let mut input = match ffmpeg::format::input_with_interrupt_and_dictionary(
        request.stream.url.as_str(),
        move || interrupt.load(Ordering::Acquire),
        options,
    ) {
        Ok(input) => input,
        Err(_) if cancelled.load(Ordering::Acquire) => return WorkerOutcome::Cancelled,
        Err(_) => return WorkerOutcome::Failed(WorkerFailurePhase::Opening),
    };
    if !audio.is_current(request.identity.id, cancelled) {
        return WorkerOutcome::Cancelled;
    }

    let input_duration = duration_from_input(&input);
    let (stream_index, stream_time_base, stream_duration, parameters) =
        match input.streams().best(ffmpeg::media::Type::Audio) {
            Some(stream) => (
                stream.index(),
                stream.time_base(),
                stream.duration(),
                stream.parameters(),
            ),
            None => return WorkerOutcome::Failed(WorkerFailurePhase::Opening),
        };
    let duration_seconds =
        input_duration.or_else(|| duration_from_stream(stream_duration, stream_time_base));
    let context = match ffmpeg::codec::context::Context::from_parameters(parameters) {
        Ok(context) => context,
        Err(_) => return WorkerOutcome::Failed(WorkerFailurePhase::Opening),
    };
    let mut decoder = match context.decoder().audio() {
        Ok(decoder) => decoder,
        Err(_) => return WorkerOutcome::Failed(WorkerFailurePhase::Opening),
    };
    if decoder.rate() == 0 || request.output.sample_rate == 0 || request.output.channels == 0 {
        return WorkerOutcome::Failed(WorkerFailurePhase::Opening);
    }
    let destination_layout = ffmpeg::ChannelLayout::default(i32::from(request.output.channels));
    let destination_format = ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed);
    let mut resampler = match ffmpeg::software::resampling::Context::get(
        decoder.format(),
        decoder.channel_layout(),
        decoder.rate(),
        destination_format,
        destination_layout,
        request.output.sample_rate,
    ) {
        Ok(resampler) => resampler,
        Err(_) => return WorkerOutcome::Failed(WorkerFailurePhase::Opening),
    };
    let _ = events.send(WorkerEvent::Opened {
        identity: request.identity,
        duration_seconds,
    });

    if let Some(seconds) = seek_seconds.filter(|seconds| *seconds > 0.0) {
        let timestamp = (seconds * f64::from(ffmpeg::ffi::AV_TIME_BASE)).round() as i64;
        if input.seek(timestamp, ..).is_err() {
            return if cancelled.load(Ordering::Acquire) {
                WorkerOutcome::Cancelled
            } else {
                WorkerOutcome::Failed(WorkerFailurePhase::Decoding(DecodeFailureStage::Seek))
            };
        }
        decoder.flush();
    }

    let mut packet = ffmpeg::Packet::empty();
    let mut started = false;
    let mut consecutive_invalid_packets = 0_usize;
    loop {
        if !audio.is_current(request.identity.id, cancelled) {
            return WorkerOutcome::Cancelled;
        }
        match packet.read(&mut input) {
            Ok(()) => {
                if packet.stream() != stream_index {
                    continue;
                }
                match submit_packet(&mut decoder, &packet, &mut consecutive_invalid_packets) {
                    PacketSubmit::Accepted => {}
                    PacketSubmit::SkipInvalid => continue,
                    PacketSubmit::Failed => {
                        return decode_error_outcome(cancelled, DecodeFailureStage::SubmitPacket);
                    }
                }
                match drain_decoder_frames(
                    &mut decoder,
                    &mut resampler,
                    stream_time_base,
                    request.identity,
                    audio,
                    cancelled,
                    events,
                    &mut started,
                    false,
                ) {
                    FrameDrain::Continue => {}
                    FrameDrain::Cancelled => return WorkerOutcome::Cancelled,
                    FrameDrain::Failed(stage) => {
                        return WorkerOutcome::Failed(WorkerFailurePhase::Decoding(stage));
                    }
                }
            }
            Err(ffmpeg::Error::Eof) => break,
            Err(_) if cancelled.load(Ordering::Acquire) => return WorkerOutcome::Cancelled,
            Err(_) => {
                return WorkerOutcome::Failed(WorkerFailurePhase::Decoding(
                    DecodeFailureStage::PacketRead,
                ));
            }
        }
    }

    match decoder.send_eof() {
        Ok(()) | Err(ffmpeg::Error::Eof) => {}
        Err(_) => return decode_error_outcome(cancelled, DecodeFailureStage::EofSignal),
    }
    match drain_decoder_frames(
        &mut decoder,
        &mut resampler,
        stream_time_base,
        request.identity,
        audio,
        cancelled,
        events,
        &mut started,
        true,
    ) {
        FrameDrain::Continue => {}
        FrameDrain::Cancelled => return WorkerOutcome::Cancelled,
        FrameDrain::Failed(stage) => {
            return WorkerOutcome::Failed(WorkerFailurePhase::Decoding(stage));
        }
    }
    match flush_resampler(
        &mut resampler,
        request.identity,
        audio,
        cancelled,
        events,
        &mut started,
    ) {
        FrameDrain::Continue => {}
        FrameDrain::Cancelled => return WorkerOutcome::Cancelled,
        FrameDrain::Failed(stage) => {
            return WorkerOutcome::Failed(WorkerFailurePhase::Decoding(stage));
        }
    }
    if !started {
        return WorkerOutcome::Failed(WorkerFailurePhase::Decoding(
            DecodeFailureStage::NoAudioFrames,
        ));
    }
    if audio.wait_until_empty(request.identity.id, cancelled) {
        WorkerOutcome::EofDrained
    } else {
        WorkerOutcome::Cancelled
    }
}

fn decode_error_outcome(cancelled: &AtomicBool, stage: DecodeFailureStage) -> WorkerOutcome {
    if cancelled.load(Ordering::Acquire) {
        WorkerOutcome::Cancelled
    } else {
        WorkerOutcome::Failed(WorkerFailurePhase::Decoding(stage))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PacketSubmit {
    Accepted,
    SkipInvalid,
    Failed,
}

fn classify_packet_submit(
    result: Result<(), ffmpeg::Error>,
    consecutive_invalid_packets: &mut usize,
) -> PacketSubmit {
    match result {
        Ok(()) => {
            *consecutive_invalid_packets = 0;
            PacketSubmit::Accepted
        }
        Err(ffmpeg::Error::InvalidData) => {
            *consecutive_invalid_packets = consecutive_invalid_packets.saturating_add(1);
            if *consecutive_invalid_packets <= MAX_CONSECUTIVE_INVALID_PACKETS {
                PacketSubmit::SkipInvalid
            } else {
                PacketSubmit::Failed
            }
        }
        Err(_) => PacketSubmit::Failed,
    }
}

fn submit_packet(
    decoder: &mut ffmpeg::decoder::Audio,
    packet: &ffmpeg::Packet,
    consecutive_invalid_packets: &mut usize,
) -> PacketSubmit {
    classify_packet_submit(decoder.send_packet(packet), consecutive_invalid_packets)
}

enum FrameDrain {
    Continue,
    Cancelled,
    Failed(DecodeFailureStage),
}

#[allow(clippy::too_many_arguments)]
fn drain_decoder_frames(
    decoder: &mut ffmpeg::decoder::Audio,
    resampler: &mut ffmpeg::software::resampling::Context,
    time_base: ffmpeg::Rational,
    identity: AttemptIdentity,
    audio: &PcmBuffer,
    cancelled: &AtomicBool,
    events: &mpsc::UnboundedSender<WorkerEvent>,
    started: &mut bool,
    draining_at_eof: bool,
) -> FrameDrain {
    let mut decoded = ffmpeg::frame::Audio::empty();
    let mut eof_drain_eagain = false;
    loop {
        match decoder.receive_frame(&mut decoded) {
            Ok(()) => match resample_and_enqueue(
                &decoded, time_base, resampler, identity, audio, cancelled, events, started,
            ) {
                FrameDrain::Continue => {}
                result => return result,
            },
            Err(ffmpeg::Error::Eof) => return FrameDrain::Continue,
            Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => {
                if !draining_at_eof {
                    return FrameDrain::Continue;
                }
                if eof_drain_eagain {
                    return FrameDrain::Failed(DecodeFailureStage::DecoderDrain);
                }
                // A few decoders report one EAGAIN after the NULL packet before
                // reporting EOF. Retry once so their delayed frame queue is not
                // mistaken for a truncated stream.
                eof_drain_eagain = true;
            }
            Err(_) if cancelled.load(Ordering::Acquire) => return FrameDrain::Cancelled,
            Err(_) => return FrameDrain::Failed(DecodeFailureStage::DecoderDrain),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn resample_and_enqueue(
    decoded: &ffmpeg::frame::Audio,
    time_base: ffmpeg::Rational,
    resampler: &mut ffmpeg::software::resampling::Context,
    identity: AttemptIdentity,
    audio: &PcmBuffer,
    cancelled: &AtomicBool,
    events: &mpsc::UnboundedSender<WorkerEvent>,
    started: &mut bool,
) -> FrameDrain {
    let position_seconds = timestamp_to_seconds(decoded.timestamp(), time_base);
    let mut converted = ffmpeg::frame::Audio::empty();
    if resampler.run(decoded, &mut converted).is_err() {
        return if cancelled.load(Ordering::Acquire) {
            FrameDrain::Cancelled
        } else {
            FrameDrain::Failed(DecodeFailureStage::Resample)
        };
    }
    enqueue_resampled_frame(
        &converted,
        position_seconds,
        identity,
        audio,
        cancelled,
        events,
        started,
    )
}

#[allow(clippy::too_many_arguments)]
fn flush_resampler(
    resampler: &mut ffmpeg::software::resampling::Context,
    identity: AttemptIdentity,
    audio: &PcmBuffer,
    cancelled: &AtomicBool,
    events: &mpsc::UnboundedSender<WorkerEvent>,
    started: &mut bool,
) -> FrameDrain {
    loop {
        if resampler.delay().is_none() {
            return FrameDrain::Continue;
        }
        let mut converted = ffmpeg::frame::Audio::empty();
        converted.set_format(resampler.output().format);
        converted.set_channel_layout(resampler.output().channel_layout);
        let delay = match resampler.flush(&mut converted) {
            Ok(delay) => delay,
            Err(_) if cancelled.load(Ordering::Acquire) => return FrameDrain::Cancelled,
            Err(_) => return FrameDrain::Failed(DecodeFailureStage::ResamplerDrain),
        };
        if converted.samples() == 0 {
            return FrameDrain::Continue;
        }
        match enqueue_resampled_frame(
            &converted, None, identity, audio, cancelled, events, started,
        ) {
            FrameDrain::Continue => {}
            result => return result,
        }
        if delay.is_none() {
            return FrameDrain::Continue;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn enqueue_resampled_frame(
    frame: &ffmpeg::frame::Audio,
    position_seconds: Option<f64>,
    identity: AttemptIdentity,
    audio: &PcmBuffer,
    cancelled: &AtomicBool,
    events: &mpsc::UnboundedSender<WorkerEvent>,
    started: &mut bool,
) -> FrameDrain {
    if frame.samples() == 0 {
        return FrameDrain::Continue;
    }
    let samples = frame.plane::<f32>(0);
    if samples.is_empty() {
        return FrameDrain::Continue;
    }
    if !audio.push(identity.id, samples, cancelled) {
        return FrameDrain::Cancelled;
    }
    if !*started {
        *started = true;
        let _ = events.send(WorkerEvent::Started {
            identity,
            position_seconds,
        });
    }
    FrameDrain::Continue
}

fn initialize_ffmpeg() -> Result<(), EngineError> {
    if FFMPEG_INITIALIZED
        .get_or_init(|| {
            // FFmpeg's process-global logger can otherwise emit signed input
            // URLs or request headers before the actor classifies an error.
            ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);
            ffmpeg::init().map_err(|_| ())?;
            ffmpeg::format::network::init();
            Ok(())
        })
        .is_err()
    {
        return Err(EngineError::Unavailable(
            "initialize FFmpeg runtime".to_owned(),
        ));
    }
    Ok(())
}

fn ffmpeg_input_options(
    headers: &BTreeMap<String, String>,
    io_timeout: Duration,
) -> Result<ffmpeg::Dictionary<'_>, ()> {
    let io_timeout_us = u64::try_from(io_timeout.as_micros()).map_err(|_| ())?;
    if io_timeout_us == 0 {
        return Err(());
    }
    let mut options = ffmpeg::Dictionary::new();
    options.set("rw_timeout", &io_timeout_us.to_string());
    if let Some(headers) = ffmpeg_http_headers(headers)? {
        options.set("headers", &headers);
    }
    Ok(options)
}

fn ffmpeg_http_headers(headers: &BTreeMap<String, String>) -> Result<Option<String>, ()> {
    if headers.is_empty() {
        return Ok(None);
    }
    let mut serialized = String::new();
    for (name, value) in headers {
        if !is_http_token(name) || value.contains(['\r', '\n', '\0']) {
            return Err(());
        }
        serialized.push_str(name);
        serialized.push_str(": ");
        serialized.push_str(value);
        serialized.push_str("\r\n");
    }
    Ok(Some(serialized))
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn duration_from_input(input: &ffmpeg::format::context::Input) -> Option<f64> {
    let duration = input.duration();
    (duration > 0)
        .then(|| duration as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE))
        .filter(|duration| duration.is_finite() && *duration > 0.0)
}

fn duration_from_stream(duration: i64, time_base: ffmpeg::Rational) -> Option<f64> {
    (duration > 0)
        .then(|| duration as f64 * f64::from(time_base))
        .filter(|duration| duration.is_finite() && *duration > 0.0)
}

fn timestamp_to_seconds(timestamp: Option<i64>, time_base: ffmpeg::Rational) -> Option<f64> {
    timestamp
        .map(|timestamp| timestamp as f64 * f64::from(time_base))
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::mpsc as std_mpsc;
    use std::thread;
    use std::time::Duration;

    use ffmpeg_next as ffmpeg;
    use tokio::sync::{mpsc, watch};
    use url::Url;
    use uuid::Uuid;

    use crate::domain::SongKey;

    use super::{
        ActivePlayback, ActiveWorker, ActorState, AttemptIdentity, AudioEngine, DecodeFailureStage,
        EndBehavior, EndCause, EngineCommand, EngineError, EngineState, FrameDrain,
        MAX_CONSECUTIVE_INVALID_PACKETS, OutputFormat, OutputRuntime, PacketSubmit, PcmBuffer,
        PlaybackSnapshot, SessionRef, StreamSource, WorkerEvent, WorkerFailurePhase, WorkerRequest,
        classify_eof, classify_packet_submit, ffmpeg_http_headers, ffmpeg_input_options,
        flush_resampler, handle_command, handle_worker_event, initialize_ffmpeg, run_decode_worker,
    };

    fn stream() -> StreamSource {
        StreamSource {
            url: Url::parse("https://media.example.test/track.m4s").unwrap(),
            headers: BTreeMap::new(),
            expires_at_epoch_ms: None,
        }
    }

    fn output() -> OutputRuntime {
        OutputRuntime {
            audio: Arc::new(PcmBuffer::new(96_000)),
            volume: Arc::new(AtomicU8::new(100)),
            format: OutputFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            io_timeout: Duration::from_secs(15),
        }
    }

    fn active_state(session: SessionRef, engine_state: EngineState) -> ActorState {
        let song_key = SongKey::new("bilibili", "BV1us411d75p").unwrap();
        ActorState {
            snapshot: PlaybackSnapshot {
                generation: session.generation,
                session_id: Some(session.session_id),
                state: engine_state,
                song_key: Some(song_key),
                end_behavior: Some(EndBehavior::NotifyController),
                ..PlaybackSnapshot::default()
            },
            active: Some(ActivePlayback {
                session,
                stream: stream(),
                end_behavior: EndBehavior::NotifyController,
                paused: false,
            }),
            active_worker: None,
            retiring_workers: Vec::new(),
            pending_launch: None,
            playback_committed: false,
            position_base_seconds: Some(0.0),
            next_attempt_id: 2,
        }
    }

    #[test]
    fn stale_stop_cannot_replace_the_current_ffmpeg_session() {
        let current = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 2,
        };
        let mut state = active_state(current, EngineState::Playing);
        let output = output();
        let (snapshots, _) = watch::channel(PlaybackSnapshot::default());
        let (events, _) = mpsc::unbounded_channel();

        let result = handle_command(
            EngineCommand::Stop {
                session: SessionRef {
                    session_id: Uuid::new_v4(),
                    generation: 1,
                },
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        );

        assert!(matches!(result, Err(EngineError::Rejected(message)) if message.contains("stale")));
        assert_eq!(state.snapshot.session_id, Some(current.session_id));
        assert_eq!(state.snapshot.state, EngineState::Playing);
    }

    #[test]
    fn pause_and_resume_update_the_pcm_gate_and_snapshot() {
        let session = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 1,
        };
        let mut state = active_state(session, EngineState::Playing);
        let output = output();
        let (snapshots, _) = watch::channel(PlaybackSnapshot::default());
        let (events, _) = mpsc::unbounded_channel();

        handle_command(
            EngineCommand::Pause { session },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .unwrap();
        assert_eq!(state.snapshot.state, EngineState::Paused);
        assert!(output.audio.is_paused());

        handle_command(
            EngineCommand::Resume { session },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .unwrap();
        assert_eq!(state.snapshot.state, EngineState::Playing);
        assert!(!output.audio.is_paused());
    }

    #[tokio::test]
    async fn worker_failure_silences_buffered_pcm_before_recovery() {
        let session = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 1,
        };
        let mut state = active_state(session, EngineState::Playing);
        let output = output();
        output.audio.activate(1, false);
        state.active_worker = Some(ActiveWorker {
            identity: AttemptIdentity { id: 1, session },
            cancel: Arc::new(AtomicBool::new(false)),
            task: tokio::spawn(async {}),
        });
        let cancelled = AtomicBool::new(false);
        assert!(output.audio.push(1, &[0.25, -0.25], &cancelled));
        let (snapshots, _) = watch::channel(PlaybackSnapshot::default());
        let (events, _) = mpsc::unbounded_channel();

        handle_worker_event(
            WorkerEvent::Failed {
                identity: AttemptIdentity { id: 1, session },
                phase: WorkerFailurePhase::Decoding(DecodeFailureStage::PacketRead),
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .await;

        assert_eq!(state.snapshot.state, EngineState::Failed);
        assert_eq!(output.audio.active_attempt.load(Ordering::Acquire), 0);
        assert!(
            output
                .audio
                .samples
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cancelling_a_stalled_http_open_finishes_within_the_io_deadline() {
        initialize_ffmpeg().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let server = thread::spawn(move || {
            if listener.accept().is_ok() {
                let _ = accepted_tx.send(());
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
            }
        });
        let session = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 1,
        };
        let identity = AttemptIdentity { id: 1, session };
        let audio = Arc::new(PcmBuffer::new(96_000));
        audio.activate(identity.id, false);
        let cancelled = Arc::new(AtomicBool::new(false));
        let (events, _) = mpsc::unbounded_channel();
        let request = WorkerRequest {
            identity,
            stream: StreamSource {
                url: Url::parse(&format!("http://{address}/stalled.mp3")).unwrap(),
                headers: BTreeMap::new(),
                expires_at_epoch_ms: None,
            },
            end_behavior: EndBehavior::NotifyController,
            seek_seconds: None,
            output: OutputFormat {
                sample_rate: 48_000,
                channels: 2,
            },
            io_timeout: Duration::from_secs(1),
        };
        let task_audio = audio.clone();
        let task_cancelled = cancelled.clone();
        let task = tokio::task::spawn_blocking(move || {
            run_decode_worker(request, task_audio, task_cancelled, events);
        });

        accepted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        cancelled.store(true, Ordering::Release);
        audio.retire(identity.id);
        let mut task = task;
        let finished = tokio::time::timeout(Duration::from_secs(3), &mut task).await;
        let _ = release_tx.send(());
        server.join().unwrap();

        assert!(finished.is_ok());
        finished.unwrap().unwrap();
    }

    #[test]
    fn resampler_flush_allocates_a_frame_for_delayed_samples() {
        initialize_ffmpeg().unwrap();
        let format = ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed);
        let layout = ffmpeg::ChannelLayout::STEREO;
        let mut resampler = ffmpeg::software::resampling::Context::get(
            format, layout, 44_100, format, layout, 48_000,
        )
        .unwrap();
        let mut decoded = ffmpeg::frame::Audio::new(format, 1_024, layout);
        decoded.set_rate(44_100);
        decoded.plane_mut::<f32>(0).fill(0.0);
        let mut converted = ffmpeg::frame::Audio::empty();
        resampler.run(&decoded, &mut converted).unwrap();
        assert!(resampler.delay().is_some());

        let audio = PcmBuffer::new(96_000);
        audio.activate(1, false);
        let (events, _) = mpsc::unbounded_channel();
        let identity = AttemptIdentity {
            id: 1,
            session: SessionRef {
                session_id: Uuid::new_v4(),
                generation: 1,
            },
        };
        let mut started = false;
        let result = flush_resampler(
            &mut resampler,
            identity,
            &audio,
            &AtomicBool::new(false),
            &events,
            &mut started,
        );

        assert!(matches!(result, FrameDrain::Continue));
        assert!(started);
    }

    #[test]
    fn isolated_invalid_packet_is_skipped_and_success_resets_the_streak() {
        let mut consecutive = 0;

        assert_eq!(
            classify_packet_submit(Err(ffmpeg::Error::InvalidData), &mut consecutive),
            PacketSubmit::SkipInvalid
        );
        assert_eq!(consecutive, 1);
        assert_eq!(
            classify_packet_submit(Ok(()), &mut consecutive),
            PacketSubmit::Accepted
        );
        assert_eq!(consecutive, 0);
    }

    #[test]
    fn too_many_consecutive_invalid_packets_fail_the_worker() {
        let mut consecutive = 0;
        for _ in 0..MAX_CONSECUTIVE_INVALID_PACKETS {
            assert_eq!(
                classify_packet_submit(Err(ffmpeg::Error::InvalidData), &mut consecutive),
                PacketSubmit::SkipInvalid
            );
        }

        assert_eq!(
            classify_packet_submit(Err(ffmpeg::Error::InvalidData), &mut consecutive),
            PacketSubmit::Failed
        );
    }

    #[test]
    fn non_data_submit_errors_fail_immediately() {
        let mut consecutive = 0;

        assert_eq!(
            classify_packet_submit(
                Err(ffmpeg::Error::Other {
                    errno: ffmpeg::error::EAGAIN,
                }),
                &mut consecutive,
            ),
            PacketSubmit::Failed
        );
    }

    #[test]
    fn ffmpeg_input_options_set_a_positive_network_io_timeout() {
        let headers = BTreeMap::new();
        let options = ffmpeg_input_options(&headers, Duration::from_millis(1_250)).unwrap();

        assert_eq!(options.get("rw_timeout"), Some("1250000"));
        assert!(ffmpeg_input_options(&headers, Duration::ZERO).is_err());
    }

    #[tokio::test]
    async fn seek_defers_the_next_attempt_and_does_not_claim_requested_position() {
        let session = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 1,
        };
        let mut state = active_state(session, EngineState::Playing);
        state.retiring_workers.push(ActiveWorker {
            identity: AttemptIdentity { id: 1, session },
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            task: tokio::spawn(async {}),
        });
        let output = output();
        let (snapshots, _) = watch::channel(PlaybackSnapshot::default());
        let (events, _) = mpsc::unbounded_channel();

        handle_command(
            EngineCommand::Seek {
                session,
                position_seconds: 73.5,
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .unwrap();

        assert_eq!(state.snapshot.state, EngineState::Loading);
        assert_eq!(state.snapshot.position_seconds, None);
        assert_eq!(
            state
                .pending_launch
                .as_ref()
                .and_then(|launch| launch.seek_seconds),
            Some(73.5)
        );
        assert!(state.active_worker.is_none());
    }

    #[tokio::test]
    async fn start_keeps_the_restored_seek_position_for_the_next_launch() {
        // 新会话（generation 2）以恢复起始位置起播：Start 命令的 seek_seconds
        // 必须进入 pending_launch，让下一次解码尝试从该进度续播。
        let new_session = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 2,
        };
        let mut state = active_state(
            SessionRef {
                session_id: Uuid::new_v4(),
                generation: 1,
            },
            EngineState::Playing,
        );
        state.retiring_workers.push(ActiveWorker {
            identity: AttemptIdentity {
                id: 1,
                session: new_session,
            },
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            task: tokio::spawn(async {}),
        });
        let output = output();
        let (snapshots, _) = watch::channel(PlaybackSnapshot::default());
        let (events, _) = mpsc::unbounded_channel();

        handle_command(
            EngineCommand::Start {
                session_id: new_session.session_id,
                generation: new_session.generation,
                song_key: SongKey::new("bilibili", "BV1us411d75p").unwrap(),
                stream: stream(),
                end_behavior: EndBehavior::NotifyController,
                seek_seconds: Some(37.0),
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .unwrap();

        assert_eq!(state.snapshot.state, EngineState::Loading);
        assert_eq!(
            state
                .pending_launch
                .as_ref()
                .and_then(|launch| launch.seek_seconds),
            Some(37.0)
        );
        assert!(state.active_worker.is_none());
    }

    #[tokio::test]
    async fn repeated_stream_failure_can_refresh_from_the_known_restart_position() {
        let session = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 1,
        };
        let mut state = active_state(session, EngineState::Playing);
        state.playback_committed = true;
        state.position_base_seconds = Some(47.0);
        state.snapshot.position_seconds = Some(47.0);
        let output = output();
        output.audio.activate(1, false);
        state.active_worker = Some(ActiveWorker {
            identity: AttemptIdentity { id: 1, session },
            cancel: Arc::new(AtomicBool::new(false)),
            task: tokio::spawn(async {}),
        });
        let (snapshots, _) = watch::channel(PlaybackSnapshot::default());
        let (events, _) = mpsc::unbounded_channel();

        handle_worker_event(
            WorkerEvent::Looping {
                identity: AttemptIdentity { id: 1, session },
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .await;
        assert_eq!(state.snapshot.state, EngineState::Loading);
        assert_eq!(state.snapshot.position_seconds, Some(0.0));

        handle_worker_event(
            WorkerEvent::Failed {
                identity: AttemptIdentity { id: 1, session },
                phase: WorkerFailurePhase::Opening,
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .await;
        handle_command(
            EngineCommand::RefreshStream {
                session_id: session.session_id,
                generation: session.generation,
                stream: stream(),
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .unwrap();

        assert_eq!(
            state
                .pending_launch
                .as_ref()
                .and_then(|launch| launch.seek_seconds),
            Some(0.0)
        );
    }

    #[test]
    fn refresh_rejects_committed_playback_without_position_evidence() {
        let session = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 1,
        };
        let mut state = active_state(session, EngineState::Failed);
        state.playback_committed = true;
        state.snapshot.position_seconds = None;
        let output = output();
        let (snapshots, _) = watch::channel(PlaybackSnapshot::default());
        let (events, _) = mpsc::unbounded_channel();

        let result = handle_command(
            EngineCommand::RefreshStream {
                session_id: session.session_id,
                generation: session.generation,
                stream: stream(),
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        );

        assert!(
            matches!(result, Err(EngineError::Rejected(message)) if message.contains("position"))
        );
        assert_eq!(
            state.snapshot.last_end_cause,
            Some(EndCause::RecoveryPositionUnknown)
        );
    }

    #[tokio::test]
    async fn retired_worker_events_are_fenced_from_a_replacement_session() {
        let old_session = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 1,
        };
        let new_session = SessionRef {
            session_id: Uuid::new_v4(),
            generation: 2,
        };
        let mut state = active_state(old_session, EngineState::Playing);
        let output = output();
        output.audio.activate(1, false);
        state.active_worker = Some(ActiveWorker {
            identity: AttemptIdentity {
                id: 1,
                session: old_session,
            },
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            task: tokio::spawn(async {}),
        });
        let (snapshots, _) = watch::channel(PlaybackSnapshot::default());
        let (events, _) = mpsc::unbounded_channel();

        handle_command(
            EngineCommand::Start {
                session_id: new_session.session_id,
                generation: new_session.generation,
                song_key: SongKey::new("bilibili", "BV1us411d75p").unwrap(),
                stream: stream(),
                end_behavior: EndBehavior::NotifyController,
                seek_seconds: None,
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .unwrap();
        handle_worker_event(
            WorkerEvent::Failed {
                identity: AttemptIdentity {
                    id: 1,
                    session: old_session,
                },
                phase: WorkerFailurePhase::Decoding(DecodeFailureStage::PacketRead),
            },
            &mut state,
            &snapshots,
            &output,
            &events,
        )
        .await;

        assert_eq!(state.snapshot.session_id, Some(new_session.session_id));
        assert_eq!(state.snapshot.state, EngineState::Loading);
        assert_eq!(state.snapshot.last_end_cause, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "makes a public network request and plays the complete Bilibili track"]
    async fn bilibili_public_stream_reaches_natural_end() {
        let client = reqwest::Client::builder().build().unwrap();
        let view = client
            .get("https://api.bilibili.com/x/web-interface/view?bvid=BV1us411d75p")
            .header("Referer", "https://www.bilibili.com/")
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(
            view.get("code").and_then(serde_json::Value::as_i64),
            Some(0)
        );
        let cid = view
            .pointer("/data/cid")
            .and_then(serde_json::Value::as_u64)
            .unwrap();
        let duration_seconds = view
            .pointer("/data/duration")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(600);
        let playurl = client
            .get(format!(
                "https://api.bilibili.com/x/player/playurl?bvid=BV1us411d75p&cid={cid}&fnval=4048&fourk=1"
            ))
            .header("Referer", "https://www.bilibili.com/")
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(
            playurl.get("code").and_then(serde_json::Value::as_i64),
            Some(0)
        );
        let stream_url = playurl
            .pointer("/data/dash/audio")
            .and_then(serde_json::Value::as_array)
            .and_then(|audio| {
                audio.iter().max_by_key(|stream| {
                    stream
                        .get("bandwidth")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or_default()
                })
            })
            .and_then(|stream| {
                stream
                    .get("base_url")
                    .or_else(|| stream.get("baseUrl"))
                    .and_then(serde_json::Value::as_str)
            })
            .and_then(|value| Url::parse(value).ok())
            .unwrap();

        let engine = super::FfmpegEngine::spawn(super::FfmpegConfig {
            command_timeout: std::time::Duration::from_secs(3),
            io_timeout: std::time::Duration::from_secs(15),
            pcm_buffer_duration: std::time::Duration::from_secs(2),
        })
        .await
        .unwrap();
        let mut snapshots = engine.subscribe();
        let session_id = Uuid::new_v4();
        engine
            .command(EngineCommand::Start {
                session_id,
                generation: 1,
                song_key: SongKey::new("bilibili", "BV1us411d75p").unwrap(),
                stream: StreamSource {
                    url: stream_url,
                    headers: BTreeMap::from([
                        ("Referer".to_owned(), "https://www.bilibili.com/".to_owned()),
                        ("User-Agent".to_owned(), "Mozilla/5.0".to_owned()),
                    ]),
                    expires_at_epoch_ms: None,
                },
                end_behavior: EndBehavior::NotifyController,
                seek_seconds: None,
            })
            .await
            .unwrap();

        let timeout_seconds = duration_seconds.saturating_add(30).clamp(30, 900);
        tokio::time::timeout(std::time::Duration::from_secs(timeout_seconds), async {
            loop {
                let snapshot = snapshots.borrow_and_update().clone();
                if snapshot.last_end_cause == Some(EndCause::NaturalEnd) {
                    assert_eq!(snapshot.state, EngineState::Stopped);
                    break;
                }
                assert_ne!(
                    snapshot.state,
                    EngineState::Failed,
                    "{:#?}",
                    snapshot.failure
                );
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        engine.shutdown().await.unwrap();
    }

    #[test]
    fn ffmpeg_headers_preserve_all_provider_headers_without_url_logging() {
        let headers = BTreeMap::from([
            ("Referer".to_owned(), "https://www.bilibili.com/".to_owned()),
            ("User-Agent".to_owned(), "Miliastra/1.0".to_owned()),
        ]);

        assert_eq!(
            ffmpeg_http_headers(&headers).unwrap().as_deref(),
            Some("Referer: https://www.bilibili.com/\r\nUser-Agent: Miliastra/1.0\r\n")
        );
    }

    #[test]
    fn ffmpeg_headers_reject_line_injection() {
        let headers = BTreeMap::from([(
            "Cookie".to_owned(),
            "SESSDATA=secret\r\nInjected: value".to_owned(),
        )]);

        assert!(ffmpeg_http_headers(&headers).is_err());
    }

    #[test]
    fn eof_requires_position_and_duration_evidence() {
        assert_eq!(
            classify_eof(&PlaybackSnapshot::default()),
            EndCause::DecodeFailure
        );

        let snapshot = PlaybackSnapshot {
            position_seconds: Some(178.0),
            duration_seconds: Some(180.0),
            ..PlaybackSnapshot::default()
        };
        assert_eq!(classify_eof(&snapshot), EndCause::NaturalEnd);
    }
}
