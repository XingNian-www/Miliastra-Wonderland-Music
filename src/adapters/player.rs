use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::features::playback::{MusicPlayerBackend, PlayerStatus};
use crate::runtime::identity::BusinessOperationIdAllocator;
use crate::runtime::player::TransportState;
use crate::runtime::player_io::{
    ControlDispatchOutcome, ObservationWaitOutcome, PlayerControl, PlayerObservationRevision,
    PlayerOperationReceiveError, PlayerRuntimeHandle,
};

#[derive(Clone)]
pub(crate) struct PlayerRuntimeBackend {
    runtime: PlayerRuntimeHandle,
    operation_ids: BusinessOperationIdAllocator,
}

impl PlayerRuntimeBackend {
    pub(crate) fn new(runtime: PlayerRuntimeHandle) -> Self {
        Self {
            runtime,
            operation_ids: BusinessOperationIdAllocator::new(),
        }
    }

    fn dispatch(&self, control: PlayerControl) -> Result<String> {
        let operation_id = self
            .operation_ids
            .allocate()
            .map_err(|error| anyhow!("播放器控制操作编号耗尽: {error}"))?;
        let operation = self
            .runtime
            .submit_control(operation_id, control)
            .map_err(|error| anyhow!("提交播放器控制操作失败: {error}"))?;
        let result = operation
            .wait()
            .map_err(|error: PlayerOperationReceiveError| {
                anyhow!("等待播放器控制结果失败: {error}")
            })?;
        match result.outcome {
            ControlDispatchOutcome::Acknowledged { response } => Ok(response),
            ControlDispatchOutcome::Rejected { reason }
            | ControlDispatchOutcome::NotSent { reason }
            | ControlDispatchOutcome::OutcomeUnknown { reason } => {
                Err(anyhow!("播放器控制未确认: {reason}"))
            }
        }
    }
}

impl MusicPlayerBackend for PlayerRuntimeBackend {
    fn status(&self) -> Result<PlayerStatus> {
        let observation = self.runtime.latest_observation().or_else(|| {
            match self.runtime.wait_for_observation_after(
                PlayerObservationRevision::INITIAL,
                Duration::from_secs(1),
            ) {
                ObservationWaitOutcome::Advanced(observation) => Some(observation),
                ObservationWaitOutcome::TimedOut | ObservationWaitOutcome::RuntimeStopped => None,
            }
        });
        let observation = observation.ok_or_else(|| anyhow!("播放器运行时尚未发布观测"))?;
        let observation = observation.observation();
        let transport = observation
            .fresh_transport()
            .or(observation.transport)
            .map(|transport| match transport {
                TransportState::Playing => "playing",
                TransportState::Paused => "paused",
                TransportState::Stopped => "stopped",
            })
            .unwrap_or("unknown");
        Ok(PlayerStatus {
            status: transport.to_string(),
            current_uri: observation
                .fresh_identity()
                .map(|identity| identity.uri)
                .unwrap_or_default(),
            name: observation.title.clone().unwrap_or_default(),
            singer: observation.artist.clone().unwrap_or_default(),
            album_name: observation.album_name.clone().unwrap_or_default(),
            lyric_line_text: observation.lyric_line_text.clone().unwrap_or_default(),
            duration: observation
                .duration
                .map_or(0.0, |duration| duration.as_secs_f64()),
            progress: observation
                .progress
                .map_or(0.0, |progress| progress.as_secs_f64()),
            playback_rate: observation.playback_rate.unwrap_or(1.0),
            volume: observation.volume.unwrap_or_default(),
        })
    }

    fn play_uri(&self, uri: &str) -> Result<String> {
        self.dispatch(PlayerControl::PlayUri(uri.to_string()))
    }

    fn pause(&self) -> Result<String> {
        self.dispatch(PlayerControl::Pause)
    }

    fn resume(&self) -> Result<String> {
        self.dispatch(PlayerControl::Resume)
    }

    fn next(&self) -> Result<String> {
        self.dispatch(PlayerControl::Next)
    }

    fn previous(&self) -> Result<String> {
        self.dispatch(PlayerControl::Previous)
    }

    fn set_volume(&self, volume: &str) -> Result<String> {
        let volume = volume
            .trim()
            .parse::<u8>()
            .map_err(|_| anyhow!("播放器音量不是有效的 0-100 数字"))?;
        self.dispatch(PlayerControl::SetVolume(volume))
    }
}
