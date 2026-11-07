mod ffmpeg;

use async_trait::async_trait;
use tokio::sync::watch;
use uuid::Uuid;

use crate::domain::{EndBehavior, PlaybackSnapshot, SessionRef, SongKey, StreamSource};

pub(crate) use ffmpeg::{FfmpegConfig, FfmpegEngine};

#[derive(Clone, Debug)]
pub(crate) enum EngineCommand {
    Start {
        session_id: Uuid,
        generation: u64,
        song_key: SongKey,
        stream: StreamSource,
        end_behavior: EndBehavior,
        /// 起始播放位置（秒）：恢复播放时从该位置起播；None 表示从头播放。
        seek_seconds: Option<f64>,
    },
    RefreshStream {
        session_id: Uuid,
        generation: u64,
        stream: StreamSource,
    },
    Pause {
        session: SessionRef,
    },
    Resume {
        session: SessionRef,
    },
    Stop {
        session: SessionRef,
    },
    Seek {
        session: SessionRef,
        position_seconds: f64,
    },
    SetVolume {
        volume: u8,
    },
    /// 独立的输出「压低增益」（TTS 说话时压低音乐用），0–100，100=不压低。
    ///
    /// 与 `SetVolume` 的关键区别：本命令**只改实际输出增益**，不修改
    /// `PlaybackSnapshot.volume`、不发布快照、不落库，因此 `@状态`/状态栏
    /// 显示的音量保持不变（符合「不回显示」）。
    /// 目前由上层（task-49 TTS 编排）通过引擎命令通道构造；本 crate 内暂无调用点。
    #[allow(dead_code)]
    SetDuckGain {
        gain: u8,
    },
}

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum EngineError {
    #[error("playback engine is unavailable: {0}")]
    Unavailable(String),
    #[error("playback command was rejected: {0}")]
    Rejected(String),
    #[error("playback command timed out")]
    TimedOut,
}

#[async_trait]
pub(crate) trait AudioEngine: Send + Sync + 'static {
    async fn command(&self, command: EngineCommand) -> Result<(), EngineError>;
    fn subscribe(&self) -> watch::Receiver<PlaybackSnapshot>;
}
