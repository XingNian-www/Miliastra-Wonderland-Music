//! 独立 TTS 语音播放器（播放 fish.audio 返回的 WAV/PCM）。
//!
//! 设计目标（见 `technical-docs/team-reports/tts-ffmpeg-fish-audio-feasibility.md` 方案 A）：
//! - **与音乐播放完全隔离**：自己的线程（`tts-runtime`）+ 自己的 CPAL 输出流，独立 PCM
//!   缓冲；不占用、不暂停、不替换音乐 session，也**不触碰**音乐的 `PlaybackSnapshot`。
//! - 音乐侧压低由引擎独立的 `EngineCommand::SetDuckGain` 完成（本模块只负责播语音）。
//! - **流健康可观测**：CPAL 输出流错误 / 线程异常退出会反映到 [`SpeechHealth`] 与
//!   [`SpeechPlayer::is_alive`]，`is_speaking` 自动回落；可 [`SpeechPlayer::restart`] 恢复。
//! - 失败可降级：无音频设备 / 解码失败时返回 [`SpeechError`]，调用方静默跳过即可。
//!
//! 输入支持：
//! - [`SpeechAudio::Pcm`]：已解码的交错 f32 PCM（样本范围 -1.0..=1.0）；
//! - [`SpeechAudio::Wav`]：WAV 文件字节（PCM16 / IEEE-F32，单/多声道）。
//!
//! 说明：非 WAV/PCM 的压缩格式（如 mp3/opus）由上层先用现有 ffmpeg 解码，或直接向
//! fish.audio 请求 `format=wav|pcm`（本项目 P0 建议），本模块 P0 不做容器解析。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use thiserror::Error;
use tokio::sync::watch;

/// 语音播放器/输出流的健康状态。
///
/// - [`SpeechHealth::Healthy`]：已初始化且流仍存活；
/// - [`SpeechHealth::Degraded`]：输出流报错（如设备被拔出/独占失败）或线程异常退出，
///   `is_alive` 会变 false，`is_speaking` 回落；调用方应停止 duck 并（可选）`restart`；
/// - [`SpeechHealth::Closed`]：正常关闭（`shutdown`/`Drop`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeechHealth {
    Healthy,
    Degraded { reason: String },
    Closed,
}

impl SpeechHealth {
    /// 是否健康可用。
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// 降级原因（仅 `Degraded` 有值）。
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Degraded { reason } => Some(reason),
            _ => None,
        }
    }
}

/// 语音播放错误。
#[derive(Debug, Clone, Error)]
pub enum SpeechError {
    /// 无可用音频输出设备 / 打开输出流失败。
    #[error("speech audio device is unavailable: {0}")]
    Unavailable(String),
    /// 音频数据损坏/形状非法（可重试或丢弃该条语音，不应据此判定设备不可用）。
    #[error("speech audio decode failed: {0}")]
    Decode(String),
    /// 音频格式合法但当前不支持（如 WAV 24-bit）。
    #[error("speech audio format is unsupported: {0}")]
    UnsupportedFormat(String),
    /// 播放器线程已关闭/已降级，无法继续投递。
    #[error("speech player is closed")]
    Closed,
}

/// 交给语音播放器的音频输入。
#[derive(Debug, Clone)]
pub enum SpeechAudio {
    /// 已解码的交错 f32 PCM。
    Pcm {
        sample_rate: u32,
        channels: u16,
        /// 交错样本（帧数 × 声道），范围 -1.0..=1.0。
        samples: Vec<f32>,
    },
    /// WAV 文件字节（RIFF/WAVE，PCM16 或 IEEE-F32）。
    Wav(Vec<u8>),
}

