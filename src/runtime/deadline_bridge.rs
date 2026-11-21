use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::business::{
    BusinessEvent, BusinessRuntime, BusinessRuntimeError, BusinessRuntimeEventSink,
    BusinessRuntimeHandle, BusinessRuntimeSnapshot,
};
use super::deadline::{BusinessDeadlineEvent, BusinessDeadlineToken};
use miliastra_kernel::timer::{
    TimerEventSink, TimerRuntime, TimerRuntimeEvent, TimerRuntimeHandle, TimerRuntimeStartError,
    TimerRuntimeStarter, TimerShutdownError,
};

/// 到期事件的转交计数。
///
/// 两个数刻意分开：`forwarded` 是真正交给业务运行时的条数，`discarded`
/// 是业务运行时已经停掉之后、仍被计时器排空的条数。后者大于零说明关闭过程中
/// 有一批截止时间已经无法生效——这正是排障时最需要看到的信息，不能和
/// 「正常转交」混成一个数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BusinessDeadlineBridgeSnapshot {
    forwarded_count: u64,
    discarded_count: u64,
}

impl BusinessDeadlineBridgeSnapshot {
    pub(crate) const fn forwarded_count(self) -> u64 {
        self.forwarded_count
    }

    pub(crate) const fn discarded_count(self) -> u64 {
        self.discarded_count
    }
}

/// 计时器线程与停机方共享的投递账本。
///
/// 写入方是计时器线程，读取方是**已经 join 过它**的停机路径；`join` 本身
/// 提供了 happens-before，因此这里用 `Relaxed` 就足够，不必为可见性多付
/// 一次内存栅栏。计数只增不减，所以也不需要锁。
#[derive(Debug, Default)]
struct DeadlineDeliveryLedger {
    forwarded: AtomicU64,
    discarded: AtomicU64,
}

impl DeadlineDeliveryLedger {
    fn record_forwarded(&self) {
        self.forwarded.fetch_add(1, Ordering::Relaxed);
    }

    fn record_discarded(&self) {
        self.discarded.fetch_add(1, Ordering::Relaxed);
    }

    /// 读取当前计数。调用方必须已经停止（`join`）计时器线程。
    fn snapshot(&self) -> BusinessDeadlineBridgeSnapshot {
        BusinessDeadlineBridgeSnapshot {
            forwarded_count: self.forwarded.load(Ordering::Relaxed),
            discarded_count: self.discarded.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
pub(crate) enum BusinessRuntimeGroupStartError {
    Timer(TimerRuntimeStartError),
    Business(BusinessRuntimeError),
}

impl Display for BusinessRuntimeGroupStartError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timer(error) => write!(formatter, "failed to start deadline timer: {error}"),
            Self::Business(error) => write!(formatter, "failed to start business runtime: {error}"),
        }
    }
}

impl Error for BusinessRuntimeGroupStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timer(error) => Some(error),
            Self::Business(error) => Some(error),
        }
    }
}

/// 计时器关闭阶段的收尾结果。
///
/// 业务运行时先于计时器停下时，剩下的到期事件已经无处可投。这不算计时器
/// 故障，而是一次**有损关闭**：必须把丢弃条数带回给调用方，让日志能说清
/// 「多少条截止时间没能生效」，而不是笼统报一句失败。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BusinessDeadlineRuntimeShutdownError {
    Timer(TimerShutdownError),
    DestinationStopped {
        forwarded_count: u64,
        discarded_count: u64,
    },
}

impl Display for BusinessDeadlineRuntimeShutdownError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timer(error) => write!(formatter, "deadline timer shutdown failed: {error}"),
            Self::DestinationStopped {
                forwarded_count,
                discarded_count,
            } => write!(
                formatter,
                "business runtime stopped after {forwarded_count} deadline events were forwarded; \
                 {discarded_count} later events were drained without delivery"
            ),
        }
    }
}

impl Error for BusinessDeadlineRuntimeShutdownError {}

