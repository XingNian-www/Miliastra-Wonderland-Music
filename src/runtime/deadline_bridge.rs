use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};

use super::business::{
    BusinessEvent, BusinessRuntime, BusinessRuntimeError, BusinessRuntimeEventSink,
    BusinessRuntimeHandle, BusinessRuntimeSnapshot,
};
use super::deadline::{BusinessDeadlineEvent, BusinessDeadlineToken};
#[cfg(test)]
use super::timer::TimerRuntimeHandle;
use super::timer::{TimerRuntime, TimerRuntimeEvent, TimerRuntimeStartError, TimerShutdownError};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BusinessDeadlineBridgeSnapshot {
    forwarded_count: u64,
}

impl BusinessDeadlineBridgeSnapshot {
    pub(crate) const fn forwarded_count(self) -> u64 {
        self.forwarded_count
    }
}

#[derive(Debug)]
pub(crate) enum BusinessRuntimeGroupStartError {
    Timer(TimerRuntimeStartError),
    Business(BusinessRuntimeError),
    BridgeSpawn(std::io::Error),
}

impl Display for BusinessRuntimeGroupStartError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timer(error) => write!(formatter, "failed to start deadline timer: {error}"),
            Self::Business(error) => write!(formatter, "failed to start business runtime: {error}"),
            Self::BridgeSpawn(error) => {
                write!(
                    formatter,
                    "failed to start business deadline bridge: {error}"
                )
            }
        }
    }
}

impl Error for BusinessRuntimeGroupStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timer(error) => Some(error),
            Self::Business(error) => Some(error),
            Self::BridgeSpawn(error) => Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BusinessDeadlineBridgeShutdownError {
    BusinessRuntimeStopped {
        forwarded_count: u64,
        discarded_count: u64,
    },
    WorkerPanicked,
}

impl Display for BusinessDeadlineBridgeShutdownError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BusinessRuntimeStopped {
                forwarded_count,
                discarded_count,
            } => write!(
                formatter,
                "business runtime stopped after {forwarded_count} deadline events were forwarded; \
                 {discarded_count} later events were drained without delivery"
            ),
            Self::WorkerPanicked => formatter.write_str("business deadline bridge worker panicked"),
        }
    }
}

impl Error for BusinessDeadlineBridgeShutdownError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BusinessDeadlineRuntimeShutdownError {
    Timer(TimerShutdownError),
    Bridge(BusinessDeadlineBridgeShutdownError),
    TimerAndBridge {
        timer: TimerShutdownError,
        bridge: BusinessDeadlineBridgeShutdownError,
    },
}

impl Display for BusinessDeadlineRuntimeShutdownError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timer(error) => write!(formatter, "deadline timer shutdown failed: {error}"),
            Self::Bridge(error) => write!(formatter, "deadline bridge shutdown failed: {error}"),
            Self::TimerAndBridge { timer, bridge } => write!(
                formatter,
                "deadline timer and bridge shutdown failed: timer={timer}; bridge={bridge}"
            ),
        }
    }
}

impl Error for BusinessDeadlineRuntimeShutdownError {}

struct BusinessDeadlineBridge {
    worker: Option<
        JoinHandle<Result<BusinessDeadlineBridgeSnapshot, BusinessDeadlineBridgeShutdownError>>,
    >,
}

impl BusinessDeadlineBridge {
    fn start(
        events: Receiver<TimerRuntimeEvent<BusinessDeadlineToken>>,
        business: BusinessRuntimeEventSink,
    ) -> Result<Self, std::io::Error> {
        let worker = thread::Builder::new()
            .name("business-deadline-bridge".to_string())
            .spawn(move || run_business_deadline_bridge(events, business))?;
        Ok(Self {
            worker: Some(worker),
        })
    }

    fn join(
        &mut self,
    ) -> Result<BusinessDeadlineBridgeSnapshot, BusinessDeadlineBridgeShutdownError> {
        let Some(worker) = self.worker.take() else {
            return Ok(BusinessDeadlineBridgeSnapshot::default());
        };
        worker
            .join()
            .map_err(|_| BusinessDeadlineBridgeShutdownError::WorkerPanicked)?
    }
}