/// 已解码的 PCM（交错）。
#[derive(Debug, Clone, PartialEq)]
pub struct PcmAudio {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl SpeechAudio {
    /// 解码成统一的交错 f32 PCM。
    ///
    /// 错误分类：数据损坏/形状非法 → [`SpeechError::Decode`]；格式合法但不支持 →
    /// [`SpeechError::UnsupportedFormat`]。
    pub fn decode(self) -> Result<PcmAudio, SpeechError> {
        match self {
            SpeechAudio::Pcm {
                sample_rate,
                channels,
                samples,
            } => {
                if sample_rate == 0 || channels == 0 {
                    return Err(SpeechError::Decode(
                        "PCM sample_rate/channels must be non-zero".to_owned(),
                    ));
                }
                if samples.len() % usize::from(channels) != 0 {
                    return Err(SpeechError::Decode(
                        "PCM sample count is not a multiple of channels".to_owned(),
                    ));
                }
                Ok(PcmAudio {
                    sample_rate,
                    channels,
                    samples,
                })
            }
            SpeechAudio::Wav(bytes) => decode_wav(&bytes),
        }
    }
}

/// 解析 WAV（RIFF/WAVE）。仅支持 `audio_format` = 1 (PCM16) 或 3 (IEEE float32)。
fn decode_wav(bytes: &[u8]) -> Result<PcmAudio, SpeechError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(SpeechError::Decode(
            "not a RIFF/WAVE file".to_owned(),
        ));
    }
    let mut pos = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // (format, channels, sample_rate, bits)
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body_start = pos + 8;
        let declared_end = body_start.saturating_add(size);
        // fish.audio 流式 WAV 会在 `data` chunk 里写占位长度（实测 0xFFFFFF00），
        // 而实际数据在文件尾就结束。仅对 `data` 的“高位哨兵占位值”做
        // “声明长度超过剩余字节 → 按实际剩余字节截断”的容错：
        // - 其它 chunk（fmt 等）声明超长 → 仍严格报错；
        // - data 声明一个正常但略大的长度（真实截断）→ 仍严格报错，不静默吞数据；
        // - clamp 后还必须能组成整数帧且非空（见下方校验）。
        // 注：RIFF 总长度字段当前不参与校验，因此同一占位值不会造成误判。
        const STREAMING_SIZE_SENTINEL: usize = 0x7FFF_FF00;
        let body_end = if declared_end > bytes.len() {
            if id == b"data" && size >= STREAMING_SIZE_SENTINEL {
                bytes.len()
            } else {
                return Err(SpeechError::Decode(
                    "wav chunk is truncated".to_owned(),
                ));
            }
        } else {
            declared_end
        };
        let body = &bytes[body_start..body_end];
        if id == b"fmt " {
            if body.len() < 16 {
                return Err(SpeechError::Decode(
                    "wav fmt chunk too short".to_owned(),
                ));
            }
            let audio_format = u16::from_le_bytes([body[0], body[1]]);
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let bits = u16::from_le_bytes([body[14], body[15]]);
            fmt = Some((audio_format, channels, sample_rate, bits));
        } else if id == b"data" {
            data = Some(body);
        }
        // chunk 以偶数字节对齐。
        pos = body_end + (size & 1);
    }
    let (audio_format, channels, sample_rate, bits) = fmt.ok_or_else(|| {
        SpeechError::Decode("wav is missing fmt chunk".to_owned())
    })?;
    let data =
        data.ok_or_else(|| SpeechError::Decode("wav is missing data chunk".to_owned()))?;
    if channels == 0 || sample_rate == 0 {
        return Err(SpeechError::Decode(
            "wav has invalid channels/sample_rate".to_owned(),
        ));
    }
    let (samples, bytes_per_sample) = match (audio_format, bits) {
        (1, 16) => (
            data.chunks_exact(2)
                .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32768.0)
                .collect::<Vec<_>>(),
            2usize,
        ),
        (3, 32) => (
            data.chunks_exact(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect::<Vec<_>>(),
            4usize,
        ),
        (format, bits) => {
            return Err(SpeechError::UnsupportedFormat(format!(
                "wav audio_format={format}, bits={bits} not supported (need PCM16 or F32)"
            )))
        }
    };
    // 真实截断的严格边界：剩余数据必须能组成整数个帧（不能靠 clamp 静默吞掉半帧）。
    let frame_size = usize::from(channels) * bytes_per_sample;
    if data.len() % frame_size != 0 {
        return Err(SpeechError::Decode(
            "wav data length is not a whole number of frames".to_owned(),
        ));
    }
    if samples.is_empty() {
        return Err(SpeechError::Decode(
            "wav contains no audio frames".to_owned(),
        ));
    }
    Ok(PcmAudio {
        sample_rate,
        channels,
        samples,
    })
}