/// 计时器到期事件直投业务运行时的出口。
///
/// 这个实现就是取代 `business-deadline-bridge` 线程的全部内容：计时器线程每产出
/// 一条事件，就在这里同步投给业务运行时的事件入口。没有中间通道，也就没有
/// 「为了把两级通道接起来而必须存在的转发线程」，以及随之而来的第二份队列。
///
/// 投递语义与原先经 `mpsc::sync_channel` 转发时逐字等价：
///
/// - [`BusinessRuntimeEventSink::submit`] 在业务队列满时阻塞，因此计时器线程
///   跟着等，事件不丢——这条背压是刻意保留的；
/// - 业务运行时停下之后，`submit` 立刻失败。此后每一条事件都计入丢弃，
///   计时器线程**继续排空**而不是立刻退出：丢弃条数要反映真实积压量，
///   否则日志只能说「丢了一条」，看不出还压着多少。
///
/// 正因为要「继续排空」，这个 sink 永远返回 `Ok`：`Err` 在
/// [`TimerEventSink`] 里的含义是「接收方已停止，计时器可以收尾」，而这里
/// 计时器还没到收尾的时候——它要陪业务运行时一起走完关闭流程。
struct BusinessDeadlineSink {
    business: BusinessRuntimeEventSink,
    ledger: Arc<DeadlineDeliveryLedger>,
    destination_stopped: bool,
}

impl BusinessDeadlineSink {
    fn new(business: BusinessRuntimeEventSink, ledger: Arc<DeadlineDeliveryLedger>) -> Self {
        Self {
            business,
            ledger,
            destination_stopped: false,
        }
    }
}

impl TimerEventSink<BusinessDeadlineToken> for BusinessDeadlineSink {
    fn deliver(&mut self, event: TimerRuntimeEvent<BusinessDeadlineToken>) -> Result<(), ()> {
        if self.destination_stopped {
            self.ledger.record_discarded();
            return Ok(());
        }
        let routed = BusinessDeadlineEvent::from(event);
        if self.business.submit(BusinessEvent::Timer(routed)).is_err() {
            // 下游状态不会恢复，不必再为后续事件重复探测。
            self.destination_stopped = true;
            self.ledger.record_discarded();
            return Ok(());
        }
        self.ledger.record_forwarded();
        Ok(())
    }
}

/// 在业务运行时创建前持有计时器；事件出口连接前即可注入其句柄。
///
/// 组合根有一段真实的顺序约束：计时器句柄必须在业务运行时**构造之前**交出去
/// （构造参数里就带着它），而业务运行时的事件入口只有**构造之后**才能取得。
/// 两段式启动正好落在这个顺序上——[`Self::start`] 只建命令通道并交出句柄，
/// [`Self::build_with`] 拿到业务运行时之后再把事件出口接上去，计时器工作线程
/// 此刻才真正开跑。
///
/// 两段之间的语义是明确的：
///
/// - 未启动期间提交的命令留在有界命令队列里，并发上限不变，满了照旧报
///   [`miliastra_kernel::timer::TimerSubmitError::QueueFull`]，绝不静默丢弃；
/// - 未启动期间丢弃 builder，命令通道随之关闭，已发出的句柄立即报
///   `RuntimeStopped`，不会有线程被落下。
pub(crate) struct BusinessRuntimeGroupBuilder {
    timer: Option<TimerRuntimeStarter<BusinessDeadlineToken>>,
    ledger: Arc<DeadlineDeliveryLedger>,
}

impl BusinessRuntimeGroupBuilder {
    pub(crate) fn start(queue_capacity: usize) -> Result<Self, BusinessRuntimeGroupStartError> {
        let timer =
            TimerRuntime::prepare(queue_capacity).map_err(BusinessRuntimeGroupStartError::Timer)?;
        Ok(Self {
            timer: Some(timer),
            ledger: Arc::new(DeadlineDeliveryLedger::default()),
        })
    }

    pub(crate) fn handle(&self) -> TimerRuntimeHandle<BusinessDeadlineToken> {
        self.timer
            .as_ref()
            .expect("deadline timer is active while borrowed")
            .handle()
    }

    pub(crate) fn build_with(
        self,
        build: impl FnOnce() -> Result<BusinessRuntime, BusinessRuntimeError>,
    ) -> Result<BusinessRuntimeGroup, BusinessRuntimeGroupStartError> {
        let business = build().map_err(BusinessRuntimeGroupStartError::Business)?;
        self.attach(business)
    }

    fn attach(
        mut self,
        business: BusinessRuntime,
    ) -> Result<BusinessRuntimeGroup, BusinessRuntimeGroupStartError> {
        let starter = self
            .timer
            .take()
            .expect("deadline timer starter is consumed exactly once");
        self.attach_with(business, starter, TimerRuntimeStarter::start)
    }