fn run_business_deadline_bridge(
    events: Receiver<TimerRuntimeEvent<BusinessDeadlineToken>>,
    business: BusinessRuntimeEventSink,
) -> Result<BusinessDeadlineBridgeSnapshot, BusinessDeadlineBridgeShutdownError> {
    let mut snapshot = BusinessDeadlineBridgeSnapshot::default();
    let mut destination_stopped = false;
    let mut discarded_count = 0_u64;
    while let Ok(event) = events.recv() {
        if destination_stopped {
            discarded_count = discarded_count.saturating_add(1);
            continue;
        }
        let event = BusinessDeadlineEvent::from(event);
        if business.submit(BusinessEvent::Timer(event)).is_err() {
            destination_stopped = true;
            discarded_count = discarded_count.saturating_add(1);
            continue;
        }
        snapshot.forwarded_count = snapshot.forwarded_count.saturating_add(1);
    }
    if destination_stopped {
        Err(
            BusinessDeadlineBridgeShutdownError::BusinessRuntimeStopped {
                forwarded_count: snapshot.forwarded_count,
                discarded_count,
            },
        )
    } else {
        Ok(snapshot)
    }
}

/// Owns the timer before the business runtime exists. Its handle can be injected before the event
/// bridge is attached.
pub(crate) struct BusinessRuntimeGroupBuilder {
    timer: Option<TimerRuntime<BusinessDeadlineToken>>,
    events: Option<Receiver<TimerRuntimeEvent<BusinessDeadlineToken>>>,
}

impl BusinessRuntimeGroupBuilder {
    pub(crate) fn start(queue_capacity: usize) -> Result<Self, BusinessRuntimeGroupStartError> {
        let (event_sender, events) = mpsc::sync_channel(queue_capacity);
        let timer = TimerRuntime::start(queue_capacity, event_sender)
            .map_err(BusinessRuntimeGroupStartError::Timer)?;
        Ok(Self {
            timer: Some(timer),
            events: Some(events),
        })
    }

    #[cfg(test)]
    fn handle(&self) -> TimerRuntimeHandle<BusinessDeadlineToken> {
        self.timer
            .as_ref()
            .expect("deadline timer is active while borrowed")
            .handle()
    }

    fn attach(
        self,
        business: BusinessRuntime,
    ) -> Result<BusinessRuntimeGroup, BusinessRuntimeGroupStartError> {
        self.attach_with_bridge(business, BusinessDeadlineBridge::start)
    }

    fn attach_with_bridge(
        mut self,
        business: BusinessRuntime,
        start_bridge: impl FnOnce(
            Receiver<TimerRuntimeEvent<BusinessDeadlineToken>>,
            BusinessRuntimeEventSink,
        ) -> Result<BusinessDeadlineBridge, std::io::Error>,
    ) -> Result<BusinessRuntimeGroup, BusinessRuntimeGroupStartError> {
        let event_sink = business.event_sink();
        let events = self
            .events
            .take()
            .expect("a fresh deadline timer owns its event receiver");
        let timer = self
            .timer
            .take()
            .expect("a fresh deadline timer owns its runtime");
        let bridge = match start_bridge(events, event_sink) {
            Ok(bridge) => bridge,
            Err(error) => {
                cleanup_failed_attach(timer, business);
                return Err(BusinessRuntimeGroupStartError::BridgeSpawn(error));
            }
        };
        Ok(BusinessRuntimeGroup {
            deadlines: Some(BusinessDeadlineRuntime {
                timer: Some(timer),
                bridge: Some(bridge),
            }),
            business: Some(business),
        })
    }

    pub(crate) fn build_with(
        self,
        build: impl FnOnce() -> Result<BusinessRuntime, BusinessRuntimeError>,
    ) -> Result<BusinessRuntimeGroup, BusinessRuntimeGroupStartError> {
        let business = build().map_err(BusinessRuntimeGroupStartError::Business)?;
        self.attach(business)
    }
}

impl Drop for BusinessRuntimeGroupBuilder {
    fn drop(&mut self) {
        self.events.take();
        if let Some(timer) = self.timer.take() {
            let _ = timer.shutdown();
        }
    }
}

