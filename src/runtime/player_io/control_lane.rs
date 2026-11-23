//! 控制回执补全的**单 lane + 有界队列**。
//!
//! 旧实现为每条 deferred 控制命令起一个 `player-control-completion` 线程，
//! 线程数随命令数增长（200 条命令 = 200 条线程，全部阻塞在端口侧等待）。
//! 这里收敛为一条常驻 lane：端口侧 `Deferred` 作业进有界队列，由同一个线程
//! 串行取作业并等待回执；队列满时显式回 `OutcomeUnknown`，不再按命令起线程。
//!
//! 取消语义：等待方可凭 [`ControlCancellationHandle`] 在超时/放弃时立即把取消
//! 传给端口侧闭包（“超时置取消”），作业若还在队列里则跳过执行（不迟到执行命令）。

use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use super::{
    ControlCancellation, ControlCompletion, ControlDispatchOutcome, ControlOperationResult,
    ControlRequest, complete_control_request,
};

/// 一条控制操作的取消句柄：完成 lane 登记端口侧取消闭包，等待方超时时触发。
pub(super) struct ControlCancellationHandle {
    slot: Mutex<CancellationSlot>,
}

#[derive(Default)]
struct CancellationSlot {
    cancelled: bool,
    cancellation: Option<ControlCancellation>,
}

impl ControlCancellationHandle {
    pub(super) fn new() -> Self {
        Self {
            slot: Mutex::new(CancellationSlot::default()),
        }
    }

    /// 完成 lane 取出作业时登记端口侧取消闭包；若等待方已放弃，立刻取消。
    pub(super) fn register(&self, cancellation: ControlCancellation) {
        let mut slot = self.lock();
        if slot.cancelled {
            drop(slot);
            cancellation();
            return;
        }
        slot.cancellation = Some(cancellation);
    }

    /// 等待方超时/关停：标记已取消并执行端口侧取消（只执行一次）。
    pub(super) fn cancel(&self) {
        let cancellation = {
            let mut slot = self.lock();
            slot.cancelled = true;
            slot.cancellation.take()
        };
        if let Some(cancellation) = cancellation {
            cancellation();
        }
    }