    /// 允许注入「绑定事件出口并启动线程」这一步，单测可以在这里强制启动失败。
    fn attach_with(
        self,
        business: BusinessRuntime,
        starter: TimerRuntimeStarter<BusinessDeadlineToken>,
        start_timer: impl FnOnce(
            TimerRuntimeStarter<BusinessDeadlineToken>,
            BusinessDeadlineSink,
        )
            -> Result<TimerRuntime<BusinessDeadlineToken>, TimerRuntimeStartError>,
    ) -> Result<BusinessRuntimeGroup, BusinessRuntimeGroupStartError> {
        let sink = BusinessDeadlineSink::new(business.event_sink(), Arc::clone(&self.ledger));
        let timer = match start_timer(starter, sink) {
            Ok(timer) => timer,
            Err(error) => {
                cleanup_failed_attach(business);
                return Err(BusinessRuntimeGroupStartError::Timer(error));
            }
        };
        Ok(BusinessRuntimeGroup {
            deadlines: Some(BusinessDeadlineRuntime {
                timer: Some(timer),
                ledger: Arc::clone(&self.ledger),
            }),
            business: Some(business),
        })
    }
}

impl Drop for BusinessRuntimeGroupBuilder {
    fn drop(&mut self) {
        // 两段式启动里还没有工作线程，丢弃 starter 就是关闭命令通道：
        // 已交出去的句柄会立刻看到 `RuntimeStopped`，不会有线程被落下。
        self.timer.take();
    }
}

fn cleanup_failed_attach(business: BusinessRuntime) {
    if let Err(error) = business.prepare_shutdown() {
        log::error!("业务期限启动失败后的业务静默关闭失败：{error}");
    }
    if let Err(error) = business.shutdown() {
        log::error!("业务期限启动失败后的业务运行时关闭失败：{error}");
    }
}

/// 持有唯一业务计时器，确保停机顺序一致。
///
/// 计时器线程本身就是事件投递者，所以这里只有一个要 join 的线程，
/// 也不存在「先排空桥接、再停计时器」的两段排空。
struct BusinessDeadlineRuntime {
    timer: Option<TimerRuntime<BusinessDeadlineToken>>,
    ledger: Arc<DeadlineDeliveryLedger>,
}

impl BusinessDeadlineRuntime {
    fn stop(
        &mut self,
    ) -> Result<BusinessDeadlineBridgeSnapshot, BusinessDeadlineRuntimeShutdownError> {
        let Some(timer) = self.timer.take() else {
            return Ok(BusinessDeadlineBridgeSnapshot::default());
        };
        timer
            .shutdown()
            .map_err(BusinessDeadlineRuntimeShutdownError::Timer)?;
        // join 已经完成，账本里是最终值。
        let snapshot = self.ledger.snapshot();
        if snapshot.discarded_count() > 0 {
            return Err(BusinessDeadlineRuntimeShutdownError::DestinationStopped {
                forwarded_count: snapshot.forwarded_count(),
                discarded_count: snapshot.discarded_count(),
            });
        }
        Ok(snapshot)
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

/// 将业务工作线程与唯一计时器一起持有。停机先静默业务状态并停止外部工作线程，
/// 再停计时器线程（它同时是事件投递者），最后退出工作线程。
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
        if let Some(business) = self.business.as_mut() {
            business.stop_external_workers();
        }
        let (deadline_snapshot, deadline_error) = match self
            .deadlines
            .take()
            .map(|mut deadlines| deadlines.stop())
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
        CardGameCommandStart, CardGameDeadlineKind, CardGameDeadlineToken, CardGameEffectClaim,
        CardGameEffectResult, CardGameResume, CardGameService, LandlordCommand, LandlordConfig,
    };
    use crate::features::idiom_chain::{
        IdiomChainDeadlineKind, IdiomChainDeadlineToken, IdiomChainService,
    };
    use crate::features::turtle_soup::{TurtleSoupDeadlineKind, TurtleSoupDeadlineToken};
    use crate::features::undercover::{UndercoverDeadlineKind, UndercoverDeadlineToken};
    use crate::runtime::business::{BusinessRuntime, BusinessRuntimeError};
    use crate::runtime::deadline::BusinessDeadlineToken;
    use miliastra_kernel::identity::{BusinessOperationId, SessionGeneration};
    use miliastra_kernel::timer::{
        DeadlineCancellation, DeadlineSchedule, TimerCommandOutcome, TimerRuntime,
        TimerRuntimeEvent, TimerSubmitError,
    };