fn cleanup_failed_attach(timer: TimerRuntime<BusinessDeadlineToken>, business: BusinessRuntime) {
    if let Err(error) = business.prepare_shutdown() {
        log::error!("业务期限 bridge 启动失败后的业务静默关闭失败: {error}");
    }
    if let Err(error) = timer.shutdown() {
        log::error!("业务期限 bridge 启动失败后的计时运行时关闭失败: {error}");
    }
    if let Err(error) = business.shutdown() {
        log::error!("业务期限 bridge 启动失败后的业务运行时关闭失败: {error}");
    }
}

/// Owns the single business timer and its attached event bridge so shutdown order cannot diverge.
struct BusinessDeadlineRuntime {
    timer: Option<TimerRuntime<BusinessDeadlineToken>>,
    bridge: Option<BusinessDeadlineBridge>,
}

impl BusinessDeadlineRuntime {
    fn shutdown(
        mut self,
    ) -> Result<BusinessDeadlineBridgeSnapshot, BusinessDeadlineRuntimeShutdownError> {
        self.stop()
    }

    fn stop(
        &mut self,
    ) -> Result<BusinessDeadlineBridgeSnapshot, BusinessDeadlineRuntimeShutdownError> {
        let timer_result = self.timer.take().map(TimerRuntime::shutdown).transpose();
        let bridge_result = self
            .bridge
            .as_mut()
            .map(BusinessDeadlineBridge::join)
            .transpose();
        self.bridge.take();
        match (timer_result, bridge_result) {
            (Ok(_), Ok(Some(snapshot))) => Ok(snapshot),
            (Ok(_), Ok(None)) => Ok(BusinessDeadlineBridgeSnapshot::default()),
            (Err(timer), Ok(_)) => Err(BusinessDeadlineRuntimeShutdownError::Timer(timer)),
            (Ok(_), Err(bridge)) => Err(BusinessDeadlineRuntimeShutdownError::Bridge(bridge)),
            (Err(timer), Err(bridge)) => {
                Err(BusinessDeadlineRuntimeShutdownError::TimerAndBridge { timer, bridge })
            }
        }
    }
}

impl Drop for BusinessDeadlineRuntime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BusinessRuntimeGroupSnapshot {
    deadlines: BusinessDeadlineBridgeSnapshot,
    business: BusinessRuntimeSnapshot,
}

impl BusinessRuntimeGroupSnapshot {
    pub(crate) const fn deadlines(self) -> BusinessDeadlineBridgeSnapshot {
        self.deadlines
    }

    pub(crate) const fn business(self) -> BusinessRuntimeSnapshot {
        self.business
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BusinessRuntimeGroupShutdownError {
    prepare: Option<BusinessRuntimeError>,
    deadlines: Option<BusinessDeadlineRuntimeShutdownError>,
    finish: Option<BusinessRuntimeError>,
}

impl BusinessRuntimeGroupShutdownError {
    pub(crate) const fn prepare_error(&self) -> Option<&BusinessRuntimeError> {
        self.prepare.as_ref()
    }

    pub(crate) const fn deadline_error(&self) -> Option<&BusinessDeadlineRuntimeShutdownError> {
        self.deadlines.as_ref()
    }

    pub(crate) const fn finish_error(&self) -> Option<&BusinessRuntimeError> {
        self.finish.as_ref()
    }
}

impl Display for BusinessRuntimeGroupShutdownError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("business runtime group shutdown failed")?;
        if let Some(error) = &self.prepare {
            write!(formatter, "; prepare={error}")?;
        }
        if let Some(error) = &self.deadlines {
            write!(formatter, "; deadlines={error}")?;
        }
        if let Some(error) = &self.finish {
            write!(formatter, "; finish={error}")?;
        }
        Ok(())
    }
}

impl Error for BusinessRuntimeGroupShutdownError {}

/// Owns the business worker together with its sole timer and bridge. There is no public path that
/// can stop the business destination before the timer event stream has been drained.
pub(crate) struct BusinessRuntimeGroup {
    deadlines: Option<BusinessDeadlineRuntime>,
    business: Option<BusinessRuntime>,
}