/// 把输入 PCM 转成设备要求的采样率/声道（交错）。纯函数，便于 Linux 单测。
///
/// 声道策略：单声道→多声道复制；多声道→单声道取平均；多→多取前 N 个声道。
/// 采样率策略：线性插值重采样。
fn prepare_for_device(input: &PcmAudio, target_rate: u32, target_channels: u16) -> Vec<f32> {
    let in_channels = usize::from(input.channels.max(1));
    let out_channels = usize::from(target_channels.max(1));
    let frames = input.samples.len() / in_channels;
    if frames == 0 {
        return Vec::new();
    }
    // 先拆分成每声道，做声道映射到 target_channels。
    let mut per_channel: Vec<Vec<f32>> = Vec::with_capacity(out_channels);
    for out_ch in 0..out_channels {
        let mut channel = Vec::with_capacity(frames);
        for frame in 0..frames {
            let base = frame * in_channels;
            let value = if in_channels == 1 {
                input.samples[base]
            } else if out_channels == 1 {
                let sum: f32 = (0..in_channels).map(|c| input.samples[base + c]).sum();
                sum / in_channels as f32
            } else {
                input.samples[base + out_ch.min(in_channels - 1)]
            };
            channel.push(value);
        }
        per_channel.push(channel);
    }
    // 采样率重采样（每声道线性插值）。
    let out_frames = if input.sample_rate == target_rate || input.sample_rate == 0 {
        frames
    } else {
        ((frames as f64) * f64::from(target_rate) / f64::from(input.sample_rate)).round().max(1.0)
            as usize
    };
    let mut interleaved = vec![0.0_f32; out_frames * out_channels];
    if input.sample_rate == target_rate || input.sample_rate == 0 {
        for frame in 0..frames {
            for ch in 0..out_channels {
                interleaved[frame * out_channels + ch] = per_channel[ch][frame];
            }
        }
        return interleaved;
    }
    let ratio = f64::from(input.sample_rate) / f64::from(target_rate);
    for out_frame in 0..out_frames {
        let src_pos = (out_frame as f64) * ratio;
        let i0 = src_pos.floor() as usize;
        let i1 = (i0 + 1).min(frames - 1);
        let frac = (src_pos - i0 as f64) as f32;
        for ch in 0..out_channels {
            let a = per_channel[ch][i0.min(frames - 1)];
            let b = per_channel[ch][i1];
            interleaved[out_frame * out_channels + ch] = a * (1.0 - frac) + b * frac;
        }
    }
    interleaved
}

/// 语音播放器的共享健康/状态监督器（可由 CPAL error callback 跨线程上报）。
struct Supervisor {
    alive: Arc<AtomicBool>,
    failed: AtomicBool,
    health: watch::Sender<SpeechHealth>,
    active: watch::Sender<bool>,
}

impl Supervisor {
    fn new() -> Self {
        let (health, _health_rx) = watch::channel(SpeechHealth::Healthy);
        let (active, _active_rx) = watch::channel(false);
        Self {
            alive: Arc::new(AtomicBool::new(false)),
            failed: AtomicBool::new(false),
            health,
            active,
        }
    }

    fn set_speaking(&self, speaking: bool) {
        self.active.send_replace(speaking);
    }

    /// CPAL 输出流错误 / 线程异常：标记降级并立即让 `is_speaking` 回落。
    fn fail(&self, reason: impl Into<String>) {
        let reason = reason.into();
        self.failed.store(true, Ordering::Release);
        self.health.send_replace(SpeechHealth::Degraded { reason });
        self.active.send_replace(false);
    }

    fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// 若尚未降级，标记为正常关闭。
    fn mark_closed_if_healthy(&self) {
        if !self.is_failed() {
            self.health.send_replace(SpeechHealth::Closed);
        }
        self.active.send_replace(false);
    }
}

/// 线程退出时保证 `alive=false` 且健康状态落到 Closed/Degraded（覆盖 panic 路径）。
struct AliveGuard {
    supervisor: Arc<Supervisor>,
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.supervisor.alive.store(false, Ordering::Release);
        self.supervisor.mark_closed_if_healthy();
    }
}

/// 语音 PCM 缓冲（独立于音乐 `PcmBuffer`）。
struct SpeechBuffer {
    samples: Mutex<VecDeque<f32>>,
    playing: AtomicBool,
    supervisor: Arc<Supervisor>,
}

impl SpeechBuffer {
    fn new(supervisor: Arc<Supervisor>) -> Self {
        Self {
            samples: Mutex::new(VecDeque::new()),
            playing: AtomicBool::new(false),
            supervisor,
        }
    }

    fn enqueue(&self, pcm: Vec<f32>) {
        if pcm.is_empty() {
            return;
        }
        {
            let mut queue = self.samples.lock().unwrap_or_else(|error| error.into_inner());
            queue.extend(pcm);
        }
        self.playing.store(true, Ordering::Release);
        self.supervisor.set_speaking(true);
    }

