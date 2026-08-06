use std::time::Duration;

use std::error::Error;
use std::fmt::{Display, Formatter};

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

#[derive(Debug)]
struct PlayerControlFailure {
    code: Option<String>,
    reason: String,
}

impl Display for PlayerControlFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "播放器控制未确认: {}", self.reason)
    }
}

impl Error for PlayerControlFailure {}

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
            ControlDispatchOutcome::Rejected { reason, code } => {
                Err(anyhow::Error::new(PlayerControlFailure { code, reason }))
            }
            ControlDispatchOutcome::NotSent { reason }
            | ControlDispatchOutcome::OutcomeUnknown { reason } => {
                Err(anyhow::Error::new(PlayerControlFailure {
                    code: None,
                    reason,
                }))
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
            requester: String::new(),
            runtime_identity: observation.runtime.runtime_identity.clone(),
            session_id: observation.runtime.session_id.clone(),
            generation: observation.runtime.generation,
            end_behavior: observation.runtime.end_behavior.clone(),
            last_end_cause: observation.runtime.last_end_cause.clone(),
            failure_code: observation.runtime.failure_code.clone(),
            failure_message: observation.runtime.failure_message.clone(),
            failure_retryable: observation.runtime.failure_retryable,
            failure_provider: observation.runtime.failure_provider.clone(),
            failure_retry_after_ms: observation.runtime.failure_retry_after_ms,
        })
    }

    fn play_uri(&self, uri: &str) -> Result<String> {
        self.dispatch(PlayerControl::PlayUri(uri.to_string()))
    }

    fn is_track_unavailable_error(&self, error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<PlayerControlFailure>()
            .and_then(|failure| failure.code.as_deref())
            == Some("track_unavailable")
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

#[cfg(test)]
mod tests {
    use super::PlayerRuntimeBackend;
    use crate::features::playback::MusicPlayerBackend;
    use crate::runtime::player::RawPlayerSample;
    use crate::runtime::player_io::{
        ControlDispatchOutcome, PickedCandidate, PlayerControl, PlayerControlPort,
        PlayerObservationPort, PlayerObservationReadError, PlayerRuntime, PlayerRuntimeConfig,
        PlayerSearchError, PlayerSearchPort, SearchCandidate,
    };

    struct FailingObservationPort;

    impl PlayerObservationPort for FailingObservationPort {
        fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
            Err(PlayerObservationReadError::new(
                "test observation unavailable",
            ))
        }
    }

    struct TrackUnavailableControlPort;

    impl PlayerControlPort for TrackUnavailableControlPort {
        fn dispatch(&mut self, _control: &PlayerControl) -> ControlDispatchOutcome {
            ControlDispatchOutcome::rejected_with_code(
                "playerd failure [track_unavailable]: no playable URL",
                "track_unavailable",
            )
        }
    }

    struct EmptySearchPort;

    impl PlayerSearchPort for EmptySearchPort {
        fn search_text(
            &mut self,
            _keyword: &str,
            _source: &str,
        ) -> Result<String, PlayerSearchError> {
            Ok(String::new())
        }

        fn search_candidates(
            &mut self,
            _keyword: &str,
            _source: &str,
        ) -> Result<Vec<SearchCandidate>, PlayerSearchError> {
            Ok(Vec::new())
        }

        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> Result<Option<PickedCandidate>, PlayerSearchError> {
            Ok(None)
        }
    }

    #[test]
    fn runtime_backend_preserves_track_unavailable_code() {
        let runtime = PlayerRuntime::start(
            FailingObservationPort,
            TrackUnavailableControlPort,
            EmptySearchPort,
            PlayerRuntimeConfig::default(),
        )
        .expect("player runtime should start");
        let backend = PlayerRuntimeBackend::new(runtime.handle());

        let error = backend
            .play_uri("miliastra://track/qqmusic/track-1")
            .expect_err("the fake playerd should reject the track");

        assert!(backend.is_track_unavailable_error(&error));
        assert_eq!(
            error.to_string(),
            "播放器控制未确认: playerd failure [track_unavailable]: no playable URL"
        );
        runtime.shutdown().expect("player runtime should shut down");
    }
}