impl BusinessRuntimeGroup {
    pub(crate) fn business_handle(&self) -> BusinessRuntimeHandle {
        self.business
            .as_ref()
            .expect("business runtime group is active while borrowed")
            .handle()
    }

    pub(crate) fn event_sink(&self) -> BusinessRuntimeEventSink {
        self.business
            .as_ref()
            .expect("business runtime group is active while borrowed")
            .event_sink()
    }

    pub(crate) fn shutdown(
        mut self,
    ) -> Result<BusinessRuntimeGroupSnapshot, BusinessRuntimeGroupShutdownError> {
        self.stop()
    }

    fn stop(&mut self) -> Result<BusinessRuntimeGroupSnapshot, BusinessRuntimeGroupShutdownError> {
        let prepare = self
            .business
            .as_ref()
            .and_then(|business| business.prepare_shutdown().err());
        let (deadline_snapshot, deadline_error) = match self
            .deadlines
            .take()
            .map(BusinessDeadlineRuntime::shutdown)
            .transpose()
        {
            Ok(snapshot) => (snapshot.unwrap_or_default(), None),
            Err(error) => (BusinessDeadlineBridgeSnapshot::default(), Some(error)),
        };
        let (business_snapshot, finish) = match self
            .business
            .take()
            .map(BusinessRuntime::shutdown)
            .transpose()
        {
            Ok(snapshot) => (snapshot.unwrap_or_default(), None),
            Err(error) => (BusinessRuntimeSnapshot::default(), Some(error)),
        };
        if prepare.is_some() || deadline_error.is_some() || finish.is_some() {
            Err(BusinessRuntimeGroupShutdownError {
                prepare,
                deadlines: deadline_error,
                finish,
            })
        } else {
            Ok(BusinessRuntimeGroupSnapshot {
                deadlines: deadline_snapshot,
                business: business_snapshot,
            })
        }
    }
}

impl Drop for BusinessRuntimeGroup {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::features::card_games::{
        CardGameDeadlineKind, CardGameDeadlineToken, CardGameService, LandlordConfig,
    };
    use crate::features::entertainment::EntertainmentCoordinator;
    use crate::features::idiom_chain::{
        IdiomChainDeadlineKind, IdiomChainDeadlineToken, IdiomChainService,
    };
    use crate::features::turtle_soup::{TurtleSoupDeadlineKind, TurtleSoupDeadlineToken};
    use crate::features::undercover::{UndercoverDeadlineKind, UndercoverDeadlineToken};
    use crate::runtime::business::{BusinessRuntime, BusinessRuntimeError};
    use crate::runtime::deadline::BusinessDeadlineToken;
    use crate::runtime::identity::{BusinessOperationId, SessionGeneration};
    use crate::runtime::timer::{
        DeadlineCancellation, DeadlineSchedule, TimerCommandOutcome, TimerRuntime,
        TimerRuntimeEvent, TimerSubmitError,
    };

    fn business_runtime(queue_capacity: usize) -> BusinessRuntime {
        let entertainment = EntertainmentCoordinator::new();
        BusinessRuntime::start(
            queue_capacity,
            IdiomChainService::from_entries_for_test(
                &["画蛇添足", "足智多谋"],
                entertainment.clone(),
                None,
            ),
            CardGameService::new(LandlordConfig::default(), entertainment),
        )
        .unwrap()
    }

    fn schedule(
        token: impl Into<BusinessDeadlineToken>,
        operation: u64,
    ) -> DeadlineSchedule<BusinessDeadlineToken> {
        DeadlineSchedule::new(
            token.into(),
            BusinessOperationId::new(operation),
            SessionGeneration::INITIAL,
            Instant::now(),
        )
    }