    fn stop(&self) {
        {
            let mut queue = self.samples.lock().unwrap_or_else(|error| error.into_inner());
            queue.clear();
        }
        if self.playing.swap(false, Ordering::AcqRel) {
            self.supervisor.set_speaking(false);
        }
    }

    fn fill<T>(&self, output: &mut [T])
    where
        T: SizedSample + FromSample<f32>,
    {
        let silence = T::from_sample(0.0_f32);
        let Ok(mut queue) = self.samples.try_lock() else {
            output.fill(silence);
            return;
        };
        let mut any = false;
        for value in output.iter_mut() {
            match queue.pop_front() {
                Some(sample) => {
                    *value = T::from_sample(sample);
                    any = true;
                }
                None => *value = silence,
            }
        }
        let drained = queue.is_empty();
        drop(queue);
        if any && drained && self.playing.swap(false, Ordering::AcqRel) {
            self.supervisor.set_speaking(false);
        }
    }
}

enum SpeechCommand {
    Speak(PcmAudio),
    Stop,
    Shutdown,
}

/// 已打开的语音输出（真实 CPAL 流；测试用 fake 无流）。
struct SpeechOutput {
    _stream: Option<Stream>,
    target_rate: u32,
    target_channels: u16,
}

/// 语音输出工厂：在 `tts-runtime` 线程内打开设备并构建流（CPAL 流必须同线程创建）。
type OutputFactory =
    dyn Fn(&Arc<SpeechBuffer>, &Arc<Supervisor>) -> Result<SpeechOutput, SpeechError> + Send + Sync;

/// 独立语音播放器句柄。内部持有 `tts-runtime` 线程与独立 CPAL 输出流。
pub struct SpeechPlayer {
    tx: Mutex<mpsc::Sender<SpeechCommand>>,
    supervisor: Arc<Supervisor>,
    factory: Arc<OutputFactory>,
    thread: Option<JoinHandle<()>>,
}

impl SpeechPlayer {
    /// 创建播放器：在专用线程打开默认输出设备并常驻 CPAL 流。
    ///
    /// 无可用音频设备时返回 `SpeechError::Unavailable`（调用方静默降级即可）。
    pub fn spawn() -> Result<Self, SpeechError> {
        let factory: Arc<OutputFactory> = Arc::new(open_default_output);
        Self::spawn_with_factory(factory)
    }

    fn spawn_with_factory(factory: Arc<OutputFactory>) -> Result<Self, SpeechError> {
        let parts = spawn_speech_thread(factory)?;
        Ok(Self {
            tx: Mutex::new(parts.tx),
            supervisor: parts.supervisor,
            factory: parts.factory,
            thread: Some(parts.thread),
        })
    }

    /// 异步投递一段音频：立即返回，实际播放由 `tts-runtime` 线程完成。
    ///
    /// 播放器已降级/关闭时返回 [`SpeechError::Closed`]。
    pub fn speak(&self, audio: SpeechAudio) -> Result<(), SpeechError> {
        if !self.is_alive() {
            return Err(SpeechError::Closed);
        }
        let pcm = audio.decode()?;
        let sender = self.tx.lock().unwrap_or_else(|error| error.into_inner());
        sender
            .send(SpeechCommand::Speak(pcm))
            .map_err(|_| SpeechError::Closed)
    }

    /// 停止当前语音（清空缓冲），不影响音乐。
    pub fn stop(&self) -> Result<(), SpeechError> {
        let sender = self.tx.lock().unwrap_or_else(|error| error.into_inner());
        sender.send(SpeechCommand::Stop).map_err(|_| SpeechError::Closed)
    }

    /// 当前是否正在输出语音。流错误/线程退出后恒为 false（健康回落）。
    pub fn is_speaking(&self) -> bool {
        self.is_alive() && *self.supervisor.active.borrow()
    }

    /// 线程/输出流是否仍存活。
    pub fn is_alive(&self) -> bool {
        self.supervisor.alive.load(Ordering::Acquire)
    }

    /// 当前健康状态快照。
    pub fn health(&self) -> SpeechHealth {
        let guard = self.supervisor.health.borrow();
        (*guard).clone()
    }

    /// 降级原因（健康时返回 None）。
    pub fn last_error(&self) -> Option<String> {
        self.health().reason().map(str::to_owned)
    }

    /// 订阅健康状态变化（供上层停止 duck / 触发重启）。
    pub fn subscribe_health(&self) -> watch::Receiver<SpeechHealth> {
        self.supervisor.health.subscribe()
    }

