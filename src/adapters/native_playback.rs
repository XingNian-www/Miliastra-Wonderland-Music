use std::time::Duration;

use miliastra_playback::{
    EndBehavior, EndCause, EngineState, PlaybackEligibility, PlaybackError, PlaybackHandle,
    ProviderId, SearchCandidate, SearchQuery,
};

use crate::features::song_request::PickedCandidate;
use crate::runtime::player::{PlayerRuntimeMetadata, RawPlayerSample, TransportState};
use crate::runtime::player_io::{
    ControlDispatch, ControlDispatchOutcome, PlayerControl, PlayerControlPort,
    PlayerObservationPort, PlayerObservationReadError, PlayerSearchError, PlayerSearchPort,
};

#[derive(Clone)]
pub(crate) struct NativePlaybackAdapter {
    playback: PlaybackHandle,
}

impl NativePlaybackAdapter {
    pub(crate) fn new(playback: PlaybackHandle) -> Self {
        Self { playback }
    }
}

impl PlayerObservationPort for NativePlaybackAdapter {
    fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
        let snapshot = self
            .playback
            .snapshot()
            .map_err(|error| PlayerObservationReadError::new(error.to_string()))?;
        let track = snapshot.track.clone();
        let metadata = track.as_ref().map(|track| &track.metadata);
        let failure = snapshot.failure.as_ref();
        Ok(RawPlayerSample {
            track: track.clone(),
            transport: transport_state(snapshot.state),
            title: metadata.map(|metadata| metadata.title.clone()),
            artist: metadata.map(|metadata| metadata.artists.join(" / ")),
            album_name: metadata.and_then(|metadata| metadata.album.clone()),
            lyric_line_text: snapshot.lyric_line_text,
            progress: snapshot.position_seconds.map(Duration::from_secs_f64),
            duration: snapshot
                .duration_seconds
                .map(Duration::from_secs_f64)
                .or_else(|| {
                    metadata
                        .and_then(|metadata| metadata.duration_ms)
                        .map(Duration::from_millis)
                }),
            playback_rate: Some(1.0),
            volume: Some(i64::from(snapshot.volume)),
            runtime: PlayerRuntimeMetadata {
                runtime_identity: snapshot.runtime_identity,
                session_id: snapshot
                    .session_id
                    .map(|session_id| session_id.to_string())
                    .unwrap_or_default(),
                generation: snapshot.generation,
                end_behavior: snapshot
                    .end_behavior
                    .map(end_behavior_name)
                    .unwrap_or_default()
                    .to_owned(),
                last_end_cause: snapshot
                    .last_end_cause
                    .map(end_cause_name)
                    .unwrap_or_default()
                    .to_owned(),
                failure_code: failure
                    .map(|failure| failure.code.clone())
                    .unwrap_or_default(),
                failure_message: failure
                    .map(|failure| failure.message.clone())
                    .unwrap_or_default(),
                failure_retryable: failure.is_some_and(|failure| failure.retryable),
                failure_provider: failure
                    .and_then(|failure| failure.provider.clone())
                    .unwrap_or_default(),
                failure_retry_after_ms: failure
                    .and_then(|failure| failure.retry_after_ms)
                    .unwrap_or_default(),
            },
        })
    }
}

impl PlayerControlPort for NativePlaybackAdapter {
    fn dispatch(&mut self, control: &PlayerControl) -> ControlDispatch {
        if let PlayerControl::Play(track) = control {
            return match self.playback.play(track.clone()) {
                Ok(operation) => {
                    let cancellation = self.playback.clone();
                    ControlDispatch::deferred_with_cancel(
                        move || match operation.wait() {
                            Ok(()) => ControlDispatchOutcome::acknowledged("ok"),
                            Err(error) => dispatch_error(error),
                        },
                        move || {
                            let _ = cancellation.stop();
                        },
                    )
                }
                Err(error) => ControlDispatch::immediate(dispatch_error(error)),
            };
        }
        let result = match control {
            PlayerControl::Pause => self.playback.pause(),
            PlayerControl::Resume => self.playback.resume(),
            PlayerControl::SetVolume(volume) => self.playback.set_volume(*volume),
            PlayerControl::Next | PlayerControl::Previous => {
                return ControlDispatch::immediate(ControlDispatchOutcome::not_sent(
                    "native playback queue navigation is owned by the application",
                ));
            }
            PlayerControl::Play(_) => unreachable!("play control handled above"),
        };
        ControlDispatch::immediate(match result {
            Ok(()) => ControlDispatchOutcome::acknowledged("ok"),
            Err(error) => dispatch_error(error),
        })
    }
}

