use std::sync::Arc;

use crate::features::playback::{PlaybackService, QueueRemoveOutcome};

use super::{BusinessRuntimeError, BusinessStateSink, PlaybackRuntimeMessage};

/// 持有播放服务并在 actor 线程内处理全部播放消息。
pub(super) struct PlaybackRuntimeState {
    service: Option<PlaybackService>,
    state_sink: Option<Arc<dyn BusinessStateSink>>,
}

impl PlaybackRuntimeState {
    pub(super) fn new(
        service: Option<PlaybackService>,
        state_sink: Option<Arc<dyn BusinessStateSink>>,
    ) -> Self {
        let state = Self {
            service,
            state_sink,
        };
        state.publish_queue();
        state
    }

    pub(super) fn handle(&mut self, message: PlaybackRuntimeMessage) {
        match message {
            PlaybackRuntimeMessage::PushQueue { item, response } => {
                let result = self.service_mut().and_then(|service| {
                    service.push_queue(item).map_err(playback_operation_failed)
                });
                if result.is_ok() {
                    self.publish_queue();
                }
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::RemoveQueue { removal, response } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .remove_queue(removal)
                        .map_err(playback_operation_failed)
                });
                if matches!(result, Ok(QueueRemoveOutcome::Removed { .. })) {
                    self.publish_queue();
                }
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::RemoveQueueIndexes { indexes, response } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .remove_queue_indexes(indexes)
                        .map_err(playback_operation_failed)
                });
                if result.as_ref().is_ok_and(|removed| !removed.is_empty()) {
                    self.publish_queue();
                }
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::ClearQueue(response) => {
                let result = self
                    .service_mut()
                    .and_then(|service| service.clear_queue().map_err(playback_operation_failed));
                if result.as_ref().is_ok_and(|count| *count > 0) {
                    self.publish_queue();
                }
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::QueueContains { item, response } => {
                let result = self
                    .service
                    .as_ref()
                    .map(|service| service.queue_contains(&item))
                    .ok_or(BusinessRuntimeError::RuntimeStopped);
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::QueueSnapshot(response) => {
                let result = self
                    .service
                    .as_ref()
                    .map(PlaybackService::queue_snapshot)
                    .ok_or(BusinessRuntimeError::RuntimeStopped);
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::PlaybackPoolAvailable(response) => {
                let result = self
                    .service
                    .as_ref()
                    .and_then(|service| service.playback_pool_available().ok())
                    .ok_or(BusinessRuntimeError::RuntimeStopped);
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::RecordPlaybackPoolTrack { track, response } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .record_playback_pool_track(track)
                        .map_err(playback_operation_failed)
                });
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::PickPlaybackPoolTrack { exclude, response } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .pick_playback_pool_track(exclude.as_ref())
                        .map_err(playback_operation_failed)
                });
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::StateSnapshot(response) => {
                let result = self
                    .service
                    .as_ref()
                    .map(PlaybackService::playback_state_snapshot)
                    .ok_or(BusinessRuntimeError::RuntimeStopped);
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::UpdatePlaybackState { update, response } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .apply_playback_state_update(update)
                        .map_err(playback_operation_failed)
                });
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::CheckSongDedup {
                candidate,
                response,
            } => {
                let result = self
                    .service
                    .as_ref()
                    .map(|service| service.song_dedup_limited(&candidate))
                    .ok_or(BusinessRuntimeError::RuntimeStopped);
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::RecordSongDedup {
                candidate,
                response,
            } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .record_song_dedup(candidate)
                        .map_err(playback_operation_failed)
                });
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::ObserveExternalPlayback {
                identity,
                now,
                protect_after,
                response,
            } => {
                let result = self
                    .service
                    .as_mut()
                    .map(|service| service.observe_external_playback(&identity, now, protect_after))
                    .ok_or(BusinessRuntimeError::RuntimeStopped);
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::ClearExternalPlaybackTracker(response) => {
                let result = self
                    .service
                    .as_mut()
                    .map(PlaybackService::clear_external_playback_tracker)
                    .ok_or(BusinessRuntimeError::RuntimeStopped);
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::ReconcilePlayerSession { binding, response } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .reconcile_player_session(binding)
                        .map_err(playback_operation_failed)
                });
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::ClaimTerminalOutcome {
                request_id,
                outcome,
                handled_at_ms,
                response,
            } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .claim_terminal_outcome(request_id, outcome, handled_at_ms)
                        .map_err(playback_operation_failed)
                });
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::RecordPlaybackAttempt {
                provider,
                locator,
                started_at_ms,
                result: attempt_result,
                response,
            } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .record_playback_attempt(provider, locator, started_at_ms, attempt_result)
                        .map_err(playback_operation_failed)
                });
                let _ = response.send(result);
            }
            PlaybackRuntimeMessage::RecordControlOperation {
                operation,
                requested_at_ms,
                completed,
                response,
            } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .record_control_operation(operation, requested_at_ms, completed)
                        .map_err(playback_operation_failed)
                });
                let _ = response.send(result);
            }
        }
    }

    fn service_mut(&mut self) -> Result<&mut PlaybackService, BusinessRuntimeError> {
        self.service
            .as_mut()
            .ok_or(BusinessRuntimeError::RuntimeStopped)
    }

    fn publish_queue(&self) {
        if let (Some(state_sink), Some(service)) = (&self.state_sink, &self.service) {
            state_sink.publish_playback_queue(service.queue_snapshot());
        }
    }
}

fn playback_operation_failed(error: anyhow::Error) -> BusinessRuntimeError {
    BusinessRuntimeError::PlaybackOperationFailed(format!("{error:#}"))
}