    #[test]
    fn real_bridge_channels_forward_each_expiration_exactly_once_before_shutdown() {
        let business_runtime = business_runtime(8);
        let timer_owner = BusinessRuntimeGroupBuilder::start(8).unwrap();
        let timer = timer_owner.handle();
        let runtime_group = timer_owner.attach(business_runtime).unwrap();
        let business = runtime_group.business_handle();
        let schedules = [
            schedule(
                CardGameDeadlineToken::new(1, CardGameDeadlineKind::LobbyExpiry),
                11,
            ),
            schedule(
                UndercoverDeadlineToken::new(2, UndercoverDeadlineKind::PhaseIdle),
                12,
            ),
            schedule(
                TurtleSoupDeadlineToken::new(3, TurtleSoupDeadlineKind::SessionIdle),
                13,
            ),
            schedule(
                IdiomChainDeadlineToken::new(4, IdiomChainDeadlineKind::SessionIdle),
                14,
            ),
        ];
        for schedule in schedules {
            timer.schedule(schedule).unwrap();
        }

        let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
        thread::spawn(move || {
            result_sender.send(runtime_group.shutdown()).unwrap();
        });
        let snapshot = result_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("two-phase shutdown must drain timer events without deadlocking")
            .unwrap();

        assert_eq!(snapshot.deadlines().forwarded_count(), 8);
        assert_eq!(snapshot.business().timer_counts().card_game(), 1);
        assert_eq!(snapshot.business().timer_counts().undercover(), 1);
        assert_eq!(snapshot.business().timer_counts().turtle_soup(), 1);
        assert_eq!(snapshot.business().timer_counts().idiom_chain(), 1);
        assert_eq!(snapshot.business().timer_counts().command_completed(), 4);
        assert_eq!(snapshot.business().timer_counts().command_failed(), 0);
        assert_eq!(
            business.snapshot(),
            Err(BusinessRuntimeError::RuntimeStopped)
        );
    }

    #[test]
    fn shutdown_reports_a_destination_stopped_before_expiration_delivery() {
        let business_runtime = business_runtime(8);
        let event_sink = business_runtime.event_sink();
        let (event_sender, events) = mpsc::sync_channel(8);
        let timer_runtime = TimerRuntime::start(8, event_sender).unwrap();
        let timer = timer_runtime.handle();
        let mut bridge = BusinessDeadlineBridge::start(events, event_sink).unwrap();
        business_runtime.shutdown().unwrap();
        for id in [1, 2, 3] {
            timer
                .schedule(schedule(
                    CardGameDeadlineToken::new(id, CardGameDeadlineKind::TurnExpiry),
                    id,
                ))
                .unwrap();
        }
        timer_runtime.shutdown().unwrap();

        assert!(matches!(
            bridge.join(),
            Err(
                BusinessDeadlineBridgeShutdownError::BusinessRuntimeStopped {
                    forwarded_count: 0,
                    discarded_count: 6,
                }
            )
        ));
        assert!(matches!(
            timer.schedule(schedule(
                CardGameDeadlineToken::new(4, CardGameDeadlineKind::TurnExpiry),
                4,
            )),
            Err(TimerSubmitError::RuntimeStopped)
        ));
    }

    #[test]
    fn cloned_timer_handle_is_stopped_by_structured_shutdown() {
        let business_runtime = business_runtime(8);
        let timer_owner = BusinessRuntimeGroupBuilder::start(1).unwrap();
        let timer = timer_owner.handle();
        let runtime_group = timer_owner.attach(business_runtime).unwrap();

        runtime_group.shutdown().unwrap();

        assert!(matches!(
            timer.schedule(schedule(
                CardGameDeadlineToken::new(1, CardGameDeadlineKind::TurnWarning),
                1,
            )),
            Err(TimerSubmitError::RuntimeStopped)
        ));
    }