    /// 订阅说话状态（true=正在说），供上层做 duck 的淡入淡出编排。
    pub fn subscribe_active(&self) -> watch::Receiver<bool> {
        self.supervisor.active.subscribe()
    }

    /// 关闭旧线程并重新打开语音输出（设备恢复/流降级后由上层调用）。
    ///
    /// 失败时旧播放器保持关闭状态，健康为 `Closed/Degraded`，不会卡住上层。
    pub fn restart(&mut self) -> Result<(), SpeechError> {
        self.shutdown_inner();
        let parts = spawn_speech_thread(self.factory.clone())?;
        self.tx = Mutex::new(parts.tx);
        self.supervisor = parts.supervisor;
        self.thread = Some(parts.thread);
        Ok(())
    }

    /// 关闭播放器：停止语音并等待线程退出（消耗句柄，保持既有调用方兼容）。
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        if let Ok(sender) = self.tx.lock() {
            let _ = sender.send(SpeechCommand::Shutdown);
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        self.supervisor.alive.store(false, Ordering::Release);
    }
}

impl Drop for SpeechPlayer {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

struct SpeechParts {
    tx: mpsc::Sender<SpeechCommand>,
    supervisor: Arc<Supervisor>,
    factory: Arc<OutputFactory>,
    thread: JoinHandle<()>,
}

fn spawn_speech_thread(factory: Arc<OutputFactory>) -> Result<SpeechParts, SpeechError> {
    let supervisor = Arc::new(Supervisor::new());
    let buffer = Arc::new(SpeechBuffer::new(supervisor.clone()));
    let (command_tx, command_rx) = mpsc::channel::<SpeechCommand>();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), SpeechError>>(1);