    /// 作业还在队列里（尚未登记）：转发取消闭包并标记，避免漏取消。
    pub(super) fn cancel_queued(&self, cancellation: Option<ControlCancellation>) {
        self.lock().cancelled = true;
        if let Some(cancellation) = cancellation {
            cancellation();
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    fn lock(&self) -> MutexGuard<'_, CancellationSlot> {
        self.slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(super) struct ControlCompletionJob {
    pub(super) request: ControlRequest,
    pub(super) completion: ControlCompletion,
    pub(super) cancellation: Option<ControlCancellation>,
    pub(super) started_at: Instant,
}

#[derive(Default)]
struct ControlCompletionState {
    queue: VecDeque<ControlCompletionJob>,
    in_flight: Option<Arc<ControlCancellationHandle>>,
    stopping: bool,
    worker: Option<JoinHandle<()>>,
}

/// 作业没能进队列的原因（调用方据此显式回执，而不是静默丢弃）。
pub(super) enum CompletionSubmitError {
    QueueFull,
    Stopping,
}

pub(super) struct ControlCompletionLane {
    state: Mutex<ControlCompletionState>,
    ready: Condvar,
    capacity: usize,
    panicked: AtomicBool,
}

impl ControlCompletionLane {
    pub(super) fn start(capacity: usize) -> std::io::Result<Arc<Self>> {
        let lane = Arc::new(Self {
            state: Mutex::new(ControlCompletionState::default()),
            ready: Condvar::new(),
            capacity: capacity.max(1),
            panicked: AtomicBool::new(false),
        });
        let worker = Arc::downgrade(&lane);
        let handle = thread::Builder::new()
            .name("player-control-completion".to_owned())
            .spawn(move || run_lane(worker))?;
        lane.lock().worker = Some(handle);
        Ok(lane)
    }

    /// 有界队列：满了显式拒绝，绝不按命令数起线程。
    pub(super) fn submit(
        &self,
        job: ControlCompletionJob,
    ) -> Result<(), (CompletionSubmitError, ControlCompletionJob)> {
        let mut state = self.lock();
        if state.stopping {
            return Err((CompletionSubmitError::Stopping, job));
        }
        if state.queue.len() >= self.capacity {
            return Err((CompletionSubmitError::QueueFull, job));
        }
        state.queue.push_back(job);
        self.ready.notify_one();
        Ok(())
    }

    /// 关停：停止收新作业、取消在途与排队作业、排空队列后 join 唯一的 lane 线程。
    pub(super) fn cancel_and_join(&self) -> bool {
        let (queued, in_flight, worker) = {
            let mut state = self.lock();
            state.stopping = true;
            let queued = state.queue.drain(..).collect::<Vec<_>>();
            let in_flight = state.in_flight.take();
            let worker = state.worker.take();
            self.ready.notify_all();
            (queued, in_flight, worker)
        };
        // 在途作业先取消，唤醒阻塞在端口侧的 completion，lane 线程才能退出。
        if let Some(cancellation) = in_flight {
            cancellation.cancel();
        }
        for job in queued {
            job.request.cancellation.cancel_queued(job.cancellation);
            let _ = job.request.response.send(ControlOperationResult {
                operation_id: job.request.operation_id,
                control: job.request.control,
                outcome: ControlDispatchOutcome::outcome_unknown(
                    "player runtime shut down before control completion finished",
                ),
                started_at: Some(job.started_at),
                finished_at: Instant::now(),
            });
        }
        let mut panicked = self.panicked.swap(false, Ordering::AcqRel);
        if let Some(worker) = worker {
            panicked |= worker.join().is_err();
        }
        panicked
    }

    fn lock(&self) -> MutexGuard<'_, ControlCompletionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait<'a>(
        &self,
        state: MutexGuard<'a, ControlCompletionState>,
    ) -> MutexGuard<'a, ControlCompletionState> {
        self.ready
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn run_lane(weak: Weak<ControlCompletionLane>) {
    loop {
        let Some(lane) = weak.upgrade() else {
            return;
        };
        let job = {
            let mut state = lane.lock();
            loop {
                if state.stopping {
                    return;
                }
                if let Some(job) = state.queue.pop_front() {
                    state.in_flight = Some(Arc::clone(&job.request.cancellation));
                    break job;
                }
                state = lane.wait(state);
            }
        };
        let cancellation_handle = Arc::clone(&job.request.cancellation);
        if let Some(cancellation) = job.cancellation {
            cancellation_handle.register(cancellation);
        }
        // 已经超时/取消的作业不再执行：否则命令会在放弃之后迟到生效。
        let outcome = if cancellation_handle.is_cancelled() {
            ControlDispatchOutcome::not_sent(
                "player control was cancelled before its completion ran",
            )
        } else {
            let completion = AssertUnwindSafe(|| (job.completion)());
            match panic::catch_unwind(completion) {
                Ok(outcome) => outcome,
                Err(_) => {
                    lane.panicked.store(true, Ordering::Release);
                    ControlDispatchOutcome::outcome_unknown("player control completion panicked")
                }
            }
        };
        complete_control_request(job.request, outcome, job.started_at);
        lane.lock().in_flight = None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use super::super::{
        BusinessOperationId, ControlDispatch, ControlDispatchOutcome, ControlOperationResult,
        PlayerControl, PlayerControlPort, PlayerLane, PlayerObservationPort,
        PlayerObservationReadError, PlayerOperation, PlayerOperationReceiveError, PlayerRuntime,
        PlayerRuntimeConfig, PlayerSearchError, PlayerSearchPort, RawPlayerSample,
    };

    /// Linux `/proc/self/task/<tid>/comm` 里可读的线程名（前缀匹配）。
    ///
    /// 注意：comm 只保留前 15 字节，`player-control-completion` 与
    /// `player-control-runtime` 都会截断成 `player-control-`，因此按前缀计数。
    #[cfg(target_os = "linux")]
    fn count_named(prefix: &str) -> usize {
        let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
            return 0;
        };
        entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| std::fs::read_to_string(entry.path().join("comm")).ok())
            .filter(|name| name.trim().starts_with(prefix))
            .count()
    }

    struct NoObservation;

    impl PlayerObservationPort for NoObservation {
        fn read_sample(&mut self) -> Result<RawPlayerSample, PlayerObservationReadError> {
            Err(PlayerObservationReadError::new(
                "test observation unavailable",
            ))
        }
    }

    struct NoSearch;

    impl PlayerSearchPort for NoSearch {
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
        ) -> Result<Vec<super::super::SearchCandidate>, PlayerSearchError> {
            Ok(Vec::new())
        }

        fn search_and_pick(
            &mut self,
            _keyword: &str,
            _source: &str,
            _prefer_accompaniment: bool,
        ) -> Result<Option<super::super::PickedCandidate>, PlayerSearchError> {
            Ok(None)
        }
    }

    /// 所有 deferred 完成都卡在同一个闸门上：验证“200 条命令不涨线程”时，
    /// 旧实现会留下 200 条阻塞线程，新实现只有 1 条 lane 线程。
    struct GatedCompletionPort {
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl PlayerControlPort for GatedCompletionPort {
        fn dispatch(&mut self, _control: &PlayerControl) -> ControlDispatch {
            let completion_gate = Arc::clone(&self.gate);
            let cancel_gate = Arc::clone(&self.gate);
            ControlDispatch::deferred_with_cancel(
                move || {
                    let (released, changed) = &*completion_gate;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = changed.wait(released).unwrap();
                    }
                    ControlDispatchOutcome::acknowledged("released")
                },
                move || {
                    let (released, changed) = &*cancel_gate;
                    *released.lock().unwrap() = true;
                    changed.notify_all();
                },
            )
        }
    }

    /// 记录取消是否送达，并让 completion 等取消后才回执。
    struct CancellableControlPort {
        cancelled: Arc<AtomicBool>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl PlayerControlPort for CancellableControlPort {
        fn dispatch(&mut self, _control: &PlayerControl) -> ControlDispatch {
            let cancelled = Arc::clone(&self.cancelled);
            let release = Arc::clone(&self.release);
            let completion_release = Arc::clone(&self.release);
            ControlDispatch::deferred_with_cancel(
                move || {
                    let (released, changed) = &*completion_release;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = changed.wait(released).unwrap();
                    }
                    ControlDispatchOutcome::acknowledged("cancelled")
                },
                move || {
                    cancelled.store(true, Ordering::Release);
                    let (released, changed) = &*release;
                    *released.lock().unwrap() = true;
                    changed.notify_all();
                },
            )
        }
    }

    fn runtime(control: impl PlayerControlPort) -> PlayerRuntime {
        PlayerRuntime::start(
            NoObservation,
            control,
            NoSearch,
            PlayerRuntimeConfig::default(),
        )
        .expect("player runtime should start")
    }

    /// 等待者阻塞在回执通道上时不得占住 `state` 锁：另一线程必须能在 500ms 内
    /// 取到锁并看到 `Awaiting`。变异（把 `recv` 放回锁内）时这里会等满 700ms。
    #[test]
    fn player_wait_does_not_hold_lock() {
        let (responder, receiver) = std::sync::mpsc::sync_channel(1);
        let operation = Arc::new(PlayerOperation::new(
            BusinessOperationId::new(1),
            PlayerLane::Control,
            receiver,
        ));
        let waiter = {
            let operation = Arc::clone(&operation);
            std::thread::spawn(move || operation.wait())
        };
        // 回执故意晚到：旧实现里 waiter 持锁阻塞在 recv 上，主线程会被锁挡住。
        let late_responder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            let _ = responder.send(ControlOperationResult {
                operation_id: BusinessOperationId::new(1),
                control: PlayerControl::Pause,
                outcome: ControlDispatchOutcome::acknowledged("late"),
                started_at: None,
                finished_at: Instant::now(),
            });
        });

        // 给 waiter 一点时间真正进入 recv。
        std::thread::sleep(Duration::from_millis(50));
        let started_at = Instant::now();
        let state = operation.lock_state();
        let waited = started_at.elapsed();
        let awaiting = matches!(*state, super::super::PlayerOperationState::Awaiting);
        drop(state);

        assert!(
            waited < Duration::from_millis(500),
            "wait() 持锁阻塞了 {waited:?}：等待回执期间不应占住 state 锁"
        );
        assert!(awaiting, "wait() 阻塞期间回执通道应已被取走");

        late_responder.join().unwrap();
        assert_eq!(
            waiter.join().unwrap().unwrap().outcome,
            ControlDispatchOutcome::acknowledged("late")
        );
    }

