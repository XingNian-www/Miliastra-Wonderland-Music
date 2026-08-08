use std::sync::Arc;

use crate::features::hall::HallStateService;

use super::{BusinessRuntimeError, BusinessStateSink, HallRuntimeMessage};

/// 持有大厅服务并在 actor 线程内处理全部大厅消息。
pub(super) struct HallRuntimeState {
    service: Option<HallStateService>,
    state_sink: Option<Arc<dyn BusinessStateSink>>,
}

impl HallRuntimeState {
    pub(super) fn new(
        service: Option<HallStateService>,
        state_sink: Option<Arc<dyn BusinessStateSink>>,
    ) -> Self {
        let state = Self {
            service,
            state_sink,
        };
        state.publish();
        state
    }

    pub(super) fn handle(&mut self, message: HallRuntimeMessage) {
        match message {
            HallRuntimeMessage::PatchState { patch, response } => {
                let result = self
                    .service_mut()
                    .and_then(|service| service.patch(patch).map_err(hall_operation_failed));
                if result.is_ok() {
                    self.publish();
                }
                let _ = response.send(result);
            }
            HallRuntimeMessage::StateSnapshot(response) => {
                let result = self
                    .service
                    .as_ref()
                    .map(HallStateService::snapshot)
                    .ok_or(BusinessRuntimeError::RuntimeStopped);
                let _ = response.send(result);
            }
            HallRuntimeMessage::UpdateRemainingMinutes { minutes, response } => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .update_remaining_minutes(minutes)
                        .map_err(hall_operation_failed)
                });
                if result.is_ok() {
                    self.publish();
                }
                let _ = response.send(result);
            }
            HallRuntimeMessage::ClearRemainingMinutes(response) => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .clear_remaining_minutes()
                        .map_err(hall_operation_failed)
                });
                if result.is_ok() {
                    self.publish();
                }
                let _ = response.send(result);
            }
            HallRuntimeMessage::ClearCountdownCache(response) => {
                let result = self.service_mut().and_then(|service| {
                    service
                        .clear_countdown_cache()
                        .map_err(hall_operation_failed)
                });
                if result.as_ref().is_ok_and(|cleared| *cleared) {
                    self.publish();
                }
                let _ = response.send(result);
            }
        }
    }

    fn service_mut(&mut self) -> Result<&mut HallStateService, BusinessRuntimeError> {
        self.service
            .as_mut()
            .ok_or(BusinessRuntimeError::RuntimeStopped)
    }

    fn publish(&self) {
        if let (Some(state_sink), Some(service)) = (&self.state_sink, &self.service) {
            state_sink.publish_hall_remaining_minutes(service.snapshot().remaining_minutes_now());
        }
    }
}

fn hall_operation_failed(error: anyhow::Error) -> BusinessRuntimeError {
    BusinessRuntimeError::HallOperationFailed(format!("{error:#}"))
}