    let thread_supervisor = supervisor.clone();
    let thread_buffer = buffer.clone();
    let thread_factory = factory.clone();
    let thread = thread::Builder::new()
        .name("tts-runtime".to_owned())
        .spawn(move || {
            run_speech_thread(
                thread_factory,
                command_rx,
                thread_supervisor,
                thread_buffer,
                ready_tx,
            )
        })
        .map_err(|error| SpeechError::Unavailable(format!("spawn tts-runtime: {error}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(SpeechParts {
            tx: command_tx,
            supervisor,
            factory,
            thread,
        }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(_) => {
            let _ = thread.join();
            Err(SpeechError::Unavailable(
                "tts-runtime exited during initialization".to_owned(),
            ))
        }
    }
}

fn run_speech_thread(
    factory: Arc<OutputFactory>,
    command_rx: mpsc::Receiver<SpeechCommand>,
    supervisor: Arc<Supervisor>,
    buffer: Arc<SpeechBuffer>,
    ready_tx: mpsc::SyncSender<Result<(), SpeechError>>,
) {
    // 任何路径退出（含 panic）都保证 alive=false + 健康回落。
    let _guard = AliveGuard {
        supervisor: supervisor.clone(),
    };

    let output = match factory(&buffer, &supervisor) {
        Ok(output) => output,
        Err(error) => {
            supervisor.fail(error.to_string());
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    supervisor.alive.store(true, Ordering::Release);
    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    // 常驻：保持 stream 存活，串行处理投递的语音。
    // 用 recv_timeout 周期性检查 CPAL error callback 是否已把流标记为降级。
    loop {
        match command_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(SpeechCommand::Speak(pcm)) => {
                if supervisor.is_failed() {
                    break;
                }
                let prepared = prepare_for_device(&pcm, output.target_rate, output.target_channels);
                buffer.enqueue(prepared);
            }
            Ok(SpeechCommand::Stop) => buffer.stop(),
            Ok(SpeechCommand::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if supervisor.is_failed() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    buffer.stop();
    drop(output);
    // 正常退出由 AliveGuard 标记 Closed；流错误路径已标记 Degraded。
}

fn open_default_output(
    buffer: &Arc<SpeechBuffer>,
    supervisor: &Arc<Supervisor>,
) -> Result<SpeechOutput, SpeechError> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| {
        SpeechError::Unavailable("no default audio output device is available".to_owned())
    })?;
    let supported = device.default_output_config().map_err(|error| {
        SpeechError::Unavailable(format!("query speech output device: {error}"))
    })?;
    let sample_format = supported.sample_format();
    let stream_config: StreamConfig = supported.into();
    if stream_config.channels == 0 || stream_config.sample_rate == 0 {
        return Err(SpeechError::Unavailable(
            "speech output device returned an invalid format".to_owned(),
        ));
    }
    let stream = build_speech_stream(
        &device,
        stream_config,
        sample_format,
        buffer.clone(),
        supervisor.clone(),
    )?;
    stream.play().map_err(|error| {
        SpeechError::Unavailable(format!("start speech output stream: {error}"))
    })?;
    Ok(SpeechOutput {
        _stream: Some(stream),
        target_rate: stream_config.sample_rate,
        target_channels: stream_config.channels,
    })
}

fn build_speech_stream(
    device: &cpal::Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    buffer: Arc<SpeechBuffer>,
    supervisor: Arc<Supervisor>,
) -> Result<Stream, SpeechError> {
    macro_rules! build {
        ($sample:ty) => {
            build_typed_speech_stream::<$sample>(device, config, buffer, supervisor)
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
        unsupported => Err(SpeechError::Unavailable(format!(
            "speech output sample format {unsupported:?} is unsupported"
        ))),
    }
}

fn build_typed_speech_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    buffer: Arc<SpeechBuffer>,
    supervisor: Arc<Supervisor>,
) -> Result<Stream, SpeechError>
where
    T: SizedSample + FromSample<f32>,
{
    let data_buffer = buffer.clone();
    // CPAL error callback 不能空实现：设备丢失/独占失败必须让上层可观测并回落 duck。
    let error_buffer = buffer;
    let error_supervisor = supervisor;
    device
        .build_output_stream(
            config,
            move |output: &mut [T], _| data_buffer.fill(output),
            move |error| {
                error_buffer.stop();
                error_supervisor.fail(format!("speech output stream error: {error}"));
            },
            None,
        )
        .map_err(|error| SpeechError::Unavailable(format!("create speech output stream: {error}")))
}

#[cfg(test)]
mod tests {
    use super::{
        PcmAudio, SpeechAudio, SpeechBuffer, SpeechError, SpeechHealth, SpeechOutput,
        SpeechPlayer, Supervisor, decode_wav, prepare_for_device,
    };
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn wav_pcm16_mono(sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte_rate
        out.extend_from_slice(&2u16.to_le_bytes()); // block_align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for sample in samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    /// 测试用输出工厂：不打开真实声卡，只提供一个假的 target 格式。
    fn fake_factory() -> Arc<super::OutputFactory> {
        Arc::new(|_buffer, _supervisor| {
            Ok(SpeechOutput {
                _stream: None,
                target_rate: 16_000,
                target_channels: 1,
            })
        })
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not reached within 2s");
    }

    #[test]
    fn wav_pcm16_mono_decodes_to_f32() {
        let bytes = wav_pcm16_mono(16_000, &[0, i16::MAX, i16::MIN, 16_384]);
        let decoded = decode_wav(&bytes).unwrap();
        assert_eq!(decoded.sample_rate, 16_000);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.samples.len(), 4);
        assert!((decoded.samples[0]).abs() < 1e-6);
        assert!((decoded.samples[1] - 1.0).abs() < 1e-4);
        assert!((decoded.samples[2] + 1.0).abs() < 1e-4);
        assert!((decoded.samples[3] - 0.5).abs() < 1e-4);
    }

    /// 构造只含标准 44 字节头 + 指定 data 声明长度的 WAV（payload 由调用方追加）。
    fn wav_header_with_data_size(
        sample_rate: u32,
        data_size: u32,
        channels: u16,
        bits: u16,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&data_size.wrapping_add(36).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        let block_align = u32::from(channels) * u32::from(bits / 8);
        out.extend_from_slice(&(sample_rate * block_align).to_le_bytes());
        out.extend_from_slice(&(block_align as u16).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_size.to_le_bytes());
        out
    }

    /// fish 流式 WAV：data 声明 0xFFFFFF00，实际 466,944 字节（44.1kHz mono PCM16）。
    /// RIFF 总长度也写成占位值，确认当前实现不依赖它。
    #[test]
    fn streaming_data_size_placeholder_is_clamped_to_actual_bytes() {
        let frames = 233_472usize; // 466,944 bytes / 2
        let mut payload = Vec::with_capacity(frames * 2);
        for index in 0..frames {
            payload.extend_from_slice(&((index % 30_000) as i16).to_le_bytes());
        }
        let mut bytes = wav_header_with_data_size(44_100, 0xFFFF_FF00, 1, 16);
        bytes[4..8].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        bytes.extend_from_slice(&payload);
        assert_eq!(bytes.len(), 44 + 466_944);

        let pcm = decode_wav(&bytes).unwrap();
        assert_eq!(pcm.sample_rate, 44_100);
        assert_eq!(pcm.channels, 1);
        assert_eq!(pcm.samples.len(), frames);
        assert!((pcm.samples[0]).abs() < 1e-6);
    }

    /// clamp 只允许“剩余字节能组成整数帧”；半帧属于真实截断，必须继续失败。
    #[test]
    fn streaming_placeholder_with_partial_frame_is_still_rejected() {
        let mut bytes = wav_header_with_data_size(16_000, 0xFFFF_FF00, 1, 16);
        bytes.extend_from_slice(&[0x01, 0x02, 0x03]);
        assert!(matches!(decode_wav(&bytes), Err(SpeechError::Decode(_))));
    }

    #[test]
    fn streaming_placeholder_without_audio_frames_is_rejected() {
        let bytes = wav_header_with_data_size(16_000, 0xFFFF_FF00, 1, 16);
        assert!(matches!(decode_wav(&bytes), Err(SpeechError::Decode(_))));
    }

    /// 非 data chunk 的声明长度超过剩余字节仍严格报错，不被 clamp 掩盖。
    #[test]
    fn non_data_chunk_truncation_is_still_strict() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&100u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8]); // 声明 16，只剩 8
        assert!(matches!(decode_wav(&bytes), Err(SpeechError::Decode(_))));
    }