impl PlayerSearchPort for NativePlaybackAdapter {
    fn search_text(&mut self, keyword: &str, source: &str) -> Result<String, PlayerSearchError> {
        self.search_candidates(keyword, source)
            .map(|candidates| format_candidates(&candidates))
    }

    fn search_candidates(
        &mut self,
        keyword: &str,
        source: &str,
    ) -> Result<Vec<SearchCandidate>, PlayerSearchError> {
        let providers = providers(source)?;
        self.playback
            .search(SearchQuery {
                keyword: keyword.to_owned(),
                providers,
                limit: 10,
            })
            .map_err(|error| PlayerSearchError::new(error.to_string()))
    }

    fn search_and_pick(
        &mut self,
        keyword: &str,
        source: &str,
        prefer_accompaniment: bool,
    ) -> Result<Option<PickedCandidate>, PlayerSearchError> {
        let candidates = self.search_candidates(keyword, source)?;
        let formatted = format_candidates(&candidates);
        let playable = candidates
            .iter()
            .filter(|candidate| candidate.eligibility != PlaybackEligibility::Ineligible)
            .cloned()
            .collect::<Vec<_>>();
        let preferred = if prefer_accompaniment {
            playable
                .iter()
                .find(|candidate| is_accompaniment(&candidate.text))
                .cloned()
                .or_else(|| SearchCandidate::select_preferred_equivalent(&playable))
        } else {
            SearchCandidate::select_preferred_equivalent(&playable)
        };
        Ok(preferred
            .map(|candidate| PickedCandidate::with_snapshot(candidate, candidates, formatted)))
    }
}

fn providers(source: &str) -> Result<Vec<ProviderId>, PlayerSearchError> {
    let source = source.trim();
    if source.is_empty() {
        return Ok(Vec::new());
    }
    let mut providers = Vec::new();
    for value in source.split(',').map(str::trim) {
        if value.is_empty() {
            return Err(PlayerSearchError::new(
                "provider list contains an empty value",
            ));
        }
        let provider = value
            .parse::<ProviderId>()
            .map_err(|_| PlayerSearchError::new(format!("unknown provider: {value}")))?;
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    Ok(providers)
}

fn format_candidates(candidates: &[SearchCandidate]) -> String {
    candidates
        .iter()
        .enumerate()
        .map(|(index, candidate)| format!("{}. {}", index + 1, candidate.text))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_accompaniment(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("伴奏") || text.contains("instrumental") || text.contains("karaoke")
}

fn transport_state(state: EngineState) -> Option<TransportState> {
    match state {
        EngineState::Playing => Some(TransportState::Playing),
        EngineState::Paused => Some(TransportState::Paused),
        EngineState::Idle | EngineState::Stopped | EngineState::Failed => {
            Some(TransportState::Stopped)
        }
        EngineState::Resolving | EngineState::Loading => None,
    }
}

fn end_behavior_name(end_behavior: EndBehavior) -> &'static str {
    match end_behavior {
        EndBehavior::Stop => "stop",
        EndBehavior::RepeatCurrent => "repeat_current",
        EndBehavior::NotifyController => "notify_controller",
    }
}

fn end_cause_name(end_cause: EndCause) -> &'static str {
    match end_cause {
        EndCause::NaturalEnd => "natural_end",
        EndCause::Replaced => "replaced",
        EndCause::StoppedByController => "stopped_by_controller",
        EndCause::StreamRejected => "stream_rejected",
        EndCause::DecodeFailure => "decode_failure",
        EndCause::RecoveryPositionUnknown => "recovery_position_unknown",
        EndCause::EngineExited => "engine_exited",
    }
}

fn dispatch_error(error: PlaybackError) -> ControlDispatchOutcome {
    let code = error.code();
    match error {
        PlaybackError::Busy | PlaybackError::RuntimeStopped => {
            ControlDispatchOutcome::not_sent(error.to_string())
        }
        _ => ControlDispatchOutcome::rejected_with_code(error.to_string(), code),
    }
}

#[cfg(test)]
mod tests {
    use super::providers;
    use miliastra_playback::ProviderId;

    #[test]
    fn provider_list_accepts_the_ai_multi_platform_source() {
        assert_eq!(
            providers("qqmusic,netease,bilibili").unwrap(),
            vec![
                ProviderId::QqMusic,
                ProviderId::Netease,
                ProviderId::Bilibili,
            ]
        );
    }

    #[test]
    fn provider_list_deduplicates_and_rejects_unknown_values() {
        assert_eq!(
            providers(" qqmusic,netease,qqmusic ").unwrap(),
            vec![ProviderId::QqMusic, ProviderId::Netease]
        );
        assert!(providers("qqmusic,unknown").is_err());
        assert!(providers("qqmusic,,netease").is_err());
    }
}