    #[test]
    fn early_group_drop_drains_accepted_events_and_stops_all_handles() {
        let business_runtime = business_runtime(1);
        let timer_owner = BusinessRuntimeGroupBuilder::start(4).unwrap();
        let timer = timer_owner.handle();
        let runtime_group = timer_owner.attach(business_runtime).unwrap();
        let business = runtime_group.business_handle();
        let submit_by = Instant::now() + Duration::from_secs(1);
        for id in 1..=16 {
            loop {
                match timer.schedule(schedule(
                    CardGameDeadlineToken::new(id, CardGameDeadlineKind::TurnExpiry),
                    id,
                )) {
                    Ok(()) => break,
                    Err(TimerSubmitError::QueueFull) => {
                        assert!(Instant::now() < submit_by, "timer queue did not drain");
                        thread::yield_now();
                    }
                    Err(TimerSubmitError::RuntimeStopped) => {
                        panic!("timer stopped before early-drop test")
                    }
                }
            }
        }
        let (dropped_sender, dropped_receiver) = mpsc::sync_channel(1);

        thread::spawn(move || {
            drop(runtime_group);
            dropped_sender.send(()).unwrap();
        });

        dropped_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("group Drop must use the bounded two-phase shutdown path");
        assert!(matches!(
            timer.schedule(schedule(
                CardGameDeadlineToken::new(17, CardGameDeadlineKind::TurnExpiry),
                17,
            )),
            Err(TimerSubmitError::RuntimeStopped)
        ));
        assert_eq!(
            business.snapshot(),
            Err(BusinessRuntimeError::RuntimeStopped)
        );
    }