    /// data 声明一个正常但略大的长度（真实截断）仍严格失败，不按 clamp 静默接受。
    #[test]
    fn data_chunk_real_truncation_is_still_rejected() {
        let mut bytes = wav_header_with_data_size(16_000, 1_000, 1, 16);
        bytes.extend_from_slice(&[0u8; 900]); // 声明 1000，实际 900（整帧但确实不足）
        assert!(matches!(decode_wav(&bytes), Err(SpeechError::Decode(_))));
    }

    #[test]
    fn wav_corruption_is_decode_error_not_unsupported() {
        assert!(matches!(decode_wav(b"not a wav"), Err(SpeechError::Decode(_))));
        // 截断的 chunk → Decode
        let mut bytes = wav_pcm16_mono(16_000, &[0, 1]);
        bytes.truncate(20);
        assert!(matches!(decode_wav(&bytes), Err(SpeechError::Decode(_))));
    }

    #[test]
    fn wav_unsupported_bits_is_unsupported_format() {
        // 构造 24-bit fmt：audio_format=1, channels=1, rate=16000, bits=24
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36u32 + 6).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&24u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&6u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        assert!(matches!(
            decode_wav(&bytes),
            Err(SpeechError::UnsupportedFormat(_))
        ));
    }

    #[test]
    fn pcm_passthrough_validates_shape_as_decode_error() {
        let audio = SpeechAudio::Pcm {
            sample_rate: 8_000,
            channels: 2,
            samples: vec![0.0; 4],
        };
        let pcm = audio.decode().unwrap();
        assert_eq!(pcm.channels, 2);
        let bad = SpeechAudio::Pcm {
            sample_rate: 8_000,
            channels: 2,
            samples: vec![0.0; 3],
        };
        assert!(matches!(bad.decode(), Err(SpeechError::Decode(_))));
        let zero = SpeechAudio::Pcm {
            sample_rate: 0,
            channels: 2,
            samples: vec![0.0; 4],
        };
        assert!(matches!(zero.decode(), Err(SpeechError::Decode(_))));
    }

    #[test]
    fn prepare_duplicates_mono_to_stereo_and_resamples() {
        let input = PcmAudio {
            sample_rate: 8_000,
            channels: 1,
            samples: vec![0.25, 0.5, -0.25, -0.5],
        };
        let out = prepare_for_device(&input, 16_000, 2);
        assert_eq!(out.len(), 8 * 2);
        assert!((out[0] - out[1]).abs() < 1e-6);
    }

    #[test]
    fn prepare_downmixes_stereo_to_mono() {
        let input = PcmAudio {
            sample_rate: 48_000,
            channels: 2,
            samples: vec![1.0, 0.0, 0.0, 1.0],
        };
        let out = prepare_for_device(&input, 48_000, 1);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn speech_buffer_toggles_active_when_drained() {
        let supervisor = Arc::new(Supervisor::new());
        let buffer = SpeechBuffer::new(supervisor.clone());
        let rx = supervisor.active.subscribe();
        assert!(!*rx.borrow());
        buffer.enqueue(vec![0.1, 0.2, 0.3]);
        assert!(*rx.borrow());
        let mut out = vec![0.0_f32; 8];
        buffer.fill(&mut out);
        assert!((out[0] - 0.1).abs() < 1e-6);
        assert!((out[2] - 0.3).abs() < 1e-6);
        assert!((out[3]).abs() < 1e-6);
        assert!(!*rx.borrow(), "缓冲排空后应回到未说话状态");
    }

    #[test]
    fn speech_buffer_stop_clears_and_deactivates() {
        let supervisor = Arc::new(Supervisor::new());
        let buffer = SpeechBuffer::new(supervisor.clone());
        let rx = supervisor.active.subscribe();
        buffer.enqueue(vec![0.5; 16]);
        assert!(*rx.borrow());
        buffer.stop();
        assert!(!*rx.borrow());
        let mut out = vec![1.0_f32; 4];
        buffer.fill(&mut out);
        assert!(out.iter().all(|value| value.abs() < 1e-6));
    }

    #[test]
    fn speech_player_health_and_speaking_fall_back_on_stream_error() {
        let player = SpeechPlayer::spawn_with_factory(fake_factory()).unwrap();
        assert!(player.is_alive());
        assert_eq!(player.health(), SpeechHealth::Healthy);
        assert!(!player.is_speaking());

        player
            .speak(SpeechAudio::Pcm {
                sample_rate: 16_000,
                channels: 1,
                samples: vec![0.25; 1_600],
            })
            .unwrap();
        wait_until(|| player.is_speaking());

        // 模拟 CPAL error callback：健康降级、is_speaking 回落、线程退出。
        player.supervisor.fail("simulated cpal stream error");
        wait_until(|| !player.is_alive());
        assert!(matches!(player.health(), SpeechHealth::Degraded { .. }));
        assert!(player
            .last_error()
            .unwrap()
            .contains("simulated cpal stream error"));
        assert!(!player.is_speaking());
        // 降级后 speak 返回 Closed，不会永久等待。
        let error = player
            .speak(SpeechAudio::Pcm {
                sample_rate: 16_000,
                channels: 1,
                samples: vec![0.0; 16],
            })
            .unwrap_err();
        assert!(matches!(error, SpeechError::Closed));
        player.shutdown();
    }

    #[test]
    fn speech_player_restart_recovers_after_stream_error() {
        let mut player = SpeechPlayer::spawn_with_factory(fake_factory()).unwrap();
        player.supervisor.fail("simulated cpal stream error");
        wait_until(|| !player.is_alive());
        assert!(!player.health().is_healthy());

        player.restart().unwrap();
        assert!(player.is_alive());
        assert_eq!(player.health(), SpeechHealth::Healthy);
        assert!(!player.is_speaking());
        let health = player.subscribe_health();
        player.shutdown();
        assert_eq!(*health.borrow(), SpeechHealth::Closed);
    }

    #[test]
    fn speech_player_consecutive_speak_stop_shutdown_do_not_leak_or_block() {
        let player = SpeechPlayer::spawn_with_factory(fake_factory()).unwrap();
        for index in 0..5 {
            player
                .speak(SpeechAudio::Pcm {
                    sample_rate: 16_000,
                    channels: 1,
                    samples: vec![0.01 * index as f32; 320],
                })
                .unwrap();
            player.stop().unwrap();
        }
        let health = player.subscribe_health();
        let active = player.subscribe_active();
        player.shutdown();
        // shutdown 已 join 线程：健康落 Closed、说话状态回落，无残留线程/永久等待。
        assert_eq!(*health.borrow(), SpeechHealth::Closed);
        assert!(!*active.borrow());
    }

    #[test]
    fn speech_player_spawn_failure_reports_unavailable_without_leak() {
        let factory: Arc<super::OutputFactory> = Arc::new(|_buffer, _supervisor| {
            Err(SpeechError::Unavailable("no test device".to_owned()))
        });
        match SpeechPlayer::spawn_with_factory(factory) {
            Err(error) => assert!(matches!(error, SpeechError::Unavailable(_))),
            Ok(_) => panic!("expected spawn failure to be reported, got a live player"),
        }
    }

    #[test]
    fn speech_health_reason_and_flags() {
        let supervisor = Arc::new(Supervisor::new());
        assert_eq!(*supervisor.health.borrow(), SpeechHealth::Healthy);
        supervisor.fail("boom");
        assert_eq!(
            *supervisor.health.borrow(),
            SpeechHealth::Degraded {
                reason: "boom".to_owned()
            }
        );
        assert!(!supervisor.health.borrow().is_healthy());
        assert_eq!(supervisor.health.borrow().reason(), Some("boom"));
    }
}