    /// 连发 200 条 deferred 控制命令后，completion 线程数不随命令数增长。
    ///
    /// 旧实现每条命令起一条线程（此处全卡在闸门上，可观测到 ~200 条）；新实现 1 条常驻 lane。
    ///
    /// 阈值 64：`/proc/self/task` 是进程级视图，并行跑时他人 runtime 也贡献同前缀线程（曾假红在 `+8`）；要判别的是 200 vs 1 的数量级爆炸，64 够且抗噪。
    #[cfg(target_os = "linux")]
    #[test]
    fn control_completion_thread_count_is_constant() {
        // comm 截断到 15 字节：前缀同时覆盖 control runtime 与 completion 两条线程，
        // 旧实现下每条命令都会多出一条同前缀线程。
        const NAME: &str = "player-control-";
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let runtime = runtime(GatedCompletionPort {
            gate: Arc::clone(&gate),
        });
        let handle = runtime.handle();
        let before = count_named(NAME);

        let mut operations = Vec::with_capacity(200);
        for index in 0..200u64 {
            let mut attempts = 0;
            let operation = loop {
                match handle
                    .submit_control(BusinessOperationId::new(index + 1), PlayerControl::Pause)
                {
                    Ok(operation) => break operation,
                    Err(error) if attempts < 10_000 => {
                        attempts += 1;
                        std::thread::yield_now();
                        let _ = error;
                    }
                    Err(error) => panic!("submit {index} failed: {error}"),
                }
            };
            operations.push(operation);
        }

        // 旧实现的线程在 dispatch 时立即创建；短窗口轮询峰值，避免命名竞态，
        // 也避免长时间窗口把并行测试里其它 runtime 的线程算进来。
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut peak = count_named(NAME);
        while Instant::now() < deadline {
            let current = count_named(NAME);
            peak = peak.max(current);
            if current > before + 64 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(operations);
        runtime.shutdown().unwrap();

        assert!(
            peak <= before + 64,
            "200 条控制命令后 completion 线程数从 {before} 涨到 {peak}：线程数必须恒定"
        );
    }

    /// 超时语义：`wait()` 在配置了超时后必须置取消并排空迟到回执，
    /// 而不是把回执留在通道里（旧实现只返回 TimedOut）。
    #[test]
    fn control_timeout_sets_cancellation_and_drains_the_reply() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let runtime = runtime(CancellableControlPort {
            cancelled: Arc::clone(&cancelled),
            release: Arc::new((Mutex::new(false), Condvar::new())),
        });
        let handle = runtime
            .handle()
            .with_control_wait_timeout(Duration::from_millis(50));
        let operation = handle
            .submit_control(
                BusinessOperationId::new(7),
                PlayerControl::Play(
                    crate::features::playback::test_track(
                        "miliastra://track/qqmusic/timeout",
                        "timeout",
                    ),
                    true,
                ),
            )
            .unwrap();

        assert_eq!(
            operation.wait(),
            Err(PlayerOperationReceiveError::TimedOut(PlayerLane::Control))
        );
        assert!(
            cancelled.load(Ordering::Acquire),
            "超时后必须把取消送到端口侧，与 UI 侧语义一致"
        );
        // 排空：迟到回执已被消费，因此不会再被当成“待取结果”。
        assert_eq!(
            operation.try_result(),
            Err(PlayerOperationReceiveError::AlreadyCompleted(
                PlayerLane::Control
            ))
        );
        runtime.shutdown().unwrap();
    }
}
