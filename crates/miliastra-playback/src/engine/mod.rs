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
    #[cfg(test)]
    Seek {
        session: SessionRef,
        position_seconds: f64,
    },
    SetVolume {
        volume: u8,
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