    #[test]
    fn unattached_saturated_timer_owner_drops_receiver_before_joining_worker() {
        let timer_owner = BusinessRuntimeGroupBuilder::start(1).unwrap();
        let timer = timer_owner.handle();
        timer
            .schedule(schedule(
                CardGameDeadlineToken::new(1, CardGameDeadlineKind::TurnExpiry),
                1,
            ))
            .unwrap();
        let submit_by = Instant::now() + Duration::from_secs(1);
        loop {
            match timer.schedule(DeadlineSchedule::new(
                BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                    2,
                    CardGameDeadlineKind::TurnExpiry,
                )),
                BusinessOperationId::new(2),
                SessionGeneration::INITIAL,
                Instant::now() + Duration::from_secs(60),
            )) {
                Ok(()) => break,
                Err(TimerSubmitError::QueueFull) => {
                    assert!(
                        Instant::now() < submit_by,
                        "timer worker did not take token 1"
                    );
                    thread::yield_now();
                }
                Err(TimerSubmitError::RuntimeStopped) => panic!("timer stopped unexpectedly"),
            }
        }
        assert!(matches!(
            timer.schedule(schedule(
                CardGameDeadlineToken::new(3, CardGameDeadlineKind::TurnExpiry),
                3,
            )),
            Err(TimerSubmitError::QueueFull)
        ));
        let (dropped_sender, dropped_receiver) = mpsc::sync_channel(1);

        thread::spawn(move || {
            drop(timer_owner);
            dropped_sender.send(()).unwrap();
        });

        dropped_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("unattached owner must close its event receiver before joining timer");
        assert!(matches!(
            timer.schedule(schedule(
                CardGameDeadlineToken::new(4, CardGameDeadlineKind::TurnExpiry),
                4,
            )),
            Err(TimerSubmitError::RuntimeStopped)
        ));
    }

    #[test]
    fn failed_attach_cleanup_stops_timer_then_business_handles() {
        let timer_owner = BusinessRuntimeGroupBuilder::start(2).unwrap();
        let timer = timer_owner.handle();
        let business_runtime = business_runtime(2);
        let business = business_runtime.handle();

        let error = match timer_owner
            .attach_with_bridge(business_runtime, |_events, _event_sink| {
                Err(std::io::Error::other("forced bridge spawn failure"))
            }) {
            Ok(_) => panic!("injected bridge failure should fail attachment"),
            Err(error) => error,
        };

        assert!(matches!(
            error,
            BusinessRuntimeGroupStartError::BridgeSpawn(_)
        ));
        assert!(matches!(
            timer.schedule(schedule(
                CardGameDeadlineToken::new(1, CardGameDeadlineKind::TurnExpiry),
                1,
            )),
            Err(TimerSubmitError::RuntimeStopped)
        ));
        assert_eq!(
            business.snapshot(),
            Err(BusinessRuntimeError::RuntimeStopped)
        );
    }

    #[test]
    fn application_timer_reports_queue_full_without_bypassing_the_bounded_channel() {
        let (event_sender, events) = mpsc::sync_channel(1);
        let runtime = TimerRuntime::<BusinessDeadlineToken>::start(1, event_sender).unwrap();
        let timer = runtime.handle();
        timer
            .schedule(schedule(
                CardGameDeadlineToken::new(1, CardGameDeadlineKind::TurnExpiry),
                1,
            ))
            .unwrap();
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(1)).unwrap(),
            TimerRuntimeEvent::CommandCompleted(_)
        ));
        let future = Instant::now() + Duration::from_secs(1);
        timer
            .schedule(DeadlineSchedule::new(
                BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                    2,
                    CardGameDeadlineKind::TurnExpiry,
                )),
                BusinessOperationId::new(2),
                SessionGeneration::INITIAL,
                future,
            ))
            .unwrap();
        let submit_by = Instant::now() + Duration::from_secs(1);
        loop {
            match timer.schedule(DeadlineSchedule::new(
                BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                    3,
                    CardGameDeadlineKind::TurnExpiry,
                )),
                BusinessOperationId::new(3),
                SessionGeneration::INITIAL,
                future,
            )) {
                Ok(()) => break,
                Err(TimerSubmitError::QueueFull) => {
                    assert!(
                        Instant::now() < submit_by,
                        "timer worker did not take token 2"
                    );
                    thread::yield_now();
                }
                Err(TimerSubmitError::RuntimeStopped) => panic!("timer stopped unexpectedly"),
            }
        }

        assert!(matches!(
            timer.schedule(DeadlineSchedule::new(
                BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                    4,
                    CardGameDeadlineKind::TurnExpiry,
                )),
                BusinessOperationId::new(4),
                SessionGeneration::INITIAL,
                future,
            )),
            Err(TimerSubmitError::QueueFull)
        ));
        assert!(matches!(
            events.recv_timeout(Duration::from_secs(1)).unwrap(),
            TimerRuntimeEvent::DeadlineExpired(event)
                if event.token()
                    == &BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                        1,
                        CardGameDeadlineKind::TurnExpiry,
                    ))
        ));
        for _ in 0..2 {
            assert!(matches!(
                events.recv_timeout(Duration::from_secs(1)).unwrap(),
                TimerRuntimeEvent::CommandCompleted(_)
            ));
        }
        runtime.shutdown().unwrap();
    }

    #[test]
    fn command_routing_preserves_current_and_previous_schedule_correlation() {
        let (event_sender, events) = mpsc::sync_channel(4);
        let runtime = TimerRuntime::<BusinessDeadlineToken>::start(4, event_sender).unwrap();
        let timer = runtime.handle();
        let token = BusinessDeadlineToken::from(CardGameDeadlineToken::new(
            7,
            CardGameDeadlineKind::TurnExpiry,
        ));
        let deadline = Instant::now() + Duration::from_secs(60);
        timer
            .schedule(DeadlineSchedule::new(
                token.clone(),
                BusinessOperationId::new(11),
                SessionGeneration::new(1),
                deadline,
            ))
            .unwrap();
        timer
            .reschedule(DeadlineSchedule::new(
                token,
                BusinessOperationId::new(12),
                SessionGeneration::new(2),
                deadline,
            ))
            .unwrap();
        let _scheduled = events.recv_timeout(Duration::from_secs(1)).unwrap();
        let routed =
            BusinessDeadlineEvent::from(events.recv_timeout(Duration::from_secs(1)).unwrap());

        assert!(matches!(
            routed,
            BusinessDeadlineEvent::CardGame(TimerRuntimeEvent::CommandCompleted(completed))
                if completed.token().id() == 7
                    && completed.operation_id() == BusinessOperationId::new(12)
                    && completed.session_generation() == SessionGeneration::new(2)
                    && matches!(
                        completed.result(),
                        Ok(TimerCommandOutcome::Rescheduled(previous))
                            if previous.token().id() == 7
                                && previous.operation_id() == BusinessOperationId::new(11)
                                && previous.session_generation() == SessionGeneration::new(1)
                    )
        ));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn business_runtime_stays_responsive_without_waiting_for_timer_acknowledgements() {
        let business_runtime = business_runtime(8);
        let timer_owner = BusinessRuntimeGroupBuilder::start(1).unwrap();
        let timer = timer_owner.handle();
        let runtime_group = timer_owner.attach(business_runtime).unwrap();
        let business = runtime_group.business_handle();
        timer
            .schedule(schedule(
                CardGameDeadlineToken::new(1, CardGameDeadlineKind::TurnWarning),
                1,
            ))
            .unwrap();

        assert!(business.snapshot().is_ok());
        let final_snapshot = runtime_group.shutdown().unwrap();
        assert_eq!(
            final_snapshot.business().timer_counts().command_completed(),
            1
        );
        assert_eq!(final_snapshot.business().timer_counts().card_game(), 1);
    }

    #[test]
    fn command_errors_cross_the_bridge_with_no_waitable_ack_to_drop() {
        let business_runtime = business_runtime(8);
        let timer_owner = BusinessRuntimeGroupBuilder::start(8).unwrap();
        let timer = timer_owner.handle();
        let runtime_group = timer_owner.attach(business_runtime).unwrap();
        let future = Instant::now() + Duration::from_secs(60);
        timer
            .schedule(DeadlineSchedule::new(
                BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                    1,
                    CardGameDeadlineKind::TurnExpiry,
                )),
                BusinessOperationId::new(11),
                SessionGeneration::new(1),
                future,
            ))
            .unwrap();
        timer
            .schedule(DeadlineSchedule::new(
                BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                    1,
                    CardGameDeadlineKind::TurnExpiry,
                )),
                BusinessOperationId::new(12),
                SessionGeneration::new(2),
                future,
            ))
            .unwrap();
        timer
            .reschedule(DeadlineSchedule::new(
                BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                    2,
                    CardGameDeadlineKind::TurnExpiry,
                )),
                BusinessOperationId::new(13),
                SessionGeneration::new(3),
                future,
            ))
            .unwrap();
        timer
            .cancel(DeadlineCancellation::new(
                BusinessDeadlineToken::from(CardGameDeadlineToken::new(
                    3,
                    CardGameDeadlineKind::TurnExpiry,
                )),
                BusinessOperationId::new(14),
                SessionGeneration::new(4),
            ))
            .unwrap();

        let snapshot = runtime_group.shutdown().unwrap();

        assert_eq!(snapshot.deadlines().forwarded_count(), 4);
        assert_eq!(snapshot.business().timer_counts().command_completed(), 4);
        assert_eq!(snapshot.business().timer_counts().command_failed(), 2);
        assert_eq!(snapshot.business().timer_counts().card_game(), 0);
    }

    #[test]
    fn saturated_timer_and_business_queues_drain_during_two_phase_shutdown() {
        const DEADLINE_COUNT: u64 = 64;

        let business_runtime = business_runtime(1);
        let timer_owner = BusinessRuntimeGroupBuilder::start(4).unwrap();
        let timer = timer_owner.handle();
        let runtime_group = timer_owner.attach(business_runtime).unwrap();
        let submit_by = Instant::now() + Duration::from_secs(2);
        for id in 1..=DEADLINE_COUNT {
            loop {
                match timer.schedule(schedule(
                    CardGameDeadlineToken::new(id, CardGameDeadlineKind::TurnExpiry),
                    id,
                )) {
                    Ok(()) => break,
                    Err(TimerSubmitError::QueueFull) => {
                        assert!(Instant::now() < submit_by, "timer queue did not drain");
                        thread::yield_now();
                    }
                    Err(TimerSubmitError::RuntimeStopped) => {
                        panic!("timer stopped while accepting saturation test work")
                    }
                }
            }
        }

        let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
        thread::spawn(move || {
            result_sender.send(runtime_group.shutdown()).unwrap();
        });
        let snapshot = result_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("two-phase shutdown must drain saturated queues without deadlocking")
            .unwrap();

        assert_eq!(snapshot.deadlines().forwarded_count(), DEADLINE_COUNT * 2);
        assert_eq!(
            snapshot.business().timer_counts().command_completed(),
            DEADLINE_COUNT
        );
        assert_eq!(snapshot.business().timer_counts().command_failed(), 0);
        assert_eq!(
            snapshot.business().timer_counts().card_game(),
            DEADLINE_COUNT
        );
    }
}
