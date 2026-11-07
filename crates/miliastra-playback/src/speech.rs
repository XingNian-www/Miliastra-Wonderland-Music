//! 独立 TTS 语音播放器（播放 fish.audio 返回的 WAV/PCM）。
//!
//! 设计目标（见 `technical-docs/team-reports/tts-ffmpeg-fish-audio-feasibility.md` 方案 A）：
//! - **与音乐播放完全隔离**：自己的线程（`tts-runtime`）+ 自己的 CPAL 输出流，独立 PCM
//!   缓冲；不占用、不暂停、不替换音乐 session，也**不触碰**音乐的 `PlaybackSnapshot`。
//! - 音乐侧压低由引擎独立的 `EngineCommand::SetDuckGain` 完成（本模块只负责播语音）。
//! - 失败可降级：无音频设备 / 解码失败时返回 `SpeechError`，调用方静默跳过即可。
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

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use thiserror::Error;
use tokio::sync::watch;

/// 语音播放错误。
#[derive(Debug, Clone, Error)]
pub enum SpeechError {
    /// 无可用音频输出设备 / 打开输出流失败。
    #[error("speech audio device is unavailable: {0}")]
    Unavailable(String),
    /// 音频数据无法解析（目前支持 WAV PCM16 / IEEE-F32）。
    #[error("speech audio format is unsupported: {0}")]
    UnsupportedFormat(String),
    /// 播放器线程已关闭，无法继续投递。
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
    pub fn decode(self) -> Result<PcmAudio, SpeechError> {
        match self {
            SpeechAudio::Pcm {
                sample_rate,
                channels,
                samples,
            } => {
                if sample_rate == 0 || channels == 0 {
                    return Err(SpeechError::UnsupportedFormat(
                        "sample_rate/channels must be non-zero".to_owned(),
                    ));
                }
                if samples.len() % usize::from(channels) != 0 {
                    return Err(SpeechError::UnsupportedFormat(
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
        return Err(SpeechError::UnsupportedFormat(
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
        let body_end = body_start.saturating_add(size);
        if body_end > bytes.len() {
            break;
        }
        let body = &bytes[body_start..body_end];
        if id == b"fmt " {
            if body.len() < 16 {
                return Err(SpeechError::UnsupportedFormat(
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
        SpeechError::UnsupportedFormat("wav is missing fmt chunk".to_owned())
    })?;
    let data = data.ok_or_else(|| SpeechError::UnsupportedFormat("wav is missing data chunk".to_owned()))?;
    if channels == 0 || sample_rate == 0 {
        return Err(SpeechError::UnsupportedFormat(
            "wav has invalid channels/sample_rate".to_owned(),
        ));
    }
    let samples = match (audio_format, bits) {
        (1, 16) => data
            .chunks_exact(2)
            .map(|pair| f32::from(i16::from_le_bytes([pair[0], pair[1]])) / 32768.0)
            .collect::<Vec<_>>(),
        (3, 32) => data
            .chunks_exact(4)
            .map(|chunk| {
                f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
            })
            .collect::<Vec<_>>(),
        (format, bits) => {
            return Err(SpeechError::UnsupportedFormat(format!(
                "wav audio_format={format}, bits={bits} not supported (need PCM16 or F32)"
            )))
        }
    };
    if samples.len() % usize::from(channels) != 0 {
        return Err(SpeechError::UnsupportedFormat(
            "wav data length is not a multiple of channels".to_owned(),
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

/// 语音 PCM 缓冲（独立于音乐 `PcmBuffer`）。
struct SpeechBuffer {
    samples: Mutex<VecDeque<f32>>,
    playing: AtomicBool,
    /// 说话状态广播（true=正在输出语音），供上层做 duck 编排。
    active: watch::Sender<bool>,
}

impl SpeechBuffer {
    fn new(active: watch::Sender<bool>) -> Self {
        Self {
            samples: Mutex::new(VecDeque::new()),
            playing: AtomicBool::new(false),
            active,
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
        self.active.send_replace(true);
    }

    fn stop(&self) {
        {
            let mut queue = self.samples.lock().unwrap_or_else(|error| error.into_inner());
            queue.clear();
        }
        if self.playing.swap(false, Ordering::AcqRel) {
            self.active.send_replace(false);
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
            self.active.send_replace(false);
        }
    }
}

enum SpeechCommand {
    Speak(PcmAudio),
    Stop,
    Shutdown,
}

/// 独立语音播放器句柄。内部持有 `tts-runtime` 线程与独立 CPAL 输出流。
pub struct SpeechPlayer {
    tx: Mutex<mpsc::Sender<SpeechCommand>>,
    active: watch::Receiver<bool>,
    thread: Option<JoinHandle<()>>,
}

impl SpeechPlayer {
    /// 创建播放器：在专用线程打开默认输出设备并常驻 CPAL 流。
    ///
    /// 无可用音频设备时返回 `SpeechError::Unavailable`（调用方静默降级即可）。
    pub fn spawn() -> Result<Self, SpeechError> {
        let (command_tx, command_rx) = mpsc::channel::<SpeechCommand>();
        let (active_tx, active_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(), SpeechError>>(1);

        let thread = thread::Builder::new()
            .name("tts-runtime".to_owned())
            .spawn(move || run_speech_thread(command_rx, active_tx, ready_tx))
            .map_err(|error| SpeechError::Unavailable(format!("spawn tts-runtime: {error}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx: Mutex::new(command_tx),
                active: active_rx,
                thread: Some(thread),
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

    /// 异步投递一段音频：立即返回，实际播放由 `tts-runtime` 线程完成。
    pub fn speak(&self, audio: SpeechAudio) -> Result<(), SpeechError> {
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

    /// 当前是否正在输出语音。
    pub fn is_speaking(&self) -> bool {
        *self.active.borrow()
    }

    /// 订阅说话状态（true/正在说），供上层做 duck 的淡入淡出编排。
    pub fn subscribe_active(&self) -> watch::Receiver<bool> {
        self.active.clone()
    }

    /// 关闭播放器：停止语音并等待线程退出。
    pub fn shutdown(mut self) {
        {
            let sender = self.tx.lock().unwrap_or_else(|error| error.into_inner());
            let _ = sender.send(SpeechCommand::Shutdown);
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SpeechPlayer {
    fn drop(&mut self) {
        if let Ok(sender) = self.tx.lock() {
            let _ = sender.send(SpeechCommand::Shutdown);
        }
        if let Some(handle) = self.thread.take() {
            // 线程可能在等待 CPAL 回调，join 一般很快；不 join 也不会泄漏 Stream。
            let _ = handle.join();
        }
    }
}

fn run_speech_thread(
    command_rx: mpsc::Receiver<SpeechCommand>,
    active_tx: watch::Sender<bool>,
    ready_tx: mpsc::SyncSender<Result<(), SpeechError>>,
) {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        let _ = ready_tx.send(Err(SpeechError::Unavailable(
            "no default audio output device is available".to_owned(),
        )));
        return;
    };
    let supported = match device.default_output_config() {
        Ok(config) => config,
        Err(error) => {
            let _ = ready_tx.send(Err(SpeechError::Unavailable(format!(
                "query speech output device: {error}"
            ))));
            return;
        }
    };
    let sample_format = supported.sample_format();
    let stream_config: StreamConfig = supported.into();
    if stream_config.channels == 0 || stream_config.sample_rate == 0 {
        let _ = ready_tx.send(Err(SpeechError::Unavailable(
            "speech output device returned an invalid format".to_owned(),
        )));
        return;
    }
    let target_rate = stream_config.sample_rate;
    let target_channels = stream_config.channels;
    let buffer = Arc::new(SpeechBuffer::new(active_tx.clone()));

    let stream = match build_speech_stream(&device, stream_config, sample_format, buffer.clone()) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    if let Err(error) = stream.play() {
        let _ = ready_tx.send(Err(SpeechError::Unavailable(format!(
            "start speech output stream: {error}"
        ))));
        return;
    }
    let _ = ready_tx.send(Ok(()));

    // 常驻：保持 stream 存活，串行处理投递的语音。
    for command in command_rx {
        match command {
            SpeechCommand::Speak(pcm) => {
                let prepared = prepare_for_device(&pcm, target_rate, target_channels);
                buffer.enqueue(prepared);
            }
            SpeechCommand::Stop => buffer.stop(),
            SpeechCommand::Shutdown => break,
        }
    }
    buffer.stop();
    drop(stream);
}

fn build_speech_stream(
    device: &cpal::Device,
    config: StreamConfig,
    sample_format: SampleFormat,
    buffer: Arc<SpeechBuffer>,
) -> Result<Stream, SpeechError> {
    macro_rules! build {
        ($sample:ty) => {
            build_typed_speech_stream::<$sample>(device, config, buffer)
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
) -> Result<Stream, SpeechError>
where
    T: SizedSample + FromSample<f32>,
{
    device
        .build_output_stream(
            config,
            move |output: &mut [T], _| buffer.fill(output),
            move |_| {
                // 语音设备错误只影响语音，不能让音乐停；这里静默（上层可据 active 观察）。
            },
            None,
        )
        .map_err(|error| SpeechError::Unavailable(format!("create speech output stream: {error}")))
}

#[cfg(test)]
mod tests {
    use super::{PcmAudio, SpeechAudio, SpeechBuffer, decode_wav, prepare_for_device};
    use tokio::sync::watch;

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

    #[test]
    fn wav_rejects_non_riff() {
        assert!(decode_wav(b"not a wav").is_err());
    }

    #[test]
    fn pcm_passthrough_validates_shape() {
        let audio = SpeechAudio::Pcm {
            sample_rate: 8_000,
            channels: 2,
            samples: vec![0.0; 4],
        };
        let pcm = audio.decode().unwrap();
        assert_eq!(pcm.channels, 2);
        // 样本数不是声道整数倍 → 报错
        let bad = SpeechAudio::Pcm {
            sample_rate: 8_000,
            channels: 2,
            samples: vec![0.0; 3],
        };
        assert!(bad.decode().is_err());
    }

    #[test]
    fn prepare_duplicates_mono_to_stereo_and_resamples() {
        let input = PcmAudio {
            sample_rate: 8_000,
            channels: 1,
            samples: vec![0.25, 0.5, -0.25, -0.5],
        };
        let out = prepare_for_device(&input, 16_000, 2);
        // 24000 帧/秒不变下：4 帧 @8k → 8 帧 @16k，双声道 → 16 个样本
        assert_eq!(out.len(), 8 * 2);
        // 每帧两个声道相同（mono→stereo 复制）
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
        let (tx, rx) = watch::channel(false);
        let buffer = SpeechBuffer::new(tx);
        assert!(!*rx.borrow());
        buffer.enqueue(vec![0.1, 0.2, 0.3]);
        assert!(*rx.borrow());
        let mut out = vec![0.0_f32; 8];
        buffer.fill(&mut out);
        // 3 个样本被消费，缓冲排空 → active 回 false
        assert!((out[0] - 0.1).abs() < 1e-6);
        assert!((out[2] - 0.3).abs() < 1e-6);
        assert!((out[3]).abs() < 1e-6);
        assert!(!*rx.borrow(), "缓冲排空后应回到未说话状态");
    }

    #[test]
    fn speech_buffer_stop_clears_and_deactivates() {
        let (tx, rx) = watch::channel(false);
        let buffer = SpeechBuffer::new(tx);
        buffer.enqueue(vec![0.5; 16]);
        assert!(*rx.borrow());
        buffer.stop();
        assert!(!*rx.borrow());
        let mut out = vec![1.0_f32; 4];
        buffer.fill(&mut out);
        assert!(out.iter().all(|value| value.abs() < 1e-6));
    }
}
