use std::time::Duration;

use std::error::Error;
use std::fmt::{Display, Formatter};

use anyhow::{Result, anyhow};
use miliastra_playback::PlayableTrack;

use crate::features::playback::{MusicPlayerBackend, PlayerStatus};
use crate::runtime::player::TransportState;
use crate::runtime::player_io::{
    ControlDispatchOutcome, ObservationWaitOutcome, PlayerControl, PlayerObservationRevision,
    PlayerOperationReceiveError, PlayerRuntimeHandle,
};
use miliastra_kernel::identity::BusinessOperationIdAllocator;

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
        let identity = observation.fresh_identity();
        let current_track = identity.as_ref().and_then(|identity| {
            observation
                .track
                .as_ref()
                .filter(|track| track.track_ref.key == identity.key)
                .cloned()
        });
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
            current_track,
            current_uri: identity
                .map(|identity| identity.key.to_string())
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

    fn play(&self, track: &PlayableTrack) -> Result<String> {
        self.dispatch(PlayerControl::Play(track.clone()))
    }

    /// 歌曲级不可播放（无音源/需要VIP/无版权）——这类错误会触发自动换源尝试其他平台；
    /// 认证/限流/超时等平台级错误不在此列。
    fn is_track_unavailable_error(&self, error: &anyhow::Error) -> bool {
        matches!(
            error
                .downcast_ref::<PlayerControlFailure>()
                .and_then(|failure| failure.code.as_deref()),
            Some("track_unavailable" | "track_vip_required" | "track_no_copyright")
        )
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
            .map_err(|_| anyhow!("音量必须是 0-100 的数字"))?;
        if volume > 100 {
            return Err(anyhow!("音量必须是 0-100 的数字"));
        }
        self.dispatch(PlayerControl::SetVolume(volume))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::PlayerRuntimeBackend;
    use crate::features::playback::MusicPlayerBackend;
    use crate::runtime::player::{RawPlayerSample, TransportState};
    use crate::runtime::player_io::{
        ControlDispatch, ControlDispatchOutcome, ObservationWaitOutcome, PickedCandidate,
        PlayerControl, PlayerControlPort, PlayerObservationPort, PlayerObservationReadError,
        PlayerRuntime, PlayerRuntimeConfig, PlayerSearchError, PlayerSearchPort, SearchCandidate,
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

    struct ConstantObservationPort {
        sample: RawPlayerSample,
    }

    impl PlayerObservationPort for ConstantObservationPort {
        fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
            Ok(self.sample.clone())
        }
    }

    impl PlayerControlPort for TrackUnavailableControlPort {
        fn dispatch(&mut self, _control: &PlayerControl) -> ControlDispatch {
            ControlDispatch::immediate(ControlDispatchOutcome::rejected_with_code(
                "playback failure [track_unavailable]: no playable URL",
                "track_unavailable",
            ))
        }
    }

    struct AcknowledgingControlPort;

    impl PlayerControlPort for AcknowledgingControlPort {
        fn dispatch(&mut self, control: &PlayerControl) -> ControlDispatch {
            ControlDispatch::immediate(match control {
                PlayerControl::SetVolume(volume) => {
                    ControlDispatchOutcome::acknowledged(&format!("volume {volume}"))
                }
                _ => ControlDispatchOutcome::acknowledged("ok"),
            })
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
    fn runtime_backend_rejects_volume_outside_the_0_100_range() {
        let runtime = PlayerRuntime::start(
            FailingObservationPort,
            AcknowledgingControlPort,
            EmptySearchPort,
            PlayerRuntimeConfig::default(),
        )
        .expect("player runtime should start");
        let backend = PlayerRuntimeBackend::new(runtime.handle());

        for invalid in ["150", "101", "-1", "abc", "50.5", ""] {
            let error = backend
                .set_volume(invalid)
                .expect_err("out-of-range volume should be rejected");
            assert!(
                error.to_string().contains("0-100"),
                "unexpected message for {invalid:?}: {error}"
            );
        }

        assert!(backend.set_volume("0").is_ok());
        assert!(backend.set_volume("50").is_ok());
        assert!(backend.set_volume(" 75 ").is_ok());
        assert!(backend.set_volume("100").is_ok());
        runtime.shutdown().expect("player runtime should shut down");
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
        let track = crate::features::playback::test_track(
            "miliastra://track/qqmusic/track-1",
            "测试歌曲 - 测试歌手",
        );

        let error = backend
            .play(&track)
            .expect_err("the fake player should reject the track");

        assert!(backend.is_track_unavailable_error(&error));
        assert_eq!(
            error.to_string(),
            "播放器控制未确认: playback failure [track_unavailable]: no playable URL"
        );
        runtime.shutdown().expect("player runtime should shut down");
    }

    #[test]
    fn runtime_backend_derives_the_display_uri_from_the_stable_track_key() {
        let track = crate::features::playback::test_track(
            "miliastra://track/netease/track-2",
            "测试歌曲 - 测试歌手",
        );
        let config = PlayerRuntimeConfig {
            normal_observation_interval: Duration::from_millis(2),
            fast_observation_interval: Duration::from_millis(1),
            ..PlayerRuntimeConfig::default()
        };
        let runtime = PlayerRuntime::start(
            ConstantObservationPort {
                sample: RawPlayerSample::new(track.clone(), TransportState::Playing),
            },
            TrackUnavailableControlPort,
            EmptySearchPort,
            config,
        )
        .expect("player runtime should start");
        let handle = runtime.handle();
        let first = match handle.wait_for_observation_after(
            crate::runtime::player_io::PlayerObservationRevision::INITIAL,
            Duration::from_secs(1),
        ) {
            ObservationWaitOutcome::Advanced(observation) => observation,
            _ => panic!("first observation should arrive"),
        };
        match handle.wait_for_observation_after(first.revision(), Duration::from_secs(1)) {
            ObservationWaitOutcome::Advanced(_) => {}
            _ => panic!("stable observation should arrive"),
        }

        let status = PlayerRuntimeBackend::new(handle)
            .status()
            .expect("player status");

        assert_eq!(status.current_track, Some(track));
        assert_eq!(status.current_uri, "miliastra://track/netease/track-2");
        runtime.shutdown().expect("player runtime should shut down");
    }
}