    fn business_runtime(queue_capacity: usize) -> BusinessRuntime {
        BusinessRuntime::start(
            queue_capacity,
            IdiomChainService::from_entries_for_test(&["画蛇添足", "足智多谋"], None),
            CardGameService::new(LandlordConfig::default()),
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

    /// 独立搭一套「计时器 + 直投出口」，便于直接观察账本与关闭结果。
    fn direct_delivery(
        events: BusinessRuntimeEventSink,
    ) -> (
        TimerRuntime<BusinessDeadlineToken>,
        Arc<DeadlineDeliveryLedger>,
    ) {
        let ledger = Arc::new(DeadlineDeliveryLedger::default());
        let sink = BusinessDeadlineSink::new(events, Arc::clone(&ledger));
        let timer = TimerRuntime::start_with_sink(8, sink).unwrap();
        (timer, ledger)
    }

    #[test]
    fn real_timer_runtime_drives_card_game_timeout_into_a_business_effect() {
        let service = CardGameService::new(LandlordConfig {
            lobby_timeout_seconds: 1,
            ..LandlordConfig::default()
        });
        let business_runtime = BusinessRuntime::start(
            8,
            IdiomChainService::from_entries_for_test(&["画蛇添足", "足智多谋"], None),
            service,
        )
        .unwrap();
        let timer_owner = BusinessRuntimeGroupBuilder::start(8).unwrap();
        let runtime_group = timer_owner.attach(business_runtime).unwrap();
        let business = runtime_group.business_handle();
        let started_at = Instant::now();
        let verification = match business
            .begin_card_game("甲", &LandlordCommand::Start, started_at)
            .unwrap()
        {
            CardGameCommandStart::Suspended(request) => request,
            CardGameCommandStart::Completed(_) => panic!("start should require verification"),
        };
        assert_eq!(
            business.claim_card_game_effect(verification.key).unwrap(),
            CardGameEffectClaim::Claimed
        );
        let hall = match business
            .resume_card_game(
                verification.key,
                CardGameEffectResult::FriendVerify(Ok(true)),
            )
            .unwrap()
        {
            CardGameResume::Suspended(request) => request,
            other => panic!("verified start should announce lobby: {other:?}"),
        };
        assert_eq!(
            business.claim_card_game_effect(hall.key).unwrap(),
            CardGameEffectClaim::Claimed
        );
        assert!(matches!(
            business
                .resume_card_game(hall.key, CardGameEffectResult::HallDelivery(Ok(())),)
                .unwrap(),
            CardGameResume::Completed(_)
        ));

        let wait_until = Instant::now() + Duration::from_secs(3);
        let timed = loop {
            if let Some(outcome) = business
                .poll_card_game_timed_outcome(Instant::now(), true)
                .unwrap()
            {
                break outcome;
            }
            assert!(
                Instant::now() < wait_until,
                "real card-game deadline did not expire"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(timed.action(), "lobby-timeout");
        let request = timed.into_request();
        assert_eq!(
            business.claim_card_game_effect(request.key).unwrap(),
            CardGameEffectClaim::Claimed
        );
        assert!(matches!(
            business
                .resume_card_game(request.key, CardGameEffectResult::HallDelivery(Ok(())))
                .unwrap(),
            CardGameResume::Completed(_)
        ));
        assert_eq!(business.active_entertainment().unwrap(), None);
        runtime_group.shutdown().unwrap();
    }

    #[test]
    fn timer_thread_delivers_each_expiration_exactly_once_without_a_bridge_thread() {
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
            .expect("shutdown must drain timer events without deadlocking")
            .unwrap();

        assert_eq!(snapshot.deadlines().forwarded_count(), 8);
        assert_eq!(snapshot.deadlines().discarded_count(), 0);
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

    /// 直投改造的核心断言：进程里只有计时器线程在投递到期事件，
    /// 不存在 `business-deadline-bridge` 这层纯转发线程。
    ///
    /// 线程名只在 Linux 的 `/proc/self/task/<tid>/comm` 上可读，
    /// 因此这条用例只在 Linux 生效；Windows 侧由「事件恰好投递一次」
    /// 那几条用例间接覆盖同一语义。
    #[cfg(target_os = "linux")]
    #[test]
    fn no_bridge_thread_is_spawned_for_deadline_delivery() {
        fn thread_names() -> Vec<String> {
            let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
                return Vec::new();
            };
            entries
                .filter_map(std::result::Result::ok)
                .filter_map(|entry| std::fs::read_to_string(entry.path().join("comm")).ok())
                .map(|name| name.trim().to_string())
                .collect()
        }

        // Linux /proc/self/task/*/comm 最多保留 15 字节，且新线程在 spawn 返回后
        // 才执行 prctl(PR_SET_NAME)；短轮询等待计时器线程完成命名，避免并行调度下的竞态。
        let before = thread_names()
            .into_iter()
            .filter(|name| name.starts_with("business-deadli"))
            .count();
        let business_runtime = business_runtime(4);
        let timer_owner = BusinessRuntimeGroupBuilder::start(4).unwrap();
        let runtime_group = timer_owner.attach(business_runtime).unwrap();
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut names = thread_names();
        while !names.iter().any(|name| name == "timer-runtime") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
            names = thread_names();
        }
        let timers = names.iter().filter(|name| *name == "timer-runtime").count();
        let bridges = names
            .iter()
            .filter(|name| name.starts_with("business-deadli"))
            .count();

        assert!(timers >= 1, "启动业务运行时组后应当有计时器线程：{names:?}");
        assert_eq!(
            bridges, before,
            "计时器应当直接把事件投给业务运行时，不应再起转发线程：{names:?}"
        );
        runtime_group.shutdown().unwrap();
    }

    #[test]
    fn shutdown_reports_discarded_expirations_after_the_business_runtime_stopped() {
        let business_runtime = business_runtime(8);
        let (timer, ledger) = direct_delivery(business_runtime.event_sink());
        let timer_handle = timer.handle();
        business_runtime.shutdown().unwrap();
        for id in [1, 2, 3] {
            timer_handle
                .schedule(schedule(
                    CardGameDeadlineToken::new(id, CardGameDeadlineKind::TurnExpiry),
                    id,
                ))
                .unwrap();
        }
        let mut deadlines = BusinessDeadlineRuntime {
            timer: Some(timer),
            ledger,
        };

        assert!(matches!(
            deadlines.stop(),
            Err(BusinessDeadlineRuntimeShutdownError::DestinationStopped {
                forwarded_count: 0,
                discarded_count: 6,
            })
        ));
        assert!(matches!(
            timer_handle.schedule(schedule(
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
        let (dropped_sender, dropped_receiver) = std::sync::mpsc::sync_channel(1);

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
    fn unattached_saturated_timer_owner_drops_commands_before_joining_worker() {
        let timer_owner = BusinessRuntimeGroupBuilder::start(1).unwrap();
        let timer = timer_owner.handle();
        timer
            .schedule(schedule(
                CardGameDeadlineToken::new(1, CardGameDeadlineKind::TurnExpiry),
                1,
            ))
            .unwrap();
        // 两段式启动：还没有工作线程消费命令队列，所以第二条命令立刻打满容量。
        assert!(matches!(
            timer.schedule(schedule(
                CardGameDeadlineToken::new(2, CardGameDeadlineKind::TurnExpiry),
                2,
            )),
            Err(TimerSubmitError::QueueFull)
        ));
        let (dropped_sender, dropped_receiver) = std::sync::mpsc::sync_channel(1);

        thread::spawn(move || {
            drop(timer_owner);
            dropped_sender.send(()).unwrap();
        });

        dropped_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("unattached owner must close its command channel before joining timer");
        assert!(matches!(
            timer.schedule(schedule(
                CardGameDeadlineToken::new(3, CardGameDeadlineKind::TurnExpiry),
                3,
            )),
            Err(TimerSubmitError::RuntimeStopped)
        ));
    }

    #[test]
    fn failed_attach_cleanup_stops_business_then_timer_handles() {
        let mut timer_owner = BusinessRuntimeGroupBuilder::start(2).unwrap();
        let timer = timer_owner.handle();
        let business_runtime = business_runtime(2);
        let business = business_runtime.handle();
        let starter = timer_owner
            .timer
            .take()
            .expect("deadline timer starter is consumed exactly once");

        let error = match timer_owner.attach_with(business_runtime, starter, |_starter, _sink| {
            Err(TimerRuntimeStartError::Spawn(std::io::Error::other(
                "forced timer spawn failure",
            )))
        }) {
            Ok(_) => panic!("injected timer failure should fail attachment"),
            Err(error) => error,
        };

        assert!(matches!(error, BusinessRuntimeGroupStartError::Timer(_)));
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
        let (event_sender, events) = std::sync::mpsc::sync_channel(1);
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
        let (event_sender, events) = std::sync::mpsc::sync_channel(4);
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
    fn command_errors_reach_the_business_runtime_without_a_waitable_ack_to_drop() {
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
        assert_eq!(snapshot.deadlines().discarded_count(), 0);
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
